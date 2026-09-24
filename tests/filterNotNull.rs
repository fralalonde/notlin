use clap::Parser;
use std::path::PathBuf;

/// Kotlin `Array<T>.filterNotNull()` must lower to a Java stream expression
/// (the JDK has no such member): filter Objects::nonNull then collect,
/// producing a List — not a bare extension member.
#[test]
fn filter_not_null_lowers_to_stream() {
    let source = "package neutral.fnn\nfun clean(ctx: Array<String?>): List<String> = ctx.filterNotNull().map { it.length }\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    let m = files
        .iter()
        .map(|(_, c)| c.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = errors;
    assert!(
        m.contains("Objects::nonNull") || m.contains("filter(Objects::nonNull"),
        "filterNotNull not lowered via Objects::nonNull: {m}"
    );
    assert!(
        !m.contains("filterNotNull"),
        "bare filterNotNull survived: {m}"
    );
    let _ = &cli;
}
