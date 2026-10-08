use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::fs;

#[test]
fn retains_nullable_primitive_override_that_would_change_the_jvm_return_type() {
    let root = std::env::temp_dir().join(format!(
        "notlin-nullable-primitive-contract-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("Definitions.kt");
    fs::write(
        &path,
        "package sample\ninterface Contract {\n    val count: Int?\n}\ndata class Implementation(override val count: Int) : Contract\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", path.to_str().unwrap()]);
    let source = fs::read_to_string(&path).unwrap();
    let (files, errors, _warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    assert!(
        !files.iter().any(|(name, _)| name == "Implementation.java"),
        "files: {:?}; untranslated: {:?}; declarations: {:?}",
        files.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        coverage.untranslated,
        index.declarations().collect::<Vec<_>>()
    );
    assert!(
        coverage
            .untranslated
            .iter()
            .any(|name| name == "Implementation"),
        "Int? lowers to Integer while Int lowers to int, so Java cannot override the getter"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn translates_nullable_covariant_override_when_narrow_type_is_java() {
    let root = std::env::temp_dir().join(format!(
        "notlin-nullable-java-covariance-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("Definitions.kt");
    fs::write(
        &path,
        "package sample\ninterface Identifier\nopen class Base : Identifier\ninterface Contract {\n    val priority: Identifier?\n}\ndata class Implementation(override val priority: Ref) : Contract\n",
    )
    .unwrap();
    fs::write(
        root.join("Ref.java"),
        "package sample;\npublic class Ref extends Base {}\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let source_file = index
        .kotlin_files()
        .find(|file| file.path.ends_with("Definitions.kt"))
        .expect("fixture Kotlin source indexed");
    let reference = index
        .resolve_type(source_file, "Ref")
        .expect("Java narrow type resolves from the mixed index");
    assert_eq!(reference.language, notlin::workspace::SourceLanguage::Java);
    assert!(
        index.supertype_closure_contains(source_file, reference, "Identifier"),
        "the Java-to-Kotlin supertype chain must prove Ref implements Identifier: {reference:?}"
    );
    let cli = Cli::parse_from(["notlin", "--in-place", path.to_str().unwrap()]);
    let source = fs::read_to_string(&path).unwrap();
    let (files, errors, _warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    let java = files
        .iter()
        .find(|(name, _)| name == "Implementation.java")
        .map(|(_, text)| text)
        .unwrap_or_else(|| {
            panic!(
                "the mixed Kotlin/Java index proves Ref is an Identifier; untranslated: {:?}",
                coverage.untranslated
            )
        });
    assert!(
        java.contains("Ref getPriority()"),
        "the Java subtype must remain the covariant getter return:\n{java}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicit_override_method_preserves_non_null_return_metadata() {
    let root = std::env::temp_dir().join(format!(
        "notlin-non-null-override-method-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("NullableContract.java"),
        "package sample;\npublic interface NullableContract { @org.jetbrains.annotations.Nullable String getValue(); }\n",
    )
    .unwrap();
    fs::write(
        root.join("NullableMethodContract.java"),
        "package sample;\npublic interface NullableMethodContract { @org.jetbrains.annotations.Nullable String getLabel(); }\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("org/jetbrains/annotations")).unwrap();
    fs::write(
        root.join("org/jetbrains/annotations/Nullable.java"),
        "package org.jetbrains.annotations;\n@java.lang.annotation.Target({java.lang.annotation.ElementType.METHOD, java.lang.annotation.ElementType.PARAMETER, java.lang.annotation.ElementType.FIELD, java.lang.annotation.ElementType.TYPE_USE})\n@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)\npublic @interface Nullable {}\n",
    )
    .unwrap();
    fs::write(
        root.join("org/jetbrains/annotations/NotNull.java"),
        "package org.jetbrains.annotations;\n@java.lang.annotation.Target({java.lang.annotation.ElementType.METHOD, java.lang.annotation.ElementType.PARAMETER, java.lang.annotation.ElementType.FIELD, java.lang.annotation.ElementType.TYPE_USE})\n@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)\npublic @interface NotNull {}\n",
    )
    .unwrap();
    let path = root.join("NullablePropertyContract.kt");
    fs::write(
        &path,
        "package sample\n\
         interface NullablePropertyContract {\n\
             val value: String?\n\
         }\n",
    )
    .unwrap();
    fs::write(
        root.join("NarrowPropertyContract.kt"),
        "package sample\n\
         interface NarrowPropertyContract : NullablePropertyContract {\n\
             override val value: String\n\
         }\n",
    )
    .unwrap();
    fs::write(
        root.join("NarrowMethodContract.kt"),
        "package sample\n\
         interface NarrowMethodContract : NullableMethodContract {\n\
             override fun getLabel(): String\n\
         }\n",
    )
    .unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "jetbrains", "--in-place"])
        .arg(&root)
        .output()
        .expect("run workspace migration");
    assert!(
        output.status.success(),
        "workspace migration failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let java =
        fs::read_to_string(root.join("NarrowPropertyContract.java")).unwrap_or_else(|error| {
            panic!(
                "fixpoint migration should emit NarrowPropertyContract.java: {error}; {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    assert!(
        java.contains("@org.jetbrains.annotations.NotNull") && java.contains("String getValue();"),
        "non-null property override metadata must reach the Java getter:\n{java}"
    );
    let method_java = fs::read_to_string(root.join("NarrowMethodContract.java"))
        .expect("getter-method override should migrate against a Java nullable contract");
    assert!(
        method_java.contains("@org.jetbrains.annotations.NotNull")
            && method_java.contains("String getLabel();"),
        "non-null method override metadata must remain visible:\n{method_java}"
    );
    fs::remove_dir_all(root).unwrap();
}
