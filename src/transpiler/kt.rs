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
pub fn text<'s>(node: tree_sitter::Node<'_>, source: &'s str) -> &'s str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

pub fn java_parameter_type(node: tree_sitter::Node, source: &str, annots: AnnotationSet) -> String {
    let java = java_type_ann(node, source, annots);
    if node.kind() != "user_type" {
        return java;
    }
    let name = text(node, source)
        .split('<')
        .next()
        .unwrap_or_default()
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim();
    if !matches!(name, "Iterable" | "MutableIterable") {
        return java;
    }
    let Some(type_arguments) = child(node, "type_arguments") else {
        return java;
    };
    let arguments: Vec<_> = type_arguments
        .children(&mut type_arguments.walk())
        .filter(|child| child.is_named())
        .collect();
    if arguments.len() != 1 {
        return java;
    }
    let argument = arguments[0];
    let argument_java = if argument.kind() == "type_projection" {
        java_type_projection(argument, source)
    } else {
        java_type(argument, source)
    };
    if argument_java.starts_with('?') {
        java
    } else {
        format!("Iterable<? extends {}>", argument_java)
    }
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

/// True when this type reference names the Kotlin stdlib `Unit` value type.
/// Bare `Unit` follows Kotlin import and same-file declaration rules; aliases
/// are accepted only when they explicitly target `kotlin.Unit`. Unknown star
/// imports and a source declaration named `Unit` fail closed.
pub fn is_canonical_unit_type(node: tree_sitter::Node, source: &str) -> bool {
    if node.kind() != "user_type" {
        return false;
    }
    is_canonical_unit_reference(node, source)
}

/// True when a type or value reference is bound to the Kotlin stdlib Unit.
/// Expression callers must additionally check local value shadowing in their
/// lexical scope.
pub fn is_canonical_unit_reference(node: tree_sitter::Node, source: &str) -> bool {
    if !matches!(
        node.kind(),
        "user_type" | "identifier" | "simple_identifier" | "navigation_expression"
    ) {
        return false;
    }
    let spelling = text(node, source).trim().replace('`', "");
    if spelling == "kotlin.Unit.INSTANCE" {
        return false;
    }
    let could_be_unit = spelling == "Unit"
        || spelling == "kotlin.Unit"
        || source.contains(&format!(" as {spelling}"));
    if !could_be_unit {
        return false;
    }
    let mut root = node;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut imports = Vec::new();
    let mut pending = vec![root];
    while let Some(current) = pending.pop() {
        if current.kind() == "import" {
            imports.push(text(current, source).trim().to_string());
        }
        pending.extend(current.named_children(&mut current.walk()));
    }
    let mut alias_target = None;
    let mut has_conflicting_wildcard = false;
    let mut explicit_unit_import = None;
    let mut kotlin_qualifier_is_aliased = false;
    for import in imports {
        let Some(import) = import.strip_prefix("import ") else {
            continue;
        };
        let (path, alias) = import
            .split_once(" as ")
            .map_or((import.trim(), None), |(path, alias)| {
                (path.trim(), Some(alias.trim().replace('`', "")))
            });
        if alias.as_deref() == Some(spelling.as_str()) {
            alias_target = Some(path.to_string());
        }
        if alias.as_deref() == Some("kotlin") && path != "kotlin" {
            kotlin_qualifier_is_aliased = true;
        }
        if spelling == "Unit" && alias.is_none() && path.ends_with(".Unit") {
            explicit_unit_import = Some(path.to_string());
        }
        if spelling == "Unit" && alias.is_none() && path.ends_with(".*") && path != "kotlin.*" {
            has_conflicting_wildcard = true;
        }
    }
    if spelling == "kotlin.Unit" {
        return !kotlin_qualifier_is_aliased
            && !declaration_shadows_reference(node, root, source, "kotlin")
            && !has_lexical_type_parameter_named(node, source, "kotlin");
    }
    if has_lexical_type_parameter_named(node, source, &spelling)
        || declaration_shadows_reference(node, root, source, &spelling)
    {
        return false;
    }
    if let Some(target) = alias_target {
        return target == "kotlin.Unit";
    }
    if spelling != "Unit" || has_conflicting_wildcard {
        return false;
    }
    explicit_unit_import.is_none_or(|path| path == "kotlin.Unit")
}

fn declaration_shadows_reference(
    reference: tree_sitter::Node,
    root: tree_sitter::Node,
    source: &str,
    name: &str,
) -> bool {
    let mut pending = vec![root];
    while let Some(candidate) = pending.pop() {
        if matches!(
            candidate.kind(),
            "class_declaration" | "object_declaration" | "type_alias" | "enum_class_declaration"
        ) && field(candidate, "name")
            .is_some_and(|identifier| text(identifier, source).replace('`', "") == name)
        {
            let mut owner = candidate.parent();
            let mut lexical_owner = None;
            while let Some(scope) = owner {
                if scope == root {
                    break;
                }
                if matches!(
                    scope.kind(),
                    "class_declaration"
                        | "object_declaration"
                        | "function_declaration"
                        | "property_declaration"
                ) {
                    lexical_owner = Some(scope);
                    break;
                }
                owner = scope.parent();
            }
            let visible = lexical_owner.is_none_or(|scope| {
                reference.start_byte() >= scope.start_byte()
                    && reference.end_byte() <= scope.end_byte()
            });
            if visible {
                return true;
            }
        }
        pending.extend(candidate.named_children(&mut candidate.walk()));
    }
    false
}

fn has_lexical_type_parameter_named(node: tree_sitter::Node, source: &str, name: &str) -> bool {
    let mut scope = node.parent();
    while let Some(current) = scope {
        if matches!(
            current.kind(),
            "class_declaration" | "function_declaration" | "type_alias"
        ) && let Some(parameters) = child(current, "type_parameters")
        {
            let mut pending = vec![parameters];
            while let Some(parameter) = pending.pop() {
                if parameter.kind() == "type_parameter"
                    && field(parameter, "name")
                        .or_else(|| child(parameter, "identifier"))
                        .is_some_and(|identifier| text(identifier, source).replace('`', "") == name)
                {
                    return true;
                }
                pending.extend(parameter.named_children(&mut parameter.walk()));
            }
        }
        scope = current.parent();
    }
    false
}

/// Java method return context differs from a stored or generic Unit value:
/// Kotlin functions returning the stdlib Unit use the JVM `void` descriptor.
pub fn java_return_type(node: tree_sitter::Node, source: &str, annots: AnnotationSet) -> String {
    if is_canonical_unit_type(node, source) {
        "void".to_string()
    } else {
        java_type_ann(node, source, annots)
    }
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
    let force_wildcard = projected.is_some_and(|child| has_canonical_jvm_wildcard(child, source));
    let java = projected
        .map(|child| java_type(child, source))
        .unwrap_or_else(|| "Object".to_string());
    let java = crate::transpiler::types::boxed_name(&java)
        .unwrap_or(&java)
        .to_string();
    match variance {
        Some("out") => format!("? extends {java}"),
        Some("in") => format!("? super {java}"),
        _ if force_wildcard => format!("? extends {java}"),
        _ => java,
    }
}

/// Recognize only the canonical fully-qualified annotation. A simple spelling
/// can be shadowed by a source declaration or import alias, so it must never
/// silently alter the Java generic signature.
fn has_canonical_jvm_wildcard(node: tree_sitter::Node, source: &str) -> bool {
    if node.kind() != "user_type" {
        return false;
    }
    let Some(modifiers) = child(node, "type_modifiers") else {
        return false;
    };
    modifiers
        .named_children(&mut modifiers.walk())
        .filter(|modifier| modifier.kind() == "annotation")
        .any(|annotation| {
            let annotation = text(annotation, source).trim();
            annotation == "@kotlin.jvm.JvmWildcard" || annotation == "@kotlin.jvm.JvmWildcard()"
        })
}

fn java_type_modifiers(node: tree_sitter::Node, source: &str) -> String {
    let Some(modifiers) = child(node, "type_modifiers") else {
        return String::new();
    };
    let mut rendered = String::new();
    for modifier in modifiers.named_children(&mut modifiers.walk()) {
        if modifier.kind() != "annotation" {
            continue;
        }
        let annotation = text(modifier, source).trim();
        if matches!(
            annotation,
            "@kotlin.jvm.JvmWildcard" | "@kotlin.jvm.JvmWildcard()"
        ) {
            continue;
        }
        rendered.push_str(annotation);
        rendered.push(' ');
    }
    rendered
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
                } else if child.kind() == "type_modifiers" {
                    // Consume only the compiler annotation we lower explicitly;
                    // unrelated type-use annotations may carry runtime metadata.
                    out.push_str(&java_type_modifiers(node, source));
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
                // Kotlin's Mutable* collections ARE the java.util interfaces
                // at the JVM level; see `map_type_name`.
                ("MutableList<", "List<"),
                ("MutableMap<", "Map<"),
                ("MutableSet<", "Set<"),
                ("MutableCollection<", "Collection<"),
                ("MutableIterable<", "Iterable<"),
                ("MutableIterator<", "Iterator<"),
                ("MutableListIterator<", "ListIterator<"),
                ("MutableMap.MutableEntry<", "Map.Entry<"),
            ] {
                if let Some(stripped) = whole.strip_prefix(kt_name) {
                    return format!("{}{}", j_name, stripped);
                }
            }
            let mapped = match whole {
                "Unit" if is_canonical_unit_type(node, source) => "kotlin.Unit",
                "Unit" => "Unit",
                _ if is_canonical_unit_type(node, source) => "kotlin.Unit",
                _ => crate::transpiler::types::map_type_name(whole),
            };
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
        "type_projection" => node
            .children(&mut node.walk())
            .find(|child| child.is_named())
            .map(|inner| java_type(inner, source))
            .unwrap_or_else(|| text(node, source).to_string()),
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

#[cfg(test)]
mod tests {
    use super::{java_type, text};
    use tree_sitter::Node;

    fn named_type<'tree>(
        node: Node<'tree>,
        source: &str,
        starts_with: &str,
    ) -> Option<Node<'tree>> {
        if node.kind() == "user_type" && text(node, source).starts_with(starts_with) {
            return Some(node);
        }
        node.named_children(&mut node.walk())
            .find_map(|child| named_type(child, source, starts_with))
    }

    fn render_outer_type(source: &str, prefix: &str) -> String {
        let tree = crate::transpiler::parse_tree(source);
        let ty = named_type(tree.root_node(), source, prefix).expect("type node in fixture");
        java_type(ty, source)
    }

    #[test]
    fn canonical_jvm_wildcard_becomes_java_extends_projection() {
        let source = "interface Box { val items: List<@kotlin.jvm.JvmWildcard Base> }";
        assert_eq!(render_outer_type(source, "List<"), "List<? extends Base>");
    }

    #[test]
    fn simple_or_shadowed_jvm_wildcard_does_not_activate_wildcard_lowering() {
        let source =
            "annotation class JvmWildcard\ninterface Box { val items: List<@JvmWildcard Base> }";
        let java = render_outer_type(source, "List<");
        assert_eq!(java, "List<@JvmWildcard Base>");
        assert!(!java.contains("? extends"));
    }

    #[test]
    fn unrelated_type_use_annotations_are_preserved() {
        let source = "interface Box { val items: List<@example.Marker Base> }";
        assert_eq!(
            render_outer_type(source, "List<"),
            "List<@example.Marker Base>"
        );
    }

    #[test]
    fn collection_types_do_not_gain_implicit_wildcards() {
        assert_eq!(
            render_outer_type("interface Box { val items: List<Base> }", "List<"),
            "List<Base>"
        );
        assert_eq!(
            render_outer_type(
                "interface Box { val items: MutableList<Base> }",
                "MutableList<"
            ),
            "List<Base>"
        );
    }

    #[test]
    fn explicit_out_and_jvm_wildcard_do_not_duplicate_projection() {
        let source = "interface Box { val items: List<out @kotlin.jvm.JvmWildcard Base> }";
        assert_eq!(render_outer_type(source, "List<"), "List<? extends Base>");
    }
}
