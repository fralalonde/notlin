//! Conservative source-graph proof for value-position enum `when` lowering.
//!
//! This module records only syntax and symbols that are sufficient to emit a
//! Java switch over an exact enum classifier. It deliberately does not infer
//! exhaustiveness from an unresolved name or from the absence of other types.

use super::{
    FactStatus, KotlinType, SemanticProvider, SourceLocation, SymbolId, TypeRef,
    is_property_parameter, kotlin_import_lines, lexical_parameter_type, package_name,
    resolve_required_call, symbol_id_for_node,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use tree_sitter::Node;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumWhenLowering {
    pub location: SourceLocation,
    pub subject: SourceLocation,
    pub enum_owner: SymbolId,
    pub arms: Vec<EnumWhenArm>,
    /// The result location for an explicit `else`; absent when all enum
    /// entries are listed and the emitter must synthesize the unmatched throw.
    pub default_result: Option<SourceLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumWhenArm {
    pub labels: Vec<SymbolId>,
    pub result: SourceLocation,
}

pub(super) fn establish(
    source: &str,
    node: Node<'_>,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
) -> Option<EnumWhenLowering> {
    if node.kind() != "when_expression" || node.has_error() {
        return None;
    }
    if provider.source_for_file(file)? != source {
        return None;
    }
    value_return_context(source, node)?;

    let subject = when_subject(node)?;
    let enum_owner = subject_enum_owner(source, subject, node, file, provider)?;
    let entries = match provider.enum_entries(&enum_owner) {
        FactStatus::Established(entries) if !entries.is_empty() => entries,
        FactStatus::Established(_) | FactStatus::Unknown | FactStatus::Inferred(_) => return None,
        FactStatus::Ambiguous(_) => return None,
    };
    let entry_set = entries.iter().cloned().collect::<BTreeSet<_>>();
    if entry_set.len() != entries.len()
        || entries
            .iter()
            .any(|entry| !valid_entry_symbol(entry, &enum_owner))
    {
        return None;
    }

    let mut cursor = node.walk();
    let syntax_entries = node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "when_entry")
        .collect::<Vec<_>>();
    if syntax_entries.is_empty() {
        return None;
    }

    let mut arms = Vec::with_capacity(syntax_entries.len());
    let mut covered = BTreeSet::new();
    let mut default_result = None;
    for (index, entry) in syntax_entries.iter().enumerate() {
        if entry.has_error() {
            return None;
        }
        let (conditions, result) = when_entry_parts(source, *entry)?;
        let result = unwrap_single_expression(result)?;
        if !safe_arm_result(result) {
            return None;
        }
        if conditions.is_empty() {
            // Empty conditions are the grammar shape for `else`. It must be
            // unique and last; otherwise do not reinterpret malformed input.
            if default_result.is_some() || index + 1 != syntax_entries.len() {
                return None;
            }
            default_result = Some(location(source, file, result));
            continue;
        }
        let mut labels = Vec::with_capacity(conditions.len());
        for condition in conditions {
            let label =
                resolve_enum_label(source, condition, file, &enum_owner, &entries, provider)?;
            if !covered.insert(label.clone()) {
                return None;
            }
            labels.push(label);
        }
        arms.push(EnumWhenArm {
            labels,
            result: location(source, file, result),
        });
    }

    if default_result.is_none() && covered != entry_set {
        return None;
    }
    if arms.is_empty() {
        return None;
    }

    Some(EnumWhenLowering {
        location: location(source, file, node),
        subject: location(source, file, subject),
        enum_owner,
        arms,
        default_result,
    })
}

fn location(source: &str, file: &Path, node: Node<'_>) -> SourceLocation {
    SourceLocation {
        file: file.to_path_buf(),
        snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    }
}

fn when_subject(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let wrapper = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "when_subject")?;
    let mut cursor = wrapper.walk();
    let children = wrapper
        .named_children(&mut cursor)
        .filter(|child| !child.kind().contains("comment"))
        .collect::<Vec<_>>();
    match children.as_slice() {
        [subject]
            if !matches!(
                subject.kind(),
                "variable_declaration" | "property_declaration"
            ) =>
        {
            Some(*subject)
        }
        _ => None,
    }
}

fn subject_enum_owner<P: SemanticProvider + ?Sized>(
    source: &str,
    subject: Node<'_>,
    when: Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<SymbolId> {
    let source_type = match subject.kind() {
        "identifier" | "simple_identifier" => {
            let name = subject.utf8_text(source.as_bytes()).ok()?.trim();
            let ty = lexical_parameter_type(source, when, name);
            if let Some(ty) = ty {
                enum_from_type_ref(source, file, &package_name(source), &ty, provider)?
            } else {
                if super::is_proven_local_shadow(subject, name, source) {
                    return None;
                }
                let owner = enclosing_owner_for_subject(source, subject, file, provider)?;
                direct_property_enum_owner(source, subject, &owner, name, provider)?
            }
        }
        "navigation_expression" => {
            let (base, property) = navigation_parts(source, subject)?;
            if base.utf8_text(source.as_bytes()).ok()?.trim() != "this" {
                return None;
            }
            let owner = enclosing_owner_for_subject(source, subject, file, provider)?;
            direct_property_enum_owner(
                source,
                property,
                &owner,
                property.utf8_text(source.as_bytes()).ok()?.trim(),
                provider,
            )?
        }
        "call_expression" => stable_call_enum_owner(source, subject, file, provider)?,
        _ => return None,
    };
    (source_type.kind == "enum").then_some(source_type)
}

fn enum_from_type_ref<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    package: &str,
    ty: &TypeRef,
    provider: &P,
) -> Option<SymbolId> {
    let TypeRef {
        ty: KotlinType::Named { name, arguments },
        nullable: false,
    } = ty
    else {
        return None;
    };
    if name.is_empty()
        || !arguments.is_empty()
        || provider.has_type_alias_in_package(package, name)
        || import_aliases_name(source, name)
    {
        return None;
    }
    match provider.resolve_type_in_file(file, package, name) {
        FactStatus::Established(owner) if owner.kind == "enum" => {
            let owner_symbols = provider
                .symbols_named(&owner.name)
                .into_iter()
                .filter(|symbol| symbol.id == owner)
                .collect::<Vec<_>>();
            let [symbol] = owner_symbols.as_slice() else {
                return None;
            };
            let owner_source = provider.source_for_file(&owner.file)?;
            if symbol.location.snapshot_hash != *blake3::hash(owner_source.as_bytes()).as_bytes() {
                return None;
            }
            matches!(provider.enum_entries(&owner), FactStatus::Established(entries) if !entries.is_empty())
                .then_some(owner)
        }
        FactStatus::Established(_)
        | FactStatus::Unknown
        | FactStatus::Inferred(_)
        | FactStatus::Ambiguous(_) => None,
    }
}

fn direct_property_enum_owner<P: SemanticProvider + ?Sized>(
    source: &str,
    reference: Node<'_>,
    owner: &SymbolId,
    name: &str,
    provider: &P,
) -> Option<SymbolId> {
    let mut owner_path = owner.owner_path.clone();
    owner_path.push(owner.name.clone());
    let matches = provider
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| {
            symbol.id.kind == "property"
                && symbol.id.file == owner.file
                && symbol.id.package == owner.package
                && symbol.id.owner_path == owner_path
                && symbol.id.receiver.is_none()
        })
        .collect::<Vec<_>>();
    let [property] = matches.as_slice() else {
        return None;
    };
    if !has_explicit_property_type(source, reference, property.location.start_byte) {
        return None;
    }
    let FactStatus::Established(ty) = &property.ty else {
        return None;
    };
    let property_source = provider.source_for_file(&property.id.file)?;
    if property.location.snapshot_hash != *blake3::hash(property_source.as_bytes()).as_bytes() {
        return None;
    }
    enum_from_type_ref(
        property_source,
        &property.id.file,
        &property.id.package,
        ty,
        provider,
    )
}

fn has_explicit_property_type(_source: &str, reference: Node<'_>, start_byte: usize) -> bool {
    let mut ancestor = Some(reference);
    let mut owner = None;
    while let Some(node) = ancestor {
        if matches!(node.kind(), "class_declaration" | "object_declaration") {
            owner = Some(node);
            break;
        }
        if matches!(
            node.kind(),
            "lambda_literal" | "function_literal" | "anonymous_function"
        ) {
            return false;
        }
        ancestor = node.parent();
    }
    let Some(owner) = owner else {
        return false;
    };
    let mut stack = vec![owner];
    while let Some(node) = stack.pop() {
        if node.start_byte() == start_byte
            && matches!(node.kind(), "property_declaration" | "class_parameter")
        {
            if node.has_error() {
                return false;
            }
            if node.kind() == "property_declaration" {
                let mut cursor = node.walk();
                let Some(variable) = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "variable_declaration")
                else {
                    return false;
                };
                return variable.child_by_field_name("type").is_some()
                    || super::named_type_child(variable).is_some();
            }
            if !is_property_parameter(node) {
                return false;
            }
            return node.child_by_field_name("type").is_some()
                || super::named_type_child(node).is_some();
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn enclosing_owner_for_subject<P: SemanticProvider + ?Sized>(
    source: &str,
    subject: Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<SymbolId> {
    super::enclosing_dispatch_owner(source, subject, file, provider)
}

fn stable_call_enum_owner<P: SemanticProvider + ?Sized>(
    source: &str,
    call: Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<SymbolId> {
    if super::call_argument_count(call) != 0 {
        return None;
    }
    let callee = super::call_callee_node(call)?;
    let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
    let FactStatus::Established(member) =
        resolve_required_call(source, file, call, callee, name, provider)
    else {
        return None;
    };
    if !member.parameters.is_empty()
        || member.receiver.is_some()
        || member
            .file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
    {
        return None;
    }
    let matches = provider
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| symbol.id == member)
        .collect::<Vec<_>>();
    let [symbol] = matches.as_slice() else {
        return None;
    };
    let producer = provider.source_for_file(&member.file)?;
    if symbol.location.snapshot_hash != *blake3::hash(producer.as_bytes()).as_bytes() {
        return None;
    }
    let is_kotlin = member.file.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("kt") || extension.eq_ignore_ascii_case("kts")
    });
    if member.owner_path.is_empty() {
        if !is_kotlin
            || !has_ordinary_public_indexed_function(
                producer,
                &member.file,
                symbol.location.start_byte,
                &member,
            )
        {
            return None;
        }
    } else if !provider.has_stable_noarg_jvm_signature(&member) {
        return None;
    }
    let FactStatus::Established(ty) = &symbol.ty else {
        return None;
    };
    enum_from_type_ref(producer, &member.file, &member.package, ty, provider)
}

fn has_ordinary_public_indexed_function(
    source: &str,
    file: &Path,
    start_byte: usize,
    member: &SymbolId,
) -> bool {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let Some(tree) = parser.parse(source, None) else {
        return false;
    };
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "function_declaration"
            && node.start_byte() == start_byte
            && symbol_id_for_node(source, node, file) == *member
        {
            return super::public_kotlin_callable(source, node)
                && super::own_type_parameters(node).is_none()
                && !super::has_type_constraints(node)
                && has_explicit_return_type(node, source);
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn navigation_parts<'tree>(source: &str, node: Node<'tree>) -> Option<(Node<'tree>, Node<'tree>)> {
    let mut cursor = node.walk();
    let children = node
        .named_children(&mut cursor)
        .filter(|child| !child.kind().contains("comment"))
        .collect::<Vec<_>>();
    let [base, property] = children.as_slice() else {
        return None;
    };
    if !matches!(property.kind(), "identifier" | "simple_identifier")
        || base.utf8_text(source.as_bytes()).is_err()
    {
        return None;
    }
    Some((*base, *property))
}

fn when_entry_parts<'tree>(
    source: &str,
    entry: Node<'tree>,
) -> Option<(Vec<Node<'tree>>, Node<'tree>)> {
    let text = entry.utf8_text(source.as_bytes()).ok()?.trim();
    let prefix = text.split_once("->")?.0.trim();
    let is_else = prefix == "else";
    let mut cursor = entry.walk();
    let named = entry
        .named_children(&mut cursor)
        .filter(|child| !child.kind().contains("comment"))
        .collect::<Vec<_>>();
    let result = *named.last()?;
    if is_else {
        return (named.len() == 1).then_some((Vec::new(), result));
    }
    if named.len() < 2 {
        return None;
    }
    Some((named[..named.len() - 1].to_vec(), result))
}

fn unwrap_single_expression(mut node: Node<'_>) -> Option<Node<'_>> {
    while node.kind() == "control_structure_body" {
        let mut cursor = node.walk();
        let children = node
            .named_children(&mut cursor)
            .filter(|child| !child.kind().contains("comment"))
            .collect::<Vec<_>>();
        let [only] = children.as_slice() else {
            return None;
        };
        node = *only;
    }
    Some(node)
}

fn safe_arm_result(node: Node<'_>) -> bool {
    if matches!(node.kind(), "throw_expression") {
        return !contains_disallowed_control(node);
    }
    if matches!(
        node.kind(),
        "block"
            | "function_body"
            | "return_expression"
            | "jump_expression"
            | "if_expression"
            | "when_expression"
            | "try_expression"
            | "for_statement"
            | "while_statement"
            | "do_while_statement"
            | "lambda_literal"
            | "anonymous_function"
            | "property_declaration"
            | "local_variable_declaration"
    ) {
        return false;
    }
    !contains_disallowed_control(node)
}

fn contains_disallowed_control(node: Node<'_>) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.id() != node.id()
            && matches!(
                current.kind(),
                "return_expression"
                    | "break_expression"
                    | "continue_expression"
                    | "for_statement"
                    | "while_statement"
                    | "do_while_statement"
                    | "block"
                    | "lambda_literal"
                    | "anonymous_function"
            )
        {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
    false
}

fn value_return_context(source: &str, node: Node<'_>) -> Option<()> {
    let mut ancestor = node.parent();
    let mut return_expression = false;
    let mut function = None;
    while let Some(current) = ancestor {
        match current.kind() {
            "lambda_literal" | "function_literal" | "anonymous_function" | "companion_object"
            | "object_literal" | "anonymous_object" => return None,
            "return_expression" => {
                let text = current.utf8_text(source.as_bytes()).ok()?.trim_start();
                if !text.starts_with("return") || text.starts_with("return@") {
                    return None;
                }
                return_expression = true;
            }
            "function_declaration" => {
                function = Some(current);
                break;
            }
            "class_declaration" | "object_declaration" | "interface_declaration" => return None,
            "function_body" => {
                let text = current.utf8_text(source.as_bytes()).ok()?.trim_start();
                if !text.starts_with('=') && !return_expression {
                    return None;
                }
            }
            _ => {}
        }
        ancestor = current.parent();
    }
    let function = function?;
    if super::callable_receiver(source, function).is_some()
        || function_has_callable_ancestor(function)
    {
        return None;
    }
    if !return_expression && !function_body_is_expression(source, function, node) {
        return None;
    }
    if return_expression && !direct_expression_parent_chain(node, function, true) {
        return None;
    }
    if !return_expression && !direct_expression_parent_chain(node, function, false) {
        return None;
    }
    if !has_explicit_return_type(function, source) {
        return None;
    }
    Some(())
}

fn function_has_callable_ancestor(function: Node<'_>) -> bool {
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
            "source_file" | "class_declaration" | "interface_declaration" | "object_declaration"
        ) {
            return false;
        }
        ancestor = node.parent();
    }
    false
}

fn direct_expression_parent_chain(
    node: Node<'_>,
    function: Node<'_>,
    stop_at_return: bool,
) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if stop_at_return && parent.kind() == "return_expression" {
            return true;
        }
        if parent.kind() == "function_body" {
            return !stop_at_return;
        }
        if parent.kind() == "function_declaration" || parent.id() == function.id() {
            return false;
        }
        if parent.kind() != "parenthesized_expression" {
            return false;
        }
        current = parent.parent();
    }
    false
}

fn function_body_is_expression(source: &str, function: Node<'_>, node: Node<'_>) -> bool {
    let mut current = Some(node);
    while let Some(candidate) = current {
        if candidate.id() == function.id() {
            return false;
        }
        if candidate.kind() == "function_body" {
            return candidate
                .utf8_text(source.as_bytes())
                .is_ok_and(|text| text.trim_start().starts_with('='));
        }
        current = candidate.parent();
    }
    false
}

fn has_explicit_return_type(function: Node<'_>, source: &str) -> bool {
    function.child_by_field_name("return_type").is_some() || {
        let parameters_end = function
            .child_by_field_name("parameters")
            .or_else(|| {
                let mut cursor = function.walk();
                function
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "function_value_parameters")
            })
            .map(|node| node.end_byte());
        parameters_end.is_some_and(|end| {
            let mut cursor = function.walk();
            function.named_children(&mut cursor).any(|child| {
                child.start_byte() > end
                    && (child.kind().contains("type")
                        || child.kind() == "user_type"
                        || child.kind() == "nullable_type")
                    && child.utf8_text(source.as_bytes()).is_ok()
            })
        })
    }
}

fn resolve_enum_label<P: SemanticProvider + ?Sized>(
    source: &str,
    condition: Node<'_>,
    file: &Path,
    enum_owner: &SymbolId,
    entries: &[SymbolId],
    provider: &P,
) -> Option<SymbolId> {
    let condition = unwrap_condition(condition)?;
    let text = condition.utf8_text(source.as_bytes()).ok()?.trim();
    if text.is_empty() || text.contains('`') || text.contains('?') || text.contains('(') {
        return None;
    }
    if matches!(condition.kind(), "identifier" | "simple_identifier") {
        let allowed_import = format!("{}.{}", qualified_enum_name(enum_owner), text);
        if value_name_is_shadowed(
            source,
            condition,
            text,
            &allowed_import,
            true,
            file,
            provider,
        ) {
            return None;
        }
        return imported_enum_entry(source, enum_owner, text, entries);
    }
    if condition.kind() != "navigation_expression" {
        return None;
    }
    let (prefix, entry_node) = navigation_parts(source, condition)?;
    let entry_name = entry_node.utf8_text(source.as_bytes()).ok()?.trim();
    let qualifier = first_identifier(prefix)?;
    let qualifier_name = qualifier.utf8_text(source.as_bytes()).ok()?.trim();
    if !is_java_identifier(entry_name)
        || value_name_is_shadowed(
            source,
            qualifier,
            qualifier_name,
            &qualified_enum_name(enum_owner),
            true,
            file,
            provider,
        )
    {
        return None;
    }
    let prefix_text = prefix.utf8_text(source.as_bytes()).ok()?.trim();
    if !enum_prefix_matches(source, file, prefix_text, enum_owner, provider) {
        return None;
    }
    unique_entry(entries, enum_owner, entry_name)
}

fn unwrap_condition(mut condition: Node<'_>) -> Option<Node<'_>> {
    while matches!(condition.kind(), "when_condition" | "condition") {
        let mut cursor = condition.walk();
        let children = condition
            .named_children(&mut cursor)
            .filter(|child| !child.kind().contains("comment"))
            .collect::<Vec<_>>();
        let [only] = children.as_slice() else {
            return None;
        };
        condition = *only;
    }
    Some(condition)
}

fn enum_prefix_matches<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    prefix: &str,
    enum_owner: &SymbolId,
    provider: &P,
) -> bool {
    if prefix.is_empty() || prefix.contains('`') || import_aliases_name(source, prefix) {
        return false;
    }
    if !prefix.contains('.') {
        if import_shadows_simple_name(
            source,
            prefix,
            &qualified_enum_name(enum_owner),
            true,
            provider,
        ) {
            return false;
        }
        let exact_import = kotlin_import_lines(source).any(|line| {
            let line = line.trim().trim_start_matches("import ").trim();
            !line.contains(" as ") && line == qualified_enum_name(enum_owner)
        });
        return exact_import
            || matches!(
                provider.resolve_type_in_file(file, &package_name(source), prefix),
                FactStatus::Established(found) if found == *enum_owner
            );
    }
    prefix == qualified_enum_name(enum_owner)
}

fn first_identifier<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    if matches!(node.kind(), "identifier" | "simple_identifier") {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = first_identifier(child) {
            return Some(found);
        }
    }
    None
}

fn value_name_is_shadowed<P: SemanticProvider + ?Sized>(
    source: &str,
    reference: Node<'_>,
    name: &str,
    allowed_import: &str,
    reject_all_wildcards: bool,
    file: &Path,
    provider: &P,
) -> bool {
    if super::is_proven_local_shadow(reference, name, source)
        || super::is_proven_constructor_property_shadow(reference, name, source)
    {
        return true;
    }
    let package = package_name(source);
    if provider.symbols_named(name).iter().any(|symbol| {
        symbol.id.kind == "property"
            && symbol.id.package == package
            && symbol.id.owner_path.is_empty()
    }) {
        return true;
    }
    let mut ancestor = reference.parent();
    while let Some(node) = ancestor {
        if matches!(node.kind(), "class_declaration" | "object_declaration") {
            let scoped_owner = symbol_id_for_node(source, node, file);
            let mut path = scoped_owner.owner_path.clone();
            path.push(scoped_owner.name.clone());
            if provider.symbols_named(name).iter().any(|symbol| {
                symbol.id.kind == "property"
                    && symbol.id.file == scoped_owner.file
                    && symbol.id.package == scoped_owner.package
                    && symbol.id.owner_path == path
            }) {
                // This may be an outer receiver in an `inner` class. Rejecting
                // inaccessible outer properties only loses safe candidates.
                return true;
            }
            if !provider.companion_members(&scoped_owner, name).is_empty()
                || (scoped_owner.kind == "enum"
                    && matches!(provider.enum_entries(&scoped_owner), FactStatus::Established(entries)
                        if entries.iter().any(|entry| entry.name == name)))
            {
                return true;
            }
        }
        ancestor = node.parent();
    }
    if let Some(owner) = super::enclosing_dispatch_owner(source, reference, file, provider) {
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        if provider.symbols_named(name).iter().any(|symbol| {
            symbol.id.kind == "property"
                && symbol.id.file == owner.file
                && symbol.id.package == owner.package
                && symbol.id.owner_path == path
        }) {
            return true;
        }
        if !provider.companion_members(&owner, name).is_empty()
            || (owner.kind == "enum"
                && matches!(provider.enum_entries(&owner), FactStatus::Established(entries)
                    if entries.iter().any(|entry| entry.name == name)))
        {
            return true;
        }
        if !provider.has_complete_inheritance(&owner)
            || has_ancestor_property_or_java_owner(provider, &owner, name)
        {
            return true;
        }
    }
    import_shadows_simple_name(source, name, allowed_import, reject_all_wildcards, provider)
}

fn has_ancestor_property_or_java_owner<P: SemanticProvider + ?Sized>(
    provider: &P,
    owner: &SymbolId,
    name: &str,
) -> bool {
    let mut pending = match provider.instance_contract_parents(owner) {
        FactStatus::Established(parents) => parents,
        FactStatus::Unknown | FactStatus::Inferred(_) | FactStatus::Ambiguous(_) => return true,
    };
    let mut seen = BTreeSet::new();
    while let Some(parent) = pending.pop() {
        if !seen.insert(parent.clone()) {
            continue;
        }
        if parent
            .file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
        {
            // Java fields participate in Kotlin expression lookup. Continue
            // only when the source snapshot proves this ancestor has neither
            // a field of the same name nor a bean getter that can expose one.
            if !matches!(
                provider.java_field_named(&parent, name),
                FactStatus::Established(false)
            ) || !provider.java_property_getters(&parent, name).is_empty()
            {
                return true;
            }
            if java_noarg_getter_may_expose_name(provider, &parent, name) {
                return true;
            }
        }
        let mut path = parent.owner_path.clone();
        path.push(parent.name.clone());
        if provider.symbols_named(name).iter().any(|symbol| {
            symbol.id.kind == "property"
                && symbol.id.file == parent.file
                && symbol.id.package == parent.package
                && symbol.id.owner_path == path
        }) {
            return true;
        }
        if !provider.companion_members(&parent, name).is_empty()
            || (parent.kind == "enum"
                && matches!(provider.enum_entries(&parent), FactStatus::Established(entries)
                    if entries.iter().any(|entry| entry.name == name)))
        {
            return true;
        }
        let parents = match provider.instance_contract_parents(&parent) {
            FactStatus::Established(parents) => parents,
            FactStatus::Unknown | FactStatus::Inferred(_) | FactStatus::Ambiguous(_) => {
                return true;
            }
        };
        pending.extend(parents);
    }
    false
}

fn import_shadows_simple_name(
    source: &str,
    name: &str,
    allowed_path: &str,
    reject_all_wildcards: bool,
    provider: &(impl SemanticProvider + ?Sized),
) -> bool {
    kotlin_import_lines(source).any(|import| {
        let import = import.trim();
        let (path, alias) = import
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((import, None));
        if alias.is_some_and(|alias| alias == name) {
            return true;
        }
        if path.ends_with(".*") {
            if !reject_all_wildcards || path == format!("{allowed_path}.*") {
                return false;
            }
            if matches!(path, "java.util.*" | "java.lang.*") {
                return provider.symbols_named(name).iter().any(|symbol| {
                    symbol.id.kind == "property"
                        && symbol.id.package == path.trim_end_matches(".*")
                        && symbol.id.owner_path.is_empty()
                });
            }
            return true;
        }
        path.rsplit('.').next() == Some(name) && path != allowed_path
    })
}

fn java_noarg_getter_may_expose_name<P: SemanticProvider + ?Sized>(
    provider: &P,
    owner: &SymbolId,
    property: &str,
) -> bool {
    if !owner
        .file
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
    {
        return false;
    }
    let mut chars = property.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let capitalized = first.to_uppercase().collect::<String>() + chars.as_str();
    let getter_names = [format!("get{capitalized}"), format!("is{capitalized}")];
    getter_names.iter().any(|getter| {
        provider.symbols_named(getter).iter().any(|symbol| {
            matches!(symbol.id.kind.as_str(), "function" | "method")
                && symbol.id.file == owner.file
                && symbol.id.package == owner.package
                && symbol.id.owner_path == owner_path(owner)
                && symbol.id.parameters.is_empty()
        })
    })
}

fn owner_path(owner: &SymbolId) -> Vec<String> {
    owner
        .owner_path
        .iter()
        .cloned()
        .chain(std::iter::once(owner.name.clone()))
        .collect()
}

fn imported_enum_entry(
    source: &str,
    enum_owner: &SymbolId,
    name: &str,
    entries: &[SymbolId],
) -> Option<SymbolId> {
    if !is_java_identifier(name) || import_aliases_name(source, name) {
        return None;
    }
    let full = qualified_enum_name(enum_owner);
    let imports = kotlin_import_lines(source)
        .map(|line| line.trim().trim_start_matches("import ").trim())
        .collect::<Vec<_>>();
    let exact = format!("{full}.{name}");
    let wildcard = format!("{full}.*");
    if !imports
        .iter()
        .any(|import| *import == exact || *import == wildcard)
    {
        return None;
    }
    unique_entry(entries, enum_owner, name)
}

fn unique_entry(entries: &[SymbolId], owner: &SymbolId, name: &str) -> Option<SymbolId> {
    let matches = entries
        .iter()
        .filter(|entry| {
            entry.name == name
                && entry.kind == "enum_entry"
                && entry.file == owner.file
                && entry.package == owner.package
                && entry.owner_path
                    == owner
                        .owner_path
                        .iter()
                        .cloned()
                        .chain(std::iter::once(owner.name.clone()))
                        .collect::<Vec<_>>()
        })
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [entry] => Some(entry.clone()),
        _ => None,
    }
}

fn valid_entry_symbol(entry: &SymbolId, owner: &SymbolId) -> bool {
    entry.kind == "enum_entry"
        && entry.file == owner.file
        && entry.package == owner.package
        && entry.module == owner.module
        && entry.owner_path
            == owner
                .owner_path
                .iter()
                .cloned()
                .chain(std::iter::once(owner.name.clone()))
                .collect::<Vec<_>>()
        && is_java_identifier(&entry.name)
}

fn qualified_enum_name(owner: &SymbolId) -> String {
    owner
        .package
        .split('.')
        .chain(owner.owner_path.iter().map(String::as_str))
        .chain(std::iter::once(owner.name.as_str()))
        .collect::<Vec<_>>()
        .join(".")
}

fn import_aliases_name(source: &str, name: &str) -> bool {
    kotlin_import_lines(source).any(|import| {
        import
            .split_once(" as ")
            .is_some_and(|(_, alias)| alias.trim() == name)
    })
}

pub(super) fn is_java_identifier(name: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "abstract",
        "assert",
        "boolean",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extends",
        "final",
        "finally",
        "float",
        "for",
        "goto",
        "if",
        "implements",
        "import",
        "instanceof",
        "int",
        "interface",
        "long",
        "native",
        "new",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "static",
        "strictfp",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "try",
        "void",
        "volatile",
        "while",
        "true",
        "false",
        "null",
        "_",
    ];
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic())
        || !chars.all(|character| {
            character == '_' || character == '$' || character.is_ascii_alphanumeric()
        })
    {
        return false;
    }
    !KEYWORDS.contains(&name)
}
