use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::fs;
use std::path::PathBuf;

/// A call into a reified-inline companion function of a declaration that is
/// RETAINED in Kotlin has no Java-callable ABI (reified inline functions are
/// inlined only at Kotlin call sites; Java cannot resolve them). Emitting a
/// bare `Owner.make()` produces uncompilable Java, so the calling declaration
/// must taint back to Kotlin instead of emitting broken Java.
#[test]
fn reified_companion_calls_taint_caller() {
    let root = std::env::temp_dir().join(format!("notlin-reified2-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    fs::write(
        root.join("Owner.kt"),
        "package neutral.kind\ninterface Owner {\n    companion object {\n        inline fun <reified T : Any> make(): T = throw NotImplementedError()\n    }\n}\n",
    )
    .unwrap();

    let caller = "package neutral.kind\nenum class Kind(val any: Any) : Owner {\n    A(Owner.make<String>()),\n    B(\"b\")\n}\n";
    fs::write(root.join("Kind.kt"), caller).unwrap();

    let cli = Cli::parse_from(vec![
        "notlin",
        "--root",
        root.to_string_lossy().as_ref(),
        "Kind.kt",
    ]);
    let index = SourceIndex::discover(&root).unwrap();
    let (files, errors, _warnings, _cov) = transpiler::transpile_with_workspace(
        caller,
        &PathBuf::from("Kind.kt"),
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    let kind = files
        .iter()
        .find(|(n, _)| n == "Kind.java")
        .map(|(_, c)| c.as_str());
    match kind {
        Some(c) => assert!(
            !c.contains("Owner.make("),
            "un-callable reified companion leaked into Java: {c}"
        ),
        None => {}
    }
    let _ = fs::remove_dir_all(root);
}
