#[test]
fn canonical_workspace_root_is_used_even_when_cwd_is_elsewhere() {
    let root = std::env::temp_dir().join(format!("notlin-display-root-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let canonical = std::fs::canonicalize(&root).unwrap();
    notlin::paths::set_root(&canonical);
    assert_eq!(
        notlin::paths::display(canonical.join("nested").join("Example.kt")),
        "nested/Example.kt"
    );
    std::fs::remove_dir(root).unwrap();
}
