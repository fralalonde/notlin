use clap::Parser;
use notlin::transpiler;
use std::path::PathBuf;

/// Kotlin enum `EnumKind.entries` lowers to Java `values()` — `getEntries()`
/// is a Kotlin ABI static that does not survive pure-Java enums and cannot
/// resolve at the call site.
#[test]
fn enum_entries_becomes_values() {
    let source =
        "package neutral.ee\\nenum class Flag { A, B }\\nfun all(): List<Flag> = Flag.entries\\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    let m = files
        .iter()
        .find(|(n, _)| n == "M.java")
        .map(|(_, c)| c.as_str())
        .unwrap_or_default();
    let _ = errors;
    assert!(
        !m.contains(".entries") && !m.contains("getEntries()"),
        "enum entries not lowered to values(): {m}"
    );
}
