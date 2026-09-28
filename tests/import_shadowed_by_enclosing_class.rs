//! A Kotlin simple name that resolves to an import must stay resolvable once
//! the file is Java.
//!
//! Kotlin resolves a classifier to its explicit import, even when the
//! enclosing declaration has the same simple name. Java gives the enclosing
//! class's own name priority inside its own body, so a migrated
//! `class Length(val unit: Unit<Length>)` (Kotlin: the imported
//! `…quantity.Length`) stops satisfying a bound like
//! `Unit<Q : Quantity<Q>>` — javac reads the enclosing `Length` instead and
//! reports "type argument Length is not within bounds of type-variable Q".
//! The emitter therefore qualifies the colliding name inside type-argument
//! lists.

use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::fs;

fn root_for(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("notlin-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    root
}

#[test]
fn imported_name_shadowed_by_the_enclosing_class_is_qualified_in_type_arguments() {
    let root = root_for("shadowed-type-argument");
    fs::create_dir_all(root.join("neutral/measure")).unwrap();
    fs::create_dir_all(root.join("neutral/quantity")).unwrap();
    fs::create_dir_all(root.join("neutral/model")).unwrap();
    // The bound makes the type argument significant: `Q : Quantity<Q>`.
    fs::write(
        root.join("neutral/measure/Quantity.kt"),
        "package neutral.measure\n\ninterface Quantity<Q : Quantity<Q>>\n\ninterface Unit<Q : Quantity<Q>>\n",
    )
    .unwrap();
    fs::write(
        root.join("neutral/quantity/Length.kt"),
        "package neutral.quantity\n\nimport neutral.measure.Quantity\n\ninterface Length : Quantity<Length>\n",
    )
    .unwrap();
    // The enclosing class is `neutral.model.Length` while the referenced
    // `Length` is the imported one — the shape Kotlin resolves to the import
    // and Java resolves to itself.
    let provider = root.join("neutral/model/Length.kt");
    fs::write(
        &provider,
        "package neutral.model\n\nimport neutral.measure.Quantity\nimport neutral.measure.Unit\nimport neutral.quantity.Length\n\nclass Length(val unit: Unit<Length>) : Quantity<Length> {\n    val quantity: Quantity<Length> get() = unit\n}\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", provider.to_str().unwrap()]);
    let source = fs::read_to_string(&provider).unwrap();
    let (files, errors, warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &provider,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    let java = files
        .iter()
        .find(|(name, _)| name == "Length.java")
        .map(|(_, text)| text.clone())
        .unwrap_or_else(|| {
            panic!(
                "Length must translate; errors={errors:?} warnings={warnings:?} untranslated={:?}",
                coverage.untranslated
            )
        });
    assert!(
        java.contains("Unit<neutral.quantity.Length>"),
        "a type argument named after the enclosing class must be qualified: javac \
         resolves the bare name to the enclosing class and rejects the bound\n{java}"
    );
    assert!(
        !java.contains("Unit<Length>"),
        "the bare shadowed type argument must not survive:\n{java}"
    );
    assert!(
        java.contains("Quantity<neutral.quantity.Length>"),
        "every type-argument occurrence of the shadowed name must be qualified, \
         not just the first\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn an_unshadowed_type_argument_is_left_alone() {
    // Control: with no colliding import, `Unit<Length>` is the enclosing class
    // and must keep its bare spelling (qualifying it would be wrong).
    let root = root_for("unshadowed-type-argument");
    fs::create_dir_all(root.join("neutral/measure")).unwrap();
    fs::create_dir_all(root.join("neutral/model")).unwrap();
    fs::write(
        root.join("neutral/measure/Quantity.kt"),
        "package neutral.measure\n\ninterface Quantity<Q : Quantity<Q>>\n\ninterface Unit<Q : Quantity<Q>>\n",
    )
    .unwrap();
    let provider = root.join("neutral/model/Length.kt");
    fs::write(
        &provider,
        "package neutral.model\n\nimport neutral.measure.Quantity\nimport neutral.measure.Unit\n\nclass Length(val unit: Unit<Length>) : Quantity<Length> {\n    val quantity: Quantity<Length> get() = unit\n}\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", provider.to_str().unwrap()]);
    let source = fs::read_to_string(&provider).unwrap();
    let (files, errors, _, _) = transpiler::transpile_with_workspace(
        &source,
        &provider,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    let java = files
        .iter()
        .find(|(name, _)| name == "Length.java")
        .map(|(_, text)| text.clone())
        .unwrap_or_else(|| panic!("Length must translate; errors={errors:?}"));
    assert!(
        java.contains("Unit<Length>"),
        "without a colliding import the enclosing class is the intended type \
         and must stay bare:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
