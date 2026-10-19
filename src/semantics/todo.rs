//! Source-bound proof for rewriting the exact Kotlin stdlib `TODO` call to a
//! throw. The call is safe only where its `Nothing` type is already compatible
//! with the surrounding declaration and its name cannot denote source code.

use super::{
    FactStatus, SemanticProvider, SourceLocation, is_proven_constructor_property_shadow,
    is_proven_local_function_shadow, is_proven_local_shadow, kotlin_import_lines,
    resolve_required_call,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tree_sitter::Node;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoLowering {
    pub location: SourceLocation,
    pub reason: Option<SourceLocation>,
}

pub(super) fn establish(
    source: &str,
    call: Node<'_>,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
) -> Option<TodoLowering> {
    if call.kind() != "call_expression"
        || call.has_error()
        || provider.source_for_file(file)? != source
    {
        return None;
    }
    let (callee, reason) = call_parts(source, call)?;
    if callee.utf8_text(source.as_bytes()).ok()?.trim() != "TODO" {
        return None;
    }
    if !matches!(
        resolve_required_call(source, file, call, callee, "TODO", provider),
        FactStatus::Unknown
    ) {
        return None;
    }
    if is_proven_local_shadow(callee, "TODO", source)
        || is_proven_local_function_shadow(callee, "TODO", source)
        || is_proven_constructor_property_shadow(callee, "TODO", source)
        || super::same_package_factory_name_is_shadowed(source, call, "TODO", provider)
        || local_or_same_package_classifier_shadows(source, callee, "TODO", provider)
        || java_ancestor_value_shadows(source, callee, file, "TODO", provider)
        || import_shadows_todo(source, provider)
    {
        return None;
    }
    if !throw_compatible_context(source, call) {
        return None;
    }
    Some(TodoLowering {
        location: source_location(source, file, call),
        reason: reason.map(|node| source_location(source, file, node)),
    })
}

fn call_parts<'tree>(
    source: &str,
    call: Node<'tree>,
) -> Option<(Node<'tree>, Option<Node<'tree>>)> {
    let mut cursor = call.walk();
    let named = call
        .named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect::<Vec<_>>();
    let callee = *named.first()?;
    if !matches!(callee.kind(), "identifier" | "simple_identifier")
        || named.iter().any(|child| {
            matches!(
                child.kind(),
                "type_arguments" | "lambda_literal" | "annotated_lambda" | "lambda_expression"
            )
        })
    {
        return None;
    }
    let argument_list = named.iter().copied().find(|child| {
        matches!(
            child.kind(),
            "value_arguments" | "argument_list" | "arguments" | "value_argument_list"
        )
    });
    let Some(arguments) = argument_list else {
        return (named.len() == 1).then_some((callee, None));
    };
    if named.len() != 2 {
        return None;
    }
    let mut cursor = arguments.walk();
    let values = arguments
        .named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .collect::<Vec<_>>();
    match values.as_slice() {
        [] => Some((callee, None)),
        [argument] if argument.kind() == "value_argument" => {
            let mut cursor = argument.walk();
            let expressions = argument
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra() && child.kind() != "annotation")
                .collect::<Vec<_>>();
            let [literal] = expressions.as_slice() else {
                return None;
            };
            if literal.kind() != "string_literal" {
                return None;
            }
            let text = literal.utf8_text(source.as_bytes()).ok()?.trim();
            if text.len() < 2
                || !text.starts_with('"')
                || !text.ends_with('"')
                || text.starts_with("\"\"\"")
                || text.contains('$')
            {
                return None;
            }
            let argument_text = argument.utf8_text(source.as_bytes()).ok()?.trim();
            if argument_text != text {
                // This excludes named/spread argument syntax without parsing
                // identifiers that might themselves occur in a literal.
                return None;
            }
            Some((callee, Some(*literal)))
        }
        _ => None,
    }
}

fn import_shadows_todo<P: SemanticProvider + ?Sized>(source: &str, provider: &P) -> bool {
    kotlin_import_lines(source).any(|line| {
        let line = line.trim();
        let (path, alias) = line
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((line, None));
        if alias.is_some_and(|alias| alias == "TODO") {
            return true;
        }
        if path.ends_with(".*") {
            if !matches!(path, "java.util.*" | "java.lang.*") {
                return true;
            }
            let package = path.trim_end_matches(".*");
            return provider.symbols_named("TODO").iter().any(|symbol| {
                matches!(symbol.id.kind.as_str(), "function" | "property")
                    && symbol.id.package == package
                    && symbol.id.owner_path.is_empty()
            });
        }
        path.rsplit('.').next() == Some("TODO") && path != "kotlin.TODO"
    })
}

fn throw_compatible_context(source: &str, call: Node<'_>) -> bool {
    let mut current = call;
    while current
        .parent()
        .is_some_and(|parent| parent.kind() == "parenthesized_expression")
    {
        current = current.parent().unwrap();
    }
    let mut ancestor = current.parent();
    let mut return_statement = None;
    let mut block = None;
    let mut function = None;
    let mut getter = None;
    while let Some(node) = ancestor {
        match node.kind() {
            "lambda_literal" | "function_literal" | "anonymous_function" | "object_literal"
            | "anonymous_object" | "companion_object" => return false,
            "return_expression" => {
                if return_statement.is_some() || !is_direct_child_or_parens(current, node) {
                    return false;
                }
                return_statement = Some(node);
            }
            "block" => {
                if block.is_some()
                    || !is_direct_child_or_parens(current, node) && return_statement.is_none()
                {
                    return false;
                }
                block = Some(node);
            }
            "function_declaration" => {
                function = Some(node);
                break;
            }
            "getter" => {
                getter = Some(node);
                break;
            }
            "class_declaration" | "object_declaration" | "interface_declaration" => return false,
            "function_body" => {
                let Ok(text) = node.utf8_text(source.as_bytes()) else {
                    return false;
                };
                let body = text.trim_start();
                if !body.starts_with('=') && return_statement.is_none() && block.is_none() {
                    return false;
                }
            }
            _ => {}
        }
        ancestor = node.parent();
    }
    if let Some(function) = function {
        if super::callable_receiver(source, function).is_some()
            || nested_callable_owner(function)
            || !has_explicit_return_type(function, source)
        {
            return false;
        }
        if let Some(return_statement) = return_statement {
            return is_direct_child_or_parens(current, return_statement)
                && return_statement_in_function_block(return_statement, function);
        }
        if let Some(block) = block {
            return is_only_block_statement(current, block)
                && block
                    .parent()
                    .is_some_and(|parent| parent.kind() == "function_body")
                && parent_function(block, function);
        }
        return function_body_is_expression(source, function, call)
            && direct_expression_body_call(call, function);
    }
    if let Some(getter) = getter {
        if return_statement.is_some()
            || block.is_some()
            || nested_callable_owner(getter)
            || !direct_expression_body_call(call, getter)
        {
            return false;
        }
        let Some(property) = getter
            .parent()
            .filter(|parent| parent.kind() == "property_declaration")
        else {
            return false;
        };
        return property_has_explicit_type(property, source);
    }
    false
}

fn has_explicit_return_type(function: Node<'_>, source: &str) -> bool {
    if let Some(ty) = function.child_by_field_name("return_type") {
        return !is_bottom_type(ty, source);
    }
    function
        .named_children(&mut function.walk())
        .find(|child| child.kind() == "function_value_parameters")
        .is_some_and(|parameters| {
            function.named_children(&mut function.walk()).any(|child| {
                child.start_byte() > parameters.end_byte() && is_type_node(child, source)
            })
        })
}

fn property_has_explicit_type(property: Node<'_>, source: &str) -> bool {
    let mut stack = property
        .named_children(&mut property.walk())
        .filter(|child| child.kind() == "variable_declaration")
        .collect::<Vec<_>>();
    while let Some(node) = stack.pop() {
        if let Some(ty) = node
            .child_by_field_name("type")
            .or_else(|| super::named_type_child(node))
        {
            return !is_bottom_type(ty, source);
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn is_type_node(node: Node<'_>, source: &str) -> bool {
    matches!(node.kind(), "user_type" | "nullable_type" | "function_type")
        && node.utf8_text(source.as_bytes()).is_ok()
        && !is_bottom_type(node, source)
}

fn is_bottom_type(node: Node<'_>, source: &str) -> bool {
    node.utf8_text(source.as_bytes()).is_ok_and(|text| {
        matches!(
            text.trim().trim_end_matches('?'),
            "Nothing" | "kotlin.Nothing"
        )
    })
}

fn function_body_is_expression(source: &str, function: Node<'_>, call: Node<'_>) -> bool {
    let Some(body) = function.child_by_field_name("body") else {
        let mut cursor = function.walk();
        let Some(body) = function
            .named_children(&mut cursor)
            .find(|child| child.kind() == "function_body")
        else {
            return false;
        };
        return body.utf8_text(source.as_bytes()).is_ok_and(|text| {
            text.trim_start().starts_with('=') && direct_expression_body_call(call, function)
        });
    };
    body.kind() == "function_body"
        && body
            .utf8_text(source.as_bytes())
            .is_ok_and(|text| text.trim_start().starts_with('='))
        && direct_expression_body_call(call, function)
}

fn direct_expression_body_call(mut call: Node<'_>, owner: Node<'_>) -> bool {
    while call
        .parent()
        .is_some_and(|parent| parent.kind() == "parenthesized_expression")
    {
        call = call.parent().unwrap();
    }
    call.parent().is_some_and(|parent| {
        parent.kind() == "function_body"
            && parent
                .parent()
                .is_some_and(|function| function.id() == owner.id())
    })
}

fn is_direct_child_or_parens(mut node: Node<'_>, ancestor: Node<'_>) -> bool {
    loop {
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.id() == ancestor.id() {
            return true;
        }
        if parent.kind() != "parenthesized_expression" {
            return false;
        }
        node = parent;
    }
}

fn return_statement_in_function_block(return_statement: Node<'_>, function: Node<'_>) -> bool {
    let Some(block) = return_statement
        .parent()
        .filter(|parent| parent.kind() == "block")
    else {
        return false;
    };
    is_only_block_statement(return_statement, block)
        && block.parent().is_some_and(|body| {
            body.kind() == "function_body"
                && body
                    .parent()
                    .is_some_and(|parent| parent.id() == function.id())
        })
}

fn is_only_block_statement(mut node: Node<'_>, block: Node<'_>) -> bool {
    while node
        .parent()
        .is_some_and(|parent| parent.kind() == "parenthesized_expression")
    {
        node = node.parent().unwrap();
    }
    if node.parent().is_none_or(|parent| parent.id() != block.id()) {
        return false;
    }
    let statements = block
        .named_children(&mut block.walk())
        .filter(|child| !child.is_extra() && !child.kind().contains("comment"))
        .collect::<Vec<_>>();
    statements.len() == 1 && statements[0].id() == node.id()
}

fn parent_function(block: Node<'_>, function: Node<'_>) -> bool {
    block.parent().is_some_and(|body| {
        body.kind() == "function_body"
            && body
                .parent()
                .is_some_and(|parent| parent.id() == function.id())
    })
}

fn nested_callable_owner(function: Node<'_>) -> bool {
    let mut ancestor = function.parent();
    while let Some(node) = ancestor {
        if matches!(
            node.kind(),
            "function_declaration"
                | "lambda_literal"
                | "function_literal"
                | "anonymous_function"
                | "object_literal"
                | "anonymous_object"
                | "companion_object"
        ) {
            return true;
        }
        if matches!(
            node.kind(),
            "source_file" | "class_declaration" | "object_declaration"
        ) {
            return false;
        }
        ancestor = node.parent();
    }
    false
}

fn local_or_same_package_classifier_shadows<P: SemanticProvider + ?Sized>(
    source: &str,
    reference: Node<'_>,
    name: &str,
    provider: &P,
) -> bool {
    let package = super::package_name(source);
    if provider
        .symbols_named(name)
        .iter()
        .any(|symbol| super::is_type_symbol(&symbol.id) && symbol.id.package == package)
    {
        return true;
    }
    let mut visible_scopes = std::collections::BTreeSet::new();
    let mut ancestor = Some(reference);
    while let Some(node) = ancestor {
        if matches!(
            node.kind(),
            "block" | "lambda_literal" | "function_literal" | "function_declaration"
        ) {
            visible_scopes.insert(node.id());
        }
        ancestor = node.parent();
    }
    let mut root = reference;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "class_declaration"
                | "enum_declaration"
                | "interface_declaration"
                | "object_declaration"
                | "type_alias"
        ) && super::node_name(node, source).as_deref() == Some(name)
            && visible_scopes.contains(&super::nearest_lexical_scope(node))
        {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn java_ancestor_value_shadows<P: SemanticProvider + ?Sized>(
    source: &str,
    reference: Node<'_>,
    file: &Path,
    name: &str,
    provider: &P,
) -> bool {
    let mut pending = Vec::new();
    let mut lexical = reference.parent();
    while let Some(node) = lexical {
        if matches!(
            node.kind(),
            "class_declaration" | "interface_declaration" | "object_declaration"
        ) {
            let owner = super::symbol_id_for_node(source, node, file);
            if !provider.has_complete_inheritance(&owner) {
                return true;
            }
            pending.extend(match provider.instance_contract_parents(&owner) {
                FactStatus::Established(parents) => parents,
                FactStatus::Unknown | FactStatus::Inferred(_) | FactStatus::Ambiguous(_) => {
                    return true;
                }
            });
        }
        lexical = node.parent();
    }
    let mut seen = std::collections::BTreeSet::new();
    while let Some(parent) = pending.pop() {
        if !seen.insert(parent.clone()) {
            continue;
        }
        if parent
            .file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
            && (!matches!(
                provider.java_field_named(&parent, name),
                FactStatus::Established(false)
            ) || !provider.java_property_getters(&parent, name).is_empty()
                || java_getter_method_may_expose(provider, &parent, name))
        {
            return true;
        }
        pending.extend(match provider.instance_contract_parents(&parent) {
            FactStatus::Established(parents) => parents,
            FactStatus::Unknown | FactStatus::Inferred(_) | FactStatus::Ambiguous(_) => {
                return true;
            }
        });
    }
    false
}

fn java_getter_method_may_expose<P: SemanticProvider + ?Sized>(
    provider: &P,
    owner: &super::SymbolId,
    property: &str,
) -> bool {
    let mut chars = property.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let capitalized = first.to_uppercase().collect::<String>() + chars.as_str();
    [format!("get{capitalized}"), format!("is{capitalized}")]
        .iter()
        .any(|getter| {
            provider.symbols_named(getter).iter().any(|symbol| {
                matches!(symbol.id.kind.as_str(), "function" | "method")
                    && symbol.id.file == owner.file
                    && symbol.id.package == owner.package
                    && symbol.id.owner_path
                        == owner
                            .owner_path
                            .iter()
                            .cloned()
                            .chain(std::iter::once(owner.name.clone()))
                            .collect::<Vec<_>>()
                    && symbol.id.parameters.is_empty()
            })
        })
}

fn source_location(source: &str, file: &Path, node: Node<'_>) -> SourceLocation {
    SourceLocation {
        file: file.to_path_buf(),
        snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    }
}
