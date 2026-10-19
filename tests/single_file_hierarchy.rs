//! Single-file CLI runs must make the same hierarchy decisions as the
//! workspace fixpoint. A Java child emitted while its Kotlin supertype stays
//! Kotlin cannot compile; the fixpoint's paired subtype/supertype rules are
//! the only place that consistency is proven, so every file-backed invocation
//! must go through it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SHAPE: &str = "package p\n\ninterface Shape {\n    val name: String\n    fun area(): Double\n}\n\nclass Circle(val r: Double) : Shape {\n    override val name: String\n        get() = \"circle\"\n    override fun area(): Double = 3.14159 * r * r\n}\n";

fn workdir(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("notlin-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn java_names(dir: &Path) -> Vec<String> {
    let mut names = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| {
                    let path = entry.ok()?.path();
                    (path.extension()? == "java")
                        .then(|| path.file_name().unwrap().to_string_lossy().into_owned())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn run(root: &Path, args: &[&str]) -> Vec<String> {
    let out = root.join("out");
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(&out).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(root)
        .args(["-o", "out"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "notlin failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    java_names(&out)
}

#[test]
fn single_file_run_emits_a_java_child_with_its_java_supertype() {
    let root = workdir("single-file-hierarchy");
    fs::write(root.join("Shape.kt"), SHAPE).unwrap();
    // A translated Circle implements Shape: the interface cannot stay Kotlin
    // while its Java implementation is emitted. The fixpoint proves this pair
    // together; the legacy hintless path split it (Circle.java alone).
    assert_eq!(
        run(&root, &["Shape.kt"]),
        ["Circle.java".to_string(), "Shape.java".to_string()]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn single_file_and_workspace_inputs_agree_on_hierarchy_decisions() {
    let root = workdir("single-file-agreement");
    fs::write(root.join("Shape.kt"), SHAPE).unwrap();
    let file = run(&root, &["Shape.kt"]);
    let tree = run(&root, &["."]);
    assert_eq!(file, tree, "engine choice must not change ownership");
    fs::remove_dir_all(root).unwrap();
}
