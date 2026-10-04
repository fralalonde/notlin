use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// A retained Kotlin declaration can safely reference a translated Java enum.
/// Java enums remain Kotlin-visible types, and the emitter adds `getEntries()`
/// when a residual Kotlin caller reads `Enum.entries`.
#[test]
fn enum_referenced_by_retained_kotlin_translates() {
    let root = std::env::temp_dir().join(format!("notlin-enumref-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Defs.kt"),
        "package neutral.er\ninterface Holder {\n    val category: Kind\n}\n",
    )
    .unwrap();
    // Kind is translated here; Defs.kt is retained (annotated) and reads
    // Kind as a Kotlin type.
    fs::write(
        root.join("Kind.kt"),
        "package neutral.er\nenum class Kind { A, B }\n",
    )
    .unwrap();
    let cli = clap::Parser::parse_from(vec![
        "notlin",
        "--root",
        root.to_string_lossy().as_ref(),
        "Kind.kt",
    ]);
    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let (files, _errors, _warnings, _cov) = notlin::transpiler::transpile_with_workspace(
        "package neutral.er\nenum class Kind { A, B }\n",
        &PathBuf::from("Kind.kt"),
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert!(
        files.iter().any(|(n, _)| n == "Kind.java"),
        "a retained Kotlin declaration can reference the translated Java enum"
    );
}

#[test]
fn enum_implementing_retained_kotlin_interface_translates() {
    let root = std::env::temp_dir().join(format!(
        "notlin-enum-retained-interface-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("State.kt"),
        "package neutral.er\ninterface State {\n    val code: String\n    fun unsupported() = mapOf(\"a\" to 1).plus(emptyMap())\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("Mode.kt"),
        "package neutral.er\nenum class Mode(override val code: String) : State { A(\"a\") }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root)
        .env_remove("NOTLIN_RETENTION_HIERARCHY")
        .args(["--in-place", "."])
        .output()
        .expect("run notlin");
    assert!(
        output.status.success(),
        "notlin failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let state_residue = fs::read_to_string(root.join("State.kt")).unwrap();
    assert!(
        !root.join("State.java").exists() && state_residue.contains("interface State"),
        "unsupported-body owner must remain Kotlin:\n{state_residue}"
    );
    assert!(
        root.join("Mode.java").exists(),
        "a Java enum can implement a retained Kotlin interface:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_dir_all(root);
}
