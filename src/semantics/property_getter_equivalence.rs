//! Source-shape gates for a narrow annotated-property getter equivalence.
//!
//! This module consumes the semantic query's already parsed source tree. It
//! deliberately performs no provider lookups or parsing of its own.

use tree_sitter::Node;

/// Return the start byte of the unique explicit `getX()` method whose entire
/// body reads the annotated property directly. Symbol identity, source
/// snapshots, and the method's resolved signature remain the caller's proof.
pub(super) fn candidate(source: &str, root: Node<'_>, property: Node<'_>) -> Option<usize> {
    if !matches!(property.kind(), "property_declaration" | "class_parameter")
        || !annotated_property_trigger(source, root, property)?
        || !public_property(source, property)
    {
        return None;
    }
    let owner = declaring_class(property)?;
    if !is_final_ordinary_top_level_class(source, owner)
        || !is_direct_member(owner, property)
        || owner_has_annotations(owner)
    {
        return None;
    }
    let property_name = declaration_name(source, property)?;
    let getter_name = java_getter_name(&property_name)?;
    let body = class_body(owner)?;
    let methods = body
        .named_children(&mut body.walk())
        .filter(|member| {
            member.kind() == "function_declaration"
                && declaration_name(source, *member).as_deref() == Some(&getter_name)
        })
        .collect::<Vec<_>>();
    let [method] = methods.as_slice() else {
        return None;
    };
    if !is_direct_member(owner, *method)
        || !method_is_public_noarg_nonextension(source, *method)
        || has_any_annotation(*method)
        || !body_is_direct_property_read(source, *method, &property_name)
    {
        return None;
    }
    Some(method.start_byte())
}

fn declaring_class(mut node: Node<'_>) -> Option<Node<'_>> {
    while let Some(parent) = node.parent() {
        if parent.kind() == "class_declaration" {
            return Some(parent);
        }
        if matches!(
            parent.kind(),
            "interface_declaration" | "object_declaration" | "enum_declaration"
        ) {
            return None;
        }
        node = parent;
    }
    None
}

fn is_final_ordinary_top_level_class(source: &str, node: Node<'_>) -> bool {
    if node.kind() != "class_declaration"
        || node
            .parent()
            .is_none_or(|parent| parent.kind() != "source_file")
        || has_type_parameters(node)
        // Compiler plugins can open subclasses of annotated parents. Without
        // plugin facts, an inherited contract cannot prove final dispatch.
        || node.named_children(&mut node.walk())
            .any(|child| child.kind() == "delegation_specifiers")
    {
        return false;
    }
    let mut modifiers = node
        .named_children(&mut node.walk())
        .filter(|child| child.kind() == "modifiers")
        .collect::<Vec<_>>();
    while let Some(part) = modifiers.pop() {
        if [
            "open",
            "abstract",
            "sealed",
            "expect",
            "actual",
            "value",
            "inline",
            "annotation",
            "enum",
            "interface",
            "inner",
            "private",
            "protected",
            "internal",
        ]
        .iter()
        .any(|word| subtree_has_word(source, part, word))
        {
            return false;
        }
        modifiers.extend(part.named_children(&mut part.walk()));
    }
    true
}

fn owner_has_annotations(owner: Node<'_>) -> bool {
    owner.named_children(&mut owner.walk()).any(|child| {
        matches!(child.kind(), "annotation" | "annotation_entry")
            || (child.kind() == "modifiers" && has_any_annotation(child))
    })
}

fn has_type_parameters(node: Node<'_>) -> bool {
    node.named_children(&mut node.walk())
        .any(|child| child.kind() == "type_parameters")
}

fn class_body(owner: Node<'_>) -> Option<Node<'_>> {
    owner
        .named_children(&mut owner.walk())
        .find(|child| child.kind() == "class_body")
}

fn is_direct_member(owner: Node<'_>, member: Node<'_>) -> bool {
    if member.kind() == "class_parameter" {
        let Some(parameters) = owner
            .named_children(&mut owner.walk())
            .find(|child| child.kind() == "primary_constructor")
            .and_then(|constructor| {
                constructor
                    .named_children(&mut constructor.walk())
                    .find(|child| child.kind() == "class_parameters")
            })
        else {
            return false;
        };
        return parameters
            .named_children(&mut parameters.walk())
            .any(|child| child.id() == member.id());
    }
    class_body(owner).is_some_and(|body| {
        body.named_children(&mut body.walk())
            .any(|child| child.id() == member.id())
    })
}

fn declaration_name(source: &str, node: Node<'_>) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        return name.utf8_text(source.as_bytes()).ok().map(str::to_owned);
    }
    if node.kind() == "property_declaration" {
        let variable = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() == "variable_declaration")?;
        return variable
            .named_children(&mut variable.walk())
            .find(|child| matches!(child.kind(), "simple_identifier" | "identifier"))
            .and_then(|name| name.utf8_text(source.as_bytes()).ok())
            .map(str::to_owned);
    }
    if node.kind() == "class_parameter" {
        return node
            .named_children(&mut node.walk())
            .find(|child| matches!(child.kind(), "simple_identifier" | "identifier"))
            .and_then(|name| name.utf8_text(source.as_bytes()).ok())
            .map(str::to_owned);
    }
    None
}

fn public_property(source: &str, property: Node<'_>) -> bool {
    if subtree_has_word(source, property, "private")
        || subtree_has_word(source, property, "protected")
        || subtree_has_word(source, property, "internal")
    {
        return false;
    }
    true
}

fn method_is_public_noarg_nonextension(source: &str, method: Node<'_>) -> bool {
    if has_type_parameters(method)
        || [
            "private",
            "protected",
            "internal",
            "open",
            "abstract",
            "external",
            "suspend",
        ]
        .iter()
        .any(|word| {
            method
                .named_children(&mut method.walk())
                .filter(|child| child.kind() == "modifiers")
                .any(|modifiers| subtree_has_word(source, modifiers, word))
        })
    {
        return false;
    }
    let Some(name) = method.child_by_field_name("name") else {
        return false;
    };
    let has_receiver_type = method
        .named_children(&mut method.walk())
        .any(|child| child.start_byte() < name.start_byte() && child.kind().contains("type"));
    let Some(parameters) = method
        .named_children(&mut method.walk())
        .find(|child| child.kind() == "function_value_parameters")
    else {
        return false;
    };
    !has_receiver_type && parameters.named_child_count() == 0
}

fn subtree_has_word(source: &str, node: Node<'_>, word: &str) -> bool {
    source
        .get(node.start_byte()..node.end_byte())
        .is_some_and(|text| {
            text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .any(|part| part == word)
        })
}

fn annotated_property_trigger(source: &str, root: Node<'_>, property: Node<'_>) -> Option<bool> {
    let imports = root
        .named_children(&mut root.walk())
        .filter(|node| node.kind() == "import")
        .map(|node| node.utf8_text(source.as_bytes()).ok().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    let mut pending = property
        .named_children(&mut property.walk())
        .filter(|child| {
            matches!(
                child.kind(),
                "modifiers" | "parameter_modifiers" | "getter" | "setter"
            )
        })
        .collect::<Vec<_>>();
    let mut annotations = Vec::new();
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "annotation" | "annotation_entry") {
            annotations.push(node.utf8_text(source.as_bytes()).ok()?.to_owned());
        } else {
            pending.extend(node.named_children(&mut node.walk()));
        }
    }
    if annotations.len() != 1 {
        return Some(false);
    }
    let annotation = compact_whitespace(&annotations[0]);
    if annotation == "@JvmField" {
        return Some(simple_annotation_unambiguous(
            &imports,
            "JvmField",
            "kotlin.jvm.JvmField",
        ));
    }
    if annotation == "@kotlin.jvm.JvmField" {
        return Some(true);
    }
    for (prefix, canonical) in [
        ("@get:JvmName(", "kotlin.jvm.JvmName"),
        ("@get:kotlin.jvm.JvmName(", "kotlin.jvm.JvmName"),
    ] {
        if let Some(value) = annotation
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(')'))
            && is_string_literal(value)
        {
            return Some(if prefix == "@get:JvmName(" {
                simple_annotation_unambiguous(&imports, "JvmName", canonical)
            } else {
                true
            });
        }
    }
    Some(false)
}

fn simple_annotation_unambiguous(imports: &[String], name: &str, canonical: &str) -> bool {
    imports.iter().all(|line| {
        let Some(import) = line.trim().strip_prefix("import ") else {
            return true;
        };
        let (path, alias) = import
            .split_once(" as ")
            .map_or((import.trim(), None), |(path, alias)| {
                (path.trim(), Some(alias.trim()))
            });
        if path.ends_with(".*") {
            path == "kotlin.jvm.*"
        } else {
            let imported_name = alias.or_else(|| path.rsplit('.').next());
            imported_name != Some(name) || path == canonical
        }
    })
}

fn compact_whitespace(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn is_string_literal(text: &str) -> bool {
    text.len() >= 2 && text.starts_with('"') && text.ends_with('"') && !text.contains(',')
}

fn has_any_annotation(node: Node<'_>) -> bool {
    let mut pending = vec![node];
    while let Some(part) = pending.pop() {
        if matches!(part.kind(), "annotation" | "annotation_entry") {
            return true;
        }
        pending.extend(part.named_children(&mut part.walk()));
    }
    false
}

fn java_getter_name(property: &str) -> Option<String> {
    if property.is_empty()
        || !property.is_ascii()
        || !property
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        || property
            .strip_prefix("is")
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_uppercase)
    {
        return None;
    }
    let mut chars = property.chars();
    Some(format!(
        "get{}{}",
        chars.next()?.to_ascii_uppercase(),
        chars.as_str()
    ))
}

fn body_is_direct_property_read(source: &str, method: Node<'_>, property: &str) -> bool {
    let Some(body) = method
        .named_children(&mut method.walk())
        .find(|child| child.kind() == "function_body")
    else {
        return false;
    };
    let mut expressions = body.named_children(&mut body.walk()).collect::<Vec<_>>();
    if let [block] = expressions.as_slice()
        && block.kind() == "block"
    {
        expressions = block.named_children(&mut block.walk()).collect();
        if let [statement] = expressions.as_slice()
            && matches!(statement.kind(), "return_statement" | "return_expression")
        {
            expressions = statement.named_children(&mut statement.walk()).collect();
        } else {
            return false;
        }
    }
    let [expression] = expressions.as_slice() else {
        return false;
    };
    let text = source
        .get(expression.start_byte()..expression.end_byte())
        .unwrap_or("");
    let compact = compact_whitespace(text);
    compact == property || compact == format!("this.{property}")
}
