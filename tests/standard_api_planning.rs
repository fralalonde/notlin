use clap::Parser;
use notlin::{cli::Cli, transpiler, workspace::SourceIndex};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_WORKSPACE: AtomicUsize = AtomicUsize::new(0);

fn workspace(
    name: &str,
    files: &[(&str, &str)],
    target: &str,
) -> (
    PathBuf,
    String,
    Vec<(String, String)>,
    usize,
    usize,
    notlin::diagnostics::FileCoverage,
) {
    let id = NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "notlin-standard-api-{name}-{}-{id}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    for (relative, contents) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    let path = root.join(target);
    let source = fs::read_to_string(&path).unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", path.to_str().unwrap()]);
    let (outputs, errors, warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    (root, source, outputs, errors, warnings, coverage)
}

const LOOKUP_KEY: &str = "package sample\npublic interface LookupKey { String getLookupKey(); }\n";

const IDENTIFIER: &str = "package sample\nimport java.util.*\nprivate const val KEY_DELIMITER: String = \"/\"\ninterface Identifier : LookupKey {\n    val objectId: UUID\n    fun text(): String { return objectId.toString() }\n    fun separatorText(): String = \"before:$KEY_DELIMITER:${KEY_DELIMITER}\"\n    val externalKey: String\n        get() { return getLookupKey().split(KEY_DELIMITER).last() }\n}\n";

#[test]
fn java_util_uuid_with_known_java_super_method_translates_in_strict_mode() {
    let (root, _, outputs, errors, _, coverage) = workspace(
        "jdk-uuid-bridge",
        &[
            ("LookupKey.java", LOOKUP_KEY),
            ("Identifier.kt", IDENTIFIER),
        ],
        "Identifier.kt",
    );
    let java = outputs
        .iter()
        .find(|(name, _)| name == "Identifier.java")
        .map(|(_, text)| text);
    assert_eq!(errors, 0);
    assert!(
        java.is_some(),
        "outputs: {outputs:?}; coverage: {coverage:?}"
    );
    assert!(
        !coverage
            .untranslated
            .iter()
            .any(|name| name == "Identifier")
    );
    let java = java.expect("translated interface output");
    assert!(
        java.contains("StringsKt.split("),
        "String.split must use Kotlin's literal delimiter semantics: {java}"
    );
    assert!(
        java.contains("CollectionsKt.last("),
        "last() must preserve Kotlin's empty-list and nullable-element behavior: {java}"
    );
    assert!(
        java.contains("new java.lang.String[]{\"/\"}"),
        "the same-file String const should be inlined into split: {java}"
    );
    assert!(
        !java.contains("KEY_DELIMITER"),
        "the generated Java must not retain a Kotlin top-level const reference: {java}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicitly_imported_domain_uuid_is_not_mistaken_for_java_util_uuid() {
    let collision = "package domain\nclass UUID\n";
    let source = "package sample\nimport java.util.*\nimport domain.UUID\ninterface Identifier : LookupKey {\n val objectId: UUID\n fun text(): String = objectId.toString()\n}\n";
    let (root, _, outputs, errors, _, coverage) = workspace(
        "uuid-name-collision",
        &[
            ("LookupKey.java", LOOKUP_KEY),
            ("domain/UUID.kt", collision),
            ("Identifier.kt", source),
        ],
        "Identifier.kt",
    );
    assert_eq!(errors, 0);
    assert!(
        !outputs.iter().any(|(name, _)| name == "Identifier.java"),
        "unexpected outputs: {outputs:?}"
    );
    assert!(
        coverage
            .untranslated
            .iter()
            .any(|name| name == "Identifier")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unknown_custom_method_chain_remains_retained_in_strict_mode() {
    let source = "package sample\ninterface Identifier : LookupKey {\n    val externalKey: String\n        get() { return getLookupKey().unknownExternalApi().last() }\n}\n";
    let (root, _, outputs, errors, _, coverage) = workspace(
        "unknown-external-chain",
        &[("LookupKey.java", LOOKUP_KEY), ("Identifier.kt", source)],
        "Identifier.kt",
    );
    assert_eq!(errors, 0);
    assert!(
        !outputs.iter().any(|(name, _)| name == "Identifier.java"),
        "unexpected outputs: {outputs:?}"
    );
    assert!(
        coverage
            .untranslated
            .iter()
            .any(|name| name == "Identifier")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn same_named_member_prevents_string_const_substitution() {
    let source = "package sample\nprivate const val KEY_DELIMITER: String = \"/\"\ninterface Identifier {\n    val KEY_DELIMITER: String\n    fun separatorText(): String = \"before:$KEY_DELIMITER\"\n}\n";
    let (root, _, outputs, errors, _, coverage) = workspace(
        "const-member-shadow",
        &[("Identifier.kt", source)],
        "Identifier.kt",
    );
    assert_eq!(errors, 0);
    let java = outputs
        .iter()
        .find(|(name, _)| name == "Identifier.java")
        .map(|(_, text)| text);
    assert!(
        java.is_some(),
        "outputs: {outputs:?}; coverage: {coverage:?}"
    );
    let java = java.unwrap();
    assert!(
        java.contains("getKEY_DELIMITER()"),
        "the member must remain a member read: {java}"
    );
    assert!(
        !java.contains("before:/"),
        "the unrelated top-level const must not replace the member: {java}"
    );
    fs::remove_dir_all(root).unwrap();
}
