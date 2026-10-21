//! Call-site queries from the JVM facts helper: one batch per file, memoized
//! per (canonical path, blake3 of the bytes on disk), and guarded so a
//! speculative snapshot never inherits facts about text the helper never saw.
//!
//! Every helper here is a scripted `sh -c` fake (the `tests/facts_client.rs`
//! pattern): the Analysis API helper does not exist in this tree yet, so these
//! tests pin our side of the contract and make no coverage claims.

use notlin::cli::JvmFactsMode;
use notlin::facts::classpath::source_hash;
use notlin::facts::client::FactsClient;
use notlin::facts::decorator::FactsProvider;
use notlin::facts::protocol::{Answer, CallSite, Query, SiteStatus};
use notlin::facts::runtime::{FactsRuntime, installed};
use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DISK_SOURCE: &str = "package sample\n\nval oldSite = 1\n";
const NEW_SOURCE: &str = "package sample\n\nval newSite = 2\n";

/// The pinned sites batch, as the fake helper would emit it.
const SITES: &str = r#"[{"byte_offset":42,"text":"list.add(1)","status":"Established","callee_owner":"java.util.ArrayList","callee_name":"add","parameter_types":["Int"],"result_type":"Boolean","result_nullable":false,"jvm_descriptor":"(I)Z"},{"byte_offset":99,"text":"list.map { it.toString() }","status":"Ambiguous","callee_owner":null,"callee_name":"map","parameter_types":[],"result_type":null,"result_nullable":true,"jvm_descriptor":null}]"#;

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "notlin-facts-callsites-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn inner() -> SyntaxSemanticProvider {
    SyntaxSemanticProvider::new([(PathBuf::from("sample/Sample.kt"), DISK_SOURCE.to_string())])
}

/// A fake helper that answers every request with the pinned batch and records
/// each request line. It echoes the `file` it was asked about, so a test can
/// prove which path went on the wire.
fn batch_helper(requests: &Path, sites: &str) -> Command {
    let script = r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> "$REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  file=$(printf '%s' "$line" | sed -n 's/.*"file":"\([^"]*\)".*/\1/p')
  printf '{"version":1,"id":%s,"answer":{"CallSites":{"file":"%s","sites":%s}}}\n' "$id" "$file" "$SITES"
done"#;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .env("REQUESTS", requests)
        .env("SITES", sites);
    command
}

/// A fake helper whose single site's `text` is the file's current bytes (no
/// newlines). That makes "which version of the file was this batch produced
/// from" observable without trusting the memo.
fn echo_helper(requests: &Path) -> Command {
    let script = r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> "$REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  file=$(printf '%s' "$line" | sed -n 's/.*"file":"\([^"]*\)".*/\1/p')
  text=$(tr -d '\n' < "$file")
  printf '{"version":1,"id":%s,"answer":{"CallSites":{"file":"%s","sites":[{"byte_offset":0,"text":"%s","status":"Established","callee_owner":"sample.Holder","callee_name":"add","parameter_types":["kotlin.Int"],"result_type":"kotlin.Boolean","result_nullable":false,"jvm_descriptor":"(I)Z"}]}}}\n' "$id" "$file" "$text"
done"#;
    let mut command = Command::new("sh");
    command.arg("-c").arg(script).env("REQUESTS", requests);
    command
}

fn request_lines(requests: &Path) -> Vec<String> {
    fs::read_to_string(requests)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_file_is_resolved_once_and_the_memo_answers_the_repeat() {
    let root = scratch("memo");
    let requests = root.join("requests.txt");
    let file = root.join("Sample.kt");
    fs::write(&file, DISK_SOURCE).unwrap();
    let runtime = FactsRuntime::with_command(batch_helper(&requests, SITES)).unwrap();

    let first = runtime.call_sites(&file).expect("first batch");
    assert_eq!(first.len(), 2);
    assert_eq!(
        first[0],
        CallSite {
            byte_offset: 42,
            text: "list.add(1)".to_string(),
            status: SiteStatus::Established,
            callee_owner: Some("java.util.ArrayList".to_string()),
            callee_name: Some("add".to_string()),
            parameter_types: vec!["Int".to_string()],
            result_type: Some("Boolean".to_string()),
            result_nullable: false,
            jvm_descriptor: Some("(I)Z".to_string()),
        }
    );
    assert_eq!(first[1].status, SiteStatus::Ambiguous);

    let repeat = runtime.call_sites(&file).expect("memoized batch");
    assert_eq!(repeat, first);

    let lines = request_lines(&requests);
    assert_eq!(
        lines.len(),
        1,
        "an unchanged file must be asked once: {lines:?}"
    );
    // The query names the canonical path, so the helper opens the bytes we
    // hashed rather than a path that could resolve elsewhere.
    let canonical = fs::canonicalize(&file).unwrap();
    assert_eq!(
        lines[0],
        format!(
            "{{\"version\":1,\"id\":0,\"query\":{{\"ResolveCalls\":{{\"file\":\"{}\"}}}}}}",
            canonical.display()
        )
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_changed_file_is_asked_again_and_the_old_batch_is_never_served() {
    let root = scratch("changed");
    let requests = root.join("requests.txt");
    let file = root.join("Sample.kt");
    fs::write(&file, DISK_SOURCE).unwrap();
    let runtime = FactsRuntime::with_command(echo_helper(&requests)).unwrap();

    let old_hash = source_hash(DISK_SOURCE);
    assert_eq!(
        runtime.call_sites(&file).map(|sites| sites[0].text.clone()),
        Some(DISK_SOURCE.replace('\n', ""))
    );

    fs::write(&file, NEW_SOURCE).unwrap();
    assert_eq!(
        runtime.call_sites(&file).map(|sites| sites[0].text.clone()),
        Some(NEW_SOURCE.replace('\n', "")),
        "new disk bytes must produce a new batch, not the memoized one"
    );
    assert_eq!(
        request_lines(&requests).len(),
        2,
        "the rewrite is a new query"
    );

    // The old hash describes text the helper no longer sees: refuse it, and do
    // not answer it from the memo either.
    assert_eq!(runtime.call_site_at(&file, 0, &old_hash), None);

    let new_hash = source_hash(NEW_SOURCE);
    assert_eq!(
        runtime
            .call_site_at(&file, 0, &new_hash)
            .map(|site| site.text),
        Some(NEW_SOURCE.replace('\n', ""))
    );
    assert_eq!(
        request_lines(&requests).len(),
        2,
        "refusals and memo hits must not cost a query"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_snapshot_hash_the_disk_does_not_have_is_refused_without_querying() {
    let root = scratch("overlay");
    let requests = root.join("requests.txt");
    let file = root.join("Sample.kt");
    fs::write(&file, DISK_SOURCE).unwrap();
    let runtime = FactsRuntime::with_command(echo_helper(&requests)).unwrap();

    // A speculative overlay: text the planner holds in memory, never written.
    let overlay_hash = source_hash("package sample\n\nval speculativeEdit = 3\n");
    assert_ne!(overlay_hash, source_hash(DISK_SOURCE));
    assert_eq!(
        runtime.call_site_at(&file, 0, &overlay_hash),
        None,
        "an overlay must never be answered from disk facts"
    );
    assert!(
        request_lines(&requests).is_empty(),
        "refusing an overlay must not even open the helper"
    );

    // The same call with the real disk hash answers, so the refusal above is
    // about the hash and not about the offset or the file.
    assert!(
        runtime
            .call_site_at(&file, 0, &source_hash(DISK_SOURCE))
            .is_some()
    );
    assert_eq!(request_lines(&requests).len(), 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_offset_the_helper_did_not_report_is_none_and_a_missing_file_never_queries() {
    let root = scratch("offsets");
    let requests = root.join("requests.txt");
    let file = root.join("Sample.kt");
    fs::write(&file, DISK_SOURCE).unwrap();
    let runtime = FactsRuntime::with_command(batch_helper(&requests, SITES)).unwrap();

    let hash = source_hash(DISK_SOURCE);
    assert!(runtime.call_site_at(&file, 42, &hash).is_some());
    assert_eq!(
        runtime.call_site_at(&file, 7, &hash),
        None,
        "an offset the helper did not establish is unknown, not a neighbour"
    );

    let missing = root.join("Gone.kt");
    assert_eq!(runtime.call_sites(&missing), None);
    assert_eq!(runtime.call_site_at(&missing, 0, &hash), None);
    assert_eq!(
        request_lines(&requests).len(),
        1,
        "an unreadable file must not reach the helper"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_malformed_or_unpinned_batch_fails_closed_and_is_asked_once() {
    let bad_batches = [
        "not json at all".to_string(),
        format!(
            "{{\"version\":1,\"id\":%s,\"answer\":{{\"CallSites\":{{\"file\":\"%s\",\"sites\":{}}}}}}}",
            SITES.replace("\"Established\"", "\"Probable\"")
        ),
    ];
    for (index, bad) in bad_batches.iter().enumerate() {
        let root = scratch(&format!("bad-{index}"));
        let requests = root.join("requests.txt");
        let file = root.join("Sample.kt");
        fs::write(&file, DISK_SOURCE).unwrap();
        // Percent placeholders keep the fake simple: it substitutes id/file.
        let script = format!(
            r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> "$REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  file=$(printf '%s' "$line" | sed -n 's/.*"file":"\([^"]*\)".*/\1/p')
  printf '{bad}\n' "$id" "$file"
done"#
        );
        let mut command = Command::new("sh");
        command.arg("-c").arg(script).env("REQUESTS", &requests);
        let runtime = FactsRuntime::with_command(command).unwrap();

        assert_eq!(
            runtime.call_sites(&file),
            None,
            "accepted a bad batch: {bad}"
        );
        assert_eq!(
            runtime.call_sites(&file),
            None,
            "a dead helper must stay dead"
        );
        assert_eq!(
            request_lines(&requests).len(),
            1,
            "a failed batch must be memoized, not retried per call site"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn a_client_stays_usable_after_a_malformed_answer() {
    let root = scratch("client");
    let requests = root.join("requests.txt");
    let file = root.join("Sample.kt");
    fs::write(&file, DISK_SOURCE).unwrap();
    // First answer is garbage, every later one is a valid Pong: the failure
    // must not poison the client for the next exchange.
    let script = r#"n=0
while IFS= read -r line; do
  n=$((n+1))
  printf '%s\n' "$line" >> "$REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ "$n" -eq 1 ]; then
    printf 'not json at all\n'
  else
    printf '{"version":1,"id":%s,"answer":{"Pong":{"version":1}}}\n' "$id"
  fi
done"#;
    let mut command = Command::new("sh");
    command.arg("-c").arg(script).env("REQUESTS", &requests);
    let mut client = FactsClient::start_with_command(command).unwrap();

    assert_eq!(
        client.query(Query::ResolveCalls { file: file.clone() }),
        None
    );
    assert_eq!(client.query(Query::Ping), Some(Answer::Pong { version: 1 }));
    assert_eq!(request_lines(&requests).len(), 2);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_decorator_forwards_to_the_run_runtime_and_shares_its_memo() {
    let root = scratch("decorator");
    let requests = root.join("requests.txt");
    let file = root.join("Sample.kt");
    fs::write(&file, DISK_SOURCE).unwrap();
    let runtime = FactsRuntime::with_command(batch_helper(&requests, SITES)).unwrap();
    let provider = FactsProvider::with_runtime(inner(), runtime.clone());

    let via_provider = provider.call_sites(&file).expect("provider batch");
    let via_runtime = runtime.call_sites(&file).expect("runtime batch");
    assert_eq!(via_provider, via_runtime);
    assert_eq!(
        provider
            .call_site_at(&file, 42, &source_hash(DISK_SOURCE))
            .map(|site| site.callee_name),
        Some(Some("add".to_string()))
    );
    assert_eq!(
        request_lines(&requests).len(),
        1,
        "provider and runtime share one memo table"
    );
    // Still a passive decorator: no trait answer changed, and no helper means
    // no call sites rather than an error.
    assert!(provider.has_classpath_facts());
    assert_eq!(FactsProvider::transparent(inner()).call_sites(&file), None);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_uncached_exchange_always_reaches_the_helper() {
    let root = scratch("uncached");
    let requests = root.join("requests.txt");
    let script = r#"while IFS= read -r line; do
  printf '%s\n' "$line" >> "$REQUESTS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  printf '{"version":1,"id":%s,"answer":{"Pong":{"version":1}}}\n' "$id"
done"#;
    let mut command = Command::new("sh");
    command.arg("-c").arg(script).env("REQUESTS", &requests);
    let mut client = FactsClient::start_with_command(command).unwrap();

    // Cached queries collapse; the uncached form is what a content-addressed
    // question uses, so it must reach the helper every time.
    assert!(client.query(Query::Ping).is_some());
    assert!(client.query(Query::Ping).is_some());
    assert_eq!(request_lines(&requests).len(), 1);
    assert!(client.query_uncached(Query::Ping).is_some());
    assert!(client.query_uncached(Query::Ping).is_some());
    assert_eq!(
        request_lines(&requests).len(),
        3,
        "uncached exchanges must not be served from the per-query cache"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn the_off_policy_installs_nothing_and_answers_nothing() {
    let root = scratch("off");
    let requests = root.join("requests.txt");
    let file = root.join("Sample.kt");
    fs::write(&file, DISK_SOURCE).unwrap();
    let provider =
        FactsProvider::from_policy(inner(), JvmFactsMode::Off).expect("off is infallible");

    assert!(!provider.has_helper());
    assert!(
        installed().is_none(),
        "--jvm-facts=off must not install a run runtime"
    );
    assert!(!requests.exists(), "no helper may be started");
    assert_eq!(provider.call_sites(&file), None);
    assert_eq!(
        provider.call_site_at(&file, 0, &source_hash(DISK_SOURCE)),
        None
    );
    fs::remove_dir_all(root).unwrap();
}
