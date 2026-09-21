//! Node helpers shared by all translation modules.
use crate::transpiler::types::AnnotationSet;

/// Get a named child by kind (first match).
pub fn child<'t>(node: tree_sitter::Node<'t>, kind: &str) -> Option<tree_sitter::Node<'t>> {
    node.children(&mut node.walk()).find(|c| c.kind() == kind)
}

/// Get all named children of a given kind.
#[allow(dead_code)]
pub fn children<'t>(
    node: tree_sitter::Node<'t>,
    kind: &str,
) -> impl Iterator<Item = tree_sitter::Node<'t>> {
    let walk = node.walk();
    node.children(&mut walk.clone())
        .filter(move |c| c.kind() == kind)
        .collect::<Vec<_>>()
        .into_iter()
}

/// Get the named child at a given field name.
pub fn field<'t>(node: tree_sitter::Node<'t>, name: &str) -> Option<tree_sitter::Node<'t>> {
    node.child_by_field_name(name)
}

/// Parent node.
pub fn parent_of<'t>(node: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
    node.parent()
}

/// Extract source text for a node.
pub fn text<'t>(node: tree_sitter::Node<'t>, source: &'t str) -> &'t str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

/// Translate a Kotlin user_type / type node into Java source text.
/// `mark_nullable` prefixes the result with the annotation set's @Nullable
/// when the type is nullable (annotation-then-hope-for-the-best policy).
/// Nullable primitives must box (`Int?` can never be `@Nullable int` — null
/// needs a reference type), so their Java name is swapped for the boxed form.
pub fn java_type_ann(node: tree_sitter::Node, source: &str, annots: AnnotationSet) -> String {
    let java = java_type(node, source);
    if node.kind() == "nullable_type" {
        let java = match java.as_str() {
            "int" => "Integer",
            "long" => "Long",
            "short" => "Short",
            "byte" => "Byte",
            "double" => "Double",
            "float" => "Float",
            "boolean" => "Boolean",
            "char" => "Character",
            other => other,
        }
        .to_string();
        if let Some(a) = annots.nullable() {
            return format!("{} {}", a, java);
        }
        return java;
    }
    java
}

pub fn java_type(node: tree_sitter::Node, source: &str) -> String {
    match node.kind() {
        "user_type" => {
            // dotted or generic types; reconstruct from children
            let mut out = String::new();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "user_type" {
                    out.push_str(&java_type(child, source));
                } else if child.is_named() {
                    out.push_str(text(child, source));
                } else {
                    // punctuation: ".", "<", ">", "," etc — keep verbatim
                    out.push_str(text(child, source));
                }
            }
            crate::transpiler::types::map_type_name(out.trim()).to_string()
        }
        "nullable_type" => {
            let inner = child(node, "user_type")
                .or_else(|| child(node, "nullable_type"))
                .or_else(|| child(node, "function_type"));
            match inner {
                Some(inner) => java_type(inner, source),
                None => text(node, source).to_string(),
            }
        }
        _ => text(node, source).to_string(),
    }
}
