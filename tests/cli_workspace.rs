use clap::Parser;
use notlin::cli::Cli;
use std::path::PathBuf;

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
