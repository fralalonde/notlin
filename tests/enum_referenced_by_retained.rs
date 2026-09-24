use std::fs;
use std::path::PathBuf;

/// A Kotlin enum referenced as a member type by a RETAINED Kotlin
/// declaration (interface property) participates in Kotlin's fake-override
/// resolution — translating it to a plain Java enum breaks kotlinc IR for
/// the retained partner. The enum must remain Kotlin whenever a retained
/// Kotlin file references it by name.
#[test]
fn enum_referenced_by_retained_kotlin_stays_kotlin() {
    let root = std::env::temp_dir().join(format!("notlin-enumref-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Defs.kt"),
        "package neutral.er\ninterface Holder {\n    val category: Kind\n}\n",
    )
    .unwrap();
    // Kind is translated here; Defs.kt is retained (annotated) and reads
    // Kind as a Kotlin type.
    fs::write(
        root.join("Kind.kt"),
        "package neutral.er\nenum class Kind { A, B }\n",
    )
    .unwrap();
    let cli = clap::Parser::parse_from(vec![
        "notlin",
        "--root",
        root.to_string_lossy().as_ref(),
        "Kind.kt",
    ]);
    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let (files, _errors, _warnings, _cov) = notlin::transpiler::transpile_with_workspace(
        "package neutral.er\nenum class Kind { A, B }\n",
        &PathBuf::from("Kind.kt"),
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert!(
        !files.iter().any(|(n, _)| n == "Kind.java"),
        "Kind translated while a retained Kotlin declaration references it as a member type"
    );
}
