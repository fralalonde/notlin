use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::fs;
use std::path::PathBuf;

/// A companion call whose argument is a `K::class` literal reaches a RETained
/// Kotlin companion fn expecting a KClass: Java cannot form a KClass literal
/// directly. Emitting `K.class` breaks javac (KClass expected). The calling
/// declaration must taint instead of emitting broken Java.
#[test]
fn kclass_companion_args_taint_caller() {
    let root = std::env::temp_dir().join(format!("notlin-kclass-arg-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Owner.kt"),
        "package neutral.kind\nclass Owner {\n    companion object {\n        fun of(type: KClass<out Enum<*>>): String = \"x\"\n    }\n}\nenum class Item { A }\n",
    )
    .unwrap();
    let caller = "package neutral.kind\nenum class Kind(val id: String) {\n    A(Owner.of(Item::class)),\n    B(\"b\")\n}\n";
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
    let kind = files.iter().find(|(n, _)| n == "Kind.java");
    if let Some((_, c)) = kind {
        assert!(
            !c.contains("Owner.Companion.of(Item.class)"),
            "KClass-bound companion call emitted as un-swappable .class: {c}"
        );
    }
    let _ = fs::remove_dir_all(root);
}
