use std::fs;
use std::path::PathBuf;

/// When pre-existing Java calls a Kotlin enum's `getEntries()` (the Kotlin
/// entries ABI), translating the enum to Java breaks that caller: Java
/// enums only expose `values()`. The enum declaration must remain Kotlin
/// until the ABI consumer is also migrated.
#[test]
fn kotlin_abi_get_entries_consumer_keeps_enum_kotlin() {
    let root = std::env::temp_dir().join(format!("notlin-enumcc-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Flag.kt"),
        "package neutral.ee\nenum class Flag { A, B }\n",
    )
    .unwrap();
    fs::write(
        root.join("Use.java"),
        "package neutral.ee\nimport java.util.*;\nclass Use {\n    List<Flag> all() { return Flag.getEntries(); }\n}\n",
    )
    .unwrap();
    let cli = clap::Parser::parse_from(vec![
        "notlin",
        "--root",
        root.to_string_lossy().as_ref(),
        "Flag.kt",
    ]);
    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let (files, _errors, _warnings, _cov) = notlin::transpiler::transpile_with_workspace(
        "package neutral.ee\nenum class Flag { A, B }\n",
        &PathBuf::from("Flag.kt"),
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    // Flag must not be translated into a plain Java enum that loses the
    // getEntries ABI its pre-existing Java consumer relies on: either an
    // error/taint surfaces or no Flag.java is emitted.
    let flag = files.iter().find(|(n, _)| n == "Flag.java");
    if let Some((_, c)) = flag {
        assert!(
            c.contains("getEntries"),
            "Flag.java emitted as plain Java enum while a Java caller still uses the Kotlin getEntries ABI: {c}"
        );
    }
}
