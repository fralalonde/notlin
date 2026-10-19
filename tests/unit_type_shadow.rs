use clap::Parser;
use notlin::{cli::Cli, transpiler, workspace::SourceIndex};
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_WORKSPACE: AtomicUsize = AtomicUsize::new(0);

fn assert_unit_shadow_retains(unit_decl: &str, use_source: &str) {
    let id = NEXT_WORKSPACE.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "notlin-unit-type-shadow-{}-{id}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Unit.kt"), unit_decl).unwrap();
    let target = root.join("Request.kt");
    fs::write(&target, use_source).unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", target.to_str().unwrap()]);
    let (_, errors, _, coverage) = transpiler::transpile_with_workspace(
        use_source,
        &target,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    assert!(
        coverage.untranslated.iter().any(|name| name == "Request"),
        "ambiguous bare Unit should retain Request; coverage: {coverage:?}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn same_package_class_unit_retains_nested_generic_type_use() {
    assert_unit_shadow_retains(
        "package shadow\nclass Unit\n",
        "package shadow\ndata class Request(val value: List<Unit>)\n",
    );
}

#[test]
fn same_package_typealias_unit_retains_nested_generic_type_use() {
    assert_unit_shadow_retains(
        "package shadow\ntypealias Unit = String\n",
        "package shadow\ndata class Request(val value: List<Unit>)\n",
    );
}

#[test]
fn kotlin_prefix_import_alias_retains_qualified_unit_type_use() {
    assert_unit_shadow_retains(
        "package shadow\nclass Facade { class Unit }\n",
        "package shadow\nimport shadow.Facade as kotlin\ndata class Request(val value: kotlin.Unit)\n",
    );
}
