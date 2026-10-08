use clap::Parser;
use std::fs;

#[test]
fn map_filter_uses_entry_set_stream() {
    let source = "class Properties(val properties: Map<String, String>) {\n    fun filled(): List<Map.Entry<String, String>> = properties.filter { e -> e.value.isNotEmpty() }\n}\n";
    let root = std::env::temp_dir().join(format!("notlin-map-filter-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("Properties.kt");
    fs::write(&path, source).unwrap();
    let cli = notlin::cli::Cli::parse_from([
        "notlin",
        "--allow-approximations",
        "--root",
        root.to_str().unwrap(),
        "Properties.kt",
    ]);
    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let (files, errors, _warnings, _coverage) = notlin::transpiler::transpile_with_workspace(
        source,
        &path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    let output = files
        .into_iter()
        .map(|(_, content)| content)
        .collect::<String>();
    assert!(output.contains(".entrySet().stream()"), "{output}");
    let _ = fs::remove_dir_all(root);
}
