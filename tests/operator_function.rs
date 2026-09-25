use clap::Parser;
use std::path::PathBuf;

#[test]
fn operator_function_becomes_plain_java_method_without_warning() {
    let source = "package neutral.operatorfn\nclass Scalar {\n    operator fun plus(other: Scalar): Scalar = this\n}\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "Scalar.kt"]);
    let (files, errors, warnings, _coverage) =
        notlin::transpiler::transpile(source, &PathBuf::from("Scalar.kt"), &cli);

    assert_eq!(errors, 0);
    assert_eq!(warnings, 0, "operator adds no Java ABI requirement");
    let java = files
        .iter()
        .find(|(name, _)| name == "Scalar.java")
        .map(|(_, content)| content.as_str())
        .expect("Scalar.java");
    assert!(java.contains("Scalar plus(Scalar other)"), "{java}");
}
