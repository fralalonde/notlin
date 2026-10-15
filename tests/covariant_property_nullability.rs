use notlin::workspace::SourceIndex;
use std::collections::HashSet;
use std::fs;

fn selected_collection_override_is_safe(implementation_type: &str) -> bool {
    let root = std::env::temp_dir().join(format!(
        "notlin-covariant-nullability-{}-{}",
        std::process::id(),
        implementation_type
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Contract.kt"),
        "package sample\nopen class Base\nclass Child : Base()\ninterface Contract { val items: List<Base> }\n",
    )
    .unwrap();
    fs::write(
        root.join("Implementation.kt"),
        format!(
            "package sample\nclass Implementation(override val items: {implementation_type}) : Contract\n"
        ),
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let implementation_file = root.join("Implementation.kt");
    let implementation = index
        .declarations()
        .find(|declaration| declaration.name == "Implementation")
        .unwrap();
    let conflict = index
        .retained_supertype_member_conflicts_in_file(
            &implementation_file,
            &implementation.supertypes,
            "Implementation",
        )
        .into_iter()
        .find(|conflict| conflict.member_name == "items")
        .unwrap();
    let safe = index.selected_covariant_property_conflict_is_safe(
        &implementation_file,
        &conflict,
        std::slice::from_ref(&root),
        &HashSet::new(),
    );
    fs::remove_dir_all(root).unwrap();
    safe
}

#[test]
fn collection_covariance_rejects_nullable_implementation_of_nonnull_contract() {
    assert!(!selected_collection_override_is_safe("List<Child>?"));
}

#[test]
fn collection_covariance_keeps_nonnull_element_narrowing() {
    assert!(selected_collection_override_is_safe("List<Child>"));
}

#[test]
fn custom_invariant_list_name_does_not_receive_builtin_covariance() {
    let root = std::env::temp_dir().join(format!(
        "notlin-custom-list-covariance-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Model.kt"),
        "package sample\nopen class Base\nclass Child : Base()\nclass List<T>\ninterface Contract { val items: List<Base> }\nclass Implementation(override val items: List<Child>) : Contract\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let implementation_file = root.join("Model.kt");
    let implementation = index
        .declarations()
        .find(|declaration| declaration.name == "Implementation")
        .unwrap();
    let conflict = index
        .retained_supertype_member_conflicts_in_file(
            &implementation_file,
            &implementation.supertypes,
            "Implementation",
        )
        .into_iter()
        .find(|conflict| conflict.member_name == "items")
        .unwrap();
    assert!(!index.selected_covariant_property_conflict_is_safe(
        &implementation_file,
        &conflict,
        std::slice::from_ref(&root),
        &HashSet::new(),
    ));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicit_unindexed_collection_shadow_fails_closed() {
    let root = std::env::temp_dir().join(format!(
        "notlin-unindexed-list-covariance-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Contract.kt"),
        "package sample\nimport external.List\nopen class Base\ninterface Contract { val items: List<Base> }\n",
    )
    .unwrap();
    fs::write(
        root.join("Implementation.kt"),
        "package sample\nimport external.List\nclass Child : Base()\nclass Implementation(override val items: List<Child>) : Contract\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let implementation_file = root.join("Implementation.kt");
    let implementation = index
        .declarations()
        .find(|declaration| declaration.name == "Implementation")
        .unwrap();
    let conflict = index
        .retained_supertype_member_conflicts_in_file(
            &implementation_file,
            &implementation.supertypes,
            "Implementation",
        )
        .into_iter()
        .find(|conflict| conflict.member_name == "items")
        .unwrap();
    assert!(!index.selected_covariant_property_conflict_is_safe(
        &implementation_file,
        &conflict,
        std::slice::from_ref(&root),
        &HashSet::new(),
    ));

    fs::remove_dir_all(root).unwrap();
}
