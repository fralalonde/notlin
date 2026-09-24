use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::fs;
use std::path::PathBuf;

fn kclass_source() -> String {
    "package neutral.props\n\
     import kotlin.reflect.KClass\n\
     enum class Kind { A, B }\n\
     class KindType private constructor(val type: Class<out Enum<*>>, val defaultValue: String?) {\n\
     \x20 companion object {\n\
     \x20     fun of(type: KClass<out Enum<*>>, defaultValue: String?): KindType {\n\
     \x20         return KindType(type.java, defaultValue)\n\
     \x20     }\n\
     \x20 }\n\
     }\n"
        .to_string()
}

/// Translated when no residual Kotlin file consumes the declaration: the
/// `of(Class...)` static replaces the KClass factory.
#[test]
fn kclass_declaration_translates_without_kotlin_consumers() {
    let root = std::env::temp_dir().join(format!("notlin-kclass-free-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("KindType.kt");
    let source = kclass_source();
    fs::write(&path, &source).unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", path.to_str().unwrap()]);
    let (files, errors, warnings, _cov) = notlin::transpiler::transpile_with_workspace(
        &source,
        &path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0, "{:?}", warnings);
    let kind_java = files
        .iter()
        .find(|(name, _)| name == "KindType.java")
        .map(|(_, c)| c.as_str())
        .expect("KindType.java must emit when no residual Kotlin consumes it");
    assert!(
        kind_java.contains("Class<"),
        "KClass must lower to Class: {kind_java}"
    );
    assert!(
        !kind_java.contains("KClass"),
        "no KClass may leak into Java: {kind_java}"
    );
    fs::remove_dir_all(root).unwrap();
}

/// Retained when a residual Kotlin file still consumes the declaration —
/// translating it would surface a `Class` ABI the KClass-bound caller can
/// no longer satisfy.
#[test]
fn kclass_declaration_retained_while_kotlin_consumers_remain() {
    let root = std::env::temp_dir().join(format!("notlin-kclass-used-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let decl_path = root.join("KindType.kt");
    let source = kclass_source();
    fs::write(&decl_path, &source).unwrap();
    let consumer = root.join("FactoryUser.kt");
    fs::write(
        &consumer,
        "package neutral.props\n\
         val made = KindType.of(Kind::class, \"d\")\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", decl_path.to_str().unwrap()]);
    let (files, errors, warnings, _coverage) = notlin::transpiler::transpile_with_workspace(
        &source,
        &decl_path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0, "{:?}", warnings);
    assert!(
        !files.iter().any(|(name, _)| name == "KindType.java"),
        "KindType must stay Kotlin while residual Kotlin consumes it: {files:?}"
    );
    fs::remove_dir_all(root).unwrap();
}

/// `X::class.java` lowers to the Java class literal, so a KClass-free
/// declaration with a reflection-free default still translates.
#[test]
fn class_literal_call_reaches_java_class_overload() {
    let source = "package neutral.props\n\
                  enum class Kind { A, B }\n\
                  class KindSpec(val type: Class<out Enum<*>>) {\n\
                  \x20   fun default(): KindSpec = KindSpec(Kind::class.java)\n\
                  }\n";
    let cli = Cli::parse_from(vec!["notlin", "KindSpec.kt"]);
    let path = PathBuf::from("KindSpec.kt");
    let (files, errors, _warnings, _cov) = transpiler::transpile(source, &path, &cli);
    assert_eq!(errors, 0);
    let spec = files
        .iter()
        .find(|(name, _)| name == "KindSpec.java")
        .map(|(_, c)| c.as_str())
        .expect("KindSpec.java");
    assert!(spec.contains("Kind.class"), "{spec}");
}
