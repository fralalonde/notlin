//! Exact lowering facts for a public reified class-token forwarding method.
//!
//! This intentionally recognizes one tiny source shape. It is not a general
//! Kotlin reified-call resolver: unsupported syntax remains unresolved.

use super::{
    FactStatus, KotlinType, SemanticProvider, SourceLocation, SymbolId, TypeRef,
    declaration_type_parameters, explicit_receiver_node, is_type_symbol, lexical_parameter_type,
    package_name,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use tree_sitter::Node;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReifiedClassCall {
    pub location: SourceLocation,
    pub receiver: SourceLocation,
    #[serde(default)]
    pub receiver_dependencies: Vec<SymbolId>,
    pub source_target: SymbolId,
    pub java_target: SymbolId,
    pub type_argument: SymbolId,
    /// The wrapper's nullable `T?` result with its single closed type argument
    /// substituted. The caller uses this exact result contract.
    pub return_type: TypeRef,
}

pub(super) fn establish<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    provider: &P,
) -> Option<ReifiedClassCall> {
    if call.kind() != "call_expression"
        || !call
            .named_children(&mut call.walk())
            .any(|child| child.kind() == "type_arguments")
        || super::call_argument_count(call) != 0
        || super::direct_trailing_lambda(call).is_some()
        || provider.source_for_file(file)? != source
    {
        return None;
    }
    let callee = super::call_callee_node(call)?;
    let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
    if !java_identifier(name) {
        return None;
    }
    if !package_is_consistent(source, call) {
        return None;
    }
    // This rule only applies to explicit generic zero-argument calls. Keep the
    // hot path for ordinary calls below the cheap shape check.
    let type_argument = exact_type_argument(call, source, file, provider)?;
    let receiver_node = explicit_receiver_node(call, callee)?;
    let gap = source.get(receiver_node.end_byte()..callee.start_byte())?;
    if gap.contains("?.") || gap.contains("::") || gap.contains("!!") {
        return None;
    }
    let (receiver_type, receiver_dependencies) =
        receiver_classifier(source, file, call, callee, receiver_node, provider)?;
    let receiver_owner = resolve_source_reference(source, file, &receiver_type, provider)?;
    if !matches!(
        provider.is_ordinary_jvm_reference(&type_argument),
        FactStatus::Established(true)
    ) {
        return None;
    }

    let declarations = owner_declaration(&receiver_owner, provider)?;
    if !declarations.owner_type_parameters.is_empty() {
        return None;
    }
    let member_path = member_owner_path(&receiver_owner);
    let members = provider
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| {
            symbol.id.file == receiver_owner.file
                && symbol.id.package == receiver_owner.package
                && symbol.id.owner_path == member_path
                && matches!(symbol.id.kind.as_str(), "function" | "method")
        })
        .collect::<Vec<_>>();
    if members.is_empty()
        || members.iter().any(|member| member.id.receiver.is_some())
        || !provider.has_complete_inheritance(&receiver_owner)
        || !provider.inherited_members(&receiver_owner, name).is_empty()
    {
        return None;
    }

    let mut wrapper_candidates = Vec::new();
    let mut token_candidates = Vec::new();
    let owner_source = provider.source_for_file(&receiver_owner.file)?;
    let owner_tree = super::parse_source(&receiver_owner.file, owner_source)?;
    if !plain_class_projection(owner_source, provider) {
        return None;
    }
    for member in members {
        let source_text = owner_source;
        if member.location.snapshot_hash != *blake3::hash(source_text.as_bytes()).as_bytes() {
            return None;
        }
        let declaration = owner_tree.root_node().named_descendant_for_byte_range(
            member.location.start_byte,
            member.location.end_byte,
        )?;
        if declaration.start_byte() != member.location.start_byte
            || declaration.end_byte() != member.location.end_byte
            || declaration.kind() != "function_declaration"
            || super::node_name(declaration, source_text).as_deref() != Some(name)
            || declaration.has_error()
            || (declaration_has_annotations(source_text, declaration)
                && !allowed_suppress_annotation(source_text, declaration, provider))
            || has_default_or_vararg(source_text, declaration)
        {
            return None;
        }
        let value_parameters = value_parameters(declaration)?;
        let type_parameters = own_type_parameters(declaration)?;
        if value_parameters.is_empty()
            && is_reified_wrapper(
                source_text,
                &member.id.file,
                declaration,
                &type_parameters,
                name,
                provider,
            )
        {
            wrapper_candidates.push((member.id.clone(), declaration, source_text.to_owned()));
        } else if value_parameters.len() == 1
            && is_class_token_target(
                source_text,
                &member.id.file,
                declaration,
                &value_parameters[0],
                &type_parameters,
                name,
                provider,
            )
        {
            token_candidates.push((member.id.clone(), declaration, source_text.to_owned()));
        } else if value_parameters.len() == 1
            && !parameter_is_proven_disjoint_class_token(
                source_text,
                &member.id.file,
                &value_parameters[0],
                provider,
            )
        {
            // An unrecognized one-argument overload may compete with the
            // class-token delegation inside the wrapper.
            return None;
        } else if value_parameters.is_empty() {
            // Another zero-argument member makes the call-site overload set
            // ambiguous or changes the wrapper's name resolution.
            return None;
        }
    }
    let [(source_target, source_declaration, source_text)] = wrapper_candidates.as_slice() else {
        return None;
    };
    let [(java_target, java_declaration, java_text)] = token_candidates.as_slice() else {
        return None;
    };
    if source_target.file != java_target.file
        || source_target.owner_path != java_target.owner_path
        || source_target.name != java_target.name
        || source_target.package != java_target.package
    {
        return None;
    }
    let (_, wrapper_bound) = type_parameter_contract(
        source_text,
        &source_target.file,
        *source_declaration,
        true,
        provider,
    )?;
    let (_, target_bound) = type_parameter_contract(
        java_text,
        &java_target.file,
        *java_declaration,
        false,
        provider,
    )?;
    if wrapper_bound != target_bound || !is_subtype_of(&type_argument, &wrapper_bound, provider) {
        return None;
    }

    Some(ReifiedClassCall {
        location: location(file, source, call),
        receiver: location(file, source, receiver_node),
        receiver_dependencies,
        source_target: source_target.clone(),
        java_target: java_target.clone(),
        type_argument: type_argument.clone(),
        return_type: TypeRef {
            ty: KotlinType::Named {
                name: qualified_source_name(&type_argument),
                arguments: Vec::new(),
            },
            nullable: true,
        },
    })
}

struct OwnerDeclaration {
    owner_type_parameters: Vec<String>,
}

fn owner_declaration<P: SemanticProvider + ?Sized>(
    owner: &SymbolId,
    provider: &P,
) -> Option<OwnerDeclaration> {
    if owner.kind != "class"
        || !owner.owner_path.is_empty()
        || owner
            .file
            .extension()
            .is_none_or(|extension| extension != "kt" && extension != "kts")
    {
        return None;
    }
    let source = provider.source_for_file(&owner.file)?;
    let symbol = unique_symbol(owner, provider)?;
    let tree = super::parse_source(&owner.file, source)?;
    let declaration = tree
        .root_node()
        .named_descendant_for_byte_range(symbol.location.start_byte, symbol.location.end_byte)?;
    if declaration.kind() != "class_declaration"
        || symbol.location.snapshot_hash != *blake3::hash(source.as_bytes()).as_bytes()
        || declaration.start_byte() != symbol.location.start_byte
        || declaration.end_byte() != symbol.location.end_byte
    {
        return None;
    }
    Some(OwnerDeclaration {
        owner_type_parameters: own_type_parameter_names(declaration, source),
    })
}

fn receiver_classifier<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    _callee: Node<'_>,
    receiver: Node<'_>,
    provider: &P,
) -> Option<(String, Vec<SymbolId>)> {
    let receiver_text = receiver.utf8_text(source.as_bytes()).ok()?.trim();
    if receiver_text.contains("?.") || receiver_text.contains("!!") {
        return None;
    }
    if matches!(receiver.kind(), "identifier" | "simple_identifier") {
        let name = receiver_text;
        if let Some(TypeRef {
            ty:
                KotlinType::Named {
                    name: ty_name,
                    arguments,
                },
            nullable: false,
        }) = lexical_parameter_type(source, call, name)
        {
            if !arguments.is_empty() || ty_name.contains('?') {
                return None;
            }
            return Some((ty_name, Vec::new()));
        }
        if super::is_proven_local_shadow(receiver, receiver_text, source)
            || matches!(
                provider.resolve_type_in_file(file, &package_name(source), receiver_text),
                FactStatus::Established(_)
            )
        {
            return None;
        }
        if let Some(access) =
            super::inherited_property_receiver_access(source, receiver, file, provider)
        {
            for symbol in [&access.source_property, &access.getter_contract] {
                let indexed = unique_symbol(symbol, provider)?;
                if indexed.location.snapshot_hash
                    != *blake3::hash(provider.source_for_file(&symbol.file)?.as_bytes()).as_bytes()
                {
                    return None;
                }
            }
            return Some((
                qualified_source_name(&access.value_owner),
                vec![access.source_property, access.getter_contract],
            ));
        }
        let owner = super::enclosing_dispatch_owner(source, receiver, file, provider)?;
        return own_property_receiver(&owner, receiver_text, provider);
    }
    if receiver.kind() != "navigation_expression" {
        return None;
    }
    let parts = named_children(receiver);
    let [base, member] = parts.as_slice() else {
        return None;
    };
    let property = member.utf8_text(source.as_bytes()).ok()?;
    if base.utf8_text(source.as_bytes()).ok()? == "this" {
        let owner = super::enclosing_dispatch_owner(source, receiver, file, provider)?;
        return own_property_receiver(&owner, property, provider);
    }
    if !super::plain_parameter_property_projection(source, call, receiver, file, provider) {
        return None;
    }
    let base_name = base.utf8_text(source.as_bytes()).ok()?;
    let owner = super::lexical_parameter_classifier(source, call, base_name, file, provider)?;
    own_property_receiver(&owner, property, provider)
}

fn own_property_receiver<P: SemanticProvider + ?Sized>(
    owner: &SymbolId,
    name: &str,
    provider: &P,
) -> Option<(String, Vec<SymbolId>)> {
    if !name.is_ascii()
        || name
            .strip_prefix("is")
            .is_some_and(|suffix| suffix.chars().next().is_some_and(char::is_uppercase))
    {
        return None;
    }
    let path = member_owner_path(owner);
    let properties = provider
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| {
            symbol.id.file == owner.file
                && symbol.id.owner_path == path
                && symbol.id.kind == "property"
        })
        .collect::<Vec<_>>();
    let [property] = properties.as_slice() else {
        return None;
    };
    if !provider.has_plain_instance_property_getter(&property.id)
        || property.location.snapshot_hash
            != *blake3::hash(provider.source_for_file(&property.id.file)?.as_bytes()).as_bytes()
    {
        return None;
    }
    Some((
        super::direct_property_classifier(owner, name, provider)?,
        vec![property.id.clone()],
    ))
}

fn exact_type_argument<P: SemanticProvider + ?Sized>(
    call: Node<'_>,
    source: &str,
    file: &Path,
    provider: &P,
) -> Option<SymbolId> {
    let arguments = call.child_by_field_name("type_arguments").or_else(|| {
        named_children(call)
            .into_iter()
            .find(|child| child.kind() == "type_arguments")
    })?;
    let projections = named_children(arguments)
        .into_iter()
        .filter(|child| child.kind() == "type_projection")
        .collect::<Vec<_>>();
    let [projection] = projections.as_slice() else {
        return None;
    };
    let text = projection.utf8_text(source.as_bytes()).ok()?.trim();
    if text.contains('?')
        || text.contains('*')
        || text.contains("in ")
        || text.contains("out ")
        || text.contains('@')
    {
        return None;
    }
    let ty_node = named_children(*projection)
        .into_iter()
        .find(|child| child.kind() != "type_projection_modifiers")?;
    let spelling = ty_node.utf8_text(source.as_bytes()).ok()?.trim();
    if declaration_type_parameters(source, call)
        .iter()
        .any(|parameter| parameter == spelling)
    {
        return None;
    }
    let _ = ty_node;
    resolve_source_reference(source, file, spelling, provider)
}

fn resolve_source_reference<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    spelling: &str,
    provider: &P,
) -> Option<SymbolId> {
    if spelling.is_empty()
        || spelling.contains('<')
        || spelling.contains('>')
        || spelling.contains('?')
        || spelling.contains('@')
        || spelling.contains("->")
        || visible_type_alias(source, file, spelling, provider)
    {
        return None;
    }
    let FactStatus::Established(id) =
        provider.resolve_type_in_file(file, &package_name(source), spelling)
    else {
        return None;
    };
    if !is_type_symbol(&id)
        || id.kind == "typealias"
        || !java_identifier(&id.name)
        || id.owner_path.iter().any(|part| !java_identifier(part))
        || (!id.package.is_empty() && id.package.split('.').any(|part| !java_identifier(part)))
        || (id.package.is_empty() && !package_name(source).is_empty())
        || !matches!(
            provider.is_ordinary_jvm_reference(&id),
            FactStatus::Established(true)
        )
    {
        return None;
    }
    let target_source = provider.source_for_file(&id.file)?;
    let symbol = unique_symbol(&id, provider)?;
    let tree = super::parse_source(&id.file, target_source)?;
    let declaration = tree
        .root_node()
        .named_descendant_for_byte_range(symbol.location.start_byte, symbol.location.end_byte)?;
    if !package_is_consistent(target_source, declaration)
        || !declaration_type_parameters(target_source, declaration).is_empty()
    {
        return None;
    }
    (symbol.location.snapshot_hash == *blake3::hash(target_source.as_bytes()).as_bytes())
        .then_some(id)
}

fn visible_type_alias<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    spelling: &str,
    provider: &P,
) -> bool {
    let simple = spelling.rsplit('.').next().unwrap_or(spelling);
    let package = package_name(source);
    if provider.has_type_alias_in_package(&package, simple) {
        return true;
    }
    for line in source.lines() {
        let Some(import) = line.trim().strip_prefix("import ") else {
            continue;
        };
        let import = import.split(';').next().unwrap_or(import).trim();
        let (path, alias) = import
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((import, None));
        let imported_simple = path.rsplit('.').next().unwrap_or(path);
        let visible = alias.unwrap_or(imported_simple);
        if visible != simple {
            continue;
        }
        if provider
            .type_aliases()
            .iter()
            .any(|((alias_package, alias_name), _)| {
                alias_name == imported_simple
                    && (path == format!("{alias_package}.{imported_simple}") || alias.is_some())
            })
        {
            return true;
        }
    }
    let _ = file;
    false
}

fn is_reified_wrapper<P: SemanticProvider + ?Sized>(
    source: &str,
    _file: &Path,
    declaration: Node<'_>,
    type_parameters: &[Node<'_>],
    name: &str,
    _provider: &P,
) -> bool {
    let Some(parameter) = type_parameters
        .first()
        .and_then(|node| type_parameter_name(*node, source))
    else {
        return false;
    };
    if !has_modifier(source, declaration, "inline")
        || type_parameters.len() != 1
        || !type_parameter_is_reified(source, type_parameters[0])
        || !public_visibility(source, declaration)
        || !exact_type_parameter_return(source, declaration)
        || !has_direct_body(
            source,
            declaration,
            &format!("{name}({parameter}::class.java)"),
        )
    {
        return false;
    }
    true
}

fn is_class_token_target<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    declaration: Node<'_>,
    parameter: &Node<'_>,
    type_parameters: &[Node<'_>],
    _name: &str,
    provider: &P,
) -> bool {
    if type_parameters.len() != 1
        || type_parameter_is_reified(source, type_parameters[0])
        || has_modifier(source, declaration, "inline")
        || !public_visibility(source, declaration)
        || !exact_type_parameter_return(source, declaration)
    {
        return false;
    }
    let Some(type_node) = parameter.child_by_field_name("type").or_else(|| {
        named_children(*parameter)
            .into_iter()
            .find(|node| node.kind().contains("type"))
    }) else {
        return false;
    };
    let text = type_node.utf8_text(source.as_bytes()).unwrap_or("").trim();
    let type_name = type_parameter_name(type_parameters[0], source).unwrap_or_default();
    let compact = remove_whitespace(text);
    compact == format!("java.lang.Class<{type_name}>")
        || (compact == format!("Class<{type_name}>")
            && plain_java_lang_class_import(source, file, provider))
}

fn parameter_is_proven_disjoint_class_token<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    parameter: &Node<'_>,
    provider: &P,
) -> bool {
    let Some(type_node) = parameter.child_by_field_name("type").or_else(|| {
        named_children(*parameter)
            .into_iter()
            .find(|node| node.kind().contains("type"))
    }) else {
        return false;
    };
    let text = type_node.utf8_text(source.as_bytes()).unwrap_or("").trim();
    if text.contains('<') || text.contains('?') || text.contains('@') || text.contains("Class") {
        return false;
    }
    let Some(id) = resolve_source_reference(source, file, text, provider) else {
        return false;
    };
    id.kind == "enum"
}

fn exact_type_parameter_return(source: &str, declaration: Node<'_>) -> bool {
    let Some(parameters) = own_type_parameters(declaration) else {
        return false;
    };
    let [parameter] = parameters.as_slice() else {
        return false;
    };
    let Some(name) = type_parameter_name(*parameter, source) else {
        return false;
    };
    declaration
        .child_by_field_name("type")
        .or_else(|| {
            named_children(declaration)
                .into_iter()
                .find(|node| node.kind() == "nullable_type" || node.kind() == "user_type")
        })
        .and_then(|node| node.utf8_text(source.as_bytes()).ok())
        .is_some_and(|text| remove_whitespace(text) == format!("{name}?"))
}

fn type_parameter_contract<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    declaration: Node<'_>,
    reified: bool,
    provider: &P,
) -> Option<(String, SymbolId)> {
    let parameters = own_type_parameters(declaration)?;
    let [parameter] = parameters.as_slice() else {
        return None;
    };
    if type_parameter_is_reified(source, *parameter) != reified {
        return None;
    }
    let text = parameter.utf8_text(source.as_bytes()).ok()?;
    let (left, bound) = text.split_once(':')?;
    let name = left.split_whitespace().last()?.trim();
    let bound = bound.trim();
    if name.is_empty()
        || bound.is_empty()
        || bound.contains('&')
        || bound.contains(',')
        || bound.contains('@')
    {
        return None;
    }
    let bound = resolve_source_reference(source, file, bound, provider)?;
    Some((name.to_owned(), bound))
}

fn is_subtype_of<P: SemanticProvider + ?Sized>(
    actual: &SymbolId,
    bound: &SymbolId,
    provider: &P,
) -> bool {
    subtype_path(actual, bound, provider).is_some()
}

/// An established inheritance path, including both endpoints. Only complete
/// source-backed edges can authorize an upcast; unrelated unknown branches
/// do not erase a path already proved through another parent.
pub(super) fn subtype_path<P: SemanticProvider + ?Sized>(
    actual: &SymbolId,
    bound: &SymbolId,
    provider: &P,
) -> Option<Vec<SymbolId>> {
    let mut pending = vec![actual.clone()];
    let mut visited = BTreeSet::from([actual.clone()]);
    let mut predecessors = std::collections::BTreeMap::<SymbolId, SymbolId>::new();
    while let Some(current) = pending.pop() {
        if &current == bound {
            let mut path = vec![current];
            while path.last()? != actual {
                path.push(predecessors.get(path.last()?)?.clone());
            }
            path.reverse();
            return Some(path);
        }
        if !provider.has_complete_inheritance(&current) {
            continue;
        }
        let FactStatus::Established(parents) = provider.instance_contract_parents(&current) else {
            continue;
        };
        for parent in parents {
            if visited.insert(parent.clone()) {
                predecessors.insert(parent.clone(), current.clone());
                pending.push(parent);
            }
        }
    }
    None
}

fn value_parameters(declaration: Node<'_>) -> Option<Vec<Node<'_>>> {
    let parameters = declaration.child_by_field_name("parameters").or_else(|| {
        named_children(declaration)
            .into_iter()
            .find(|node| node.kind() == "function_value_parameters")
    })?;
    Some(
        named_children(parameters)
            .into_iter()
            .filter(|node| node.kind() == "parameter")
            .collect(),
    )
}

fn own_type_parameters(declaration: Node<'_>) -> Option<Vec<Node<'_>>> {
    let Some(parameters) = declaration
        .child_by_field_name("type_parameters")
        .or_else(|| {
            named_children(declaration)
                .into_iter()
                .find(|node| node.kind() == "type_parameters")
        })
    else {
        return Some(Vec::new());
    };
    Some(
        named_children(parameters)
            .into_iter()
            .filter(|node| node.kind() == "type_parameter")
            .collect(),
    )
}

fn own_type_parameter_names(declaration: Node<'_>, source: &str) -> Vec<String> {
    own_type_parameters(declaration)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|node| type_parameter_name(node, source))
        .collect()
}

fn type_parameter_name(node: Node<'_>, source: &str) -> Option<String> {
    node.named_children(&mut node.walk())
        .find(|child| child.kind() == "type_identifier" || child.kind() == "identifier")
        .and_then(|child| child.utf8_text(source.as_bytes()).ok())
        .map(str::to_owned)
}

fn type_parameter_is_reified(source: &str, parameter: Node<'_>) -> bool {
    parameter
        .utf8_text(source.as_bytes())
        .is_ok_and(|text| text.split_whitespace().any(|word| word == "reified"))
}

fn has_direct_body(source: &str, declaration: Node<'_>, expected: &str) -> bool {
    let Some(body) = declaration.child_by_field_name("body").or_else(|| {
        named_children(declaration)
            .into_iter()
            .find(|child| child.kind() == "function_body")
    }) else {
        return false;
    };
    let text = body.utf8_text(source.as_bytes()).unwrap_or("").trim();
    let compact = remove_whitespace(text);
    !text.contains("//")
        && !text.contains("/*")
        && compact.strip_prefix('=').unwrap_or(&compact) == expected
}

fn declaration_has_annotations(source: &str, declaration: Node<'_>) -> bool {
    let Some(text) = declaration.utf8_text(source.as_bytes()).ok() else {
        return true;
    };
    text.contains('@')
}

fn allowed_suppress_annotation<P: SemanticProvider + ?Sized>(
    source: &str,
    declaration: Node<'_>,
    provider: &P,
) -> bool {
    let text = declaration.utf8_text(source.as_bytes()).unwrap_or("");
    if !text.contains('@')
        || remove_whitespace(text)
            .replace("@Suppress(\"UNCHECKED_CAST\")", "")
            .contains('@')
    {
        return false;
    }
    if provider
        .symbols_named("Suppress")
        .iter()
        .any(|symbol| symbol.id.package == package_name(source) && is_type_symbol(&symbol.id))
    {
        return false;
    }
    !super::kotlin_import_lines(source).any(|path| {
        path.contains(" as ")
            && path
                .split_once(" as ")
                .is_some_and(|(_, alias)| alias.trim() == "Suppress")
            || (!path.contains(" as ")
                && path.rsplit('.').next() == Some("Suppress")
                && path != "kotlin.Suppress")
            || (path.ends_with(".*") && !super::is_known_jdk_wildcard(path))
    })
}

fn has_default_or_vararg(source: &str, declaration: Node<'_>) -> bool {
    let Some(parameters) = value_parameters(declaration) else {
        return true;
    };
    parameters.into_iter().any(|parameter| {
        parameter
            .utf8_text(source.as_bytes())
            .map_or(true, |text| text.contains('=') || text.contains("vararg"))
    })
}

fn has_modifier(source: &str, declaration: Node<'_>, wanted: &str) -> bool {
    declaration
        .named_children(&mut declaration.walk())
        .find(|node| node.kind() == "modifiers")
        .and_then(|node| node.utf8_text(source.as_bytes()).ok())
        .is_some_and(|text| text.split_whitespace().any(|part| part == wanted))
}

fn public_visibility(source: &str, declaration: Node<'_>) -> bool {
    let modifiers = declaration
        .named_children(&mut declaration.walk())
        .find(|node| node.kind() == "modifiers")
        .and_then(|node| node.utf8_text(source.as_bytes()).ok())
        .unwrap_or("");
    !modifiers.split_whitespace().any(|part| {
        matches!(
            part,
            "private" | "protected" | "internal" | "suspend" | "external" | "expect" | "actual"
        )
    })
}

fn plain_class_projection<P: SemanticProvider + ?Sized>(source: &str, provider: &P) -> bool {
    if provider
        .symbols_named("java")
        .iter()
        .any(|symbol| symbol.id.package == package_name(source))
    {
        return false;
    }
    !super::kotlin_import_lines(source).any(|import| {
        let (path, visible) = import
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), alias.trim()))
            .unwrap_or_else(|| (import, import.rsplit('.').next().unwrap_or(import)));
        (visible == "java" && path != "kotlin.jvm.java")
            || (path.ends_with(".*") && !super::is_known_jdk_wildcard(path))
    })
}

fn plain_java_lang_class_import<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    provider: &P,
) -> bool {
    if visible_type_alias(source, file, "Class", provider)
        || !matches!(
            provider.resolve_type_in_file(file, &package_name(source), "Class"),
            FactStatus::Unknown
        )
    {
        return false;
    }
    let mut explicit = Vec::new();
    for path in super::kotlin_import_lines(source) {
        if let Some((raw, alias)) = path.split_once(" as ") {
            if alias.trim() == "Class" || raw.rsplit('.').next() == Some("Class") {
                return false;
            }
            continue;
        }
        if path.rsplit('.').next() == Some("Class") {
            explicit.push(path);
        } else if path.ends_with(".*") && !super::is_known_jdk_wildcard(path) {
            return false;
        }
    }
    explicit.is_empty() || explicit.as_slice() == ["java.lang.Class"]
}

fn unique_symbol<'a, P: SemanticProvider + ?Sized>(
    id: &SymbolId,
    provider: &'a P,
) -> Option<&'a super::SemanticSymbol> {
    let symbols = provider
        .symbols_named(&id.name)
        .into_iter()
        .filter(|symbol| symbol.id == *id)
        .collect::<Vec<_>>();
    let [symbol] = symbols.as_slice() else {
        return None;
    };
    Some(*symbol)
}

fn member_owner_path(owner: &SymbolId) -> Vec<String> {
    let mut path = owner.owner_path.clone();
    path.push(owner.name.clone());
    path
}

fn qualified_source_name(id: &SymbolId) -> String {
    let mut parts = Vec::new();
    if !id.package.is_empty() {
        parts.push(id.package.clone());
    }
    parts.extend(id.owner_path.clone());
    parts.push(id.name.clone());
    parts.join(".")
}

fn location(file: &Path, source: &str, node: Node<'_>) -> SourceLocation {
    SourceLocation {
        file: file.to_path_buf(),
        snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    }
}

fn java_identifier(name: &str) -> bool {
    super::enum_when::is_java_identifier(name)
}

fn remove_whitespace(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    node.named_children(&mut node.walk()).collect()
}

fn package_is_consistent(source: &str, mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        node = parent;
    }
    let actual = node
        .named_children(&mut node.walk())
        .find(|child| matches!(child.kind(), "package_header" | "package_declaration"))
        .and_then(|header| {
            header.named_children(&mut header.walk()).find(|child| {
                matches!(
                    child.kind(),
                    "qualified_identifier" | "scoped_identifier" | "identifier"
                )
            })
        })
        .and_then(|name| name.utf8_text(source.as_bytes()).ok())
        .unwrap_or("");
    actual == package_name(source)
}
