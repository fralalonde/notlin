//! Kotlin compiler-only suppression metadata must not become an unresolved
//! Java annotation. A user annotation with the same spelling must survive.
use std::{fs, process::Command};

fn emit(label: &str, source: &str) -> String {
    let root = std::env::temp_dir().join(format!("notlin-suppress-{label}-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Sample.kt"), source).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root)
        .args(["--jvm-facts=off", "-o", "out", "Sample.kt"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let java = fs::read_to_string(root.join("out/Example.java")).unwrap();
    fs::remove_dir_all(root).unwrap();
    java
}

#[test]
fn kotlin_source_only_suppress_does_not_leak_into_java() {
    for (label, imports, annotation) in [
        ("default", "", "Suppress"),
        ("qualified", "", "kotlin.Suppress"),
        ("alias", "import kotlin.Suppress as Quiet\n", "Quiet"),
    ] {
        let java = emit(
            label,
            &format!(
                "package neutral.suppress\n{imports}\nclass Example {{\n    @{annotation}(\"UNCHECKED_CAST\")\n    fun echo(text: String): String = text\n}}\n"
            ),
        );
        assert!(
            !java.contains("@Suppress(")
                && !java.contains("@kotlin.Suppress(")
                && !java.contains("@Quiet("),
            "Kotlin suppression leaked into Java:\n{java}"
        );
        assert!(java.contains("String echo("), "method lost:\n{java}");
    }
}

#[test]
fn user_defined_suppress_annotation_is_preserved() {
    let java = emit(
        "local",
        "package neutral.shadow\nannotation class Suppress(val value: String)\nclass Example {\n    @Suppress(\"custom\")\n    fun echo(text: String): String = text\n}\n",
    );
    assert!(
        java.contains("@Suppress(\"custom\")"),
        "user annotation lost:\n{java}"
    );
}

#[test]
fn wildcard_suppress_binding_is_retained_instead_of_guessed() {
    for (label, declaration) in [
        (
            "type",
            "class Example { @Suppress(\"custom\") fun echo(text: String): String = text }",
        ),
        (
            "function",
            "@Suppress(\"custom\") fun echo(text: String): String = text",
        ),
        (
            "property",
            "class Example { @Suppress(\"custom\") val text: String = \"hello\" }",
        ),
    ] {
        let root = std::env::temp_dir().join(format!(
            "notlin-suppress-wildcard-{label}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("Sample.kt"),
            format!("package neutral.wildcard\nimport external.annotations.*\n{declaration}\n"),
        )
        .unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_notlin"))
            .current_dir(&root)
            .args(["--jvm-facts=off", "-vv", "-o", "out", "Sample.kt"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            stderr.contains("Suppress annotation binding is uncertain"),
            "missing retention diagnostic:\n{stderr}"
        );
        assert!(!root.join("out/Example.java").exists());
        assert!(!root.join("out/SampleKt.java").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
