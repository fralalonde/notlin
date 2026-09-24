use clap::Parser;
use notlin::transpiler;
use std::path::PathBuf;

/// Kotlin `Iterable.stream()` (stdlib-jdk8 extension) lowers to Java's
/// `Collection.stream()` — a single `.stream()`, not `.stream().stream()`.
#[test]
fn stream_extension_does_not_double_stream() {
    let source = "package neutral.xx\nfun peeks(units: List<String>) = units.stream().first()\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "S.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("S.kt"), &cli);
    assert_eq!(errors, 0);
    let s = files
        .iter()
        .find(|(n, _)| n == "S.java")
        .map(|(_, c)| c.as_str())
        .expect("S.java");
    assert!(
        !s.contains(".stream().stream()"),
        "double .stream() emitted: {s}"
    );
}
