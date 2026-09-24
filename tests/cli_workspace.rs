use clap::Parser;
use notlin::cli::Cli;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn root_is_separate_from_translation_roots() {
    let cli = Cli::parse_from([
        "notlin",
        "--root",
        "workspace",
        "module/src/main/java",
        "other/src/main/java",
    ]);

    assert_eq!(cli.workspace_root, Some(PathBuf::from("workspace")));
    assert_eq!(
        cli.input,
        vec![
            PathBuf::from("module/src/main/java"),
            PathBuf::from("other/src/main/java")
        ]
    );
}

#[test]
fn workspace_root_alias_is_accepted() {
    let cli = Cli::parse_from(["notlin", "--workspace-root", "workspace", "selected"]);
    assert_eq!(cli.workspace_root, Some(PathBuf::from("workspace")));
    assert_eq!(cli.input, vec![PathBuf::from("selected")]);
}

#[test]
fn explicit_target_directory_is_transpiled() {
    let root = std::env::temp_dir().join(format!("notlin-explicit-target-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let target = root.join("target");
    let output_dir = root.join("java");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("Sample.kt"), "package sample\nclass Sample\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root)
        .arg("--root")
        .arg(&root)
        .arg("--out-dir")
        .arg(&output_dir)
        .arg(&target)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output_dir.join("Sample.java").is_file());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn default_run_emits_only_the_final_summary() {
    let root = std::env::temp_dir().join(format!("notlin-quiet-cli-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let source = root.join("Sample.kt");
    let output_dir = root.join("java");
    fs::write(&source, "package sample\nclass Sample\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root)
        .arg("--root")
        .arg(&root)
        .arg("--out-dir")
        .arg(&output_dir)
        .arg(&source)
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("notlin: 1 file(s) processed"),
        "{stderr}"
    );
    assert!(!stderr.contains("indexing"), "{stderr}");
    assert!(!stderr.contains("transpiling"), "{stderr}");

    fs::remove_dir_all(root).unwrap();
}
