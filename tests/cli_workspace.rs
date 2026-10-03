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
fn directory_input_migrates_in_place_without_extra_flags() {
    let root =
        std::env::temp_dir().join(format!("notlin-directory-migrate-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let target = root.join("target");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("Sample.kt"), "package sample\nclass Sample\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root)
        .arg("--root")
        .arg(&root)
        .arg(&target)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(target.join("Sample.java").is_file());
    assert!(!target.join("Sample.kt").exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn default_run_reports_phases_without_listing_individual_files() {
    let root = std::env::temp_dir().join(format!("notlin-progress-cli-{}", std::process::id()));
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
    assert!(stderr.contains("◇ index"), "{stderr}");
    assert!(stderr.contains("✓ index"), "{stderr}");
    assert!(stderr.contains("◇ plan"), "{stderr}");
    assert!(stderr.contains("notlin summary"), "{stderr}");
    assert!(stderr.contains("├─ sources"), "{stderr}");
    assert!(stderr.contains("├─ java"), "{stderr}");
    assert!(stderr.contains("├─ diagnostics"), "{stderr}");
    assert!(stderr.contains("└─ result"), "{stderr}");
    assert!(stderr.contains("success"), "{stderr}");
    assert!(!stderr.contains("Sample.kt"), "{stderr}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn verbose_run_lists_individual_source_activity() {
    let root = std::env::temp_dir().join(format!("notlin-verbose-cli-{}", std::process::id()));
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
        .arg("-v")
        .arg(&source)
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("planned Sample.kt:"), "{stderr}");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn profile_mode_keeps_individual_paths_hidden_without_verbose() {
    let root = std::env::temp_dir().join(format!("notlin-profile-cli-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let source = root.join("Hidden.kt");
    let output_dir = root.join("java");
    fs::write(&source, "package sample\nclass Hidden\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root)
        .arg("--root")
        .arg(&root)
        .arg("--out-dir")
        .arg(&output_dir)
        .arg(&source)
        .env("NOTLIN_PROFILE", "1")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("NOTLIN_PROFILE speculative round"),
        "{stderr}"
    );
    assert!(!stderr.contains("Hidden.kt"), "{stderr}");
    assert!(!stderr.contains("Hidden.java"), "{stderr}");
    assert!(!stderr.contains("samples:"), "{stderr}");

    fs::remove_dir_all(root).unwrap();
}
