//! Production-CLI proof for helper-backed constructor facts.
//!
//! These tests run the real `notlin` binary against a real directory, with a
//! scripted Track A helper standing in for the JVM one. The jar is deliberately
//! named `…-slim.jar`: that is the production capability decision
//! (`facts::runtime::specifies_full_jar`), so the client records "no Kotlin
//! queries" and never sends `ResolveCalls`. A `ResolveCalls` line reaching the
//! fake makes it exit non-zero, and the journal check below asserts the query
//! was never even attempted — a Track A-only run must not be a Track B run with
//! an empty batch.
//!
//! Nothing here needs a JDK: the emitted Java is checked textually (the real
//! javac/JVM differential lives in `constructor_facts_jvm.rs`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

const STRING_BUILDER: &str =
    include_str!("fixtures/classpath_facts/string_builder_constructors.json");
const BIG_DECIMAL: &str = include_str!("fixtures/classpath_facts/big_decimal_constructors.json");
const ILLEGAL_ARGUMENT: &str =
    include_str!("fixtures/classpath_facts/illegal_argument_constructors.json");
const ILLEGAL_STATE: &str =
    include_str!("fixtures/classpath_facts/illegal_state_constructors.json");

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "notlin-constructor-cli-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn one_line(members: &str) -> String {
    members.lines().map(str::trim).collect()
}

/// The fake Track A helper, plus the journal of requests it received.
///
/// `owners` maps an owner to the member JSON it answers with. Any other owner
/// gets `members: null` — the helper's "cannot load this type" answer, which the
/// Rust side must read as *no facts for that owner*, never as an empty member
/// list. `Ping` is answered; `ResolveCalls` kills the helper so a capability
/// regression fails visibly instead of being answered with a fabricated batch.
fn helper_script(owners: &[(&str, &str)], journal: &Path) -> String {
    let mut arms = String::new();
    for (owner, members) in owners {
        arms.push_str(&format!(
            r#"    *'"owner":"{owner}"'*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Members":{{"owner":"{owner}","members":{members}}}}}}}\n' "$id"
      ;;
"#,
            members = one_line(members)
        ));
    }
    format!(
        r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> '{journal}'
  case "$line" in
    *'"Ping"'*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Pong":{{"version":1}}}}}}\n' "$id"
      ;;
{arms}    *'"ClassMembers"'*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Members":{{"owner":"unknown","members":null}}}}}}\n' "$id"
      ;;
    *'"ResolveCalls"'*)
      exit 3
      ;;
  esac
done
"#,
        journal = journal.display()
    )
}

/// Install the fake helper under a slim-jar name so the production factory
/// treats it as Track A-only, and return (java, jar).
fn install_helper(root: &Path, owners: &[(&str, &str)]) -> (PathBuf, PathBuf) {
    let journal = root.join("journal.jsonl");
    let java = root.join("fake-java");
    fs::write(&java, helper_script(owners, &journal)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&java, fs::Permissions::from_mode(0o700)).unwrap();
    }
    // The helper's own content is irrelevant: `NOTLIN_JAVA` is the fake.
    let jar = root.join("notlin-jvm-facts-slim.jar");
    fs::write(&jar, "scripted Track A helper").unwrap();
    (java, jar)
}

struct Outcome {
    emitted: Option<String>,
    journal: String,
}

/// Run the production binary over `root` with the scripted helper.
fn run(root: &Path, owners: &[(&str, &str)], policy: &str) -> Outcome {
    let (java, jar) = install_helper(root, owners);
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .arg("--root")
        .arg(root)
        .args(["--annotations", "none", "--in-place", "--jvm-facts", policy])
        .arg(root)
        .current_dir(root)
        .env("NOTLIN_JAVA", &java)
        .env_remove("NOTLIN_KOTLIN_SOURCES")
        .env_remove("NOTLIN_CLASSPATH")
        .env_remove("NOTLIN_JVM_FACTS_KOTLIN");
    if policy == "off" {
        command.env_remove("NOTLIN_JVM_FACTS_JAR");
    } else {
        command.env("NOTLIN_JVM_FACTS_JAR", &jar);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "notlin failed ({policy}): {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let journal = fs::read_to_string(root.join("journal.jsonl")).unwrap_or_default();
    let emitted = fs::read_to_string(root.join("Constructors.java")).ok();
    Outcome { emitted, journal }
}

fn real_owners() -> Vec<(&'static str, &'static str)> {
    vec![
        ("java.lang.StringBuilder", STRING_BUILDER),
        ("java.math.BigDecimal", BIG_DECIMAL),
        ("java.lang.IllegalArgumentException", ILLEGAL_ARGUMENT),
        ("java.lang.IllegalStateException", ILLEGAL_STATE),
    ]
}

const SOURCE: &str = r#"package probe

import java.math.BigDecimal
import java.math.MathContext

fun seeded(): StringBuilder = StringBuilder("seed")

fun sized(): StringBuilder = StringBuilder(16)

fun scaled(text: String, context: MathContext): BigDecimal = BigDecimal(text, context)

fun failure(): IllegalArgumentException = IllegalArgumentException("bad")

fun state(): IllegalStateException = IllegalStateException("bad")
"#;

#[test]
fn the_production_cli_sources_new_constructor_overloads_from_a_track_a_helper() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("Constructors.kt"), SOURCE).unwrap();
    let outcome = run(&scratch.0, &real_owners(), "on");
    let java = outcome
        .emitted
        .expect("helper-backed constructors must translate, not be retained");
    for expected in [
        "new java.lang.StringBuilder(\"seed\")",
        "new java.lang.StringBuilder(16)",
        "new java.math.BigDecimal(text, context)",
        "new java.lang.IllegalArgumentException(\"bad\")",
        "new java.lang.IllegalStateException(\"bad\")",
    ] {
        assert!(java.contains(expected), "missing `{expected}`:\n{java}");
    }
    assert!(
        !outcome.journal.trim().is_empty(),
        "the run must have consulted the helper"
    );
    assert!(
        !outcome.journal.contains("ResolveCalls"),
        "a slim Track A helper must never be asked for Kotlin call sites:\n{}",
        outcome.journal
    );
    assert!(
        !scratch.0.join("Constructors.kt").exists()
            || !fs::read_to_string(scratch.0.join("Constructors.kt"))
                .unwrap()
                .contains("StringBuilder(\"seed\")"),
        "a translated declaration must not stay in Kotlin"
    );
}

#[test]
fn the_same_program_under_the_helper_off_floor_keeps_its_constructors() {
    let scratch = Scratch::new();
    fs::write(
        scratch.0.join("Constructors.kt"),
        "package probe\n\nfun seeded(): StringBuilder = StringBuilder(\"seed\")\n\nfun sized(): StringBuilder = StringBuilder(16)\n\nfun scaled(text: String, context: java.math.MathContext): java.math.BigDecimal = java.math.BigDecimal(text, context)\n",
    )
    .unwrap();
    let outcome = run(&scratch.0, &real_owners(), "off");
    assert!(
        outcome.emitted.is_none(),
        "without facts these constructors must stay Kotlin:\n{:?}",
        outcome.emitted
    );
    assert!(
        outcome.journal.is_empty(),
        "an off policy must not talk to a helper:\n{}",
        outcome.journal
    );
    let retained = fs::read_to_string(scratch.0.join("Constructors.kt")).unwrap();
    assert!(retained.contains("StringBuilder(\"seed\")"), "{retained}");
    assert!(retained.contains("StringBuilder(16)"), "{retained}");
}

#[test]
fn guard_owned_spellings_stay_retained_even_with_authoritative_facts() {
    // The helper answers every owner completely, so a bypassed guard would make
    // these translate. Source ownership and unproven argument types must win.
    for source in [
        // A source callable value named like the classifier.
        "package probe\n\nfun build(): Any {\n    val StringBuilder: (String) -> StringBuilder = { StringBuilder(it) }\n    return StringBuilder(\"seed\")\n}\n",
        // An unproven (Any) argument type.
        "package probe\n\nfun build(value: Any): StringBuilder = StringBuilder(value)\n",
        // A foreign owner behind an import.
        "package probe\n\nimport custom.StringBuilder\n\nfun build(): StringBuilder = StringBuilder(\"seed\")\n",
    ] {
        let scratch = Scratch::new();
        fs::write(scratch.0.join("Constructors.kt"), source).unwrap();
        let outcome = run(&scratch.0, &real_owners(), "on");
        assert!(
            outcome.emitted.is_none(),
            "a guard-owned call must not be established from facts: {source}\n{:?}",
            outcome.emitted
        );
        let retained = fs::read_to_string(scratch.0.join("Constructors.kt")).unwrap();
        assert!(retained.contains("StringBuilder"), "{retained}");
    }
}

#[test]
fn a_covered_owner_without_an_applicable_constructor_stays_retained_in_the_cli() {
    let scratch = Scratch::new();
    // Real reflection answer for the owner, except the only declared
    // constructor takes an Int: `StringBuilder("seed")` has no applicable
    // overload, and the table's zero-argument row must not be resurrected.
    let contradictory = "[{\"name\":\"<init>\",\"static_member\":false,\"parameter_types\":[\"int\"],\"result_type\":\"<void>\",\"parameter_nullability\":[\"unknown\"],\"result_nullability\":\"unknown\"}]";
    fs::write(
        scratch.0.join("Constructors.kt"),
        "package probe\n\nfun seeded(): StringBuilder = StringBuilder(\"seed\")\n",
    )
    .unwrap();
    let outcome = run(
        &scratch.0,
        &[("java.lang.StringBuilder", contradictory)],
        "on",
    );
    assert!(
        outcome.emitted.is_none(),
        "an authoritative negative must retain:\n{:?}",
        outcome.emitted
    );
    assert!(
        outcome.journal.contains("java.lang.StringBuilder"),
        "the owner must have been asked about:\n{}",
        outcome.journal
    );
}
