//! Canonical JDK Stream and Optional member-call facts.

use super::{SemanticProvider, SourceLocation, call_argument_count};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tree_sitter::Node;

/// A snapshot-bound call to a canonical JDK container member.
///
/// Classifier facts can continue fluent chains independently of the bounded
/// source-proven payload and scoped lambda facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerMemberCall {
    pub location: SourceLocation,
    pub receiver: SourceLocation,
    pub receiver_type: String,
    pub return_type: Option<String>,
    /// Preserve Kotlin's platform-result check at an explicit non-null boundary.
    #[serde(default)]
    pub result_check: Option<String>,
    #[serde(default)]
    pub expected_type: Option<ContainerResultType>,
    /// A non-null String fallback independently proves Optional.orElse's
    /// result even when the surrounding argument contract is unavailable.
    #[serde(default)]
    pub nonnull_fallback: Option<SourceLocation>,
    #[serde(default)]
    pub receiver_payload: Option<super::TypeRef>,
    #[serde(default)]
    pub result_payload: Option<super::TypeRef>,
    #[serde(default)]
    pub lambda_binding: Option<super::ContainerLambdaBinding>,
    #[serde(default)]
    pub dependencies: Vec<super::SymbolId>,
    #[serde(default)]
    pub owner: Option<super::SymbolId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerResultType {
    pub ty: super::TypeRef,
    pub location: SourceLocation,
}

pub(super) fn establish<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    provider: &P,
) -> Option<ContainerMemberCall> {
    if !matches!(call.kind(), "call_expression" | "method_invocation") {
        return None;
    }
    let callee = super::call_callee_node(call)?;
    let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
    if !supported_member_name(name)
        || provider.source_for_file(file)? != source
        || !plain_call_arguments(call, callee)
    {
        return None;
    }
    let receiver = super::explicit_receiver_node(call, callee)?;
    if receiver.kind() == "super_expression"
        || super::explicit_receiver_is_type_qualifier(source, call, callee, file, provider)
    {
        return None;
    }
    let navigation_gap = source.get(receiver.end_byte()..callee.start_byte())?;
    if navigation_gap.contains("?.") || navigation_gap.contains("::") {
        return None;
    }
    let typed = super::container_types::establish(source, file, call, provider);
    let receiver_type = typed
        .as_ref()
        .map(|types| types.receiver.classifier.clone())
        .or_else(|| super::explicit_receiver_type(source, call, callee, file, provider))?;
    let receiver_type = super::standard_external_type(
        source,
        provider,
        &receiver_type,
        call.kind() == "method_invocation",
    )?;
    let arity = call_argument_count(call);
    let return_type = match (receiver_type.as_str(), name, arity) {
        (
            "java.util.List"
            | "java.util.Set"
            | "java.util.Collection"
            | "kotlin.collections.List"
            | "kotlin.collections.MutableList"
            | "kotlin.collections.Set"
            | "kotlin.collections.MutableSet"
            | "kotlin.collections.Collection"
            | "kotlin.collections.MutableCollection",
            "stream",
            0,
        ) => Some("java.util.stream.Stream"),
        ("java.util.stream.Stream", "filter" | "map" | "flatMap", 1) => {
            Some("java.util.stream.Stream")
        }
        ("java.util.stream.Stream", "findAny" | "findFirst", 0) => Some("java.util.Optional"),
        ("java.util.Optional", "map" | "filter" | "flatMap" | "or", 1) => {
            Some("java.util.Optional")
        }
        ("java.util.Optional", "orElseThrow" | "get" | "isPresent" | "isEmpty", 0) => None,
        ("java.util.Optional", "orElse" | "orElseGet", 1) => None,
        // The one-argument Optional.orElseThrow overload is generic over the
        // thrown exception and may impose a checked-throws requirement.
        // Keep it outside this first bounded fact set.
        _ => return None,
    };
    if typed.as_ref().is_none_or(|types| types.lambda.is_none())
        && matches!(name, "map" | "filter" | "flatMap")
        && super::container_types::lambda_requires_binding(source, call)
    {
        return None;
    }
    let nonnull_fallback = typed.as_ref().and_then(|types| {
        if receiver_type != "java.util.Optional"
            || name != "orElse"
            || !matches!(&types.receiver.element.ty,
                super::KotlinType::Named { name, arguments }
                if name == "java.lang.String" && arguments.is_empty())
        {
            return None;
        }
        let arguments = call
            .named_children(&mut call.walk())
            .find(|node| node.kind() == "value_arguments")?;
        let argument = arguments
            .named_children(&mut arguments.walk())
            .find(|node| node.kind() == "value_argument")?;
        let literal = argument.named_child(0)?;
        if literal.kind() != "string_literal" {
            return None;
        }
        Some(SourceLocation {
            file: file.to_path_buf(),
            snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
            start_byte: literal.start_byte(),
            end_byte: literal.end_byte(),
        })
    });
    let expected_type = match explicit_result_type(source, file, call, provider) {
        Ok(expected) => expected,
        Err(()) if nonnull_fallback.is_some() => None,
        Err(()) => return None,
    };
    let result_check = (nonnull_fallback.is_none()
        && !matches!(name, "isPresent" | "isEmpty")
        && expected_type.as_ref().is_some_and(|expected| {
            !expected.ty.nullable
                && matches!(
                    expected.ty.ty,
                    super::KotlinType::Primitive(_)
                        | super::KotlinType::Named { .. }
                        | super::KotlinType::Array { .. }
                        | super::KotlinType::Function { .. }
                )
        }))
    .then(|| format!("{name}(...) must not be null"));
    if (result_check.is_some() || (return_type.is_some() && expected_type.is_some()))
        && !super::object_calls::java_qualifier_available(source, call, provider)
    {
        return None;
    }
    Some(ContainerMemberCall {
        location: SourceLocation {
            file: file.to_path_buf(),
            snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
            start_byte: call.start_byte(),
            end_byte: call.end_byte(),
        },
        receiver: SourceLocation {
            file: file.to_path_buf(),
            snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
            start_byte: receiver.start_byte(),
            end_byte: receiver.end_byte(),
        },
        receiver_type,
        return_type: return_type.map(str::to_owned),
        result_check,
        expected_type,
        nonnull_fallback,
        receiver_payload: typed.as_ref().map(|types| types.receiver.element.clone()),
        result_payload: typed
            .as_ref()
            .and_then(|types| types.result_element.clone()),
        lambda_binding: typed.as_ref().and_then(|types| types.lambda.clone()),
        dependencies: typed.map(|types| types.dependencies).unwrap_or_default(),
        owner: None,
    })
}

/// Rejection-only prescreen. Receiver, arity, and payload evidence below still
/// decide eligibility; unrelated calls need no container resolution or hashing.
pub(super) fn supported_member_name(name: &str) -> bool {
    matches!(
        name,
        "stream"
            | "filter"
            | "map"
            | "flatMap"
            | "findAny"
            | "findFirst"
            | "or"
            | "orElseThrow"
            | "get"
            | "isPresent"
            | "isEmpty"
            | "orElse"
            | "orElseGet"
    )
}

pub(super) fn explicit_result_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    mut expression: Node<'_>,
    provider: &P,
) -> Result<Option<ContainerResultType>, ()> {
    while let Some(parent) = expression.parent() {
        match parent.kind() {
            "parenthesized_expression" | "when_expression" => expression = parent,
            "when_entry" => {
                if parent
                    .named_children(&mut parent.walk())
                    .filter(|node| !node.is_extra())
                    .last()
                    .is_none_or(|body| body.id() != expression.id())
                {
                    return Ok(None);
                }
                expression = parent;
            }
            "if_expression" => {
                if parent
                    .child_by_field_name("condition")
                    .is_some_and(|condition| condition.id() == expression.id())
                {
                    return Ok(None);
                }
                expression = parent;
            }
            "block" => {
                if parent
                    .named_children(&mut parent.walk())
                    .filter(|node| !node.is_extra())
                    .last()
                    .is_none_or(|last| last.id() != expression.id())
                    || parent
                        .parent()
                        .is_none_or(|owner| !matches!(owner.kind(), "if_expression" | "when_entry"))
                {
                    return Ok(None);
                }
                expression = parent;
            }
            "return_expression" => {
                if parent
                    .utf8_text(source.as_bytes())
                    .is_ok_and(|text| text.starts_with("return@"))
                {
                    return Ok(None);
                }
                let mut owner = parent.parent();
                while let Some(node) = owner {
                    if matches!(node.kind(), "lambda_literal" | "anonymous_function") {
                        return Ok(None);
                    }
                    if matches!(node.kind(), "function_declaration" | "property_declaration") {
                        return expected_type(source, file, node, provider);
                    }
                    owner = node.parent();
                }
                return Ok(None);
            }
            "function_body" => {
                if !parent
                    .children(&mut parent.walk())
                    .any(|child| child.kind() == "=")
                {
                    return Ok(None);
                }
                let Some(owner) = parent.parent() else {
                    return Ok(None);
                };
                if owner.kind() == "getter" {
                    return match owner.parent() {
                        Some(property) => expected_type(source, file, property, provider),
                        None => Ok(None),
                    };
                }
                return expected_type(source, file, owner, provider);
            }
            "property_declaration" => return expected_type(source, file, parent, provider),
            "value_argument" => return argument_result_type(source, file, parent, provider),
            _ => return Ok(None),
        }
    }
    Ok(None)
}

fn expected_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    declaration: Node<'_>,
    provider: &P,
) -> Result<Option<ContainerResultType>, ()> {
    let Some(ty) = super::declaration_type_node(declaration) else {
        return Ok(None);
    };
    expected_type_node(source, file, ty, declaration, provider)
}

fn expected_type_node<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    ty: Node<'_>,
    declaration: Node<'_>,
    provider: &P,
) -> Result<Option<ContainerResultType>, ()> {
    let Ok(spelling) = ty.utf8_text(source.as_bytes()) else {
        return Err(());
    };
    let parsed = super::TypeRef::parse_with_type_parameters(
        spelling,
        &super::declaration_type_parameters(source, declaration),
    );
    if matches!(parsed.ty, super::KotlinType::Primitive(_)) {
        let name = spelling.trim().trim_end_matches('?');
        if provider.has_type_alias_in_package(&super::package_name(source), name)
            || !matches!(
                provider.resolve_type_in_file(file, &super::package_name(source), name),
                super::FactStatus::Unknown
            )
            || super::kotlin_import_lines(source).any(|import| {
                let (path, alias) = import.split_once(" as ").unwrap_or((import, ""));
                let visible = if alias.is_empty() {
                    path.rsplit('.').next().unwrap_or("")
                } else {
                    alias.trim()
                };
                (path.ends_with(".*") && !super::is_known_jdk_wildcard(path))
                    || (visible == name && path != format!("kotlin.{name}"))
            })
        {
            return Err(());
        }
    }
    match &parsed.ty {
        super::KotlinType::Named { name, .. } => {
            if provider.has_type_alias_in_package(&super::package_name(source), name) {
                return Err(());
            }
            let indexed = provider.resolve_type_in_file(file, &super::package_name(source), name);
            if !matches!(indexed, super::FactStatus::Established(ref symbol) if super::is_type_symbol(symbol) && symbol.kind != "typealias")
                && super::standard_external_type(source, provider, name, false).is_none()
            {
                return Err(());
            }
        }
        super::KotlinType::TypeParameter(_) | super::KotlinType::Unknown(_) => return Err(()),
        _ => {}
    }
    Ok(Some(ContainerResultType {
        ty: parsed,
        location: SourceLocation {
            file: file.to_path_buf(),
            snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
            start_byte: ty.start_byte(),
            end_byte: ty.end_byte(),
        },
    }))
}

fn argument_result_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    argument: Node<'_>,
    provider: &P,
) -> Result<Option<ContainerResultType>, ()> {
    if argument
        .children(&mut argument.walk())
        .any(|node| node.kind() == "=")
    {
        return Err(());
    }
    let arguments = argument.parent().ok_or(())?;
    let position = arguments
        .named_children(&mut arguments.walk())
        .filter(|node| node.kind() == "value_argument")
        .position(|node| node.id() == argument.id())
        .ok_or(())?;
    let call = arguments
        .parent()
        .filter(|node| node.kind() == "call_expression")
        .ok_or(())?;
    let callee = super::call_callee_node(call).ok_or(())?;
    if !plain_call_arguments(call, callee) {
        return Err(());
    }
    let name = callee.utf8_text(source.as_bytes()).map_err(|_| ())?;
    let super::FactStatus::Established(target) =
        super::resolve_required_call(source, file, call, callee, name, provider)
    else {
        return Err(());
    };
    // Java parameter enhancement and external Kotlin contracts require the
    // future compiler provider. Do not infer their null checks from erasure.
    if !target
        .file
        .extension()
        .is_some_and(|extension| extension == "kt" || extension == "kts")
        || target.receiver.is_some()
    {
        return Err(());
    }
    let target_source = provider.source_for_file(&target.file).ok_or(())?;
    let symbols = provider
        .symbols_named(&target.name)
        .into_iter()
        .filter(|symbol| symbol.id == target)
        .collect::<Vec<_>>();
    let [symbol] = symbols.as_slice() else {
        return Err(());
    };
    if symbol.location.snapshot_hash != *blake3::hash(target_source.as_bytes()).as_bytes() {
        return Err(());
    }
    let tree = super::parse_source(&target.file, target_source).ok_or(())?;
    let declaration = tree
        .root_node()
        .named_descendant_for_byte_range(symbol.location.start_byte, symbol.location.end_byte)
        .ok_or(())?;
    let parameter = if declaration.kind() == "function_declaration" {
        let parameters = declaration
            .named_children(&mut declaration.walk())
            .find(|node| node.kind() == "function_value_parameters")
            .ok_or(())?;
        let parameter = parameters
            .named_children(&mut parameters.walk())
            .filter(|node| node.kind() == "parameter")
            .nth(position)
            .ok_or(())?;
        if super::parameter_signature(target_source, parameter).starts_with("vararg:") {
            return Err(());
        }
        parameter
    } else if declaration.kind() == "class_declaration" && symbol.id.kind == "class" {
        if super::own_type_parameters(declaration).is_some()
            || super::declaration_has_modifier(
                target_source,
                declaration,
                &["inner", "value", "inline"],
            )
        {
            return Err(());
        }
        selected_constructor_parameter(
            target_source,
            declaration,
            call_argument_count(call),
            position,
        )?
    } else {
        return Err(());
    };
    let ty = super::named_type_child(parameter).ok_or(())?;
    let expected = expected_type_node(target_source, &target.file, ty, declaration, provider)?;
    if expected.as_ref().is_some_and(|expected| matches!(&expected.ty.ty, super::KotlinType::Named { name, .. } if matches!(name.rsplit('.').next(), Some("Optional" | "Stream")))) { return Err(()); }
    Ok(expected)
}

/// Resolve only an explicit, nongeneric Kotlin constructor parameter whose
/// positional arity uniquely identifies one constructor declaration. This
/// intentionally declines defaulted and vararg overload sets: the workspace
/// resolver identifies the class, not the selected constructor.
fn selected_constructor_parameter<'tree>(
    source: &str,
    class: Node<'tree>,
    argument_count: usize,
    position: usize,
) -> Result<Node<'tree>, ()> {
    let mut candidates = Vec::<Vec<Node<'tree>>>::new();

    let primary = class
        .child_by_field_name("primary_constructor")
        .or_else(|| {
            class
                .named_children(&mut class.walk())
                .find(|child| child.kind() == "primary_constructor")
        })
        .unwrap_or(class);
    if let Some(parameters) = primary
        .named_children(&mut primary.walk())
        .find(|child| child.kind() == "class_parameters")
    {
        candidates.push(
            parameters
                .named_children(&mut parameters.walk())
                .filter(|child| child.kind() == "class_parameter")
                .collect(),
        );
    }

    let body = class.child_by_field_name("body").or_else(|| {
        class
            .named_children(&mut class.walk())
            .find(|child| child.kind() == "class_body")
    });
    if let Some(body) = body {
        // `Value(...)` can select a companion's invoke member or extension.
        // A class identity and constructor arity do not prove that binding.
        if body
            .named_children(&mut body.walk())
            .any(|child| child.kind() == "companion_object")
        {
            return Err(());
        }
        for constructor in body
            .named_children(&mut body.walk())
            .filter(|child| child.kind() == "secondary_constructor")
        {
            let parameters = constructor
                .child_by_field_name("parameters")
                .or_else(|| {
                    constructor
                        .named_children(&mut constructor.walk())
                        .find(|child| child.kind() == "function_value_parameters")
                })
                .ok_or(())?;
            candidates.push(
                parameters
                    .named_children(&mut parameters.walk())
                    .filter(|child| child.kind() == "parameter")
                    .collect(),
            );
        }
    }

    let mut selected = None;
    for parameters in candidates {
        if parameters.iter().any(|parameter| {
            super::parameter_signature(source, *parameter).starts_with("vararg:")
                || parameter
                    .children(&mut parameter.walk())
                    .any(|child| child.kind() == "=")
        }) {
            return Err(());
        }
        if parameters.len() == argument_count {
            if selected.is_some() {
                return Err(());
            }
            selected = parameters.get(position).copied();
        }
    }
    selected.ok_or(())
}

pub(super) fn plain_call_arguments(call: Node<'_>, callee: Node<'_>) -> bool {
    // Explicit generic method type arguments have overload/exception semantics
    // that this classifier-only fact does not model.
    if callee.parent().is_some_and(|parent| {
        parent
            .named_children(&mut parent.walk())
            .any(|child| child.kind() == "type_arguments")
    }) {
        return false;
    }
    let arguments = call
        .child_by_field_name("arguments")
        .or_else(|| call.child_by_field_name("value_arguments"))
        .or_else(|| {
            call.named_children(&mut call.walk()).find(|child| {
                matches!(
                    child.kind(),
                    "arguments" | "argument_list" | "value_arguments" | "value_argument_list"
                )
            })
        })
        .or_else(|| {
            // The Kotlin grammar nests the parenthesized call under an outer
            // call when a trailing lambda follows it.
            call.named_children(&mut call.walk())
                .find(|child| child.kind() == "call_expression")
                .and_then(|inner| {
                    inner
                        .child_by_field_name("arguments")
                        .or_else(|| inner.child_by_field_name("value_arguments"))
                        .or_else(|| {
                            inner.named_children(&mut inner.walk()).find(|child| {
                                matches!(
                                    child.kind(),
                                    "arguments"
                                        | "argument_list"
                                        | "value_arguments"
                                        | "value_argument_list"
                                )
                            })
                        })
                })
        });
    if let Some(arguments) = arguments {
        let mut cursor = arguments.walk();
        for argument in arguments
            .named_children(&mut cursor)
            .filter(|child| matches!(child.kind(), "value_argument" | "argument"))
        {
            if argument
                .children(&mut argument.walk())
                .any(|child| matches!(child.kind(), "=" | "spread_expression" | "*"))
            {
                return false;
            }
        }
    }
    true
}
