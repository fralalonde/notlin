//! Required-call resolution through Track B call-site facts.
//!
//! These tests drive the *planner* (`plan_required_calls`) with a scripted
//! `sh` helper, so they pin the Rust contract without a JVM:
//!
//! * an `Established` site at the callee selector's offset resolves a call the
//!   syntax index cannot, and the resolution is recorded as a fact identity
//!   (`module = jvm-facts`), not as a declaration;
//! * the decision comes from the *site*, never from the spelling: the fixture
//!   call is named `zap` while the helper reports `map`;
//! * `Ambiguous` keeps today's retention, and so does a site at a different
//!   offset;
//! * a planned snapshot whose bytes differ from the file on disk gets nothing:
//!   the hash guard refuses before the helper is asked (`FactsRuntime::
//!   call_site_at`), which is what keeps a speculative overlay sound.

use notlin::facts::decorator::FactsProvider;
use notlin::facts::runtime::FactsRuntime;
use notlin::semantics::{
    FactStatus, SyntaxSemanticProvider, kotlin_call_site_offset, plan_required_calls,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DISK_SOURCE: &str = r#"package fixture

class Holder {
    fun run(values: MutableList<Int>): Int {
        values.zap(1)
        return values.size
    }
}
"#;

/// Same code, one trailing comment: the bytes differ, so the hash guard must
/// refuse. Offsets are untouched on purpose — the refusal has to come from the
/// hash, not from a shifted offset.
fn overlay_source() -> String {
    format!("{DISK_SOURCE}// speculative candidate revision\n")
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "notlin-required-call-facts-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

/// Byte offset of the `zap(1)` call's selector in `source`.
fn zap_offset(source: &str) -> u64 {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression" {
            let Some(offset) = kotlin_call_site_offset(node) else {
                continue;
            };
            if source[offset..].starts_with("zap(") {
                return offset as u64;
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    panic!("no `zap` call in the fixture");
}

/// A helper that answers every `ResolveCalls` with `sites` and echoes the file
/// it was asked about (the runtime refuses a batch that names another file).
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

fn site_json(offset: u64, status: &str) -> String {
    match status {
        "Established" => format!(
            r#"[{{"byte_offset":{offset},"text":"zap(1)","status":"Established","callee_owner":"kotlin.collections.MutableList","callee_name":"map","parameter_types":["kotlin.Int"],"result_type":"kotlin.Boolean","result_nullable":false,"jvm_descriptor":"(I)Z"}}]"#
        ),
        other => format!(
            r#"[{{"byte_offset":{offset},"text":"zap(1)","status":"{other}","callee_owner":null,"callee_name":null,"parameter_types":[],"result_type":null,"result_nullable":false,"jvm_descriptor":null}}]"#
        ),
    }
}

/// Plan the one-declaration fixture and return the resolution recorded for the
/// `zap` spelling plus the diagnostic codes the plan emitted.
fn plan_with(
    disk: &str,
    planned: &str,
    status: &str,
    offset_shift: i64,
    name: &str,
) -> (
    PathBuf,
    FactStatus<notlin::semantics::SymbolId>,
    Vec<String>,
) {
    let root = scratch(name);
    let file = root.join("Holder.kt");
    fs::write(&file, disk).unwrap();
    let requests = root.join("requests.log");
    let sites = site_json((zap_offset(disk) as i64 + offset_shift) as u64, status);
    let runtime =
        FactsRuntime::with_command(batch_helper(&requests, &sites)).expect("fake helper starts");
    let provider = FactsProvider::with_runtime(
        SyntaxSemanticProvider::new([(file.clone(), planned.to_string())]),
        runtime,
    );

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(planned, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(planned, tree.root_node(), &file);
    plan_required_calls(planned, &tree, &file, &provider, false, &mut plan);

    let resolution = plan
        .dependencies
        .iter()
        .find(|dependency| dependency.spelling == "zap")
        .map(|dependency| dependency.resolution.clone())
        .unwrap_or(FactStatus::Unknown);
    let codes = plan
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.clone())
        .collect::<Vec<_>>();
    (file, resolution, codes)
}

#[test]
fn established_site_resolves_the_call_as_a_fact_identity() {
    let (file, resolution, codes) =
        plan_with(DISK_SOURCE, DISK_SOURCE, "Established", 0, "established");
    let FactStatus::Established(id) = &resolution else {
        panic!("expected the helper's site to resolve the call, got {resolution:?}");
    };
    // A fact identity, not a declaration: the module marks it, and the owner
    // comes from the helper's callee_owner split at its last dot.
    assert_eq!(id.module, notlin::facts::FACT_MODULE);
    assert_eq!(id.package, "kotlin.collections");
    assert_eq!(id.owner_path, vec!["MutableList".to_string()]);
    // The *helper's* callee name, not the spelling in this file: resolution
    // came from the site's data, which is the whole point.
    assert_eq!(id.name, "map");
    assert_eq!(id.file, file);
    assert_eq!(id.parameters, vec!["kotlin.Int".to_string()]);
    assert!(!codes.iter().any(|code| code == "S001"));
}

#[test]
fn ambiguous_and_missed_offsets_keep_todays_retention() {
    for (label, status, shift) in [
        ("ambiguous", "Ambiguous", 0_i64),
        ("unknown", "Unknown", 0),
        ("offset-miss", "Established", 1),
    ] {
        let (_, resolution, codes) = plan_with(DISK_SOURCE, DISK_SOURCE, status, shift, label);
        assert!(
            matches!(resolution, FactStatus::Unknown),
            "{label}: expected Unknown, got {resolution:?}"
        );
        assert!(
            codes.iter().any(|code| code == "S001"),
            "{label}: expected the call to stay retained"
        );
    }
}

#[test]
fn a_speculative_snapshot_never_inherits_disk_facts() {
    let overlay = overlay_source();
    // Same call, same offset, same Established site — but the planned text is
    // not the text on disk, so the hash guard refuses and nothing resolves.
    let (_, resolution, codes) =
        plan_with(DISK_SOURCE, &overlay, "Established", 0, "overlay-refused");
    assert!(
        matches!(resolution, FactStatus::Unknown),
        "overlay must not inherit disk facts, got {resolution:?}"
    );
    assert!(codes.iter().any(|code| code == "S001"));
}

#[test]
fn java_files_are_never_asked_about() {
    let root = scratch("java-file");
    let file = root.join("Holder.java");
    let source = "package fixture;\nclass Holder { void run() { zap(1); } }\n";
    fs::write(&file, source).unwrap();
    let requests = root.join("requests.log");
    let runtime = FactsRuntime::with_command(batch_helper(&requests, "[]")).unwrap();
    let provider = FactsProvider::with_runtime(
        SyntaxSemanticProvider::new([(file.clone(), source.into())]),
        runtime,
    );

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), &file);
    plan_required_calls(source, &tree, &file, &provider, false, &mut plan);

    let asked = fs::read_to_string(&requests).unwrap_or_default();
    assert!(
        !asked.contains("ResolveCalls"),
        "a Java source must not reach a Kotlin call-site query: {asked}"
    );
}
