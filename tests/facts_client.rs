//! The helper client against scripted fakes: the line protocol, memoization,
//! and every failure path degrading to "no answer" instead of a panic.

use notlin::facts::client::FactsClient;
use notlin::facts::protocol::{Answer, Query};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("notlin-facts-client-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// A fake helper: records every request line it receives, answers each one
/// with the given stdout line, and exits when stdin closes.
fn fake_helper(count_path: &Path, answer_line: &str) -> Command {
    let script = r#"n=0
while read -r line; do
  n=$((n+1))
  printf '%s\n' "$line" >> "$COUNT_FILE"
  printf '%s\n' "$ANSWER_LINE"
done"#;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .env("COUNT_FILE", count_path)
        .env("ANSWER_LINE", answer_line);
    command
}

fn received_lines(count_path: &Path) -> Vec<String> {
    std::fs::read_to_string(count_path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

const PONG_ID_0: &str = "{\"version\":1,\"id\":0,\"answer\":{\"Pong\":{\"version\":1}}}";

#[test]
fn ping_round_trips_and_repeats_hit_the_cache() {
    let root = scratch("ping");
    let count = root.join("received.txt");
    let mut client = FactsClient::start_with_command(fake_helper(&count, PONG_ID_0)).unwrap();

    let first = client.query(Query::Ping).expect("first ping answered");
    assert_eq!(first, Answer::Pong { version: 1 });
    assert!(client.ping());

    let second = client.query(Query::Ping).expect("cached ping answered");
    assert_eq!(second, first);

    let lines = received_lines(&count);
    assert_eq!(
        lines.len(),
        1,
        "the repeat must not reach the helper: {lines:?}"
    );
    // Contract: the id starts at 0 and the ping line is byte-exact.
    assert_eq!(
        lines[0],
        "{\"version\":1,\"id\":0,\"query\":{\"Ping\":null}}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn ids_advance_per_query() {
    let root = scratch("ids");
    let count = root.join("received.txt");
    // Echo the incoming id back by echoing the request unchanged is not valid
    // JSON for a response, so use a fixed-id fake and accept the failure: the
    // point here is that the client keeps asking instead of caching a failure.
    let mut client = FactsClient::start_with_command(fake_helper(&count, PONG_ID_0)).unwrap();
    assert!(client.query(Query::Ping).is_some());
    assert!(
        client
            .query(Query::ClassMembers {
                owner: "java.util.UUID".into()
            })
            .is_none()
    );
    assert!(
        client
            .query(Query::ClassMembers {
                owner: "java.util.UUID".into()
            })
            .is_none()
    );
    let lines = received_lines(&count);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(
        lines[1],
        "{\"version\":1,\"id\":1,\"query\":{\"ClassMembers\":{\"owner\":\"java.util.UUID\"}}}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn mismatched_ids_are_rejected_without_panicking() {
    let root = scratch("mismatch");
    let count = root.join("received.txt");
    let mut client = FactsClient::start_with_command(fake_helper(
        &count,
        "{\"version\":1,\"id\":7,\"answer\":{\"Pong\":{\"version\":1}}}",
    ))
    .unwrap();
    assert!(client.query(Query::Ping).is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_and_version_mismatched_answers_are_rejected() {
    for answer in [
        "not json at all",
        "{\"version\":999,\"id\":0,\"answer\":{\"Pong\":{\"version\":1}}}",
        "",
    ] {
        let root = scratch("malformed");
        let count = root.join("received.txt");
        let mut client = FactsClient::start_with_command(fake_helper(&count, answer)).unwrap();
        assert!(client.query(Query::Ping).is_none(), "accepted {answer:?}");
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn a_helper_that_exits_immediately_yields_no_answer() {
    let root = scratch("exit");
    let mut command = Command::new("sh");
    command.arg("-c").arg("exit 0");
    let mut client = FactsClient::start_with_command(command).unwrap();
    assert!(client.query(Query::Ping).is_none());
    assert!(!client.ping());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_missing_program_is_a_start_error() {
    let command = Command::new("notlin-definitely-not-a-helper");
    let error = FactsClient::start_with_command(command)
        .err()
        .expect("a missing program must not start");
    assert!(error.contains("spawn failed"), "{error}");
}
