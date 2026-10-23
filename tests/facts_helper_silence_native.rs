//! Native regressions: a *real* helper must answer a request it cannot
//! satisfy, and keep serving afterwards.
//!
//! Two real jars, two shapes of "cannot answer":
//!
//! * **full jar, unindexed file** — the session has no PSI for the file. The
//!   pre-fix helper dropped the request without writing a line, so the Rust
//!   client's `read_line` blocked for the rest of the run even though the
//!   helper was alive and Track A still worked.
//! * **slim jar asked for `ResolveCalls`** — the jar has no Track B at all. A
//!   launch configuration that claims otherwise (a renamed jar, a forced
//!   `NOTLIN_JVM_FACTS_KOTLIN`, any future capability guess) reaches this
//!   helper with a query it cannot implement, which is exactly what launch-time
//!   gating cannot protect against.
//!
//! Each probe runs in a bounded child process (the `facts_helper_capabilities`
//! pattern) so a regression is a failing test in seconds, never a hung Cargo
//! suite, and each drives the real Java bytes through the real client, so the
//! wire contract is pinned end to end: an unparsable or unmatched answer would
//! retire the client and the later `ClassMembers`/`Ping` assertions would fail.
#![cfg(unix)]

use notlin::facts::client::FactsClient;
use notlin::facts::protocol::{Answer, Query, SiteStatus};
use notlin::facts::runtime::{self, FactsRuntime};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A file the session indexes. One call, so the batch has a real `Established`
/// site to prove the indexed path still works after the unanswerable request.
const INDEXED_SOURCE: &str =
    "package probe\nclass Indexed { fun convert(value: String): String = value.uppercase() }\n";

/// Readable and valid Kotlin, but outside `NOTLIN_KOTLIN_SOURCES`, so the
/// session never indexes it. This is the existing readable-but-unindexed file
/// the pre-fix helper went silent on.
const OUTSIDE_SOURCE: &str =
    "package external\nclass Outside { fun convert(value: String): String = value.lowercase() }\n";

/// Generous: JVM start plus the K2 session build, with room for a loaded
/// machine. The point is that the probe is *bounded*, not that it is tight.
const PROBE_TIMEOUT: Duration = Duration::from_secs(45);

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    let root = std::env::temp_dir().join(format!(
        "notlin-helper-silence-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    Scratch(root)
}

/// An optional helper jar this tree just built. `None` means it was never built
/// (`bash jvm-facts/build.sh --full`); the whole Track B suite needs it, so a
/// native probe skips instead of failing an unrelated gate.
fn jar_path(file_name: &str) -> Option<PathBuf> {
    let jar = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("jvm-facts/build")
        .join(file_name);
    jar.is_file().then_some(jar)
}

/// `$NOTLIN_JAVA`, else the JDK this repository pins for the helper, else PATH.
fn java_program() -> PathBuf {
    if let Some(configured) = std::env::var_os("NOTLIN_JAVA").filter(|value| !value.is_empty()) {
        return PathBuf::from(configured);
    }
    let pinned = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join(".rsdk/tools/java/25.0.4-tem/bin/java");
    if pinned.is_file() {
        return pinned;
    }
    PathBuf::from("java")
}

/// Runs one `#[ignore]`d probe in a child process and fails the test if it is
/// still running at [`PROBE_TIMEOUT`].
fn run_bounded_probe(probe: &str, root: &Path, jar: &Path) {
    let output = root.join("probe.log");
    let log = fs::File::create(&output).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", probe, "--ignored", "--nocapture"])
        .env("PROBE_ROOT", root)
        .env("PROBE_JAR", jar)
        .env("NOTLIN_JAVA", java_program())
        // Per-child scrub: an inherited helper configuration must not decide
        // this probe's launch mode.
        .env_remove("NOTLIN_JVM_FACTS_JAR")
        .env_remove("NOTLIN_KOTLIN_SOURCES")
        .env_remove("NOTLIN_CLASSPATH")
        .env_remove("NOTLIN_JVM_FACTS_KOTLIN")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let captured = fs::read_to_string(&output).unwrap_or_default();
    assert!(
        status.is_some(),
        "{probe} left the client blocked on an unanswerable request for {}s; the \
         request must be answered (absence) or the wait bounded:\n{captured}",
        PROBE_TIMEOUT.as_secs()
    );
    assert!(status.unwrap().success(), "{probe} failed:\n{captured}");
}

fn probe_root() -> PathBuf {
    PathBuf::from(std::env::var_os("PROBE_ROOT").expect("bounded parent supplies root"))
}

fn probe_jar() -> PathBuf {
    PathBuf::from(std::env::var_os("PROBE_JAR").expect("bounded parent supplies jar"))
}

#[test]
fn an_unindexed_readable_file_is_bounded_and_later_requests_still_answer() {
    let Some(jar) = jar_path("notlin-jvm-facts-full.jar") else {
        eprintln!(
            "skipping native helper probe: jvm-facts/build/notlin-jvm-facts-full.jar is not built"
        );
        return;
    };
    let root = scratch("full");
    let indexed = root.0.join("indexed");
    fs::create_dir_all(&indexed).unwrap();
    fs::write(indexed.join("Indexed.kt"), INDEXED_SOURCE).unwrap();
    fs::write(root.0.join("Outside.kt"), OUTSIDE_SOURCE).unwrap();
    run_bounded_probe("native_full_probe", &root.0, &jar);
}

#[test]
fn a_helper_without_the_query_capability_is_bounded_and_keeps_track_a() {
    let Some(jar) = jar_path("notlin-jvm-facts.jar") else {
        eprintln!("skipping native slim probe: jvm-facts/build/notlin-jvm-facts.jar is not built");
        return;
    };
    let root = scratch("slim");
    fs::write(root.0.join("Sample.kt"), OUTSIDE_SOURCE).unwrap();
    run_bounded_probe("native_slim_probe", &root.0, &jar);
}

#[test]
#[ignore = "bounded subprocess entry point, executed by the native silence tests"]
fn native_full_probe() {
    let root = probe_root();
    let jar = probe_jar();
    let indexed_root = root.join("indexed");
    let indexed = indexed_root.join("Indexed.kt");
    let outside = root.join("Outside.kt");
    // Configured, not exported: the child helper configuration is run state,
    // never a mutation of this process's environment.
    runtime::configure_kotlin_sources([&indexed_root]);

    let runtime = FactsRuntime::start(&jar).expect("full helper starts");
    assert!(
        matches!(
            runtime.query(Query::Ping),
            Some(Answer::Pong { version: 1 })
        ),
        "helper did not answer Ping"
    );

    // 1. The unanswerable request. Must return, and must not be an empty batch:
    //    an empty batch would claim "looked and found no calls".
    let started = Instant::now();
    let absent = runtime.call_sites(&outside);
    let elapsed = started.elapsed();
    assert_eq!(
        absent, None,
        "an unindexed file must be absence of evidence, not a batch"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "absence took {elapsed:?}: the helper must answer, not go silent"
    );
    eprintln!("unindexed request answered in {elapsed:?}");

    // 2. Track A survives a per-request absence.
    let members = runtime
        .member_facts("java.lang.String")
        .expect("ClassMembers must still answer after an absence");
    assert!(!members.is_empty(), "Track A returned no members");
    eprintln!("class members after absence: {}", members.len());

    // 3. An indexed file still resolves: the session was not damaged by the
    //    unanswered request.
    let sites = runtime
        .call_sites(&indexed)
        .expect("indexed batch must still answer after an absence");
    assert!(
        sites
            .iter()
            .any(|site| site.status == SiteStatus::Established),
        "indexed batch carried no established site: {sites:?}"
    );
    eprintln!("indexed sites after absence: {}", sites.len());

    // 4. Still one live helper, still speaking the protocol.
    assert!(
        matches!(
            runtime.query(Query::Ping),
            Some(Answer::Pong { version: 1 })
        ),
        "helper died after answering an absence"
    );
}

#[test]
#[ignore = "bounded subprocess entry point, executed by the native silence tests"]
fn native_slim_probe() {
    let root = probe_root();
    let jar = probe_jar();
    let file = root.join("Sample.kt");

    // The generic all-query seam deliberately records no capability: it models a
    // launch configuration that *claims* Track B (a renamed jar, a forced
    // `NOTLIN_JVM_FACTS_KOTLIN`, a future guess) and hands the helper a query it
    // cannot implement. The answer protocol, not the launch decision, keeps the
    // run moving here.
    let runtime =
        FactsRuntime::with_command(FactsClient::jar_command(&jar)).expect("slim helper starts");

    assert!(
        matches!(
            runtime.query(Query::Ping),
            Some(Answer::Pong { version: 1 })
        ),
        "slim helper did not answer Ping"
    );
    let started = Instant::now();
    assert_eq!(
        runtime.call_sites(&file),
        None,
        "an unimplemented query must be absence of evidence, not a batch"
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the unimplemented query was not answered"
    );
    eprintln!("unimplemented query answered in {:?}", started.elapsed());

    // Track A and the protocol loop are untouched by the declined query.
    let members = runtime
        .member_facts("java.lang.String")
        .expect("ClassMembers must still answer on a slim helper");
    assert!(!members.is_empty());
    assert!(matches!(
        runtime.query(Query::Ping),
        Some(Answer::Pong { version: 1 })
    ));
    eprintln!("class members after declined query: {}", members.len());
}
