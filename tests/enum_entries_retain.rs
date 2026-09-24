use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture_root(case: &str) -> PathBuf {
    std::env::temp_dir().join(format!("notlin-enum-entries-{case}-{}", std::process::id()))
}

fn run_in_place(root: &Path, enum_file: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join(enum_file))
        .output()
        .expect("run notlin")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "notlin failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_entries_bridge(java: &str, enum_name: &str) {
    assert!(
        java.contains(&format!(
            "public static kotlin.enums.EnumEntries<{enum_name}> getEntries()"
        )),
        "missing enum-specific Kotlin getEntries bridge:\n{java}"
    );
    assert!(
        java.contains("return kotlin.enums.EnumEntriesKt.enumEntries(values());"),
        "getEntries bridge must construct entries from values():\n{java}"
    );
}

#[test]
fn java_get_entries_consumer_gets_compatible_enum_bridge() {
    let root = fixture_root("java");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Flag.kt"),
        "package neutral.entries.consumer\nenum class Flag { A, B }\n",
    )
    .unwrap();
    fs::write(
        root.join("Use.java"),
        "package neutral.entries.consumer;\nclass Use {\n    kotlin.enums.EnumEntries<Flag> all() { return Flag.getEntries(); }\n}\n",
    )
    .unwrap();

    let output = run_in_place(&root, "Flag.kt");
    assert_success(&output);

    let java = fs::read_to_string(root.join("Flag.java")).unwrap();
    assert_entries_bridge(&java, "Flag");
    assert!(
        !root.join("Flag.kt").exists(),
        "enum retained solely because of entries ABI:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn residual_kotlin_entries_consumer_gets_compatible_enum_bridge() {
    let root = fixture_root("kotlin");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Mode.kt"),
        "package neutral.entries.kotlin\nenum class Mode { FIRST, SECOND }\n",
    )
    .unwrap();
    fs::write(
        root.join("Use.kt"),
        "package neutral.entries.kotlin\nfun allModes() = Mode.entries\n",
    )
    .unwrap();

    let output = run_in_place(&root, "Mode.kt");
    assert_success(&output);

    let java = fs::read_to_string(root.join("Mode.java")).unwrap();
    assert_entries_bridge(&java, "Mode");
    assert!(
        !root.join("Mode.kt").exists(),
        "enum retained solely because residual Kotlin reads entries:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn enum_without_indexed_entries_consumer_has_no_kotlin_runtime_bridge() {
    let root = fixture_root("unused");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("State.kt"),
        "package neutral.entries.unused\nenum class State { ON, OFF }\n",
    )
    .unwrap();

    let output = run_in_place(&root, "State.kt");
    assert_success(&output);

    let java = fs::read_to_string(root.join("State.java")).unwrap();
    assert!(!java.contains("getEntries()"), "unexpected bridge:\n{java}");
    assert!(
        !java.contains("kotlin.enums.EnumEntries"),
        "enum without indexed consumer gained Kotlin runtime dependency:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
