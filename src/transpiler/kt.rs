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

fn java_type_projection(node: tree_sitter::Node, source: &str) -> String {
    if text(node, source).trim() == "*" {
        return "?".to_string();
    }
    let mut variance = None;
    let mut projected = None;
    for child in node.named_children(&mut node.walk()) {
        if child.kind() == "variance_modifier" {
            variance = Some(text(child, source).trim());
        } else {
            projected = Some(child);
        }
    }
    let java = projected
        .map(|child| java_type(child, source))
        .unwrap_or_else(|| "Object".to_string());
    let java = crate::transpiler::types::boxed_name(&java)
        .unwrap_or(&java)
        .to_string();
    match variance {
        Some("out") => format!("? extends {java}"),
        Some("in") => format!("? super {java}"),
        _ => java,
    }
}

pub fn java_type(node: tree_sitter::Node, source: &str) -> String {
    match node.kind() {
        "user_type" => {
            // dotted or generic types; reconstruct from children.
            // Type arguments box primitives (`List<Int>` -> `List<Integer>`
            // — generics can't hold primitives).
            let mut out = String::new();
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_arguments" {
                    // rebuild each argument with boxing
                    let mut args = String::new();
                    let mut acur = child.walk();
                    for arg in child.children(&mut acur) {
                        if !arg.is_named() {
                            // punctuation: keep verbatim
                            args.push_str(text(arg, source));
                        } else {
                            let jt = if arg.kind() == "type_projection" {
                                java_type_projection(arg, source)
                            } else {
                                java_type(arg, source)
                            };
                            let boxed = crate::transpiler::types::boxed_name(&jt).unwrap_or(&jt);
                            args.push_str(boxed);
                        }
                    }
                    out.push_str(&args);
                } else if child.kind() == "user_type" {
                    out.push_str(&java_type(child, source));
                } else if child.is_named() {
                    out.push_str(text(child, source));
                } else {
                    // punctuation: ".", "<", ">", "," etc — keep verbatim
                    out.push_str(text(child, source));
                }
            }
            // `KClass<T>` is Java `Class<T>` through interop; the bare-name
            // map can't fire because the full text includes type args.
            let whole = out.trim();
            for (kt_name, j_name) in [
                ("KClass<", "Class<"),
                ("MutableList<", "ArrayList<"),
                ("MutableMap<", "HashMap<"),
                ("MutableSet<", "HashSet<"),
            ] {
                if whole.starts_with(kt_name) {
                    return format!("{}{}", j_name, &whole[kt_name.len()..]);
                }
            }
            let mapped = crate::transpiler::types::map_type_name(whole);
            if mapped == "__NOTLIN_ARRAY__" || out.trim().starts_with("Array<") {
                // `Array<T>` -> `T[]`; rebuild from the type_arguments child
                let mut cursor = node.walk();
                let inner = node
                    .children(&mut cursor)
                    .find(|c| c.kind() == "type_arguments")
                    .and_then(|ta| ta.children(&mut ta.walk()).find(|c| c.is_named()));
                return match inner {
                    Some(n) => format!("{}[]", java_type(n, source)),
                    None => "Object[]".to_string(),
                };
            }
            if mapped == "__NOTLIN_TRIPLE__" {
                // Triple has no JDK equivalent (JDK lacks a 3-tuple); the
                // caller taints the declaration. Type erases to Object.
                return "Object".to_string();
            }
            // Parameterized `Pair<A, B>`: the plain-name map only fires on
            // bare "Pair"; generic forms rewrite on prefix.
            if let Some(rest) = mapped.strip_prefix("Pair<") {
                return format!("java.util.AbstractMap.SimpleImmutableEntry<{}", rest);
            }
            mapped.to_string()
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
        "function_type" => {
            // `(params) -> R` has no Java counterpart -> functional interface
            // approximation: emit a Consumer/Function-shaped warning-free
            // placeholder is impossible without arity info; emit a taint
            // signal via a dedicated untranslatable marker type text.
            crate::transpiler::types::FUNCTION_TYPE_PLACEHOLDER.to_string()
        }
        _ => text(node, source).to_string(),
    }
}
