//! Spawn, query, and reap the JVM facts helper. One client per run; the
//! process dies with notlin (kill on drop) and answers are memoized per
//! query for the run. Every failure path degrades to "helper unavailable",
//! which callers map to `FactStatus::Unknown` — never to a planner error,
//! except in `--jvm-facts=on` where startup failure IS the error.

use super::protocol::{PROTOCOL_VERSION, Query, decode_response, encode_request};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub struct FactsClient {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: u64,
    cache: HashMap<Query, super::protocol::Answer>,
    kotlin_queries: bool,
}

impl FactsClient {
    /// Locate the helper jar from `$NOTLIN_JVM_FACTS_JAR`; `None` when unset
    /// (or empty, which is how an unset variable usually arrives from a shell
    /// wrapper).
    pub fn jar_path() -> Option<PathBuf> {
        std::env::var_os("NOTLIN_JVM_FACTS_JAR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    /// java binary: `$NOTLIN_JAVA` wins, else `$JAVA_HOME/bin/java`, else PATH.
    fn java_program() -> String {
        std::env::var("NOTLIN_JAVA")
            .or_else(|_| std::env::var("JAVA_HOME").map(|home| format!("{home}/bin/java")))
            .unwrap_or_else(|_| "java".to_string())
    }

    /// Start the helper jar with the java binary chosen by the environment.
    pub fn start(jar: &Path) -> Result<Self, String> {
        let (command, kotlin_queries) = Self::jar_command_with_mode(jar);
        Self::start_configured_command(command, kotlin_queries)
    }

    /// The `java -jar <helper>` command, before it is spawned. Exposed so the
    /// run-scoped runtime and tests build the process the same way.
    ///
    /// Two things beyond the jar path are decided here:
    ///
    /// * **The run's Kotlin sources and classpath.** They go on the *child's*
    ///   environment, never the parent's: `NOTLIN_KOTLIN_SOURCES` from
    ///   [`super::runtime::configure_kotlin_sources`] (falling back to whatever
    ///   the caller exported) and `NOTLIN_CLASSPATH` from
    ///   [`super::runtime::configure_classpath`]. A child that inherits a
    ///   source list from the shell keeps working when nothing is configured.
    /// * **`--kotlin`, for the full jar only.** Track B is opt-in on the helper
    ///   side; without the flag the full jar answers `ClassMembers` exactly like
    ///   the slim one and never answers `ResolveCalls`. The flag is *not* passed
    ///   to the slim jar (it has no such code) and *not* passed to a full jar
    ///   with no sources — the helper treats that combination as a startup
    ///   failure. In Track A mode the client skips `ResolveCalls` locally:
    ///   absent evidence becomes `Unknown`, never a guess or a blocking read.
    ///   See [`super::runtime::pass_kotlin_flag`].
    pub fn jar_command(jar: &Path) -> Command {
        Self::jar_command_with_mode(jar).0
    }

    /// Keep the capability decision with the command it configures; neither a
    /// second environment read nor a flag-looking jar filename can change it.
    fn jar_command_with_mode(jar: &Path) -> (Command, bool) {
        let mut command = Command::new(Self::java_program());
        command.arg("-jar").arg(jar);
        if let Some(sources) = super::runtime::kotlin_sources_env() {
            command.env("NOTLIN_KOTLIN_SOURCES", sources);
        }
        if let Some(classpath) = super::runtime::classpath_env() {
            command.env("NOTLIN_CLASSPATH", classpath);
        }
        let kotlin_queries = super::runtime::pass_kotlin_flag(jar);
        if kotlin_queries {
            command.arg("--kotlin");
        }
        (command, kotlin_queries)
    }

    /// Test seam: spawn a custom helper implementing all query families.
    /// No argument is passed implicitly; production jar launches use `start`
    /// so their configured capabilities are recorded as well.
    pub fn start_with_command(command: Command) -> Result<Self, String> {
        Self::start_configured_command(command, true)
    }

    fn start_configured_command(
        mut command: Command,
        kotlin_queries: bool,
    ) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("jvm facts helper spawn failed: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "jvm facts helper stdin missing".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "jvm facts helper stdout missing".to_string())?;
        Ok(Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
            next_id: 0,
            cache: HashMap::new(),
            kotlin_queries,
        })
    }

    /// Memoized query. `None` means the helper gave no trustworthy answer
    /// (I/O error, exit, malformed line, version or id mismatch); failures are
    /// never cached, so a later query retries the helper.
    pub fn query(&mut self, query: Query) -> Option<super::protocol::Answer> {
        if let Some(cached) = self.cache.get(&query) {
            return Some(cached.clone());
        }
        let answer = self.query_uncached(query.clone())?;
        self.cache.insert(query, answer.clone());
        Some(answer)
    }

    /// Unsupported queries are absence of evidence, not a dead helper. Check
    /// before allocating an ID or writing: a Track A helper sends no response
    /// to ResolveCalls and would otherwise leave the reader blocked forever.
    pub(super) fn supports_query(&self, query: &Query) -> bool {
        match query {
            Query::ResolveCalls { .. } => self.kotlin_queries,
            Query::ClassMembers { .. } | Query::Ping => true,
        }
    }

    /// Uncached exchange, for questions whose answer depends on state the wire
    /// does not carry. `ResolveCalls` names a file, and the file's contents can
    /// change between two identical queries: the per-query cache would answer
    /// the second one with the batch fetched for the earlier revision. Callers
    /// that memoize on their own content hash (`facts::runtime::call_sites`)
    /// must therefore reach the helper on every new hash.
    pub fn query_uncached(&mut self, query: Query) -> Option<super::protocol::Answer> {
        if !self.supports_query(&query) {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        let line = encode_request(id, query);
        self.exchange(id, &line)
    }

    /// Capability probe: the helper is alive and speaking our protocol version.
    pub fn ping(&mut self) -> bool {
        matches!(
            self.query(Query::Ping),
            Some(super::protocol::Answer::Pong { version }) if version == PROTOCOL_VERSION
        )
    }

    fn exchange(&mut self, id: u64, line: &str) -> Option<super::protocol::Answer> {
        self.stdin.write_all(line.as_bytes()).ok()?;
        self.stdin.flush().ok()?;
        let mut buffer = String::new();
        let read = self.reader.read_line(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        let response = decode_response(&buffer)?;
        (response.id == id).then_some(response.answer)
    }
}

impl Drop for FactsClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
