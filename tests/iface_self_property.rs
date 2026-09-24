use clap::Parser;
use notlin::cli::Cli;
use notlin::workspace::SourceIndex;
use std::fs;

/// Inside an interface's default getter, a bare identifier that names a
/// Kotlin property declared on the interface ITSELF (or a super-interface)
/// must lower to the Java accessor call (`activity` -> `getActivity()`),
/// not an illegal bare field read with no Java counterpart.
#[test]
fn interface_default_getter_property_reads_become_accessors() {
    let root = std::env::temp_dir().join(format!("notlin-iface-prop-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("Events.kt");
    let source = "package neutral.events\n\
                  interface Holder {\n\
                  \x20   val activity: String\n\
                  }\n\
                  interface Derived : Holder {\n\
                  \x20   val id: String\n\
                  \x20       get() = activity\n\
                  }\n";
    fs::write(&path, source).unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", path.to_str().unwrap()]);
    let (files, errors, warnings, _cov) = notlin::transpiler::transpile_with_workspace(
        source,
        &path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0, "{:?}", warnings);
    let derived = files
        .iter()
        .find(|(name, _)| name == "Derived.java")
        .map(|(_, c)| c.as_str())
        .expect("Derived.java");
    assert!(
        derived.contains("return this.getActivity();"),
        "bare `activity` must lower to the accessor: {derived}"
    );
    assert!(
        !derived.contains("return activity;"),
        "bare field read must not survive: {derived}"
    );
    fs::remove_dir_all(root).unwrap();
}
