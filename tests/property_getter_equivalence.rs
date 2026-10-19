use notlin::semantics::{SemanticProvider, SymbolId, SyntaxSemanticProvider};
use std::path::PathBuf;

fn provider(source: &str) -> SyntaxSemanticProvider {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture: {source}");
    SyntaxSemanticProvider::new(vec![(PathBuf::from("Box.kt"), source.to_owned())])
}

fn property(provider: &SyntaxSemanticProvider, name: &str) -> SymbolId {
    provider
        .symbols_named(name)
        .into_iter()
        .find(|symbol| symbol.id.kind == "property")
        .expect("indexed property")
        .id
        .clone()
}

#[test]
fn field_and_renamed_getter_properties_map_to_explicit_equivalent_methods() {
    let source = r#"package projection
import kotlin.jvm.JvmName

class Box {
    @JvmField val count: Int = 2
    fun getCount(): Int = count

    @get:JvmName("renamedLabel")
    val label: String
        get() = "label"
    fun getLabel(): String { return this.label }
}
"#;
    let provider = provider(source);
    let count = provider
        .equivalent_property_getter(&property(&provider, "count"))
        .expect("field property should have an explicit equivalent method");
    let label = provider
        .equivalent_property_getter(&property(&provider, "label"))
        .expect("renamed getter property should have an explicit equivalent method");
    assert_eq!(count.name, "getCount");
    assert_eq!(label.name, "getLabel");
    assert_eq!(count.owner_path, vec!["Box"]);
    assert_eq!(label.owner_path, vec!["Box"]);
}

#[test]
fn open_or_annotated_owners_and_nontrivial_method_bodies_are_rejected() {
    for source in [
        r#"package projection
open class Parent
class Box : Parent() {
    @JvmField val label: String = "label"
    fun getLabel(): String = label
}
"#,
        r#"package projection
open class Box {
    @JvmField val label: String = "label"
    fun getLabel(): String = label
}
"#,
        r#"package projection
@Deprecated("plugin-open")
class Box {
    @JvmField val label: String = "label"
    fun getLabel(): String = label
}
"#,
        r#"package projection
class Box {
    @JvmField val label: String = "label"
    fun getLabel(): String { println("side effect"); return label }
}
"#,
    ] {
        let provider = provider(source);
        assert!(
            provider
                .equivalent_property_getter(&property(&provider, "label"))
                .is_none(),
            "unsupported shape must remain unknown:\n{source}"
        );
    }
}

#[test]
fn mismatched_or_inferred_method_signatures_are_rejected() {
    for source in [
        r#"package projection
class Box {
    @JvmField val label: String = "label"
    fun getLabel(): Int = 1
}
"#,
        r#"package projection
class Box {
    @JvmField val label: String = "label"
    fun getLabel() = label
}
"#,
        r#"package projection
class Box {
    @JvmField val label: String = "label"
    fun getLabel(prefix: String): String = prefix + label
}
"#,
    ] {
        let provider = provider(source);
        assert!(
            provider
                .equivalent_property_getter(&property(&provider, "label"))
                .is_none(),
            "signature must be explicit, no-argument, and exactly match:\n{source}"
        );
    }
}

#[test]
fn aliased_annotation_names_and_ambiguous_methods_are_rejected() {
    let aliased = r#"package projection
import elsewhere.JvmField

class Box {
    @JvmField val label: String = "label"
    fun getLabel(): String = label
}
"#;
    let indexed = provider(aliased);
    assert!(
        indexed
            .equivalent_property_getter(&property(&indexed, "label"))
            .is_none(),
        "an imported same-spelled annotation is not proven to be kotlin.jvm.JvmField"
    );

    let ambiguous = r#"package projection
class Box {
    @JvmField val label: String = "label"
    fun getLabel(): String = label
    fun getLabel(other: String = ""): String = other
}
"#;
    let indexed = provider(ambiguous);
    assert!(
        indexed
            .equivalent_property_getter(&property(&indexed, "label"))
            .is_none(),
        "multiple same-named source methods cannot establish a unique equivalent getter"
    );
}

#[test]
fn same_package_annotation_declarations_prevent_canonical_binding_assumptions() {
    let box_source = r#"package projection
class Box {
    @JvmField val label: String = "label"
    fun getLabel(): String = label
}
"#;
    let local_shadow = SyntaxSemanticProvider::new(vec![(
        PathBuf::from("Box.kt"),
        box_source.replacen(
            "package projection",
            "package projection\nannotation class JvmField",
            1,
        ),
    )]);
    assert!(
        local_shadow
            .equivalent_property_getter(&property(&local_shadow, "label"))
            .is_none(),
        "a same-file declaration named JvmField may shadow the standard annotation"
    );

    let cross_file_shadow = SyntaxSemanticProvider::new(vec![
        (PathBuf::from("Box.kt"), box_source.to_owned()),
        (
            PathBuf::from("JvmField.kt"),
            "package projection\nannotation class JvmField\n".to_owned(),
        ),
    ]);
    assert!(
        cross_file_shadow
            .equivalent_property_getter(&property(&cross_file_shadow, "label"))
            .is_none(),
        "a same-package declaration in another file may shadow the standard annotation"
    );
}
