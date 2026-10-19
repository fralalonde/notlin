use clap::Parser;
use std::path::Path;

fn translate(source: &str) -> String {
    let cli = notlin::cli::Cli::parse_from(["notlin", "--annotations", "none", "Contract.kt"]);
    let (files, errors, _, _) =
        notlin::transpiler::transpile(source, Path::new("Contract.kt"), &cli);
    assert_eq!(errors, 0);
    files.into_iter().map(|(_, java)| java).collect()
}

#[test]
fn kotlin_static_property_markers_leave_explicit_java_members() {
    for (import, annotation) in [
        ("", "JvmStatic"),
        ("", "kotlin.jvm.JvmStatic"),
        ("import kotlin.jvm.JvmStatic\n", "JvmStatic"),
        (
            "import kotlin.jvm.JvmStatic as StaticBridge\n",
            "StaticBridge",
        ),
    ] {
        for owner in ["interface", "class"] {
            let source = format!(
                "package companion.marker\n{import}{owner} Contract {{\n    companion object {{\n        @{annotation}\n        val DEFAULT: Int = 7\n    }}\n}}\n"
            );
            let java = translate(&source);
            assert!(
                java.contains("public static final int DEFAULT = 7;"),
                "{java}"
            );
            assert!(java.contains("static int getDEFAULT()"), "{java}");
            assert!(!java.contains(&format!("@{annotation}")), "{java}");
        }
    }
}

#[test]
fn unrelated_static_annotation_spelling_is_preserved() {
    for (import, annotation) in [
        ("", "custom.JvmStatic"),
        ("import custom.JvmStatic\n", "JvmStatic"),
    ] {
        let source = format!(
            "package companion.marker\n{import}interface Contract {{\n    companion object {{\n        @{annotation}\n        val DEFAULT: Int = 7\n    }}\n}}\n"
        );
        let java = translate(&source);
        assert!(java.contains(&format!("@{annotation}")), "{java}");
    }
}

#[test]
fn source_annotation_is_preserved_without_a_workspace_index() {
    let java = translate(
        "package companion.marker\nannotation class JvmStatic\ninterface Contract {\n    companion object {\n        @JvmStatic\n        val DEFAULT: Int = 7\n    }\n}\n",
    );
    assert!(java.contains("@JvmStatic"), "{java}");
}
