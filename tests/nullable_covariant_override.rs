use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::fs;

#[test]
fn retains_nullable_primitive_override_that_would_change_the_jvm_return_type() {
    let root = std::env::temp_dir().join(format!(
        "notlin-nullable-primitive-contract-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("Definitions.kt");
    fs::write(
        &path,
        "package sample\ninterface Contract {\n    val count: Int?\n}\ndata class Implementation(override val count: Int) : Contract\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", path.to_str().unwrap()]);
    let source = fs::read_to_string(&path).unwrap();
    let (files, errors, _warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    assert!(
        !files.iter().any(|(name, _)| name == "Implementation.java"),
        "files: {:?}; untranslated: {:?}; declarations: {:?}",
        files.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        coverage.untranslated,
        index.declarations().collect::<Vec<_>>()
    );
    assert!(
        coverage
            .untranslated
            .iter()
            .any(|name| name == "Implementation"),
        "Int? lowers to Integer while Int lowers to int, so Java cannot override the getter"
    );
    fs::remove_dir_all(root).unwrap();
}
