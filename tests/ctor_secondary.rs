//! Secondary constructors vs primary-constructor default arguments.
//!
//! A call shaped like neither the primary constructor nor a default-argument
//! omission is not evidence about the primary constructor's defaults: it is a
//! secondary constructor's call, and the emitted Java keeps those. Reading such
//! a call as "an argument shape could not be read" retained declarations whose
//! callers were never omitting anything (`ItemQuantity(quantity, unit)` against
//! a four-parameter primary with a two-parameter secondary, and
//! `OrderActivityLineEntry(orderLine)` against a five-parameter primary with a
//! one-parameter secondary).

use notlin::workspace::{MemberKind, SourceIndex};
use std::fs;
use std::path::Path;
use std::process::Command;

/// A class whose primary constructor takes four parameters (one defaulted) and
/// which declares two-parameter secondary constructors, plus the calls a real
/// workspace writes against it.
const SHAPES: &str = r#"package p

import java.math.BigDecimal

class ItemQuantity(
        val quantity: BigDecimal,
        val unit: String,
        val weight: String,
        val scale: Int = 0
) {
    constructor(quantity: ItemQuantity, unit: String) : this(quantity.quantity, unit, quantity.weight, quantity.scale)
    constructor(quantity: BigDecimal, unit: String) : this(quantity, unit, "", 0)
}
"#;

fn write_fixture(root: &Path, body: &str) {
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("shapes.kt"), format!("{SHAPES}\n{body}")).unwrap();
}

#[test]
fn kotlin_secondary_constructors_are_indexed_with_their_parameters() {
    let root = Path::new("tests/tmp_scratch_secondary/index");
    write_fixture(root, "");

    let index = SourceIndex::discover(root).unwrap();
    let declaration = index
        .declarations()
        .find(|declaration| declaration.name == "ItemQuantity")
        .expect("ItemQuantity in the index");

    // Indexed as members: "does this type declare another constructor" is a
    // question the rest of the translator asks, and a missing name node used to
    // drop every Kotlin secondary constructor from the member list.
    let constructors = declaration
        .members
        .iter()
        .filter(|member| member.kind == MemberKind::Constructor)
        .count();
    assert_eq!(constructors, 2, "{:?}", declaration.members);

    // And indexed with their parameter types, in declaration order.
    assert_eq!(
        declaration.secondary_ctors,
        vec![
            vec!["ItemQuantity".to_string(), "String".to_string()],
            vec!["BigDecimal".to_string(), "String".to_string()],
        ]
    );

    let _ = fs::remove_dir_all(root);
}

/// The reported false positive: two arguments against a four-parameter primary
/// constructor is a secondary-constructor call, so it certifies nothing about
/// the primary's defaults.
#[test]
fn a_call_the_secondary_constructor_explains_is_not_omission_evidence() {
    let root = Path::new("tests/tmp_scratch_secondary/matched");
    write_fixture(
        root,
        "\nfun use(q: ItemQuantity, u: String): ItemQuantity = ItemQuantity(q, u)\n",
    );

    let index = SourceIndex::discover(root).unwrap();
    let declaration = index
        .declarations()
        .find(|declaration| declaration.name == "ItemQuantity")
        .expect("ItemQuantity in the index");

    let evidence = index.ctor_omission_evidence(declaration);
    assert!(
        evidence.unresolvable.is_empty(),
        "a secondary constructor of the same arity explains the call: {evidence:?}"
    );
    assert!(
        evidence.patterns.is_empty(),
        "and it is no omission pattern either: {evidence:?}"
    );

    let _ = fs::remove_dir_all(root);
}

/// The other half: an arity no secondary constructor has is still unreadable,
/// and must stay evidence — that call site can genuinely no longer compile.
#[test]
fn a_call_no_secondary_constructor_explains_stays_unreadable() {
    let root = Path::new("tests/tmp_scratch_secondary/unmatched");
    write_fixture(
        root,
        "\nfun use(q: ItemQuantity): ItemQuantity = ItemQuantity(q)\n",
    );

    let index = SourceIndex::discover(root).unwrap();
    let declaration = index
        .declarations()
        .find(|declaration| declaration.name == "ItemQuantity")
        .expect("ItemQuantity in the index");

    let evidence = index.ctor_omission_evidence(declaration);
    assert_eq!(
        evidence.unresolvable.len(),
        1,
        "no one-parameter constructor exists: {evidence:?}"
    );
    assert!(
        evidence.unresolvable[0].ends_with("shapes.kt:16"),
        "{evidence:?}"
    );

    let _ = fs::remove_dir_all(root);
}

/// A real omission — a named call that leaves out only the defaulted parameter
/// — is still evidence, secondary constructors or not.
#[test]
fn a_real_omission_is_still_recorded_beside_a_secondary_constructor() {
    let root = Path::new("tests/tmp_scratch_secondary/omission");
    write_fixture(
        root,
        "\nfun use(q: ItemQuantity): ItemQuantity =\n    ItemQuantity(quantity = q.quantity, unit = q.unit, weight = q.weight)\n",
    );

    let index = SourceIndex::discover(root).unwrap();
    let declaration = index
        .declarations()
        .find(|declaration| declaration.name == "ItemQuantity")
        .expect("ItemQuantity in the index");

    let evidence = index.ctor_omission_evidence(declaration);
    assert_eq!(evidence.patterns, vec![vec![3usize]], "{evidence:?}");
    assert!(evidence.named_callers, "{evidence:?}");
    assert!(evidence.unresolvable.is_empty(), "{evidence:?}");

    let _ = fs::remove_dir_all(root);
}

/// End to end: with only secondary-constructor calls in the workspace, the
/// declaration translates.
#[test]
fn secondary_constructor_callers_do_not_retain_the_declaration() {
    let root = Path::new("tests/tmp_scratch_secondary/e2e");
    write_fixture(
        root,
        "\nfun use(q: ItemQuantity, b: java.math.BigDecimal, u: String): List<ItemQuantity> =\n    listOf(ItemQuantity(q, u), ItemQuantity(b, u))\n",
    );

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("shapes.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "notlin failed:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );

    let java = fs::read_to_string(root.join("ItemQuantity.java")).unwrap_or_default();
    assert!(
        !java.is_empty(),
        "the declaration translates:\n{stderr}\n{}",
        fs::read_to_string(root.join("shapes.kt")).unwrap_or_default()
    );
    assert!(
        !stderr.contains("argument shape could not be read"),
        "no unreadable-caller evidence:\n{stderr}"
    );

    let _ = fs::remove_dir_all(root);
}
