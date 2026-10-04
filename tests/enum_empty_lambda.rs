use clap::Parser;
use std::path::PathBuf;

#[test]
fn empty_lambda_enum_argument_has_a_java_target_type() {
    let source = r#"
package neutral.enums

interface ValueHolder {
    val value: Any?
}

enum class Kind(override val value: Any?) : ValueHolder {
    EMPTY({})
}
"#;
    let cli = notlin::cli::Cli::parse_from(["notlin", "Kind.kt"]);
    let (files, errors, _warnings, _coverage) =
        notlin::transpiler::transpile(source, &PathBuf::from("Kind.kt"), &cli);

    assert_eq!(errors, 0, "unexpected translation errors");
    let java = files
        .iter()
        .find(|(name, _)| name == "Kind.java")
        .map(|(_, content)| content.as_str())
        .expect("Kind.java");
    assert!(
        java.contains("(kotlin.jvm.functions.Function0<kotlin.Unit>) () -> kotlin.Unit.INSTANCE"),
        "empty Kotlin lambda needs an explicit Function0 target in Java:\n{java}"
    );
}
