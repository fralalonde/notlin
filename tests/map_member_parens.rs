use clap::Parser;
use std::path::PathBuf;

/// Java Map methods `containsKey`, `isNotEmpty`, `hashCode` must KEEP their
/// call parens when the Kotlin source emitted a call; bare property emission
/// (`m.containsKey;`) cannot compile.
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
