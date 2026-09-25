use clap::Parser;
use std::path::PathBuf;

/// Java Map methods `containsKey`, `isNotEmpty`, `hashCode` must KEEP their
/// call parens when the Kotlin source emitted a call; bare property emission
/// (`m.containsKey;`) cannot compile.
#[test]
fn character_literal_is_direct_java_without_warning() {
    let source = "package neutral.mapc\nfun marker(): Char = 'x'\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    assert_eq!(
        warnings, 0,
        "character literal should be directly supported"
    );
    let m = files
        .iter()
        .find(|(n, _)| n == "M.java")
        .map(|(_, c)| c.as_str())
        .expect("M.java");
    assert!(m.contains("return 'x';"), "{m}");
}

#[test]
fn string_builder_append_is_a_direct_java_member_without_warning() {
    let source =
        "package neutral.mapc\nfun build(): String = StringBuilder().append(\"a\").toString()\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    assert_eq!(warnings, 0, "append should not be approximated");
    let m = files
        .iter()
        .find(|(n, _)| n == "M.java")
        .map(|(_, c)| c.as_str())
        .expect("M.java");
    assert!(
        m.contains("StringBuilder().append(\"a\").toString()"),
        "{m}"
    );
}

#[test]
fn pre_streamed_filter_is_a_direct_java_member_without_warning() {
    let source = "package neutral.mapc\nfun select(items: List<String>): java.util.stream.Stream<String> = items.stream().filter { value -> value.startsWith(\"x\") }\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    assert_eq!(warnings, 0, "Stream.filter should be a direct Java member");
    let m = files
        .iter()
        .find(|(n, _)| n == "M.java")
        .map(|(_, c)| c.as_str())
        .expect("M.java");
    assert!(m.contains("items.stream().filter("), "{m}");
}

#[test]
fn map_call_members_keep_parens() {
    let source = "package neutral.mapc\nfun has(m: Map<String, Int>) = m.containsKey(\"a\")\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    let m = files
        .iter()
        .find(|(n, _)| n == "M.java")
        .map(|(_, c)| c.as_str())
        .expect("M.java");
    assert!(
        m.contains("m.containsKey("),
        "containsKey call lost its parens: {m}"
    );
    assert!(!m.contains("containsKey;"), "bare member emission: {m}");
}
