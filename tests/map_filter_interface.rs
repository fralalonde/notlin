use clap::Parser;
use std::fs;

#[test]
fn map_filter_on_overridden_interface_property_uses_entry_set_stream() {
    let source = "interface HasProperties { val properties: Map<String, String> }\ndata class Properties(override val properties: Map<String, String>) : HasProperties {\n    override fun equals(other: Any?): Boolean {\n        if (other !is Properties) return false\n        return this.properties.filter { e -> e.value.isNotEmpty() } == other.properties.filter { e -> e.value.isNotEmpty() }\n    }\n}\n";
    let root = std::env::temp_dir().join(format!(
        "notlin-map-filter-interface-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("Properties.kt");
    fs::write(
        root.join("AOther.kt"),
        "interface AOther { val properties: List<String> }\n",
    )
    .unwrap();
    fs::write(&path, source).unwrap();
    let cli =
        notlin::cli::Cli::parse_from(["notlin", "--root", root.to_str().unwrap(), "Properties.kt"]);
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
    assert!(
        output.contains("getProperties().entrySet().stream()"),
        "{output}"
    );
    assert!(
        output.contains("((Properties) other).getProperties().entrySet().stream()"),
        "{output}"
    );
    assert!(!output.contains("getProperties().stream()"), "{output}");
    let _ = fs::remove_dir_all(root);
}
