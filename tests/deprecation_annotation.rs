//! Kotlin deprecation metadata is not Java's Deprecated annotation contract.
use std::{fs, process::Command};

fn translate(label: &str, source: &str, approximations: bool) -> (String, String) {
    let root =
        std::env::temp_dir().join(format!("notlin-deprecation-{label}-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Sample.kt"), source).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command.current_dir(&root).args([
        "--jvm-facts=off",
        "--annotations",
        "none",
        "-vv",
        "-o",
        "out",
        "Sample.kt",
    ]);
    if approximations {
        command.arg("--allow-approximations");
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let java = fs::read_to_string(root.join("out/Example.java")).unwrap_or_default();
    let diagnostics = String::from_utf8_lossy(&result.stderr).into_owned();
    fs::remove_dir_all(root).unwrap();
    (java, diagnostics)
}

#[test]
fn opted_in_kotlin_deprecation_uses_the_java_annotation_contract() {
    let source = "package neutral.deprecation\nclass Example {\n    @kotlin.Deprecated(\"use replacement\", level = kotlin.DeprecationLevel.WARNING)\n    fun legacy(): String = \"old\"\n}\n";
    let (java, diagnostics) = translate("opt-in", source, true);
    assert!(
        java.contains("@java.lang.Deprecated"),
        "Java deprecation missing:\n{java}"
    );
    assert!(
        !java.contains("DeprecationLevel"),
        "Kotlin arguments leaked:\n{java}"
    );
    assert!(
        diagnostics.contains("deprecation metadata") && diagnostics.contains("Approximated"),
        "metadata loss must be diagnosed:\n{diagnostics}"
    );
}

#[test]
fn kotlin_deprecation_metadata_is_retained_without_loss_opt_in() {
    for (label, prefix, annotation) in [
        ("default", "", "Deprecated"),
        ("qualified", "", "kotlin.Deprecated"),
        (
            "alias",
            "import kotlin.Deprecated as Obsolete\n",
            "Obsolete",
        ),
    ] {
        let source = format!(
            "package neutral.retained\n{prefix}class Example {{\n    @{annotation}(\"old\", level = kotlin.DeprecationLevel.ERROR)\n    fun legacy(): String = \"old\"\n}}\n"
        );
        let (java, diagnostics) = translate(label, &source, false);
        assert!(
            java.is_empty(),
            "metadata loss must require opt-in:\n{java}"
        );
        assert!(
            diagnostics.contains("deprecation metadata"),
            "{diagnostics}"
        );
    }
}

#[test]
fn class_annotation_wrappers_are_loss_gated_too() {
    let source = "package neutral.wrapper\n@kotlin.Deprecated(\"old\", level = kotlin.DeprecationLevel.HIDDEN)\nclass Example\n";
    let (kept, _) = translate("wrapper-default", source, false);
    assert!(
        kept.is_empty(),
        "class wrapper bypassed loss policy:\n{kept}"
    );
    let (java, diagnostics) = translate("wrapper-opt-in", source, true);
    assert!(java.contains("@java.lang.Deprecated"), "{java}");
    assert!(
        diagnostics.contains("deprecation metadata"),
        "{diagnostics}"
    );
}

#[test]
fn wildcard_deprecation_identity_stays_unproven_even_with_loss_opt_in() {
    let (java, diagnostics) = translate(
        "wildcard",
        "package neutral.uncertain\nimport external.annotations.*\nclass Example {\n    @Deprecated(\"custom\")\n    fun legacy(): String = \"old\"\n}\n",
        true,
    );
    assert!(
        java.is_empty(),
        "unproven annotation identity was erased:\n{java}"
    );
    assert!(
        diagnostics.contains("Deprecated annotation binding is uncertain"),
        "{diagnostics}"
    );
}

#[test]
fn user_and_java_deprecated_annotations_keep_their_own_contracts() {
    let (java, diagnostics) = translate(
        "custom",
        "package neutral.custom\nannotation class Deprecated(val value: String)\nclass Example {\n    @Deprecated(\"custom\")\n    fun legacy(): String = \"old\"\n}\n",
        true,
    );
    assert!(java.contains("@Deprecated(\"custom\")"), "{java}");
    assert!(
        !diagnostics.contains("deprecation metadata"),
        "{diagnostics}"
    );
    let (java, diagnostics) = translate(
        "java",
        "package neutral.javaannotation\nclass Example {\n    @java.lang.Deprecated(since = \"2\", forRemoval = true)\n    fun legacy(): String = \"old\"\n}\n",
        false,
    );
    assert!(
        java.contains("since = \"2\"") && java.contains("forRemoval = true"),
        "{java}"
    );
    assert!(
        !diagnostics.contains("deprecation metadata"),
        "{diagnostics}"
    );
}
