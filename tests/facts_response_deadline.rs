//! Bounded helper responses: a silent answer must never hang the run, and an
//! explicit per-request absence must never retire a working helper.
//!
//! Two failure modes are pinned apart on purpose:
//!
//! * **Absence** (`Answer::NoEvidence`) — the helper was asked and answered
//!   "no evidence for this request". The run keeps the helper: a later
//!   `Ping`/`ClassMembers`/indexed `ResolveCalls` still answers, and the
//!   absence is retried rather than memoized for the rest of the run.
//! * **A dead channel** (timeout, EOF, protocol fault) — nothing can be
//!   trusted any more. The helper is killed and reaped, every later query
//!   fails fast, and a late line from the old process can never be read as the
//!   answer to a newer request.
//!
//! The deadline itself is a production safeguard; these tests inject a
//! millisecond-scale one so the regression is fast. The default is pinned
//! separately, because a real full-mode helper pays JVM start plus the K2
//! session build before it answers anything.
#![cfg(unix)]

use notlin::facts::classpath::source_hash;
use notlin::facts::client::{DEFAULT_RESPONSE_TIMEOUT, FactsClient, MAX_RESPONSE_BYTES};
use notlin::facts::protocol::{Answer, Query};
use notlin::facts::runtime::FactsRuntime;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const DISK_SOURCE: &str = "package sample\n\nval oldSite = 1\n";

/// Injected deadline for the scripted helpers below.
const SHORT: Duration = Duration::from_millis(250);

/// Injected deadline for the tests whose fault must be detected by the line
/// cap or by the channel immediately: 30 s is long enough that a rejection
/// inside `ALLOWANCE` cannot be the deadline firing by accident.
const GENEROUS: Duration = Duration::from_secs(30);

/// Upper bound for "bounded": process spawn, a few file writes and teardown on
/// a loaded machine, with no relation to the injected deadline (which is 250 ms)
/// and far below the 900 ms late answer.
const ALLOWANCE: Duration = Duration::from_secs(5);

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    let root = std::env::temp_dir().join(format!(
        "notlin-facts-deadline-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    Scratch(root)
}

fn request_lines(requests: &Path) -> Vec<String> {
    fs::read_to_string(requests)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn resolves_calls(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|line| line.contains("\"ResolveCalls\""))
        .count()
}

/// Answers the first `ResolveCalls` with an absence and every later one with a
/// real batch: the same question becoming answerable is exactly why an absence
/// must not be memoized.
fn recovering_helper(requests: &Path) -> Command {
    let script = r#"n=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"ResolveCalls"'*)
      n=$((n+1))
      if [ "$n" -eq 1 ]; then
        printf '{"version":1,"id":%s,"answer":{"NoEvidence":{"query":"ResolveCalls","reason":"file is not indexed by this session"}}}\n' "$id"
      else
        file=$(printf '%s' "$line" | sed -n 's/.*"file":"\([^"]*\)".*/\1/p')
        printf '{"version":1,"id":%s,"answer":{"CallSites":{"file":"%s","sites":[{"byte_offset":0,"text":"value.uppercase()","status":"Established","callee_owner":"kotlin.text","callee_name":"uppercase","parameter_types":[],"result_type":"kotlin.String","result_nullable":false,"jvm_descriptor":null}]}}}\n' "$id" "$file"
      fi ;;
    *'"Ping":null'*)
      printf '{"version":1,"id":%s,"answer":{"Pong":{"version":1}}}\n' "$id" ;;
  esac
done"#;
    let mut command = Command::new("sh");
    command.arg("-c").arg(script).env("REQUESTS", requests);
    command
}

/// Answers `Ping`/`ClassMembers` and writes nothing for `ResolveCalls` — the
/// pre-fix full helper's behavior for a file it did not index. Records its own
/// pid so the test can prove the client killed and reaped it.
fn silent_resolve_helper(requests: &Path, pidfile: &Path) -> Command {
    let script = r#"echo $$ > "$PIDFILE"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"Ping":null'*)
      printf '{"version":1,"id":%s,"answer":{"Pong":{"version":1}}}\n' "$id" ;;
    *'"ClassMembers"'*)
      owner=$(printf '%s' "$line" | sed -n 's/.*"owner":"\([^"]*\)".*/\1/p')
      printf '{"version":1,"id":%s,"answer":{"Members":{"owner":"%s","members":[]}}}\n' "$id" "$owner" ;;
  esac
done"#;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .env("REQUESTS", requests)
        .env("PIDFILE", pidfile);
    command
}

/// Answers the first request only, after a delay longer than the injected
/// deadline: the late line must not answer anything.
fn late_answer_helper(requests: &Path, delay: &str) -> Command {
    let script = format!(
        r#"first=1
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ "$first" = 1 ]; then
    first=0
    sleep {delay}
    printf '{{"version":1,"id":%s,"answer":{{"Pong":{{"version":1}}}}}}\n' "$id"
  fi
done"#
    );
    let mut command = Command::new("sh");
    command.arg("-c").arg(script).env("REQUESTS", requests);
    command
}

/// Answers everything, and answers `ResolveCalls` with the typed per-request
/// absence the fixed helper emits.
fn no_evidence_helper(requests: &Path) -> Command {
    let script = r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> "$REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"Ping":null'*)
      printf '{"version":1,"id":%s,"answer":{"Pong":{"version":1}}}\n' "$id" ;;
    *'"ClassMembers"'*)
      owner=$(printf '%s' "$line" | sed -n 's/.*"owner":"\([^"]*\)".*/\1/p')
      printf '{"version":1,"id":%s,"answer":{"Members":{"owner":"%s","members":[]}}}\n' "$id" "$owner" ;;
    *'"ResolveCalls"'*)
      printf '{"version":1,"id":%s,"answer":{"NoEvidence":{"query":"ResolveCalls","reason":"file is not indexed by this session"}}}\n' "$id" ;;
  esac
done"#;
    let mut command = Command::new("sh");
    command.arg("-c").arg(script).env("REQUESTS", requests);
    command
}

/// Answers every request with a valid `Pong` preceded by `pad` bytes of
/// whitespace. `decode_response` trims the line, so only the byte cap stops
/// it; nothing but the frame's size is wrong with this answer.
fn padded_pong_helper(requests: &Path, pidfile: &Path, pad: usize) -> Command {
    let script = r#"echo $$ > "$PIDFILE"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  head -c "$PAD" /dev/zero | tr '\0' ' '
  printf '{"version":1,"id":%s,"answer":{"Pong":{"version":1}}}\n' "$id"
done"#;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .env("REQUESTS", requests)
        .env("PIDFILE", pidfile)
        .env("PAD", pad.to_string());
    command
}

/// Answers every request with a line that is not valid UTF-8.
fn invalid_utf8_helper(requests: &Path, pidfile: &Path) -> Command {
    let script = r#"echo $$ > "$PIDFILE"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$REQUESTS"
  printf '\377\376\n'
done"#;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .env("REQUESTS", requests)
        .env("PIDFILE", pidfile);
    command
}

/// Answers the first request with a `Pong`, then writes line after line
/// without being asked: the reader's queue fills and the reader blocks in
/// `send`, which is the state in which `Drop` must still not wait for it.
fn flooding_helper(requests: &Path) -> Command {
    let script = r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> "$REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  printf '{"version":1,"id":%s,"answer":{"Pong":{"version":1}}}\n' "$id"
  while :; do printf 'unsolicited line\n'; done
done"#;
    let mut command = Command::new("sh");
    command.arg("-c").arg(script).env("REQUESTS", requests);
    command
}

fn wait_gone(pid: &str) -> bool {
    let deadline = Instant::now() + ALLOWANCE;
    while Instant::now() < deadline {
        if !process_exists(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Whether `pid` is still a live *or zombie* process. A killed but unreaped
/// child keeps its `/proc` entry until its parent waits for it, so "gone"
/// means "killed and reaped", which is exactly what `FactsClient::fail` and
/// `Drop` owe the run.
///
/// `/proc` is Linux-only; the BSD/macOS probe uses `ps`, which lists zombies
/// too (state `Z`), so the assertion is guarded per platform instead of being
/// skipped on one of them.
#[cfg(target_os = "linux")]
fn process_exists(pid: &str) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_exists(pid: &str) -> bool {
    match Command::new("ps")
        .arg("-o")
        .arg("stat=")
        .arg("-p")
        .arg(pid)
        .output()
    {
        // No such process: `ps` exits non-zero and prints nothing.
        Ok(output) => output.status.success() && !output.stdout.is_empty(),
        // Cannot tell: treat as present so the wait does not pass falsely.
        Err(_) => true,
    }
}

/// Wait until the scripted helper has recorded its pid. The pid file is
/// written at startup, while the spawn and the first query are concurrent, so
/// reading it immediately would race a slow scheduler.
fn wait_for_pid(pidfile: &Path) -> String {
    let deadline = Instant::now() + ALLOWANCE;
    loop {
        if let Ok(text) = fs::read_to_string(pidfile) {
            let pid = text.trim().to_string();
            if !pid.is_empty() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "helper never recorded its pid in {}",
            pidfile.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_default_deadline_accommodates_a_large_k2_bootstrap() {
    assert!(
        DEFAULT_RESPONSE_TIMEOUT >= Duration::from_secs(60),
        "the default response deadline ({DEFAULT_RESPONSE_TIMEOUT:?}) is too tight for a \
         full-mode helper's first batch on a large source tree"
    );
}

#[test]
fn a_silent_answer_is_bounded_and_retires_the_helper() {
    let root = scratch("silent");
    let requests = root.0.join("requests.txt");
    let pidfile = root.0.join("helper.pid");
    let mut client = FactsClient::start_with_command_and_timeout(
        silent_resolve_helper(&requests, &pidfile),
        SHORT,
    )
    .unwrap();

    let started = Instant::now();
    assert_eq!(
        client.query(Query::ResolveCalls {
            file: root.0.join("A.kt")
        }),
        None
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < ALLOWANCE,
        "a silent answer blocked for {elapsed:?} instead of returning at the deadline"
    );

    // A stalled channel is terminal: the next query must not write another
    // request and must not read (there is no answer to read).
    let after_failure = Instant::now();
    assert_eq!(
        client.query(Query::ClassMembers {
            owner: "java.lang.String".into()
        }),
        None
    );
    assert!(
        after_failure.elapsed() < Duration::from_secs(1),
        "a retired client re-blocked on the helper's pipe"
    );
    assert_eq!(
        request_lines(&requests).len(),
        1,
        "a stalled helper must not be asked again"
    );

    // Killed *and* reaped before the client is dropped: a direct
    // `FactsClient` that is kept alive after the fault must not leave a
    // zombie behind. The pid file is written at startup, so wait for it
    // instead of racing the scheduler.
    let pid = wait_for_pid(&pidfile);
    assert!(
        wait_gone(&pid),
        "helper pid {pid} still exists after the deadline while the client is alive: the \
         stalled process was not killed and reaped"
    );
    drop(client);
}

#[test]
fn a_late_answer_cannot_answer_a_later_request() {
    let root = scratch("late");
    let requests = root.0.join("requests.txt");
    let mut client =
        FactsClient::start_with_command_and_timeout(late_answer_helper(&requests, "0.9"), SHORT)
            .unwrap();

    let started = Instant::now();
    assert_eq!(client.query_uncached(Query::Ping), None);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(700),
        "waited {elapsed:?} for a 900 ms late answer instead of abandoning it"
    );

    // The late Pong (id 0) is still in flight. It must never be read as the
    // answer to this request, and it must not make the client look alive.
    assert_eq!(
        client.query(Query::ClassMembers {
            owner: "java.lang.String".into()
        }),
        None,
        "the late answer was accepted for a newer request"
    );
}

#[test]
fn a_timed_out_exchange_retires_the_runtime_unlike_an_absence() {
    let root = scratch("retire");
    let requests = root.0.join("requests.txt");
    let pidfile = root.0.join("helper.pid");
    let file = root.0.join("Sample.kt");
    fs::write(&file, DISK_SOURCE).unwrap();
    let runtime =
        FactsRuntime::with_command_and_timeout(silent_resolve_helper(&requests, &pidfile), SHORT)
            .unwrap();

    // Alive before the silent request: this is a working helper, not a dead one.
    assert!(matches!(
        runtime.query(Query::Ping),
        Some(Answer::Pong { version: 1 })
    ));
    assert_eq!(runtime.call_sites(&file), None);
    // A timeout is a channel failure: the run must stop trusting the helper
    // rather than treat the silence as a per-request absence.
    assert_eq!(
        runtime.member_facts("java.lang.String"),
        None,
        "a stalled helper was still trusted for Track A"
    );
    assert_eq!(
        request_lines(&requests).len(),
        2,
        "the retired runtime must not ask again: {:?}",
        request_lines(&requests)
    );
}

#[test]
fn no_evidence_is_per_request_and_track_a_keeps_working() {
    let root = scratch("absence");
    let requests = root.0.join("requests.txt");
    let file = root.0.join("Sample.kt");
    fs::write(&file, DISK_SOURCE).unwrap();
    let runtime = FactsRuntime::with_command(no_evidence_helper(&requests)).unwrap();

    assert!(matches!(
        runtime.query(Query::Ping),
        Some(Answer::Pong { version: 1 })
    ));

    // Absence, not an empty batch ("looked and saw no calls").
    assert_eq!(runtime.call_sites(&file), None);
    // Retryable: the same question is asked again rather than memoized for the
    // rest of the run. An absence can be transient (a file the helper can now
    // read), and only a channel failure is terminal.
    assert_eq!(runtime.call_sites(&file), None);
    assert_eq!(
        runtime.call_site_at(&file, 0, &source_hash(DISK_SOURCE)),
        None
    );

    // Track A survives every one of those absences.
    let members = runtime
        .member_facts("java.lang.String")
        .expect("a per-request absence must not retire the helper");
    assert!(members.is_empty());
    assert!(matches!(
        runtime.query(Query::Ping),
        Some(Answer::Pong { version: 1 })
    ));

    let lines = request_lines(&requests);
    assert_eq!(
        resolves_calls(&lines),
        3,
        "each absence must reach the helper again (two batch asks and one \
         hash-guarded ask): {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("\"ClassMembers\"")),
        "Track A was never asked: {lines:?}"
    );
}

#[test]
fn an_absence_is_retried_and_the_later_answer_is_used() {
    let root = scratch("recovering");
    let requests = root.0.join("requests.txt");
    let file = root.0.join("Sample.kt");
    fs::write(&file, DISK_SOURCE).unwrap();
    let runtime = FactsRuntime::with_command(recovering_helper(&requests)).unwrap();

    assert_eq!(
        runtime.call_sites(&file),
        None,
        "the first answer is an absence"
    );
    let sites = runtime
        .call_sites(&file)
        .expect("a retried absence must be able to become an answer");
    assert_eq!(sites.len(), 1);
    assert_eq!(sites[0].callee_name.as_deref(), Some("uppercase"));

    // A real batch *is* memoized, so retrying an absence does not turn every
    // later call site into another exchange.
    assert_eq!(runtime.call_sites(&file), Some(sites));
    assert_eq!(
        resolves_calls(&request_lines(&requests)),
        2,
        "one absence, one answer, then the memo"
    );
}

#[test]
fn an_oversized_answer_frame_is_rejected_by_the_cap_and_reaps_the_helper() {
    assert!(
        MAX_RESPONSE_BYTES >= 8 * 1024 * 1024,
        "the answer cap ({MAX_RESPONSE_BYTES} bytes) is too tight for one per-file batch"
    );
    let root = scratch("oversize");
    let requests = root.0.join("requests.txt");
    let pidfile = root.0.join("helper.pid");
    // Valid JSON, one byte over the documented cap. `decode_response` trims
    // leading whitespace, so only the reader's byte budget can reject it: an
    // unbounded `read_line` accepts this frame as a Pong.
    let mut client = FactsClient::start_with_command_and_timeout(
        padded_pong_helper(&requests, &pidfile, MAX_RESPONSE_BYTES + 1),
        GENEROUS,
    )
    .unwrap();
    let pid = wait_for_pid(&pidfile);

    let started = Instant::now();
    assert_eq!(
        client.query_uncached(Query::Ping),
        None,
        "an answer line over {MAX_RESPONSE_BYTES} bytes was accepted as a Pong"
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < ALLOWANCE,
        "the oversized frame took {elapsed:?}: the cap must reject it well before the \
         {GENEROUS:?} deadline"
    );

    // Over the cap is a channel fault, not a per-request miss: the helper is
    // retired, so no later response can be accepted.
    assert_eq!(client.query_uncached(Query::Ping), None);
    assert_eq!(
        request_lines(&requests).len(),
        1,
        "the retired client asked again after the oversized frame"
    );

    // Killed *and* reaped while the client is still alive: the fault path owns
    // the reap, `Drop` only has to stay correct afterwards.
    assert!(
        wait_gone(&pid),
        "helper pid {pid} still exists after the oversized frame: the fault was not reaped \
         before the client was dropped"
    );
    drop(client);
}

#[test]
fn an_invalid_utf8_answer_is_a_channel_fault_and_reaps_the_helper() {
    let root = scratch("utf8");
    let requests = root.0.join("requests.txt");
    let pidfile = root.0.join("helper.pid");
    let mut client = FactsClient::start_with_command_and_timeout(
        invalid_utf8_helper(&requests, &pidfile),
        GENEROUS,
    )
    .unwrap();
    let pid = wait_for_pid(&pidfile);

    let started = Instant::now();
    assert_eq!(
        client.query_uncached(Query::Ping),
        None,
        "a line that is not valid UTF-8 was accepted as an answer"
    );
    assert!(
        started.elapsed() < ALLOWANCE,
        "the undecodable line was not reported as an I/O fault promptly"
    );
    assert_eq!(client.query_uncached(Query::Ping), None);
    assert_eq!(
        request_lines(&requests).len(),
        1,
        "the retired client asked again after the undecodable line"
    );
    assert!(
        wait_gone(&pid),
        "helper pid {pid} still exists after the undecodable line: the fault was not reaped \
         before the client was dropped"
    );
    drop(client);
}

#[test]
fn a_large_answer_under_the_cap_is_still_accepted() {
    let root = scratch("under-cap");
    let requests = root.0.join("requests.txt");
    let pidfile = root.0.join("helper.pid");
    // 4 MiB of leading padding plus a Pong: the cap must bound a broken
    // helper, not shrink what one legitimate batch may carry.
    let pad = 4 * 1024 * 1024;
    let mut client = FactsClient::start_with_command_and_timeout(
        padded_pong_helper(&requests, &pidfile, pad),
        GENEROUS,
    )
    .unwrap();

    let started = Instant::now();
    assert!(
        matches!(
            client.query_uncached(Query::Ping),
            Some(Answer::Pong { version: 1 })
        ),
        "a legal answer line under the cap was rejected"
    );
    assert!(
        started.elapsed() < ALLOWANCE,
        "reading a legal {pad}-byte line took {:?}",
        started.elapsed()
    );
    // The stream is still aligned: a second exchange gets its own answer.
    assert!(matches!(
        client.query_uncached(Query::Ping),
        Some(Answer::Pong { version: 1 })
    ));
}

#[test]
fn dropping_a_client_with_a_queued_backlog_returns_promptly() {
    let root = scratch("backlog");
    let requests = root.0.join("requests.txt");
    let mut client =
        FactsClient::start_with_command_and_timeout(flooding_helper(&requests), GENEROUS).unwrap();
    assert!(
        matches!(
            client.query_uncached(Query::Ping),
            Some(Answer::Pong { version: 1 })
        ),
        "the answered request must still be read before the flood"
    );
    // Let the reader fill its bounded queue and block in `send`: the state in
    // which a `Drop` that waited for the reader would hang. The receiver going
    // away is what must release it, and `Drop` must not join it.
    std::thread::sleep(Duration::from_millis(150));
    let dropped = Instant::now();
    drop(client);
    let elapsed = dropped.elapsed();
    assert!(
        elapsed < ALLOWANCE,
        "Drop took {elapsed:?} with the reader blocked on a full answer queue"
    );
}
