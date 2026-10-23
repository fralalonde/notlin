//! Production-CLI proof for helper-backed Java instance member calls.
//!
//! The real `notlin` binary runs against a scripted helper standing in for the
//! JVM one. The helper is named `…-full.jar` and `NOTLIN_KOTLIN_SOURCES` is
//! set, so the client actually asks `ResolveCalls`; the batch is scripted per
//! test. This pins the *gate* (a helper fact call becomes a verbatim Java
//! member call only with the complete slice-1 proof) without needing a JDK.
//!
//! The real-helper/real-javac differential lives in `java_member_call_jvm.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

const STREAM_MEMBERS: &str = include_str!("fixtures/classpath_facts/stream_members.json");
const STREAM_SORTED_THROWS: &str =
    include_str!("fixtures/classpath_facts/stream_sorted_throws_members.json");

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "notlin-java-member-cli-{}-{id}",
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

/// Byte offset of the `member(...)` selector in `source`.
fn selector_offset(source: &str, member: &str) -> u64 {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression"
            && let Some(offset) = notlin::semantics::kotlin_call_site_offset(node)
            && source[offset..].starts_with(&format!("{member}("))
        {
            return offset as u64;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    panic!("no `{member}` call in fixture");
}

fn site_line(
    offset: u64,
    owner: &str,
    name: &str,
    result: &str,
    nullable: bool,
    descriptor: &str,
    text: &str,
) -> String {
    format!(
        r#"[{{"byte_offset":{offset},"text":"{text}","status":"Established","callee_owner":"{owner}","callee_name":"{name}","parameter_types":[],"result_type":"{result}","result_nullable":{nullable},"jvm_descriptor":"{descriptor}"}}]"#
    )
}

/// Installs the scripted helper under a full-jar name and a fake `java`
/// launcher. `owners` is the `(owner, members)` table the helper answers;
/// every other owner gets `members: null` (the helper's Unknown). Returns
/// (java, jar, journal).
fn install_helper(
    root: &Path,
    sites: &str,
    owners: &[(&str, &str)],
) -> (PathBuf, PathBuf, PathBuf) {
    let journal = root.join("journal.jsonl");
    let java = root.join("fake-java");
    let mut arms = String::new();
    for (owner, members) in owners {
        arms.push_str(&format!(
            "        '{owner}')\n          printf '{{\"version\":1,\"id\":%s,\"answer\":{{\"Members\":{{\"owner\":\"%s\",\"members\":%s}}}}}}\\n' \"$id\" \"$asked\" '{members}'\n          ;;\n",
            members = one_line(members)
        ));
    }
    let script = format!(
        r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> '{journal}'
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"Ping"'*)
      printf '{{"version":1,"id":%s,"answer":{{"Pong":{{"version":1}}}}}}\n' "$id"
      ;;
    *'"ResolveCalls"'*)
      file=$(printf '%s' "$line" | sed -n 's/.*"file":"\([^"]*\)".*/\1/p')
      printf '{{"version":1,"id":%s,"answer":{{"CallSites":{{"file":"%s","sites":%s}}}}}}\n' "$id" "$file" '{sites}'
      ;;
    *'"ClassMembers"'*)
      asked=$(printf '%s' "$line" | sed -n 's/.*"owner":"\([^"]*\)".*/\1/p')
      case "$asked" in
{arms}        *)
          printf '{{"version":1,"id":%s,"answer":{{"Members":{{"owner":"%s","members":null}}}}}}\n' "$id" "$asked"
          ;;
      esac
      ;;
  esac
done
"#,
        journal = journal.display(),
        sites = sites,
        arms = arms,
    );
    fs::write(&java, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&java, fs::Permissions::from_mode(0o700)).unwrap();
    }
    // The file's own bytes are irrelevant: NOTLIN_JAVA is the script.
    let jar = root.join("notlin-jvm-facts-full.jar");
    fs::write(&jar, "scripted full helper").unwrap();
    (java, jar, journal)
}

struct Outcome {
    emitted: Option<String>,
    retained: Option<String>,
    journal: String,
    /// The CLI's own diagnostics, kept for failure messages.
    stderr: String,
}

fn run(root: &Path, policy: &str, sites: &str, owner: &str, members: &str, name: &str) -> Outcome {
    run_owners(root, policy, sites, &[(owner, members)], &[], name)
}

fn run_owners(
    root: &Path,
    policy: &str,
    sites: &str,
    owners: &[(&str, &str)],
    flags: &[&str],
    name: &str,
) -> Outcome {
    let (java, jar, journal) = install_helper(root, sites, owners);
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .arg("--root")
        .arg(root)
        .args(["--annotations", "none", "--in-place", "--jvm-facts", policy])
        .args(flags)
        .arg(root)
        .current_dir(root)
        .env("NOTLIN_JAVA", &java)
        .env("NOTLIN_KOTLIN_SOURCES", root)
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
    let journal = fs::read_to_string(&journal).unwrap_or_default();
    let emitted = fs::read_to_string(root.join(format!("{name}.java"))).ok();
    let retained = fs::read_to_string(root.join(format!("{name}.kt"))).ok();
    Outcome {
        emitted,
        retained,
        journal,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The admissible slice-1 shape: a proved call whose value is discarded.
///
/// A `Stream<Int>` *exit* is deliberately not signed in this slice — erasure
/// equality does not prove the instantiated result — so the positive fixture
/// discards the value, which no consumer can observe.
const STREAM_SOURCE: &str = r#"package probe

fun sorted(s: java.util.stream.Stream<String>) {
    s.sorted()
}
"#;

/// The retained shape: the same proved call returned through a parameterized
/// reference. Kept as an explicit, permanent loss record (see
/// `the_production_cli_retains_a_parameterized_stream_exit`).
const STREAM_EXIT_SOURCE: &str = r#"package probe

fun sorted(s: java.util.stream.Stream<String>): java.util.stream.Stream<String> = s.sorted()
"#;

fn stream_sites(source: &str) -> String {
    site_line(
        selector_offset(source, "sorted"),
        "java.util.stream.Stream",
        "sorted",
        "java.util.stream.Stream",
        false,
        "()Ljava/util/stream/Stream;",
        "sorted()",
    )
}

#[test]
fn the_production_cli_retains_a_receipted_interface_in_both_policies() {
    let source = "package probe\n\nimport java.util.stream.Stream\n\ninterface Endpoint {\n    fun touch(values: Stream<String>) {\n        values.sorted()\n    }\n}\n";
    let scratch = Scratch::new();
    for (label, flags) in [
        ("strict", &[][..]),
        ("approximations", &["--allow-approximations"][..]),
    ] {
        let root = scratch.0.join(label);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Endpoint.kt"), source).unwrap();
        let outcome = run_owners(
            &root,
            "on",
            &stream_sites(source),
            &[("java.util.stream.Stream", STREAM_MEMBERS)],
            flags,
            "Endpoint",
        );
        assert!(
            outcome.emitted.is_none(),
            "{label}: an unproved interface ABI was emitted: {:?}",
            outcome.emitted
        );
        let retained = outcome.retained.expect("the interface must remain Kotlin");
        assert!(retained.contains("interface Endpoint") && retained.contains("values.sorted()"));
        assert!(
            retained.contains("interface") && retained.contains("not proved"),
            "{label}: missing interface contract refusal:\n{retained}"
        );
    }
}

#[test]
fn the_production_cli_preserves_receipt_coverage_across_comment_trivia() {
    let sources = [
        "package probe\n\nimport java.util.stream.Stream\n\nfun touch(values: Stream<String>) {\n    // before\n    values.sorted()\n    /* after */\n}\n",
        "package probe\n\nimport java.util.stream.Stream\n\nclass Commented {\n    // member\n    fun touch(\n        /* parameter */ values: Stream<String>\n    ) {\n        /* before */\n        values.sorted()\n        // after\n    }\n}\n",
        "package probe\n\nimport java.util.stream.Stream\n\nfun touch(values: Stream<String>): Any? {\n    return /* value */ (values.sorted())\n}\n",
    ];
    let scratch = Scratch::new();
    for (index, source) in sources.iter().enumerate() {
        let root = scratch.0.join(index.to_string());
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Commented.kt"), source).unwrap();
        let outcome = run(
            &root,
            "on",
            &stream_sites(source),
            "java.util.stream.Stream",
            STREAM_MEMBERS,
            "Commented",
        );
        let java = outcome.emitted.unwrap_or_else(|| {
            panic!(
                "comment trivia must not refuse fixture {index}: {:?}",
                outcome.retained
            )
        });
        assert!(
            java.contains("values.sorted()") && !java.contains(".stream().sorted()"),
            "{java}"
        );
    }
}

#[test]
fn the_production_cli_preserves_comment_trivia_inside_nested_parentheses() {
    let scratch = Scratch::new();
    let expressions = [
        "(/* value */ values.sorted())",
        "(\n        // value\n        values.sorted()\n    )",
        "(/* outer */ (/* inner */ values.sorted()))",
    ];
    for (index, expression) in expressions.iter().enumerate() {
        let source = format!(
            "package probe\n\nimport java.util.stream.Stream\n\nfun opaque(values: Stream<Int>): Any? {{\n    return {expression}\n}}\n"
        );
        for (label, flags) in [
            ("approximations", &["--allow-approximations"][..]),
            ("strict", &[][..]),
        ] {
            let root = scratch.0.join(format!("{index}-{label}"));
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("ParenComment.kt"), &source).unwrap();
            let outcome = run_owners(
                &root,
                "on",
                &stream_sites(&source),
                &[("java.util.stream.Stream", STREAM_MEMBERS)],
                flags,
                "ParenComment",
            );
            let java = outcome.emitted.unwrap_or_else(|| {
                panic!(
                    "{index}/{label}: transparent comment wrapper must emit: {:?}",
                    outcome.retained
                )
            });
            assert!(
                java.contains("values.sorted()") && !java.contains(".stream().sorted()"),
                "{java}"
            );
            assert!(
                outcome.retained.is_none(),
                "{index}/{label}: a proved exit should not retain Kotlin"
            );
        }
    }
}

#[test]
fn the_production_cli_emits_a_proved_stream_sorted_call() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("StreamCall.kt"), STREAM_SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(STREAM_SOURCE),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "StreamCall",
    );
    let java = outcome
        .emitted
        .expect("a fully proved member call must translate");
    assert!(
        java.contains("s.sorted();"),
        "expected the direct member call:\n{java}"
    );
    assert!(
        !java.contains(".stream().sorted()") && !java.contains("Collectors.toList"),
        "the collection rewrite must not run for a proved member:\n{java}"
    );
    assert!(
        outcome.journal.contains("ResolveCalls"),
        "the full helper must be asked for call sites:\n{}",
        outcome.journal
    );
    assert!(
        outcome.journal.contains("java.util.stream.Stream"),
        "the physical owner must be queried:\n{}",
        outcome.journal
    );
}

/// Documented slice-1 loss, not an oversight.
///
/// The physical proof is complete for `s.sorted()`, but its *exit* is a
/// parameterized reference (`Stream<String>`). Erased descriptor equality does
/// not prove the instantiated result, and this slice has no Java
/// generic-expression typing proof, so the declaration is retained. Widening
/// this needs a typed builder, not a relaxed check: the earlier per-call proof
/// leaked exactly this class of assumption.
#[test]
fn the_production_cli_retains_a_parameterized_stream_exit() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("StreamCall.kt"), STREAM_EXIT_SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(STREAM_EXIT_SOURCE),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "StreamCall",
    );
    assert!(
        outcome.emitted.is_none(),
        "a parameterized exit is not proved by erasure: {:?}",
        outcome.emitted
    );
    assert!(
        outcome.retained.as_deref().is_some_and(
            |kotlin| kotlin.contains("s.sorted()") && kotlin.contains("unknown payload")
        ),
        "the refusal must name the representation gap: {:?}",
        outcome.retained
    );
}

#[test]
fn the_production_cli_retains_when_checked_exceptions_are_unknown() {
    // Old-shaped helper row: the field does not exist, so the run is Unknown
    // and the call keeps today's retention floor.
    let old_shape = r#"[{"name":"sorted","static_member":false,"parameter_types":[],"result_type":"java.util.stream.Stream","parameter_nullability":[],"result_nullability":"unknown"}]"#;
    let scratch = Scratch::new();
    fs::write(scratch.0.join("StreamCall.kt"), STREAM_SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(STREAM_SOURCE),
        "java.util.stream.Stream",
        old_shape,
        "StreamCall",
    );
    assert!(outcome.emitted.is_none(), "{:?}", outcome.emitted);
    assert!(
        outcome
            .retained
            .as_deref()
            .is_some_and(|kotlin| kotlin.contains("s.sorted()")),
        "the declaration must stay Kotlin: {:?}",
        outcome.retained
    );
    assert!(
        outcome.journal.contains("ResolveCalls"),
        "the helper must still have been asked:\n{}",
        outcome.journal
    );
}

#[test]
fn the_production_cli_retains_a_declaration_with_a_checked_throws_clause() {
    // The physical row declares IOException. The emitted Java has no
    // catch/throws model, so the new proof must reject and keep the retention
    // floor even though this is otherwise the same `Stream.sorted` shape as
    // the acceptance case.
    let scratch = Scratch::new();
    fs::write(scratch.0.join("StreamCall.kt"), STREAM_SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(STREAM_SOURCE),
        "java.util.stream.Stream",
        STREAM_SORTED_THROWS,
        "StreamCall",
    );
    assert!(outcome.emitted.is_none(), "{:?}", outcome.emitted);
    assert!(
        outcome
            .retained
            .as_deref()
            .is_some_and(|kotlin| kotlin.contains("s.sorted()")),
        "{:?}",
        outcome.retained
    );
}

#[test]
fn the_production_cli_emits_a_plain_imported_receiver() {
    // Unaliased import under the declared name: the emitted Java type spelling
    // is the Java simple name and the emitter writes the matching import.
    let source = "package probe\n\nimport java.util.stream.Stream\n\nfun sorted(s: Stream<String>) {\n    s.sorted()\n}\n";
    let scratch = Scratch::new();
    fs::write(scratch.0.join("StreamCall.kt"), source).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(source),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "StreamCall",
    );
    let java = outcome
        .emitted
        .expect("an exact unaliased import must translate");
    assert!(
        java.contains("s.sorted();"),
        "expected the direct member call:\n{java}"
    );
    assert!(
        java.contains("import java.util.stream.Stream;"),
        "the emitted Java must resolve the imported simple name:\n{java}"
    );
}

#[test]
fn the_production_cli_emits_a_tab_separated_import() {
    // Whitespace after the `import` keyword is Kotlin trivia, not a different
    // binding. The generated import statement is still a plain path.
    let source = "package probe\n\nimport\tjava.util.stream.Stream\n\nfun sorted(s: Stream<String>) {\n    s.sorted()\n}\n";
    let scratch = Scratch::new();
    fs::write(scratch.0.join("StreamCall.kt"), source).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(source),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "StreamCall",
    );
    let java = outcome
        .emitted
        .expect("a tab-separated import binds the same owner");
    assert!(
        java.contains("s.sorted();"),
        "expected the direct member call:\n{java}"
    );
}

#[test]
fn the_production_cli_rejects_a_block_comment_that_spells_a_java_import() {
    // The declared `Stream` really binds `com.example.Stream`; the commented
    // line must not be read as an import. The scripted helper claims the JDK
    // owner on purpose (a deliberate mutation, not native wire evidence), so
    // only the source-binding gate can reject this.
    let source = "package probe\n\nimport com.example.Stream\n\n/*\nimport java.util.stream.Stream\n*/\n\nfun sorted(s: Stream<String>) = s.sorted()\n";
    let scratch = Scratch::new();
    fs::write(scratch.0.join("StreamCall.kt"), source).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(source),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "StreamCall",
    );
    assert!(outcome.emitted.is_none(), "{:?}", outcome.emitted);
    assert!(
        outcome
            .retained
            .as_deref()
            .is_some_and(|kotlin| kotlin.contains("s.sorted()")),
        "a commented import must not prove the receiver: {:?}",
        outcome.retained
    );
}

#[test]
fn the_production_cli_retains_ambiguous_import_bindings() {
    // Kotlin rejects both shapes as conflicting imports; a non-unique binding
    // cannot prove which declaration the receiver has.
    for source in [
        "package probe\n\nimport java.util.stream.Stream\nimport com.example.Stream\n\nfun sorted(s: Stream<String>) = s.sorted()\n",
        "package probe\n\nimport java.util.stream.Stream\nimport java.util.stream.Stream\n\nfun sorted(s: Stream<String>) = s.sorted()\n",
    ] {
        let scratch = Scratch::new();
        fs::write(scratch.0.join("StreamCall.kt"), source).unwrap();
        let outcome = run(
            &scratch.0,
            "on",
            &stream_sites(source),
            "java.util.stream.Stream",
            STREAM_MEMBERS,
            "StreamCall",
        );
        assert!(
            outcome.emitted.is_none(),
            "ambiguous binding translated: {source}\n{:?}",
            outcome.emitted
        );
        assert!(
            outcome
                .retained
                .as_deref()
                .is_some_and(|kotlin| kotlin.contains("s.sorted()")),
            "{:?}",
            outcome.retained
        );
    }
}

#[test]
fn the_production_cli_retains_an_aliased_import() {
    // The emitter keeps the source spelling (`S<Integer>`) and drops aliased
    // Kotlin imports, so a translation here would be invalid Java. Retention
    // is the honest floor until emission can canonicalise aliases.
    let source = "package probe\n\nimport java.util.stream.Stream as S\n\nfun sorted(s: S<String>): S<String> = s.sorted()\n";
    let scratch = Scratch::new();
    fs::write(scratch.0.join("StreamCall.kt"), source).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(source),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "StreamCall",
    );
    assert!(
        outcome.emitted.is_none(),
        "an aliased receiver spelling has no proved Java form: {:?}",
        outcome.emitted
    );
    assert!(
        outcome
            .retained
            .as_deref()
            .is_some_and(|kotlin| kotlin.contains("s.sorted()")),
        "{:?}",
        outcome.retained
    );
}

#[test]
fn the_production_cli_retains_local_loop_and_catch_shadowed_receivers() {
    // Each fixture declares the receiver name again in a nested scope, so the
    // formal parameter is not the receiver and its java.* type proves nothing.
    for source in [
        "package probe\n\nfun sorted(values: java.util.stream.Stream<String>) {\n    if (values.hashCode() != 0) {\n        val values = listOf(\"shadow\")\n        values.sorted()\n    }\n}\n",
        "package probe\n\nfun sorted(values: java.util.stream.Stream<String>) {\n    for (values in listOf(listOf(\"shadow\"))) {\n        values.sorted()\n    }\n}\n",
        "package probe\n\nclass ShadowError : Exception() {\n    fun sorted(): List<Int> = listOf(1)\n}\n\nfun sorted(values: java.util.stream.Stream<String>) {\n    try {\n        values.count()\n    } catch (values: ShadowError) {\n        values.sorted()\n    }\n}\n",
    ] {
        let scratch = Scratch::new();
        fs::write(scratch.0.join("StreamCall.kt"), source).unwrap();
        let outcome = run(
            &scratch.0,
            "on",
            &stream_sites(source),
            "java.util.stream.Stream",
            STREAM_MEMBERS,
            "StreamCall",
        );
        assert!(
            outcome.emitted.is_none(),
            "shadowed receiver translated: {source}\n{:?}",
            outcome.emitted
        );
        assert!(
            outcome
                .retained
                .as_deref()
                .is_some_and(|kotlin| kotlin.contains("values.sorted()")),
            "{:?}",
            outcome.retained
        );
    }
}

#[test]
fn helper_off_output_is_the_unchanged_retention_floor() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("StreamCall.kt"), STREAM_SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "off",
        &stream_sites(STREAM_SOURCE),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "StreamCall",
    );
    assert!(outcome.emitted.is_none());
    assert!(
        outcome
            .retained
            .as_deref()
            .is_some_and(|kotlin| kotlin.contains("s.sorted()")),
        "helper-off must keep the call in Kotlin: {:?}",
        outcome.retained
    );
    assert!(
        outcome.journal.is_empty(),
        "an off policy must not talk to a helper:\n{}",
        outcome.journal
    );
}

// ---------------------------------------------------------------------------
// Whole-declaration checked exceptions
//
// `Stream.sorted()` proving `[]` (no checked throws) says nothing about the
// other calls in the same declaration. The emitted Java method has no
// `throws`/`catch` model, so a single physically-checked call makes javac
// reject the whole file. These scripted runs pin the planning rule; the real
// helper/JDK differential lives in `java_member_call_jvm.rs`.
// ---------------------------------------------------------------------------

const INPUT_STREAM_MEMBERS: &str =
    include_str!("fixtures/classpath_facts/input_stream_members.json");

/// Two receivers of different Java owners in ONE declaration. `read()` is
/// `Established` (so it passes every resolution gate) and its physical row
/// declares `java.io.IOException`; `sorted()` is the newly proved call.
const MIXED_SOURCE: &str = r#"package probe

fun mixed(s: java.util.stream.Stream<String>, input: java.io.InputStream): java.util.stream.Stream<String> {
    input.read()
    return s.sorted()
}
"#;

/// A checked call with no new-proof call beside it: the throws rule cannot be
/// a side effect of the member slice.
const READ_ONLY_SOURCE: &str = r#"package probe

fun readIt(input: java.io.InputStream): Int = input.read()
"#;

/// A static Java member whose descriptor has arguments: the proof must not be
/// limited to zero-parameter instance methods, and must not be a spelling
/// table (`sleep` is only recognized because the site and the row agree).
const SLEEP_SOURCE: &str = r#"package probe

fun pause() {
    java.lang.Thread.sleep(1L)
}
"#;

/// A constructor that physically declares a checked exception.
const OPEN_SOURCE: &str = r#"package probe

import java.io.FileInputStream

fun open(): FileInputStream = FileInputStream("data.txt")
"#;

/// No-throws Java members must keep translating: this is the over-retention
/// control (`markSupported()` declares nothing).
const MARK_SOURCE: &str = r#"package probe

fun markIt(input: java.io.InputStream): Boolean = input.markSupported()
"#;

/// `Site(byte_offset, owner, name, params, result, nullable, descriptor)`.
fn site_entry(
    offset: u64,
    owner: &str,
    name: &str,
    params: &str,
    result: &str,
    nullable: bool,
    descriptor: &str,
) -> String {
    format!(
        r#"{{"byte_offset":{offset},"text":"{name}()","status":"Established","callee_owner":"{owner}","callee_name":"{name}","parameter_types":{params},"result_type":"{result}","result_nullable":{nullable},"jvm_descriptor":"{descriptor}"}}"#
    )
}

fn mixed_sites(source: &str) -> String {
    let read = site_entry(
        selector_offset(source, "read"),
        "java.io.InputStream",
        "read",
        "[]",
        "int",
        false,
        "()I",
    );
    let sorted = site_entry(
        selector_offset(source, "sorted"),
        "java.util.stream.Stream",
        "sorted",
        "[]",
        "java.util.stream.Stream",
        false,
        "()Ljava/util/stream/Stream;",
    );
    format!("[{read},{sorted}]")
}

fn read_sites(source: &str) -> String {
    format!("[{}]", read_site(source))
}

fn read_site(source: &str) -> String {
    site_entry(
        selector_offset(source, "read"),
        "java.io.InputStream",
        "read",
        "[]",
        "int",
        false,
        "()I",
    )
}

fn mark_sites(source: &str) -> String {
    format!("[{}]", mark_site(source))
}

fn mark_site(source: &str) -> String {
    site_entry(
        selector_offset(source, "markSupported"),
        "java.io.InputStream",
        "markSupported",
        "[]",
        "boolean",
        false,
        "()Z",
    )
}

/// `Thread.sleep(long)` and `Thread.sleep(long, int)` — both static, both
/// declare `InterruptedException`, different descriptors. Deliberate scripted
/// evidence (the native rows are pinned in the JVM test).
const THREAD_SLEEP_MEMBERS: &str = r#"[
  {"name":"sleep","static_member":true,"parameter_types":["long"],"result_type":"<void>","parameter_nullability":["unknown"],"result_nullability":"unknown","checked_exceptions":["java.lang.InterruptedException"]},
  {"name":"sleep","static_member":true,"parameter_types":["long","int"],"result_type":"<void>","parameter_nullability":["unknown","unknown"],"result_nullability":"unknown","checked_exceptions":["java.lang.InterruptedException"]}
]"#;

/// `FileInputStream` constructors (the `String` one declares
/// `FileNotFoundException`). Deliberate scripted evidence.
const FILE_INPUT_STREAM_MEMBERS: &str = r#"[
  {"name":"<init>","static_member":false,"parameter_types":["java.lang.String"],"result_type":"<void>","parameter_nullability":["unknown"],"result_nullability":"unknown","checked_exceptions":["java.io.FileNotFoundException"]},
  {"name":"<init>","static_member":false,"parameter_types":["java.io.File"],"result_type":"<void>","parameter_nullability":["unknown"],"result_nullability":"unknown","checked_exceptions":["java.io.FileNotFoundException"]},
  {"name":"<init>","static_member":false,"parameter_types":["java.io.FileDescriptor"],"result_type":"<void>","parameter_nullability":["unknown"],"result_nullability":"unknown","checked_exceptions":[]}
]"#;

/// Old-jar shape: the same physical `read` row without any throws metadata.
/// Absent is Unknown, never "proved none".
const INPUT_STREAM_READ_NO_METADATA: &str = r#"[{"name":"read","static_member":false,"parameter_types":[],"result_type":"int","parameter_nullability":[],"result_nullability":"unknown"}]"#;

#[test]
fn the_production_cli_retains_a_mixed_declaration_whose_other_call_throws_checked() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("MixedCall.kt"), MIXED_SOURCE).unwrap();
    let outcome = run_owners(
        &scratch.0,
        "on",
        &mixed_sites(MIXED_SOURCE),
        &[
            ("java.util.stream.Stream", STREAM_MEMBERS),
            ("java.io.InputStream", INPUT_STREAM_MEMBERS),
        ],
        &[],
        "MixedCall",
    );
    assert!(
        outcome.emitted.is_none(),
        "a proved `sorted()` must not unlock a declaration whose other call declares a checked \
         exception: {:?}",
        outcome.emitted
    );
    let retained = outcome.retained.as_deref().unwrap_or_default();
    assert!(
        retained.contains("input.read()") && retained.contains("s.sorted()"),
        "{retained}"
    );
    assert!(
        retained.contains("java.io.IOException"),
        "the retention reason must name the checked exception, not an older          approximation blocker:\n{retained}"
    );
}

#[test]
fn the_production_cli_retains_a_checked_call_without_any_new_proof() {
    // No `sorted()` here: the resolution truth is `Established` and every
    // older platform/member path accepts it, so only a declaration-independent
    // throws rule keeps the declaration home.
    let scratch = Scratch::new();
    fs::write(scratch.0.join("ReadCall.kt"), READ_ONLY_SOURCE).unwrap();
    let outcome = run_owners(
        &scratch.0,
        "on",
        &read_sites(READ_ONLY_SOURCE),
        &[("java.io.InputStream", INPUT_STREAM_MEMBERS)],
        &[],
        "ReadCall",
    );
    assert!(outcome.emitted.is_none(), "{:?}", outcome.emitted);
    let retained = outcome.retained.as_deref().unwrap_or_default();
    assert!(retained.contains("input.read()"), "{retained}");
    assert!(
        retained.contains("java.io.IOException"),
        "the retention reason must name the checked exception:\n{retained}"
    );
}

#[test]
fn the_production_cli_retains_a_static_checked_call_with_arguments() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("SleepCall.kt"), SLEEP_SOURCE).unwrap();
    let sites = format!(
        "[{}]",
        site_entry(
            selector_offset(SLEEP_SOURCE, "sleep"),
            "java.lang.Thread",
            "sleep",
            r#"["kotlin.Long"]"#,
            "kotlin.Unit",
            false,
            "(J)V",
        )
    );
    let outcome = run_owners(
        &scratch.0,
        "on",
        &sites,
        &[("java.lang.Thread", THREAD_SLEEP_MEMBERS)],
        &[],
        "SleepCall",
    );
    assert!(outcome.emitted.is_none(), "{:?}", outcome.emitted);
    let retained = outcome.retained.as_deref().unwrap_or_default();
    assert!(retained.contains("Thread.sleep(1L)"), "{retained}");
    assert!(
        retained.contains("java.lang.InterruptedException"),
        "a static member with arguments must be identified by descriptor, not spelling:\n{retained}"
    );
}

#[test]
fn the_production_cli_retains_a_constructor_that_declares_a_checked_throw() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("OpenCall.kt"), OPEN_SOURCE).unwrap();
    let sites = format!(
        "[{}]",
        site_entry(
            selector_offset(OPEN_SOURCE, "FileInputStream"),
            "java.io.FileInputStream",
            "FileInputStream",
            r#"["kotlin.String"]"#,
            "java.io.FileInputStream",
            false,
            "(Ljava/lang/String;)V",
        )
    );
    let outcome = run_owners(
        &scratch.0,
        "on",
        &sites,
        &[("java.io.FileInputStream", FILE_INPUT_STREAM_MEMBERS)],
        &[],
        "OpenCall",
    );
    assert!(outcome.emitted.is_none(), "{:?}", outcome.emitted);
    let retained = outcome.retained.as_deref().unwrap_or_default();
    assert!(
        retained.contains("FileInputStream(\"data.txt\")"),
        "{retained}"
    );
    assert!(
        retained.contains("java.io.FileNotFoundException"),
        "the constructor row carries its own checked exception:\n{retained}"
    );
}

#[test]
fn a_declaration_newly_unlocked_by_the_member_proof_must_not_read_absent_metadata_as_safe() {
    // Same mixed declaration, but the `read` row carries no throws metadata at
    // all (old helper/jar shape). An absent field cannot license the emission
    // of `input.read()`, and this declaration is only eligible because of the
    // new `sorted()` proof.
    let scratch = Scratch::new();
    fs::write(scratch.0.join("MixedCall.kt"), MIXED_SOURCE).unwrap();
    let outcome = run_owners(
        &scratch.0,
        "on",
        &mixed_sites(MIXED_SOURCE),
        &[
            ("java.util.stream.Stream", STREAM_MEMBERS),
            ("java.io.InputStream", INPUT_STREAM_READ_NO_METADATA),
        ],
        &[],
        "MixedCall",
    );
    assert!(outcome.emitted.is_none(), "{:?}", outcome.emitted);
    let retained = outcome.retained.as_deref().unwrap_or_default();
    assert!(retained.contains("input.read()"), "{retained}");
    assert!(
        retained.contains("has no checked-exception evidence"),
        "absent metadata must be reported as unproved, never as safety:\n{retained}"
    );
}

#[test]
fn allow_approximations_does_not_let_a_checked_call_through() {
    // `--allow-approximations` permits unresolved calls, not invalid Java: the
    // throws evidence is positive, not missing.
    let scratch = Scratch::new();
    fs::write(scratch.0.join("MixedCall.kt"), MIXED_SOURCE).unwrap();
    let outcome = run_owners(
        &scratch.0,
        "on",
        &mixed_sites(MIXED_SOURCE),
        &[
            ("java.util.stream.Stream", STREAM_MEMBERS),
            ("java.io.InputStream", INPUT_STREAM_MEMBERS),
        ],
        &["--allow-approximations"],
        "MixedCall",
    );
    assert!(outcome.emitted.is_none(), "{:?}", outcome.emitted);
}

#[test]
fn the_production_cli_keeps_the_helper_off_floor_for_a_mixed_declaration() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("MixedCall.kt"), MIXED_SOURCE).unwrap();
    let outcome = run_owners(
        &scratch.0,
        "off",
        &mixed_sites(MIXED_SOURCE),
        &[
            ("java.util.stream.Stream", STREAM_MEMBERS),
            ("java.io.InputStream", INPUT_STREAM_MEMBERS),
        ],
        &[],
        "MixedCall",
    );
    assert!(outcome.emitted.is_none(), "{:?}", outcome.emitted);
    assert!(
        outcome
            .retained
            .as_deref()
            .is_some_and(|kotlin| kotlin.contains("s.sorted()")),
        "{:?}",
        outcome.retained
    );
    assert!(
        outcome.journal.is_empty(),
        "an off policy must not talk to a helper:\n{}",
        outcome.journal
    );
}

#[test]
fn a_checked_throwing_declaration_does_not_retain_its_siblings() {
    // Two files, one declaration each: the proved `sorted` member still
    // translates while the checked-throwing sibling stays Kotlin. Retention is
    // decided per declaration, not per run or per workspace.
    let scratch = Scratch::new();
    fs::write(scratch.0.join("StreamCall.kt"), STREAM_SOURCE).unwrap();
    fs::write(scratch.0.join("ReadCall.kt"), READ_ONLY_SOURCE).unwrap();
    let sites = format!(
        "[{},{}]",
        site_entry(
            selector_offset(STREAM_SOURCE, "sorted"),
            "java.util.stream.Stream",
            "sorted",
            "[]",
            "java.util.stream.Stream",
            false,
            "()Ljava/util/stream/Stream;",
        ),
        site_entry(
            selector_offset(READ_ONLY_SOURCE, "read"),
            "java.io.InputStream",
            "read",
            "[]",
            "int",
            false,
            "()I",
        )
    );
    let outcome = run_owners(
        &scratch.0,
        "on",
        &sites,
        &[
            ("java.util.stream.Stream", STREAM_MEMBERS),
            ("java.io.InputStream", INPUT_STREAM_MEMBERS),
        ],
        &[],
        "StreamCall",
    );
    let java = outcome
        .emitted
        .expect("the proved declaration must translate");
    assert!(java.contains("s.sorted();"), "{java}");
    assert!(!java.contains("read("), "{java}");
    assert!(
        fs::read_to_string(scratch.0.join("StreamCall.kt")).is_err(),
        "the translated declaration must not stay in Kotlin"
    );
    let retained = fs::read_to_string(scratch.0.join("ReadCall.kt"))
        .expect("the checked-throwing declaration must stay in Kotlin");
    assert!(retained.contains("input.read()"), "{retained}");
}

#[test]
fn a_no_throws_java_member_still_translates() {
    // Over-retention control: `InputStream.markSupported()` declares nothing.
    let scratch = Scratch::new();
    fs::write(scratch.0.join("MarkCall.kt"), MARK_SOURCE).unwrap();
    let outcome = run_owners(
        &scratch.0,
        "on",
        &mark_sites(MARK_SOURCE),
        &[("java.io.InputStream", INPUT_STREAM_MEMBERS)],
        &[],
        "MarkCall",
    );
    let java = outcome.emitted.unwrap_or_else(|| {
        panic!(
            "a proved no-throws member call must still translate: {:?}\n{}\n{}",
            outcome.retained, outcome.journal, outcome.stderr
        )
    });
    assert!(java.contains("input.markSupported()"), "{java}");
}

/// An unused physical row of the *same owner* that declares a checked
/// exception must not taint a declaration that never calls it. The comparison
/// is against the same row set without it.
#[test]
fn an_unused_throwing_overload_of_the_same_owner_does_not_retain() {
    const CLEAN: &str = r#"[
      {"name":"sorted","static_member":false,"parameter_types":[],"result_type":"java.util.stream.Stream","parameter_nullability":[],"result_nullability":"unknown","checked_exceptions":[]},
      {"name":"close","static_member":false,"parameter_types":[],"result_type":"<void>","parameter_nullability":[],"result_nullability":"unknown","checked_exceptions":[]}
    ]"#;
    const THROWING_SIBLING: &str = r#"[
      {"name":"sorted","static_member":false,"parameter_types":[],"result_type":"java.util.stream.Stream","parameter_nullability":[],"result_nullability":"unknown","checked_exceptions":[]},
      {"name":"onClose","static_member":false,"parameter_types":["java.lang.Runnable"],"result_type":"java.util.stream.Stream","parameter_nullability":["unknown"],"result_nullability":"unknown","checked_exceptions":["java.io.IOException"]},
      {"name":"close","static_member":false,"parameter_types":[],"result_type":"<void>","parameter_nullability":[],"result_nullability":"unknown","checked_exceptions":[]}
    ]"#;
    let mut outcomes = Vec::new();
    for members in [CLEAN, THROWING_SIBLING] {
        let scratch = Scratch::new();
        fs::write(scratch.0.join("StreamCall.kt"), STREAM_SOURCE).unwrap();
        outcomes.push(run(
            &scratch.0,
            "on",
            &stream_sites(STREAM_SOURCE),
            "java.util.stream.Stream",
            members,
            "StreamCall",
        ));
    }
    let (clean, sibling) = (&outcomes[0], &outcomes[1]);
    assert_eq!(
        clean.emitted.is_some(),
        sibling.emitted.is_some(),
        "an uncalled throwing row must not change the outcome"
    );
    assert_eq!(clean.emitted, sibling.emitted);
    assert!(
        clean
            .emitted
            .as_deref()
            .is_some_and(|java| java.contains("s.sorted();")),
        "the no-throws call itself must still translate"
    );
}

/// Scripted contradictions (the helper cannot agree with itself; native wire
/// behaviour is pinned in `java_member_call_jvm.rs`). Neither a throws entry
/// on a row whose descriptor does not describe this call nor a second
/// same-descriptor row may be turned into a verdict: the run keeps exactly the
/// outcome it has without the extra metadata.
#[test]
fn contradictory_throws_rows_do_not_manufacture_a_verdict() {
    const READ_OTHER_SHAPE_THROWS: &str = r#"[{"name":"read","static_member":false,"parameter_types":["[B"],"result_type":"int","parameter_nullability":["unknown"],"result_nullability":"unknown","checked_exceptions":["java.io.IOException"]}]"#;
    const READ_OTHER_SHAPE_CLEAN: &str = r#"[{"name":"read","static_member":false,"parameter_types":["[B"],"result_type":"int","parameter_nullability":["unknown"],"result_nullability":"unknown","checked_exceptions":[]}]"#;
    const READ_TWO_ROWS_THROWS: &str = r#"[
      {"name":"read","static_member":false,"parameter_types":[],"result_type":"int","parameter_nullability":[],"result_nullability":"unknown","checked_exceptions":["java.io.IOException"]},
      {"name":"read","static_member":false,"parameter_types":[],"result_type":"int","parameter_nullability":[],"result_nullability":"unknown","checked_exceptions":[]}
    ]"#;
    const READ_TWO_ROWS_CLEAN: &str = r#"[
      {"name":"read","static_member":false,"parameter_types":[],"result_type":"int","parameter_nullability":[],"result_nullability":"unknown","checked_exceptions":[]},
      {"name":"read","static_member":false,"parameter_types":[],"result_type":"int","parameter_nullability":[],"result_nullability":"unknown","checked_exceptions":[]}
    ]"#;
    for (with_throws, without_throws) in [
        (READ_OTHER_SHAPE_THROWS, READ_OTHER_SHAPE_CLEAN),
        (READ_TWO_ROWS_THROWS, READ_TWO_ROWS_CLEAN),
    ] {
        let mut outcomes = Vec::new();
        for members in [with_throws, without_throws] {
            let scratch = Scratch::new();
            fs::write(scratch.0.join("MixedCall.kt"), MIXED_SOURCE).unwrap();
            outcomes.push(run_owners(
                &scratch.0,
                "on",
                &mixed_sites(MIXED_SOURCE),
                &[
                    ("java.util.stream.Stream", STREAM_MEMBERS),
                    ("java.io.InputStream", members),
                ],
                &[],
                "MixedCall",
            ));
        }
        assert_eq!(
            outcomes[0].emitted, outcomes[1].emitted,
            "a contradictory row must not change the emitted Java"
        );
        assert_eq!(
            outcomes[0].retained, outcomes[1].retained,
            "a contradictory row must not change the retention decision"
        );
    }
}

/// A Kotlin `try`/`catch` around the checked call is *also* retained.
///
/// This cycle does not model handler scope, so a call inside a `catch`-guarded
/// block is kept conservatively: over-retention is the honest floor, and
/// lowering the handler is a separate contract (it needs the catch body, the
/// exception type and the emitted Java's own `try`). Pinned so the current
/// behaviour is a documented decision rather than an accident.
#[test]
fn a_catch_guarded_checked_call_is_retained_conservatively() {
    let source = r#"package probe

fun guarded(input: java.io.InputStream): Int =
    try {
        input.read()
    } catch (failure: java.io.IOException) {
        -1
    }
"#;
    let scratch = Scratch::new();
    fs::write(scratch.0.join("GuardedCall.kt"), source).unwrap();
    let outcome = run_owners(
        &scratch.0,
        "on",
        &read_sites(source),
        &[("java.io.InputStream", INPUT_STREAM_MEMBERS)],
        &[],
        "GuardedCall",
    );
    assert!(outcome.emitted.is_none(), "{:?}", outcome.emitted);
    let retained = outcome.retained.as_deref().unwrap_or_default();
    assert!(
        retained.contains("java.io.IOException"),
        "the retained declaration must explain the checked exception:\n{retained}"
    );
}

// ---------------------------------------------------------------------------
// Declaration-wide coverage: the closure the per-call proof cannot see.
//
// A receipt activates required mode for the whole co-emitted unit. Every other
// operation in that unit then needs its own contract; a missing one retains the
// declaration *before* any write, which is the only sound answer for a value or
// effect the Java form does not preserve.
// ---------------------------------------------------------------------------

/// `trim`'s Java form is not Kotlin's: this declaration is retained because the
/// operation has no contract, not because a name was blacklisted.
const TRIM_SIBLING_SOURCE: &str = r#"package probe

class Siblings {
    fun fSorted(values: java.util.stream.Stream<String>) {
        values.sorted()
    }

    fun gTrim(text: String) {
        println(text.trim())
    }
}
"#;

#[test]
fn a_class_sibling_without_a_contract_retains_the_whole_class() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("Siblings.kt"), TRIM_SIBLING_SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(TRIM_SIBLING_SOURCE),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "Siblings",
    );
    assert!(
        outcome.emitted.is_none(),
        "one unproved sibling must retain the class it is co-emitted with: {:?}",
        outcome.emitted
    );
    let retained = outcome.retained.expect("the class must stay in Kotlin");
    assert!(
        retained.contains("no lowering contract"),
        "the refusal must name the missing operation:\n{retained}"
    );
    assert!(
        retained.contains("trim") || retained.contains("println"),
        "the refusal must identify the offending call:\n{retained}"
    );
}

/// A local binding *observes* the value: the alias is exactly where an unknown
/// payload would become an assumed one.
#[test]
fn a_local_binding_of_a_signed_call_retains() {
    const SOURCE: &str = r#"package probe

fun keep(values: java.util.stream.Stream<String>) {
    val z = values.sorted()
    println(z)
}
"#;
    let scratch = Scratch::new();
    fs::write(scratch.0.join("Alias.kt"), SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(SOURCE),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "Alias",
    );
    assert!(
        outcome.emitted.is_none(),
        "an aliased result observes the payload: {:?}",
        outcome.emitted
    );
    let retained = outcome.retained.expect("retained Kotlin");
    assert!(retained.contains("val z = values.sorted()"), "{retained}");
}

/// An argument position is an exit: the callee's parameter type is not proved
/// by the receipt, so the value cannot be handed over.
#[test]
fn a_signed_call_passed_as_an_argument_retains() {
    const SOURCE: &str = r#"package probe

fun take(stream: java.util.stream.Stream<String>) {}

fun pass(values: java.util.stream.Stream<String>) {
    take(values.sorted())
}
"#;
    let scratch = Scratch::new();
    fs::write(scratch.0.join("Argument.kt"), SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(SOURCE),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "Argument",
    );
    let java = outcome.emitted.unwrap_or_default();
    assert!(
        !java.contains("pass(") && !java.contains("sorted"),
        "an argument exit is not signed by the receipt:\n{java}"
    );
    let retained = outcome.retained.expect("retained Kotlin");
    assert!(
        retained.contains("fun pass(") && retained.contains("take(values.sorted())"),
        "the calling declaration must stay Kotlin:\n{retained}"
    );
}

/// Java `void` is not a Kotlin `Unit` value: a `void` call cannot be returned
/// through `Any?` (nor assigned), because there is no value to return.
#[test]
fn a_void_call_in_a_value_position_retains() {
    const SOURCE: &str = r#"package probe

fun sink(items: java.util.ArrayList<String>): Any? = items.clear()
"#;
    let sites = site_line(
        selector_offset(SOURCE, "clear"),
        "java.util.ArrayList",
        "clear",
        "<void>",
        false,
        "()V",
        "clear()",
    );
    let scratch = Scratch::new();
    fs::write(scratch.0.join("VoidValue.kt"), SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &sites,
        "java.util.ArrayList",
        "[{\"name\":\"clear\",\"static_member\":false,\"parameter_types\":[],\"result_type\":\"<void>\",\"parameter_nullability\":[],\"result_nullability\":\"unknown\",\"checked_exceptions\":[]}]",
        "VoidValue",
    );
    assert!(
        outcome.emitted.is_none(),
        "a void call has no value representation: {:?}",
        outcome.emitted
    );
    let retained = outcome.retained.expect("retained Kotlin");
    assert!(
        retained.contains("`void`") || retained.contains("Unit"),
        "the refusal must name the void/Unit mismatch:\n{retained}"
    );
}

/// A same-workspace callee is part of the declaration's operation closure. Its
/// own lowering is not proved by the receipt that unlocked this declaration, so
/// the declaration retains instead of emitting a call it cannot justify.
#[test]
fn a_source_callee_in_a_signed_declaration_retains() {
    const SOURCE: &str = r#"package probe

fun helper(): Int = 1

fun use(values: java.util.stream.Stream<String>) {
    helper()
    values.sorted()
}
"#;
    let scratch = Scratch::new();
    fs::write(scratch.0.join("Dependency.kt"), SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(SOURCE),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "Dependency",
    );
    let java = outcome.emitted.unwrap_or_default();
    assert!(
        !java.contains("use(") && !java.contains("sorted"),
        "a source callee has no receipt of its own in this declaration:\n{java}"
    );
    let retained = outcome.retained.expect("retained Kotlin");
    assert!(
        retained.contains("fun use(") && retained.contains("helper()"),
        "the dependent declaration must stay Kotlin:\n{retained}"
    );
}

// ---------------------------------------------------------------------------
// Declaration-wide coverage (C1): the co-emitted unit is not just its calls.
// ---------------------------------------------------------------------------

/// Parent-reviewed fixture (`parent-review/noncall-probe-source/Boundary.kt`):
/// `values.sorted()` is proved, and the same block body also evaluates
/// `left + right`.
///
/// Kotlin resolves `+` on `BigDecimal` as `BigDecimal.plus`; the emitter had no
/// lowering contract for it and wrote `int sum = left.plus(right);` in a
/// declaration the `sorted()` receipt had unlocked, so javac rejected the
/// file. The operator is an *executable operation* in the co-emitted unit and
/// has to be inventoried, not left to a fallthrough. `--allow-approximations`
/// must not opt out: the flag governs resolution approximations, not the
/// declaration's physical Java form.
const BOUNDARY_SOURCE: &str = r#"package probe

import java.math.BigDecimal
import java.util.stream.Stream

class Boundary {
    fun touch(values: Stream<Int>, left: BigDecimal, right: BigDecimal) {
        values.sorted()
        val sum = left + right
    }
}
"#;

#[test]
fn the_production_cli_retains_a_class_whose_sibling_operator_is_unproved() {
    let scratch = Scratch::new();
    for (label, flags) in [
        ("strict", &[][..]),
        ("approximations", &["--allow-approximations"][..]),
    ] {
        // Each mode gets its own tree: an in-place retention appends its
        // diagnostics to the source, which would move the scripted helper's
        // byte offsets for the next mode.
        let root = scratch.0.join(label);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Boundary.kt"), BOUNDARY_SOURCE).unwrap();
        let outcome = run_owners(
            &root,
            "on",
            &stream_sites(BOUNDARY_SOURCE),
            &[("java.util.stream.Stream", STREAM_MEMBERS)],
            flags,
            "Boundary",
        );
        assert!(
            outcome.emitted.is_none(),
            "{label}: a sibling operator with no lowering contract must retain the class: {:?}",
            outcome.emitted
        );
        let retained = outcome.retained.unwrap_or_default();
        assert!(
            retained.contains("values.sorted()") && retained.contains("left + right"),
            "{label}: both operations must survive in Kotlin:\n{retained}"
        );
        assert!(
            retained.contains("no lowering contract"),
            "{label}: the refusal must come from the operation inventory, not a name rule:\n{retained}"
        );
    }
}

// ---------------------------------------------------------------------------
// Declaration ABI identity (C2): the emitted signature type is proved, never
// spelled.
// ---------------------------------------------------------------------------

/// Parent-reviewed fixture (`parent-review/alias-probe-source/Alias.kt`).
///
/// `typealias Any = Stream<Int>` makes the declared exit `Any?` the *Stream*
/// type, so Kotlin's physical descriptor is
/// `(Ljava/util/stream/Stream;)Ljava/util/stream/Stream;`. Reading the exit
/// type off its spelling produced `Object opaque(Stream<Integer>)` with
/// descriptor `…Ljava/lang/Object;` — a compilable Java method that is not the
/// Kotlin ABI. Binding the name first is what makes the difference visible.
const ALIAS_EXIT_SOURCE: &str = r#"package probe2

import java.util.stream.Stream

typealias Any = Stream<Int>

fun opaque(values: Stream<Int>): Any? = values.sorted()
"#;

#[test]
fn the_production_cli_retains_an_exit_whose_builtin_spelling_is_aliased() {
    let scratch = Scratch::new();
    for (label, flags) in [
        ("strict", &[][..]),
        ("approximations", &["--allow-approximations"][..]),
    ] {
        // One tree per mode: in-place retention writes its diagnostics into the
        // source, which would move the scripted helper's byte offsets.
        let root = scratch.0.join(label);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Alias.kt"), ALIAS_EXIT_SOURCE).unwrap();
        let outcome = run_owners(
            &root,
            "on",
            &stream_sites(ALIAS_EXIT_SOURCE),
            &[("java.util.stream.Stream", STREAM_MEMBERS)],
            flags,
            "Alias",
        );
        assert!(
            outcome.emitted.is_none(),
            "{label}: the `Any?` exit is the aliased Stream ABI, not java.lang.Object: {:?}",
            outcome.emitted
        );
        let retained = outcome.retained.unwrap_or_default();
        assert!(
            retained.contains("typealias Any") && retained.contains("values.sorted()"),
            "{label}: the declaration must survive in Kotlin:\n{retained}"
        );
        assert!(
            retained.contains("`Any`"),
            "{label}: the refusal must name the bound type:\n{retained}"
        );
    }
}

/// Parent-reviewed fixture
/// (`parent-review/parameter-alias-probe-source/ParameterAlias.kt`).
///
/// The alias is on a formal parameter slot, not the exit: Kotlin's descriptor is
/// `(Ljava/util/stream/Stream;Ljava/lang/String;)V` while the candidate wrote
/// `void link(Stream<Integer> values, int unused)` — `(…;I)V`. An unused formal
/// parameter is still part of the physical ABI, and the generic argument of the
/// receiver parameter is the same question one level down.
const PARAMETER_ALIAS_SOURCE: &str = r#"package probe3

import java.util.stream.Stream

typealias Int = String

fun link(values: Stream<Int>, unused: Int) {
    values.sorted()
}
"#;

#[test]
fn the_production_cli_retains_a_formal_parameter_spelled_as_an_aliased_builtin() {
    let scratch = Scratch::new();
    for (label, flags) in [
        ("strict", &[][..]),
        ("approximations", &["--allow-approximations"][..]),
    ] {
        // One tree per mode: in-place retention writes its diagnostics into the
        // source, which would move the scripted helper's byte offsets.
        let root = scratch.0.join(label);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("ParameterAlias.kt"), PARAMETER_ALIAS_SOURCE).unwrap();
        let outcome = run_owners(
            &root,
            "on",
            &stream_sites(PARAMETER_ALIAS_SOURCE),
            &[("java.util.stream.Stream", STREAM_MEMBERS)],
            flags,
            "ParameterAlias",
        );
        assert!(
            outcome.emitted.is_none(),
            "{label}: an aliased formal-parameter spelling changes the JVM descriptor: {:?}",
            outcome.emitted
        );
        let retained = outcome.retained.unwrap_or_default();
        assert!(
            retained.contains("unused: Int") && retained.contains("values.sorted()"),
            "{label}: the declaration must survive in Kotlin:\n{retained}"
        );
        assert!(
            retained.contains("`Int`"),
            "{label}: the refusal must name the bound parameter type:\n{retained}"
        );
    }
}

/// The guard must not become a blanket refusal: the same unused-formal shape
/// with an unshadowed builtin is still a proved `int` parameter.
const UNUSED_FORMAL_SOURCE: &str = r#"package probe

import java.util.stream.Stream

fun link(values: Stream<Int>, unused: Int) {
    values.sorted()
}
"#;

#[test]
fn the_production_cli_emits_an_unshadowed_formal_parameter() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("UnusedFormal.kt"), UNUSED_FORMAL_SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(UNUSED_FORMAL_SOURCE),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "UnusedFormal",
    );
    let java = outcome
        .emitted
        .expect("a bound builtin formal parameter is proved");
    assert!(
        java.contains("int unused"),
        "the unshadowed builtin must still emit the Java primitive:\n{java}"
    );
    assert!(
        java.contains("values.sorted();"),
        "the proved call must still be written verbatim:\n{java}"
    );
}

/// The admitted nullable `java.lang.Object` exit, at the CLI level: the physical
/// return type is proved from a bound `Any`, not from a spelling guess.
const OPAQUE_SOURCE: &str = r#"package probe

import java.util.stream.Stream

fun opaque(values: Stream<Int>): Any? = values.sorted()
"#;

#[test]
fn the_production_cli_emits_a_bound_nullable_object_exit() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("Opaque.kt"), OPAQUE_SOURCE).unwrap();
    let outcome = run(
        &scratch.0,
        "on",
        &stream_sites(OPAQUE_SOURCE),
        "java.util.stream.Stream",
        STREAM_MEMBERS,
        "Opaque",
    );
    let java = outcome
        .emitted
        .expect("a bound `Any?` exit is the proved Object ABI");
    assert!(
        java.contains("Object opaque("),
        "the exit must emit the proved java.lang.Object spelling:\n{java}"
    );
    assert!(
        java.contains("return values.sorted();"),
        "the exit must return the proved call verbatim:\n{java}"
    );
}
