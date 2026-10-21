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
        Self::start_with_command(Self::jar_command(jar))
    }

    /// The `java -jar <helper>` command, before it is spawned. Exposed so the
    /// run-scoped runtime and tests build the process the same way.
    pub fn jar_command(jar: &Path) -> Command {
        let mut command = Command::new(Self::java_program());
        command.arg("-jar").arg(jar);
        command
    }

    /// Test seam: spawn any program that speaks the line protocol. No argument
    /// is passed implicitly, so callers own the full command line.
    pub fn start_with_command(mut command: Command) -> Result<Self, String> {
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

    /// Uncached exchange, for questions whose answer depends on state the wire
    /// does not carry. `ResolveCalls` names a file, and the file's contents can
    /// change between two identical queries: the per-query cache would answer
    /// the second one with the batch fetched for the earlier revision. Callers
    /// that memoize on their own content hash (`facts::runtime::call_sites`)
    /// must therefore reach the helper on every new hash.
    pub fn query_uncached(&mut self, query: Query) -> Option<super::protocol::Answer> {
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
