use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::fs;
use std::path::PathBuf;

/// A translated Kotlin class whose companion object has plain fns must
/// expose a nested `Companion` bridge dispatching to the class statics, so
/// call sites compiled against the retained Kotlin twin (`Owner.Companion.of`)
/// keep compiling after translation.
#[test]
fn translated_class_companion_gets_bridge() {
    let root = std::env::temp_dir().join(format!("notlin-cbridge-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Owner.kt"),
        "package neutral.kind\nclass Owner(val id: Int) {\n    companion object {\n        fun of(id: Int): Owner = Owner(id)\n    }\n}\n",
    )
    .unwrap();
    let cli = Cli::parse_from(vec![
        "notlin",
        "--root",
        root.to_string_lossy().as_ref(),
        "Owner.kt",
    ]);
    let index = SourceIndex::discover(&root).unwrap();
    let (files, errors, _warnings, _cov) = transpiler::transpile_with_workspace(
        "package neutral.kind\nclass Owner(val id: Int) {\n    companion object {\n        fun of(id: Int): Owner = Owner(id)\n    }\n}\n",
        &PathBuf::from("Owner.kt"),
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    let owner = files
        .iter()
        .find(|(n, _)| n == "Owner.java")
        .map(|(_, c)| c.as_str())
        .expect("Owner.java emitted");
    assert!(
        owner.contains("public static final class Companion"),
        "translated class with plain companion fns must emit a Companion bridge: {owner}"
    );
    assert!(
        owner.contains("public static Owner of(") && owner.contains("Owner.of(id)"),
        "bridge must dispatch to the class static: {owner}"
    );
    let _ = fs::remove_dir_all(root);
}
