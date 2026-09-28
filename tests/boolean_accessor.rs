use clap::Parser;
use std::path::PathBuf;

#[test]
fn boolean_property_and_is_method_do_not_duplicate_accessor() {
    let source = "data class Session(val authenticated: Boolean) { fun isAuthenticated(): Boolean = authenticated }";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "--lombok", "Session.kt"]);
    let (files, errors, _warnings, _coverage) =
        notlin::transpiler::transpile(source, &PathBuf::from("Session.kt"), &cli);
    assert_eq!(errors, 0);
    let output = files
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<String>();
    assert!(
        output.contains("getAuthenticated()"),
        "missing property getter: {output}"
    );
    assert_eq!(
        output.matches("isAuthenticated()").count(),
        1,
        "Boolean property getter collided with explicit method: {output}"
    );
}
