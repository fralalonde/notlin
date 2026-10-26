//! Spawn, query, and reap the JVM facts helper. One client per run; the
//! process dies with notlin (kill on drop) and answers are memoized per
//! query for the run. Every failure path degrades to "helper unavailable",
//! which callers map to `FactStatus::Unknown` — never to a planner error,
//! except in `--jvm-facts=on` where startup failure IS the error.
//!
//! # Bounded responses
//!
//! The protocol is one line in, one line out, so a request is only answerable
//! if a line arrives. `read_line` on the child's stdout has no timeout in
//! `std`, and a handler that decides not to answer (a jar launched without the
//! capability its file name advertises, a helper build that predates
//! `NoEvidence`) leaves the caller blocked forever. A dedicated reader thread
//! turns the blocking read into a channel receive with
//! [`DEFAULT_RESPONSE_TIMEOUT`], and a deadline that expires retires the whole
//! client: the child is killed and reaped, the reader thread's read returns
//! instead of staying blocked on a pipe nobody will write to, and no late line
//! can be read as the answer to a later request.
//!
//! The deadline is a fail-closed ceiling, not a health check: it cannot tell a
//! helper that is merely slow (a K2 bootstrap over a large tree on a loaded
//! machine) from one that has stalled, so a request that exceeds it retires the
//! helper either way. What keeps a healthy helper alive is not the deadline but
//! the answer: a helper with nothing to say replies
//! [`super::protocol::Answer::NoEvidence`], which is a fact and keeps it.
//!
//! # Bounded memory
//!
//! The reader queues at most [`ANSWER_QUEUE_DEPTH`] answer lines and buffers
//! at most [`MAX_RESPONSE_BYTES`] bytes of any one line, newline included.
//! Both bounds turn into backpressure on the child: unbidden lines block the
//! reader in `send`, which blocks the child on its own stdout pipe, and a line
//! over the cap is a channel fault (the client retires the helper) instead of
//! an allocation. Leading whitespace counts against the cap, because the cap
//! applies to the bytes on the wire, before `decode_response` trims them.

use super::protocol::{PROTOCOL_VERSION, Query, decode_response, encode_request};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

/// How long one request may go unanswered before the helper is retired.
///
/// Deliberately generous, and fail-closed. The first `ResolveCalls` of a run is
/// sent while the helper is still starting: a JVM launch, the pinned Analysis
/// API jars, and a K2 session build over the whole source tree (about 1.5-2 s
/// on the spike, much more on a large project). The value must clear that
/// comfortably, because the client cannot distinguish a slow helper from a
/// stalled one — it only sees that no line arrived. It is therefore a ceiling
/// on how long a broken helper can block the run, not a promise that a
/// slow-but-healthy one survives: anything slower than this is retired. A
/// healthy helper answers — a batch or
/// [`super::protocol::Answer::NoEvidence`] — instead of going silent.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);

/// Largest single answer line the client will buffer, in bytes, **including**
/// the trailing newline.
///
/// One batch per file is the protocol's unit, and large batches are
/// legitimate: a `CallSites` line is hundreds of KiB of ordinary source. 64
/// MiB leaves generous headroom for that while keeping a broken or hostile
/// helper from making the reader allocate without bound. A line over the cap
/// is a channel fault, not a malformed answer: the client retires the helper
/// instead of trimming the frame. Leading whitespace counts, because the cap
/// is applied to the bytes on the wire before `decode_response` trims them.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// How many answer lines the reader thread may queue ahead of the client.
///
/// The protocol is strictly one line per request, so one slot is enough. The
/// bound only matters for a helper that writes lines nobody asked for: with a
/// bounded queue the reader blocks in [`std::sync::mpsc::SyncSender::send`]
/// and the child blocks on its own stdout pipe, instead of this process
/// growing an in-memory backlog. The receiver going away releases that `send`
/// with an error, so a retired client never waits for the reader.
pub const ANSWER_QUEUE_DEPTH: usize = 1;

pub struct FactsClient {
    child: Child,
    stdin: ChildStdin,
    /// Answer lines, read on a dedicated thread: a read has no timeout, so
    /// the exchange waits on this bounded channel (depth
    /// [`ANSWER_QUEUE_DEPTH`]) with a deadline instead.
    answers: Receiver<std::io::Result<String>>,
    next_id: u64,
    cache: HashMap<Query, super::protocol::Answer>,
    kotlin_queries: bool,
    response_timeout: Duration,
    /// Set once the exchange channel is unusable (write error, read error, EOF,
    /// deadline, or an answer that cannot belong to the request being made).
    /// A failed client never reads again, so a late line from the old process
    /// cannot be mistaken for the next request's answer.
    failed: bool,
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
        Self::start_configured_command(command, kotlin_queries, DEFAULT_RESPONSE_TIMEOUT)
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
        Self::start_configured_command(command, true, DEFAULT_RESPONSE_TIMEOUT)
    }

    /// Test seam: the same all-query custom helper with an explicit response
    /// deadline, so a hang regression is measured in milliseconds instead of
    /// the production deadline. Never unbounded.
    pub fn start_with_command_and_timeout(
        command: Command,
        response_timeout: Duration,
    ) -> Result<Self, String> {
        Self::start_configured_command(command, true, response_timeout)
    }

    fn start_configured_command(
        mut command: Command,
        kotlin_queries: bool,
        response_timeout: Duration,
    ) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("jvm facts helper spawn failed: {error}"))?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("jvm facts helper stdin missing".to_string());
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("jvm facts helper stdout missing".to_string());
            }
        };
        let (sender, answers) = mpsc::sync_channel(ANSWER_QUEUE_DEPTH);
        if let Err(error) = std::thread::Builder::new()
            .name("notlin-jvm-facts-reader".to_string())
            .spawn(move || read_answers(stdout, sender))
        {
            // No reader means every request would wait out its deadline: do not
            // leave the process behind for a client that can never be used.
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("jvm facts helper reader failed: {error}"));
        }
        Ok(Self {
            child,
            stdin,
            answers,
            next_id: 0,
            cache: HashMap::new(),
            kotlin_queries,
            response_timeout,
            failed: false,
        })
    }

    /// Memoized query. `None` means the helper gave no trustworthy answer
    /// (I/O error, exit, malformed line, version or id mismatch); failures are
    /// never cached, so a later query retries the helper.
    ///
    /// An [`super::protocol::Answer::NoEvidence`] answer is *not* cached: it
    /// says nothing about the question in general, only that this request
    /// produced no evidence, and the same question may be answerable later.
    pub fn query(&mut self, query: Query) -> Option<super::protocol::Answer> {
        if let Some(cached) = self.cache.get(&query) {
            return Some(cached.clone());
        }
        let answer = self.query_uncached(query.clone())?;
        if !matches!(answer, super::protocol::Answer::NoEvidence { .. }) {
            self.cache.insert(query, answer.clone());
        }
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
        if self.failed {
            return None;
        }
        if self.stdin.write_all(line.as_bytes()).is_err() || self.stdin.flush().is_err() {
            log::debug!("jvm facts helper stopped accepting requests");
            self.fail();
            return None;
        }
        match self.answers.recv_timeout(self.response_timeout) {
            Ok(Ok(response)) => self.accept(id, &response),
            Ok(Err(error)) => {
                log::debug!("jvm facts helper stdout failed: {error}");
                self.fail();
                None
            }
            Err(RecvTimeoutError::Timeout) => {
                log::warn!(
                    "jvm facts helper did not answer request {id} within {}s; retiring it (a slow helper and a stalled one are indistinguishable here)",
                    self.response_timeout.as_secs()
                );
                self.fail();
                None
            }
            Err(RecvTimeoutError::Disconnected) => {
                log::debug!("jvm facts helper closed its stdout");
                self.fail();
                None
            }
        }
    }

    /// One received line against the request it must answer.
    ///
    /// This is the *frame* policy, one level below the runtime. A line that
    /// does not decode fails *this* request only — `None` — and leaves the
    /// client usable: the reader consumed exactly one line, so the stream is
    /// still aligned and the next exchange starts clean. Whether that ends
    /// Track B for the run is the runtime's decision, and it is stricter:
    /// [`super::runtime::FactsRuntime::query`] treats any `None` from the
    /// client as a dead helper and retires it. A line that decodes to another
    /// id is different even here — an answer we did not ask for means answers
    /// can no longer be matched to requests at all, so the client retires
    /// immediately instead of shifting every later answer by one. The reader
    /// makes that same immediate-retire decision for a frame it refuses to
    /// buffer (over [`MAX_RESPONSE_BYTES`]) or to decode (not valid UTF-8),
    /// because those are channel faults, not malformed answers.
    fn accept(&mut self, id: u64, line: &str) -> Option<super::protocol::Answer> {
        let Some(response) = decode_response(line) else {
            log::debug!("jvm facts helper answered a line that is not a version-1 response");
            return None;
        };
        if response.id != id {
            log::debug!(
                "jvm facts helper answered id {} while {} was pending",
                response.id,
                id
            );
            self.fail();
            return None;
        }
        Some(response.answer)
    }

    /// Retire the exchange channel, kill the child, and reap it now.
    ///
    /// Killing is what unblocks the reader thread's read: the pipe it waits on
    /// closes with the child. Reaping here — not only in `Drop` — matters
    /// because a client can outlive its fault (the runtime drops its own
    /// client promptly, but a direct caller may keep this one): a killed child
    /// that is never waited for stays a zombie for the rest of the run.
    /// [`Child`] caches the exit status, so `Drop` repeating the kill/wait is
    /// harmless and `wait` after a `wait` returns the same status instead of
    /// blocking.
    fn fail(&mut self) {
        self.failed = true;
        self.retire_child();
    }

    /// Kill and reap the helper. Idempotent: on an already-reaped child both
    /// `kill` and `wait` return without touching the process table.
    fn retire_child(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Drain the helper's stdout into the exchange channel, one line per message.
///
/// Runs until stdout reaches EOF (the helper exited or was killed), a line is
/// rejected as oversized or not valid UTF-8, or the client is gone. The sender
/// is dropped on the way out, which is how a blocked exchange learns that no
/// answer is coming. `send` on the bounded channel also blocks while the queue
/// is full, which is the backpressure that keeps unbidden lines out of this
/// process's memory; dropping the receiver releases it with an error.
fn read_answers(stdout: ChildStdout, sender: SyncSender<std::io::Result<String>>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_line_bounded(&mut reader, MAX_RESPONSE_BYTES) {
            Ok(None) => break,
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(line) => {
                    if sender.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = sender.send(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "jvm facts helper answer is not valid UTF-8",
                    )));
                    break;
                }
            },
            Err(error) => {
                let _ = sender.send(Err(error));
                break;
            }
        }
    }
}

/// One newline-terminated line, buffered in at most `cap` bytes.
///
/// `BufRead::read_line` is unbounded: it grows a `String` until it sees a
/// newline, so one bogus line from the helper is an unbounded allocation here.
/// This reads the same single line through `fill_buf`/`consume`, refusing to
/// copy past `cap` bytes with the newline included. Over the cap is an I/O
/// error, not a truncated line: a frame that large means the channel cannot be
/// trusted, and the client retires the helper instead of salvaging it.
fn read_line_bounded(reader: &mut impl BufRead, cap: usize) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if chunk.is_empty() {
            // EOF. A final line without its newline is still a line, exactly as
            // `read_line` returns it; nothing buffered at all is end of stream.
            return Ok((!line.is_empty()).then_some(line));
        }
        let (take, terminated) = match chunk.iter().position(|byte| *byte == b'\n') {
            Some(position) => (position + 1, true),
            None => (chunk.len(), false),
        };
        if line.len() + take > cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("jvm facts helper answer exceeds {cap} bytes"),
            ));
        }
        line.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        if terminated {
            return Ok(Some(line));
        }
    }
}

impl Drop for FactsClient {
    /// Kill and reap. `fail` already did this on the fault path, and `Child`
    /// caches the status, so repeating it here is a no-op rather than a second
    /// wait on a vanished pid.
    fn drop(&mut self) {
        self.retire_child();
    }
}
