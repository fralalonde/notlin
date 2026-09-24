use clap::Parser;
use std::path::PathBuf;

/// Kotlin stdlib collection extensions invoked WITH parens (`isNotEmpty()`,
/// `any()`, `none()`) must lower to Java calls — never bare member reads —
/// even when the receiver's members (e.g. Map.isNotEmpty) do not exist in
/// Java and the extension requires a stream.
#[test]
fn is_not_empty_stays_a_predicate_call() {
    let source = "package neutral.mape\nfun nonzero(m: Map<String, Int>) = m.isNotEmpty()\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    let m = files
        .iter()
        .map(|(_, c)| c.as_str())
        .collect::<Vec<_>>()
        .join("\\n");
    if m.trim().is_empty() {
        panic!(
            "no files: {:?}",
            files.iter().map(|(n, _)| n).collect::<Vec<_>>()
        );
    }
    assert!(m.contains("isNotEmpty()"), "isNotEmpty emitted bare: {m}");
    assert!(!m.contains("isNotEmpty;"), "bare member: {m}");
}
