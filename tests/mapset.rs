use clap::Parser;
use std::path::PathBuf;

/// A holder class with a `K -> V` map property that uses members of the Map
/// API (containsKey, hashCode) must emit the members as valid Java calls /
/// fields, not bare identifiers.
#[test]
fn map_holder_members_emit_java_calls() {
    let source = concat!(
        "package neutral.mapset\n",
        "data class Hold(val m: Map<String, Int>) {\n",
        "    fun has(k: String) = m.containsKey(k)\n",
        "    fun bucket() = m.hashCode()\n",
        "}\n"
    );
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    let m = files
        .iter()
        .map(|(_, c)| c.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(m.contains("containsKey("), "lost parens: {m}");
    assert!(!m.contains("containsKey;"), "bare member: {m}");
    assert!(!m.contains("hashCode;"), "bare member: {m}");
}
