use clap::Parser;
use std::path::PathBuf;

/// A data-class receiver's implicit `copy(field = value)` must become a
/// constructor call with the unchanged components read through this class's
/// Java accessors. Java has no generated Kotlin copy() method.
#[test]
fn implicit_data_class_copy_rebuilds_current_instance() {
    let source = concat!(
        "package neutral.copy\n\n",
        "data class Holder(val name: String, val count: Int) {\n",
        "    fun renamed(name: String): Holder = copy(name = name)\n",
        "}\n"
    );
    let cli = notlin::cli::Cli::parse_from(["notlin", "M.kt"]);
    let (files, errors, _warnings, _coverage) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    let java = files
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(errors, 0, "{java}");
    assert!(
        !java.contains("return copy("),
        "Kotlin copy leaked into Java:\n{java}"
    );
    assert!(
        java.contains("new Holder(name, this.count())"),
        "implicit copy must preserve unchanged components:\n{java}"
    );
}

/// A receiver-qualified Kotlin copy outside the current data class has no
/// universally valid Java form without that data class's component schema.
/// Keep the caller in Kotlin rather than writing a nonexistent `.copy` call.
#[test]
fn unresolved_qualified_copy_taints_instead_of_emitting_java_copy_call() {
    let source = concat!(
        "package neutral.copy\n\n",
        "class Foreign\n",
        "fun update(value: Foreign): Foreign = value.copy()\n"
    );
    let cli = notlin::cli::Cli::parse_from(["notlin", "M.kt"]);
    let (files, errors, _warnings, _coverage) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    let java = files
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let _ = errors;
    assert!(
        !java.contains(".copy("),
        "invalid Java copy leaked:\n{java}"
    );
    assert!(
        !java.contains("Foreign update"),
        "the unsupported caller must not be emitted:\n{java}"
    );
}
