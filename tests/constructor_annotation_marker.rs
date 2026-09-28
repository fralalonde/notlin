//! Kotlin metadata is what tells an annotation processor which constructor of a
//! data class is the primary one. Java has no equivalent, so a constructor with
//! default arguments — which the migration emits as several delegating
//! constructors — becomes ambiguous for consumers that pick a constructor
//! reflectively (MapStruct: "Ambiguous constructors found ... annotate the
//! default constructor with an annotation named @Default").
//!
//! Two things must therefore survive the translation: an annotation the source
//! put on the primary constructor, and — when the migration itself introduces
//! the extra constructors — the workspace's own `Default`-named marker. The
//! marker is emitted fully qualified because the declaring file has no import
//! for it, and it may be declared in **Java**, so the index must classify a
//! Java `@interface` as an annotation declaration.

use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::fs;

fn root_for(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("notlin-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    root
}

fn translate(root: &std::path::PathBuf, provider: &std::path::Path) -> String {
    let index = SourceIndex::discover(root).unwrap();
    let cli = Cli::parse_from([
        "notlin",
        "--lombok",
        "--commons-lang",
        "--in-place",
        provider.to_str().unwrap(),
    ]);
    let source = fs::read_to_string(provider).unwrap();
    let (files, errors, warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        provider,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    files
        .iter()
        .find(|(name, _)| name == "D.java")
        .map(|(_, text)| text.clone())
        .unwrap_or_else(|| {
            panic!(
                "D must translate; errors={errors:?} warnings={warnings:?} untranslated={:?}",
                coverage.untranslated
            )
        })
}

#[test]
fn a_java_default_annotation_marks_the_constructor_that_gained_overloads() {
    let root = root_for("ctor-marker-java");
    fs::create_dir_all(root.join("neutral/probe")).unwrap();
    // Declared in Java, never in Kotlin: the index must still see it, or the
    // marker rule finds nothing and the ambiguity is never resolved.
    fs::write(
        root.join("neutral/probe/Default.java"),
        "package neutral.probe;\n\nimport java.lang.annotation.ElementType;\nimport java.lang.annotation.Retention;\nimport java.lang.annotation.RetentionPolicy;\nimport java.lang.annotation.Target;\n\n@Retention(RetentionPolicy.RUNTIME)\n@Target(ElementType.CONSTRUCTOR)\npublic @interface Default {\n}\n",
    )
    .unwrap();
    let provider = root.join("neutral/probe/D.kt");
    fs::write(
        &provider,
        "package neutral.probe\n\ndata class D(val a: Int, val b: String = \"x\")\n",
    )
    .unwrap();

    let java = translate(&root, &provider);
    assert!(
        java.contains("@neutral.probe.Default"),
        "the primary constructor must carry the workspace marker, fully \
         qualified (no import exists to resolve the simple name)\n{java}"
    );
    assert!(
        java.contains("public D(int a)"),
        "the defaulted parameter must still produce a delegating overload\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn the_marker_the_source_wrote_is_kept_verbatim() {
    let root = root_for("ctor-marker-source");
    fs::create_dir_all(root.join("neutral/probe")).unwrap();
    fs::write(
        root.join("neutral/probe/Default.java"),
        "package neutral.probe;\n\npublic @interface Default {\n}\n",
    )
    .unwrap();
    let provider = root.join("neutral/probe/D.kt");
    fs::write(
        &provider,
        "package neutral.probe\n\nimport neutral.probe.Default\n\ndata class D @Default constructor(val a: Int, val b: String = \"x\")\n",
    )
    .unwrap();

    let java = translate(&root, &provider);
    assert!(
        java.contains("@Default"),
        "an annotation written on the Kotlin primary constructor must ride on \
         the generated constructor\n{java}"
    );
    assert!(
        !java.contains("@JvmOverloads"),
        "Kotlin-ABI-only constructor annotations have no Java counterpart\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn no_marker_is_invented_when_the_workspace_declares_none() {
    let root = root_for("ctor-marker-absent");
    fs::create_dir_all(root.join("neutral/probe")).unwrap();
    let provider = root.join("neutral/probe/D.kt");
    fs::write(
        &provider,
        "package neutral.probe\n\ndata class D(val a: Int, val b: String = \"x\")\n",
    )
    .unwrap();

    let java = translate(&root, &provider);
    assert!(
        !java.to_lowercase().contains("default"),
        "with no `Default`-named annotation in the workspace nothing may be \
         invented for one\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
