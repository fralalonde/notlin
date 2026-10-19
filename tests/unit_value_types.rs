use notlin::transpiler::{kt, types::AnnotationSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use tree_sitter::Node;

fn parse_fixture_tree(source: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .expect("Kotlin grammar");
    parser.parse(source, None).expect("parse Kotlin fixture")
}

fn find_type<'tree>(node: Node<'tree>, source: &str, spelling: &str) -> Option<Node<'tree>> {
    if node.kind() == "user_type" && node.utf8_text(source.as_bytes()).ok()? == spelling {
        return Some(node);
    }
    node.named_children(&mut node.walk())
        .find_map(|child| find_type(child, source, spelling))
}

fn rendered_type(source: &str, spelling: &str) -> String {
    let tree = parse_fixture_tree(source);
    let node = find_type(tree.root_node(), source, spelling).expect("fixture type node");
    kt::java_type(node, source)
}

#[test]
fn canonical_unit_is_a_value_in_fields_and_generic_arguments() {
    assert_eq!(
        rendered_type("class Box(val item: Unit)", "Unit"),
        "kotlin.Unit"
    );
    assert_eq!(
        rendered_type("class Box(val items: List<Unit>)", "List<Unit>"),
        "List<kotlin.Unit>"
    );
    assert_eq!(
        rendered_type("class Box(val items: List<Unit>)", "Unit"),
        "kotlin.Unit"
    );
}

#[test]
fn unit_return_context_is_void_but_nullable_unit_is_a_value() {
    let source = "fun signal(): Unit {}; fun maybe(): Unit? = null";
    let tree = parse_fixture_tree(source);
    let unit = find_type(tree.root_node(), source, "Unit").expect("Unit return type");
    assert!(kt::is_canonical_unit_type(unit, source));
    assert_eq!(
        kt::java_return_type(unit, source, AnnotationSet::None),
        "void"
    );

    let nullable = tree
        .root_node()
        .named_children(&mut tree.root_node().walk())
        .find(|node| {
            node.kind() == "function_declaration"
                && node.utf8_text(source.as_bytes()).unwrap().contains("maybe")
        })
        .and_then(|decl| {
            decl.named_children(&mut decl.walk())
                .find(|node| node.kind() == "nullable_type")
        })
        .expect("nullable return type");
    assert_ne!(
        kt::java_return_type(nullable, source, AnnotationSet::None),
        "void"
    );
    assert_eq!(
        kt::java_type_ann(nullable, source, AnnotationSet::None),
        "kotlin.Unit"
    );
}

#[test]
fn explicit_alias_to_kotlin_unit_is_mapped_but_custom_unit_is_preserved() {
    assert_eq!(
        rendered_type(
            "import kotlin.Unit as Outcome\nclass Box(val item: Outcome)",
            "Outcome"
        ),
        "kotlin.Unit"
    );
    assert_eq!(
        rendered_type(
            "import kotlin.Unit as Outcome\nclass Outcome(val code: String)\nclass Box(val item: Outcome)",
            "Outcome"
        ),
        "Outcome"
    );
    assert_eq!(
        rendered_type(
            "package sample\nclass Unit(val code: String)\nclass Box(val item: Unit)",
            "Unit"
        ),
        "Unit"
    );
    assert_eq!(
        rendered_type(
            "import example.Unit as Unit\nclass Box(val item: Unit)",
            "Unit"
        ),
        "Unit"
    );
    assert_eq!(
        rendered_type("class Box<Unit>(val item: Unit)", "Unit"),
        "Unit"
    );
}

#[test]
fn unknown_star_import_does_not_claim_unqualified_unit() {
    assert_eq!(
        rendered_type("import example.types.*\nclass Box(val item: Unit)", "Unit"),
        "Unit"
    );
}

#[test]
fn nested_unit_declarations_only_shadow_references_in_their_lexical_owner() {
    let outside = "class Container { class Unit }; class Box(val item: Unit)";
    assert_eq!(rendered_type(outside, "Unit"), "kotlin.Unit");

    let inside = "class Container { class Unit; class Box(val item: Unit) }";
    assert_eq!(rendered_type(inside, "Unit"), "Unit");
}

#[test]
fn imported_kotlin_qualifier_alias_blocks_fully_qualified_shortcut() {
    let shadowed = "import example.kotlin as kotlin\nclass Box(val item: kotlin.Unit)";
    let tree = parse_fixture_tree(shadowed);
    let unit = find_type(tree.root_node(), shadowed, "kotlin.Unit").expect("qualified type");
    assert!(!kt::is_canonical_unit_type(unit, shadowed));

    let canonical = "class Box(val item: kotlin.Unit)";
    let tree = parse_fixture_tree(canonical);
    let unit = find_type(tree.root_node(), canonical, "kotlin.Unit").expect("qualified type");
    assert!(kt::is_canonical_unit_type(unit, canonical));
}

#[test]
fn unit_value_binding_respects_aliases_and_same_file_shadowing() {
    let source = "import kotlin.Unit as Empty\nfun value() = Empty";
    let tree = parse_fixture_tree(source);
    // Expression identifiers are not user_type nodes, so locate their AST node
    // directly and use the same binding rule as the type mapper.
    let mut pending = vec![tree.root_node()];
    let mut found = None;
    while let Some(node) = pending.pop() {
        if node.kind() == "identifier" && node.utf8_text(source.as_bytes()).unwrap() == "Empty" {
            found = Some(node);
            break;
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    let identifier = found.expect("Unit alias expression identifier");
    assert!(kt::is_canonical_unit_reference(identifier, source));

    let shadowed = "package sample\nclass Unit\nfun value() = Unit";
    let tree = parse_fixture_tree(shadowed);
    let mut pending = vec![tree.root_node()];
    let mut unit_value = None;
    while let Some(node) = pending.pop() {
        if node.kind() == "identifier" && node.utf8_text(shadowed.as_bytes()).unwrap() == "Unit" {
            unit_value = Some(node);
            break;
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    assert!(!kt::is_canonical_unit_reference(
        unit_value.expect("shadowed Unit identifier"),
        shadowed
    ));

    let already_instance = "fun value() = kotlin.Unit.INSTANCE";
    let tree = parse_fixture_tree(already_instance);
    let mut pending = vec![tree.root_node()];
    let mut unit_reference = None;
    while let Some(node) = pending.pop() {
        if node.kind() == "navigation_expression"
            && node.utf8_text(already_instance.as_bytes()).unwrap() == "kotlin.Unit.INSTANCE"
        {
            unit_reference = Some(node);
            break;
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    assert!(!kt::is_canonical_unit_reference(
        unit_reference.expect("fully-qualified Unit singleton"),
        already_instance
    ));
}

#[test]
fn same_package_unit_in_another_file_shadows_the_stdlib_type() {
    let root = std::env::temp_dir().join(format!("notlin-unit-shadow-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Unit.kt"),
        "package sample.unitshadow\nclass Unit(val code: String)\n",
    )
    .unwrap();
    fs::write(
        root.join("Box.kt"),
        "package sample.unitshadow\ndata class Box(val item: Unit)\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Box.kt"))
        .output()
        .expect("run notlin");
    let java = fs::read_to_string(root.join("Box.java")).unwrap_or_default();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let retained = fs::read_to_string(root.join("Box.kt")).unwrap_or_default();
    assert!(
        (java.contains("Unit item") && !java.contains("kotlin.Unit item"))
            || (java.is_empty() && retained.contains("data class Box(val item: Unit)")),
        "same-package Unit must be emitted with its source name or retained in Kotlin; stdout:\n{}\nstderr:\n{}\nJava:\n{java}\nKotlin:\n{retained}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!java.contains("kotlin.Unit item"), "{java}");
    let _ = fs::remove_dir_all(Path::new(&root));
}

#[test]
fn data_class_unit_defaults_and_methods_keep_distinct_jvm_contexts() {
    let root = std::env::temp_dir().join(format!("notlin-unit-value-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Request.kt"),
        "package sample.unitvalue\ndata class Request(val unused: Unit = Unit, val values: List<Unit> = listOf()) { fun signal(): Unit {} }\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Request.kt"))
        .output()
        .expect("run notlin");
    let java = fs::read_to_string(root.join("Request.java")).unwrap_or_default();
    assert!(
        output.status.success()
            && java.contains("kotlin.Unit unused")
            && java.contains("List<kotlin.Unit> values")
            && java.contains("void signal()")
            && java.contains("kotlin.Unit.INSTANCE"),
        "Unit values, generic arguments, and defaults must stay reference-typed while Unit methods remain void; stdout:\n{}\nstderr:\n{}\nJava:\n{java}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_dir_all(&root);
}
