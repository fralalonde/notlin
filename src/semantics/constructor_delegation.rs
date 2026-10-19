//! Snapshot-bound proof of a narrow secondary-to-secondary constructor edge.
//!
//! Only positional, exact-arity `this(...)` calls are planned.  The complete
//! constructor set must have explicit, stable parameter signatures so arity is
//! sufficient to identify one target; argument lowering remains the emitter's
//! existing responsibility.

use super::{
    FactStatus, SemanticProvider, SourceLocation, SymbolId, declaration_has_modifier,
    is_type_symbol, package_name, standard_external_type, symbol_id_for_node,
};
use crate::translation_plan::{PlannedConstructorDelegation, TranslationPlan};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tree_sitter::Node;

#[derive(Clone)]
struct Constructor<'tree> {
    node: Node<'tree>,
    primary: bool,
    parameter_types: Vec<String>,
    symbol: SymbolId,
}

#[derive(Clone, Copy)]
struct Delegation {
    target_is_this: bool,
    argument_count: usize,
}

pub(super) fn plan(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
    plan: &mut TranslationPlan,
) {
    let source_hash = *blake3::hash(source.as_bytes()).as_bytes();
    if plan.source_hash != source_hash || provider.source_for_file(file) != Some(source) {
        return;
    }

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "class_declaration"
            && has_secondary_constructors(node)
            && let Some(owner) = indexed_class_owner(source, node, file, provider, source_hash)
            && let Some(constructors) =
                indexed_class_constructors(source, node, file, provider, source_hash, &owner)
        {
            plan_class_delegations(
                source,
                file,
                provider,
                source_hash,
                &owner,
                &constructors,
                plan,
            );
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
}

fn has_secondary_constructors(class: Node<'_>) -> bool {
    let mut class_cursor = class.walk();
    let Some(body) = class
        .named_children(&mut class_cursor)
        .find(|child| child.kind() == "class_body")
    else {
        return false;
    };
    let mut body_cursor = body.walk();
    body.named_children(&mut body_cursor)
        .any(|child| child.kind() == "secondary_constructor")
}

fn indexed_class_owner<P: SemanticProvider + ?Sized>(
    source: &str,
    node: Node<'_>,
    file: &Path,
    provider: &P,
    source_hash: [u8; 32],
) -> Option<SymbolId> {
    if node.has_error()
        || super::own_type_parameters(node).is_some()
        || super::has_type_constraints(node)
        || declaration_has_modifier(source, node, &["inner", "value", "inline"])
    {
        return None;
    }
    let syntax = symbol_id_for_node(source, node, file);
    let matches = provider
        .symbols_in_file(file)
        .into_iter()
        .filter(|symbol| {
            symbol.id.kind == "class"
                && symbol.id.name == syntax.name
                && symbol.id.package == syntax.package
                && symbol.id.owner_path == syntax.owner_path
                && symbol.location.start_byte == node.start_byte()
                && symbol.location.end_byte == node.end_byte()
                && symbol.location.snapshot_hash == source_hash
        })
        .collect::<Vec<_>>();
    let [symbol] = matches.as_slice() else {
        return None;
    };
    if !matches!(
        provider.is_ordinary_jvm_reference(&symbol.id),
        FactStatus::Established(true)
    ) {
        return None;
    }
    Some(symbol.id.clone())
}

fn indexed_class_constructors<'tree, P: SemanticProvider + ?Sized>(
    source: &str,
    class: Node<'tree>,
    file: &Path,
    provider: &P,
    source_hash: [u8; 32],
    owner: &SymbolId,
) -> Option<Vec<Constructor<'tree>>> {
    let mut direct = class.walk();
    let primary_node = class
        .named_children(&mut direct)
        .find(|child| child.kind() == "primary_constructor")?;
    if primary_node.has_error() {
        return None;
    }
    let mut constructors = vec![make_constructor(
        source,
        primary_node,
        true,
        file,
        provider,
        source_hash,
        owner,
    )?];

    let mut class_cursor = class.walk();
    let body = class
        .named_children(&mut class_cursor)
        .find(|child| child.kind() == "class_body")?;
    let mut body_cursor = body.walk();
    for member in body
        .named_children(&mut body_cursor)
        .filter(|member| member.kind() == "secondary_constructor")
    {
        if member.has_error() {
            return None;
        }
        constructors.push(make_constructor(
            source,
            member,
            false,
            file,
            provider,
            source_hash,
            owner,
        )?);
    }
    (constructors.iter().all(|constructor| {
        constructor
            .parameter_types
            .iter()
            .all(|ty| stable_parameter_type(source, file, ty, provider))
    }))
    .then_some(())?;
    Some(constructors)
}

fn make_constructor<'tree, P: SemanticProvider + ?Sized>(
    source: &str,
    node: Node<'tree>,
    primary: bool,
    file: &Path,
    provider: &P,
    source_hash: [u8; 32],
    owner: &SymbolId,
) -> Option<Constructor<'tree>> {
    if !public_unannotated_constructor(source, node) {
        return None;
    }
    let parameters = parameter_list(source, node, primary)?;
    let mut types = Vec::with_capacity(parameters.len());
    let mut seen = BTreeSet::new();
    for parameter in parameters {
        if parameter.has_error() || parameter_has_default_or_vararg(parameter) {
            return None;
        }
        let name = parameter_name(source, parameter)?;
        if !seen.insert(name.clone()) {
            return None;
        }
        let type_node = parameter
            .child_by_field_name("type")
            .or_else(|| super::named_type_child(parameter))?;
        let spelling = type_node.utf8_text(source.as_bytes()).ok()?.trim();
        if !simple_supported_type_spelling(spelling) {
            return None;
        }
        let _ = name;
        types.push(spelling.to_owned());
    }

    let syntax_id = symbol_id_for_node(source, node, file);
    let matches = provider
        .symbols_in_file(file)
        .into_iter()
        .filter(|symbol| {
            symbol.id.kind == "constructor"
                && symbol.id.name == "<init>"
                && symbol.id.module == owner.module
                && symbol.id.package == owner.package
                && symbol.id.owner_path == owner_path(owner)
                && symbol.location.start_byte == node.start_byte()
                && symbol.location.end_byte == node.end_byte()
                && symbol.location.snapshot_hash == source_hash
                && symbol.id.parameters.len() == types.len()
        })
        .collect::<Vec<_>>();
    let [symbol] = matches.as_slice() else {
        return None;
    };
    if symbol.id.receiver.is_some()
        || syntax_id.name != symbol.id.name
        || syntax_id.parameters != symbol.id.parameters
    {
        return None;
    }
    Some(Constructor {
        node,
        primary,
        parameter_types: types,
        symbol: symbol.id.clone(),
    })
}

fn public_unannotated_constructor(source: &str, node: Node<'_>) -> bool {
    let modifiers = node
        .named_children(&mut node.walk())
        .find(|child| child.kind() == "modifiers");
    let Some(modifiers) = modifiers else {
        return true;
    };
    let text = modifiers.utf8_text(source.as_bytes()).unwrap_or("");
    if ["private", "protected", "internal"]
        .iter()
        .any(|visibility| {
            text.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .any(|word| word == *visibility)
        })
    {
        return false;
    }
    // The current Java constructor emitter does not preserve declaration
    // annotations such as @Throws. Reject annotations on primary and
    // secondary constructors because any secondary chain may end at primary.
    let mut pending = vec![modifiers];
    while let Some(part) = pending.pop() {
        if matches!(part.kind(), "annotation" | "annotation_entry") {
            return false;
        }
        pending.extend(part.named_children(&mut part.walk()));
    }
    true
}

fn owner_path(owner: &SymbolId) -> Vec<String> {
    let mut path = owner.owner_path.clone();
    path.push(owner.name.clone());
    path
}

fn parameter_list<'tree>(
    source: &str,
    node: Node<'tree>,
    primary: bool,
) -> Option<Vec<Node<'tree>>> {
    let expected = if primary {
        "class_parameters"
    } else {
        "function_value_parameters"
    };
    let mut pending = vec![node];
    while let Some(parent) = pending.pop() {
        let children = parent
            .named_children(&mut parent.walk())
            .collect::<Vec<_>>();
        if parent.kind() == expected {
            if contains_vararg_modifier(source, parent) {
                return None;
            }
            let parameter_kind = if primary {
                "class_parameter"
            } else {
                "parameter"
            };
            return Some(
                children
                    .into_iter()
                    .filter(|child| child.kind() == parameter_kind)
                    .collect(),
            );
        }
        pending.extend(children.into_iter().rev());
    }
    None
}

fn contains_vararg_modifier(source: &str, parameters: Node<'_>) -> bool {
    let mut pending = vec![parameters];
    while let Some(node) = pending.pop() {
        if node.kind() == "vararg"
            || node
                .utf8_text(source.as_bytes())
                .is_ok_and(|text| text == "vararg")
        {
            return true;
        }
        pending.extend(node.children(&mut node.walk()));
    }
    false
}

fn parameter_name(source: &str, parameter: Node<'_>) -> Option<String> {
    parameter
        .child_by_field_name("name")
        .or_else(|| {
            parameter
                .named_children(&mut parameter.walk())
                .find(|child| matches!(child.kind(), "identifier" | "simple_identifier"))
        })
        .and_then(|name| name.utf8_text(source.as_bytes()).ok())
        .map(str::to_owned)
}

fn parameter_has_default_or_vararg(parameter: Node<'_>) -> bool {
    let mut pending = vec![parameter];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "vararg" | "spread_parameter") {
            return true;
        }
        if !node.is_named() && matches!(node.kind(), "=" | "vararg") {
            return true;
        }
        pending.extend(node.children(&mut node.walk()));
    }
    false
}

fn simple_supported_type_spelling(spelling: &str) -> bool {
    if spelling.is_empty()
        || spelling.chars().any(|character| {
            matches!(
                character,
                '?' | '<' | '>' | '[' | ']' | '(' | ')' | '*' | '&'
            )
        })
        || spelling.contains("->")
        || spelling.starts_with('@')
    {
        return false;
    }
    let mut parts = spelling.split('.');
    parts.all(|part| {
        !part.is_empty()
            && part
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
            && part
                .chars()
                .next()
                .is_some_and(|character| character.is_alphabetic() || character == '_')
    })
}

fn stable_parameter_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    spelling: &str,
    provider: &P,
) -> bool {
    if provider
        .builtin_jvm_type_descriptor(file, spelling)
        .is_some()
    {
        return true;
    }
    if let Some(canonical) = primitive_type(source, file, spelling, provider) {
        return canonical;
    }
    if standard_external_type(source, provider, spelling, false).is_some() {
        return true;
    }
    if spelling.contains('.') {
        return matches!(
            provider.resolve_type_in_file(file, &package_name(source), spelling),
            FactStatus::Established(owner)
                if matches!(owner.kind.as_str(), "class" | "interface" | "enum" | "object")
                    && matches!(provider.is_ordinary_jvm_reference(&owner), FactStatus::Established(true))
        );
    }
    if provider.has_type_alias_in_package(&package_name(source), spelling)
        || imported_alias_for(source, spelling, provider)
    {
        return false;
    }
    let FactStatus::Established(owner) =
        provider.resolve_type_in_file(file, &package_name(source), spelling)
    else {
        return false;
    };
    if !matches!(
        owner.kind.as_str(),
        "class" | "interface" | "enum" | "object"
    ) || !matches!(
        provider.is_ordinary_jvm_reference(&owner),
        FactStatus::Established(true)
    ) {
        return false;
    }
    // A simple name supplied by an unknown wildcard package cannot be proven
    // unique by the source index. Exact imports and same-package declarations
    // are sufficiently specific for this initial planner.
    let explicit = explicit_imports(source, spelling);
    if !explicit.is_empty() {
        return explicit.len() == 1 && explicit[0] == format!("{}.{}", owner.package, owner.name);
    }
    provider.symbols_named(spelling).iter().any(|symbol| {
        is_type_symbol(&symbol.id)
            && symbol.id.file == owner.file
            && symbol.id.owner_path == owner.owner_path
            && symbol.id.name == owner.name
            && owner.package == package_name(source)
    })
}

fn primitive_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    spelling: &str,
    provider: &P,
) -> Option<bool> {
    let canonical = match spelling {
        "kotlin.Boolean" => "Boolean",
        "kotlin.Byte" => "Byte",
        "kotlin.Short" => "Short",
        "kotlin.Int" => "Int",
        "kotlin.Long" => "Long",
        "kotlin.Float" => "Float",
        "kotlin.Double" => "Double",
        "kotlin.Char" => "Char",
        "Boolean" | "Byte" | "Short" | "Int" | "Long" | "Float" | "Double" | "Char" => spelling,
        _ => return None,
    };
    if spelling.starts_with("kotlin.") {
        return Some(true);
    }
    let package = package_name(source);
    if provider.has_type_alias_in_package(&package, canonical)
        || provider
            .symbols_named(canonical)
            .iter()
            .any(|symbol| is_type_symbol(&symbol.id) && symbol.id.package == package)
    {
        return Some(false);
    }
    let imports = explicit_imports(source, canonical);
    if !imports.is_empty() && (imports.len() != 1 || imports[0] != format!("kotlin.{canonical}")) {
        return Some(false);
    }
    if imported_alias_for(source, canonical, provider) || has_unknown_type_wildcard(source) {
        return Some(false);
    }
    // Keep this path source-scoped even though the spelling is a builtin:
    // declaration aliases and import bindings above can change its meaning.
    let _ = file;
    Some(true)
}

fn explicit_imports(source: &str, simple: &str) -> Vec<String> {
    super::kotlin_import_lines(source)
        .filter_map(|import| {
            let (path, alias) = import
                .split_once(" as ")
                .map(|(path, alias)| (path.trim(), Some(alias.trim())))
                .unwrap_or((import.trim(), None));
            (alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == simple)
                .then(|| path.to_owned())
        })
        .collect()
}

fn imported_alias_for<P: SemanticProvider + ?Sized>(
    source: &str,
    simple: &str,
    provider: &P,
) -> bool {
    let aliases = provider.type_aliases();
    super::kotlin_import_lines(source).any(|import| {
        let (path, alias) = import
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((import.trim(), None));
        let visible = alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path));
        visible == simple
            && path.rsplit_once('.').is_some_and(|(package, name)| {
                aliases.iter().any(|((alias_package, alias_name), _)| {
                    alias_package == package && alias_name == name
                })
            })
    })
}

fn has_unknown_type_wildcard(source: &str) -> bool {
    super::kotlin_import_lines(source)
        .any(|import| import.ends_with(".*") && !matches!(import, "java.util.*" | "java.lang.*"))
}

fn plan_class_delegations<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    provider: &P,
    source_hash: [u8; 32],
    owner: &SymbolId,
    constructors: &[Constructor<'_>],
    plan: &mut TranslationPlan,
) {
    let mut edges = BTreeMap::<usize, Option<usize>>::new();
    let mut delegations = Vec::with_capacity(constructors.len());
    for (index, constructor) in constructors.iter().enumerate() {
        if constructor.primary {
            delegations.push(None);
            continue;
        }
        let delegation = delegation(constructor.node, source);
        let target = delegation.and_then(|delegation| {
            if !delegation.target_is_this {
                return None;
            }
            let matches = constructors
                .iter()
                .enumerate()
                .filter(|(_, candidate)| {
                    candidate.parameter_types.len() == delegation.argument_count
                })
                .map(|(candidate, _)| candidate)
                .collect::<Vec<_>>();
            (matches.len() == 1).then(|| matches[0])
        });
        edges.insert(index, target);
        delegations.push(delegation);
    }

    for (index, constructor) in constructors.iter().enumerate() {
        if constructor.primary {
            continue;
        }
        let Some(Some(target_index)) = edges.get(&index) else {
            continue;
        };
        let target = &constructors[*target_index];
        if target.primary || !acyclic_secondary_chain(*target_index, constructors, &edges) {
            continue;
        }
        let Some(delegation) = delegations[index] else {
            continue;
        };
        if !delegation.target_is_this
            || delegation.argument_count != target.parameter_types.len()
            || !same_indexed_constructor(
                provider,
                &constructor.symbol,
                constructor.node,
                source_hash,
            )
            || !same_indexed_constructor(provider, &target.symbol, target.node, source_hash)
        {
            continue;
        }
        let location = source_location(file, source_hash, constructor.node);
        let target_location = source_location(file, source_hash, target.node);
        plan.constructor_delegations
            .push(PlannedConstructorDelegation {
                symbol_id: constructor.symbol.clone(),
                target_symbol_id: target.symbol.clone(),
                location,
                target: target_location,
                owner: owner.clone(),
                parameter_types: target.parameter_types.clone(),
            });
    }
}

fn acyclic_secondary_chain(
    start: usize,
    constructors: &[Constructor<'_>],
    edges: &BTreeMap<usize, Option<usize>>,
) -> bool {
    let mut current = start;
    let mut seen = BTreeSet::new();
    loop {
        if constructors[current].primary || !seen.insert(current) {
            return constructors[current].primary;
        }
        let Some(Some(target)) = edges.get(&current) else {
            return false;
        };
        current = *target;
    }
}

fn delegation(node: Node<'_>, source: &str) -> Option<Delegation> {
    let delegation = node
        .named_children(&mut node.walk())
        .find(|child| child.kind() == "constructor_delegation_call")?;
    if delegation.has_error() {
        return None;
    }
    let text = delegation.utf8_text(source.as_bytes()).ok()?.trim_start();
    let (target_is_this, prefix_len) = if text.starts_with("this") {
        (true, 4)
    } else if text.starts_with("super") {
        (false, 5)
    } else {
        return None;
    };
    if text[prefix_len..].trim_start().starts_with('@') {
        return None;
    }
    if !text[prefix_len..].trim_start().starts_with('(') {
        return None;
    }
    let arguments = delegation
        .named_children(&mut delegation.walk())
        .find(|child| child.kind() == "value_arguments")?;
    if delegation
        .named_children(&mut delegation.walk())
        .any(|child| child.kind() == "lambda_literal")
    {
        return None;
    }
    let mut count = 0usize;
    for argument in arguments
        .named_children(&mut arguments.walk())
        .filter(|child| child.kind() == "value_argument")
    {
        if argument.has_error()
            || argument
                .children(&mut argument.walk())
                .any(|child| !child.is_named() && matches!(child.kind(), "=" | "*"))
            || argument
                .named_children(&mut argument.walk())
                .filter(|child| !child.is_extra())
                .count()
                != 1
        {
            return None;
        }
        count += 1;
    }
    Some(Delegation {
        target_is_this,
        argument_count: count,
    })
}

fn same_indexed_constructor<P: SemanticProvider + ?Sized>(
    provider: &P,
    id: &SymbolId,
    node: Node<'_>,
    source_hash: [u8; 32],
) -> bool {
    let matches = provider
        .symbols_in_file(&id.file)
        .into_iter()
        .filter(|symbol| {
            symbol.id == *id
                && symbol.location.start_byte == node.start_byte()
                && symbol.location.end_byte == node.end_byte()
                && symbol.location.snapshot_hash == source_hash
        })
        .count();
    matches == 1
}

fn source_location(file: &Path, source_hash: [u8; 32], node: Node<'_>) -> SourceLocation {
    SourceLocation {
        file: file.to_path_buf(),
        snapshot_hash: source_hash,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    }
}
