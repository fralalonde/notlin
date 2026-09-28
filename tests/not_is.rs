use clap::Parser;
use std::path::PathBuf;

#[test]
fn kotlin_not_is_becomes_negated_instanceof() {
    let source = "class Thing {\n    fun same(other: Any?): Boolean {\n        if (other !is Thing) return false\n        return true\n    }\n}\n";
    let cli = notlin::cli::Cli::parse_from(["notlin", "Thing.kt"]);
    let (files, errors, _warnings, _coverage) =
        notlin::transpiler::transpile(source, &PathBuf::from("Thing.kt"), &cli);
    assert_eq!(errors, 0);
    let output = files
        .into_iter()
        .map(|(_, content)| content)
        .collect::<String>();
    assert!(output.contains("!(other instanceof Thing)"), "{output}");
}
