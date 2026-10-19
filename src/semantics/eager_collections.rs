//! Exact call facts for the eager Kotlin collection `map` and `filterNot`
//! extensions. The facts are intentionally narrower than the Java lowering:
//! they are emitted only when callable binding and lambda value shape are
//! supported by this provider snapshot.

use super::{
    FactStatus, KotlinType, SemanticProvider, SourceLocation, SymbolId, TypeRef, Variance,
    call_argument_count, call_callee_node, explicit_receiver_node, explicit_receiver_type,
    is_proven_constructor_property_shadow, is_proven_local_shadow, package_name,
    standard_external_type,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use tree_sitter::Node;

pub(super) struct PlanningContext<'a, P: SemanticProvider + ?Sized> {
    pub source: &'a str,
    pub file: &'a Path,
    pub root: Node<'a>,
    pub imports: &'a [String],
    pub extension_symbols: &'a [SymbolId],
    pub snapshot_hash: [u8; 32],
    pub provider: &'a P,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EagerCollectionOperation {
    Map,
    FilterNot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EagerCollectionCall {
    pub location: SourceLocation,
    pub receiver: SourceLocation,
    pub lambda: SourceLocation,
    pub body: SourceLocation,
    pub parameter_name: String,
    pub receiver_type: String,
    pub element_type: Option<TypeRef>,
    #[serde(default)]
    pub property_projection: Option<EagerPropertyProjection>,
    pub operation: EagerCollectionOperation,
    pub owner: SymbolId,
}

/// A direct property read whose source identity, getter ABI and result type
/// were established before emission. The Java parameter spelling is allocated
/// with the other helper names, without changing the source binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EagerPropertyProjection {
    /// Kotlin property identity, or the exact Java synthetic-property method.
    pub target: SymbolId,
    /// Explicit method proven equivalent to reading the property, if its
    /// ordinary JVM getter is unavailable. Keep both dependency identities.
    #[serde(default)]
    pub getter_target: Option<SymbolId>,
    #[serde(default)]
    pub java_property_read: Option<super::JavaPropertyRead>,
    /// Snapshot-scoped type and compatibility evidence used by this projection.
    #[serde(default)]
    pub dependencies: Vec<SymbolId>,
    pub getter_name: String,
    pub result_type: TypeRef,
    pub java_parameter_name: String,
}

pub(super) fn establish<P: SemanticProvider + ?Sized>(
    context: &PlanningContext<'_, P>,
    call: Node<'_>,
    previous: &[EagerCollectionCall],
    owner: SymbolId,
) -> Option<EagerCollectionCall> {
    let source = context.source;
    let file = context.file;
    let root = context.root;
    let imports = context.imports;
    let extension_symbols = context.extension_symbols;
    let snapshot_hash = context.snapshot_hash;
    let provider = context.provider;
    if call.kind() != "call_expression" {
        return None;
    }
    let callee = call_callee_node(call)?;
    let spelling = callee.utf8_text(source.as_bytes()).ok()?.trim();
    let operation = match spelling {
        "map" => EagerCollectionOperation::Map,
        "filterNot" => EagerCollectionOperation::FilterNot,
        _ => return None,
    };
    let navigation = callee.parent()?;
    if navigation.kind() != "navigation_expression"
        || call_argument_count(call) != 1
        || contains_type_arguments(callee)
        || is_proven_local_shadow(callee, spelling, source)
        || is_proven_constructor_property_shadow(callee, spelling, source)
        || has_local_extension_shadow(call, spelling, source)
        || has_indexed_extension_shadow(call, spelling, source, file, extension_symbols)
        || !stdlib_import_is_proven(imports, spelling)
    {
        return None;
    }
    let receiver = explicit_receiver_node(call, callee)?;
    let navigation_gap = source.get(receiver.end_byte()..callee.start_byte())?;
    if navigation_gap.contains("?.") || navigation_gap.contains("::") {
        return None;
    }
    let (receiver_type, element_type) = if let Some(inner) = previous.iter().find(|fact| {
        fact.location.file == file
            && fact.location.end_byte == receiver.end_byte()
            && fact.location.start_byte == receiver.start_byte()
            && fact.location.snapshot_hash == snapshot_hash
            && fact.owner == owner
    }) {
        (
            "kotlin.collections.List".to_owned(),
            Some(result_element_type(source, root, inner, file, provider)?),
        )
    } else {
        let spelling = explicit_receiver_type(source, call, callee, file, provider)?;
        let parsed = TypeRef::parse(&spelling);
        if parsed.nullable {
            return None;
        }
        let canonical = standard_external_type(source, provider, &spelling, false)?;
        if !is_supported_receiver(&canonical) {
            return None;
        }
        let element_type = collection_element_type(source, call, file, provider, &spelling)?;
        (canonical, Some(element_type))
    };
    let (lambda, body, parameter_name) = supported_lambda_shape(source, call)?;
    let mut property_projection = (operation == EagerCollectionOperation::Map)
        .then(|| {
            establish_property_projection(
                source,
                body,
                &parameter_name,
                element_type.as_ref()?,
                file,
                provider,
            )
        })
        .flatten();
    if let Some(projection) = &mut property_projection
        && !projection_result_context_matches(source, call, file, provider, projection)
    {
        return None;
    }
    if (property_projection.is_none()
        && !body_returns_value(source, body, &parameter_name, file, provider, operation))
        || captures_modified_local(source, call, lambda, &parameter_name)
    {
        return None;
    }
    if element_type.as_ref().is_some_and(nullable_floating_point)
        && contains_structural_equality(body)
    {
        // Kotlin uses IEEE equality for statically typed nullable floating
        // values. The transitional renderer's Objects.equals differs for
        // signed zero and NaN; it cannot authorize this exact lowering.
        return None;
    }
    if element_type.as_ref().is_some_and(primitive_type) && contains_referential_equality(body) {
        // JVM SAMs use boxed parameters. Kotlin's numeric identity rules need
        // a separate boxing plan; Java == may silently unbox a literal peer.
        return None;
    }
    Some(EagerCollectionCall {
        location: location(file, snapshot_hash, call),
        receiver: location(file, snapshot_hash, receiver),
        lambda: location(file, snapshot_hash, lambda),
        body: location(file, snapshot_hash, body),
        parameter_name,
        receiver_type,
        element_type,
        property_projection,
        operation,
        owner,
    })
}

pub(super) fn nullable_floating_point(ty: &TypeRef) -> bool {
    let base = match &ty.ty {
        KotlinType::Nullable(inner) => inner.as_ref(),
        base if ty.nullable => base,
        _ => return false,
    };
    matches!(base, KotlinType::Primitive(name) if matches!(name.as_str(), "Float" | "Double"))
        || matches!(base, KotlinType::Named { name, arguments }
            if arguments.is_empty() && matches!(name.as_str(), "kotlin.Float" | "kotlin.Double"))
}

pub(super) fn contains_structural_equality(body: Node<'_>) -> bool {
    contains_operator(body, &["==", "!="])
}

pub(super) fn contains_referential_equality(body: Node<'_>) -> bool {
    contains_operator(body, &["===", "!=="])
}

pub(super) fn primitive_type(ty: &TypeRef) -> bool {
    let base = match &ty.ty {
        KotlinType::Nullable(inner) => inner.as_ref(),
        base => base,
    };
    matches!(base, KotlinType::Primitive(_))
        || matches!(base, KotlinType::Named { name, arguments } if arguments.is_empty() && matches!(name.as_str(), "kotlin.Boolean" | "kotlin.Byte" | "kotlin.Short" | "kotlin.Int" | "kotlin.Long" | "kotlin.Char" | "kotlin.Float" | "kotlin.Double"))
}

fn contains_operator(body: Node<'_>, operators: &[&str]) -> bool {
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if operators.contains(&node.kind()) {
            return true;
        }
        stack.extend(node.children(&mut node.walk()));
    }
    false
}

fn location(file: &Path, snapshot_hash: [u8; 32], node: Node<'_>) -> SourceLocation {
    SourceLocation {
        file: file.to_path_buf(),
        snapshot_hash,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    }
}

fn is_supported_receiver(ty: &str) -> bool {
    matches!(
        ty,
        "kotlin.collections.List"
            | "kotlin.collections.MutableList"
            | "kotlin.collections.Collection"
            | "kotlin.collections.MutableCollection"
            | "kotlin.collections.Set"
            | "kotlin.collections.MutableSet"
            | "kotlin.collections.Iterable"
            | "kotlin.collections.MutableIterable"
            | "java.util.List"
            | "java.util.Set"
            | "java.util.Collection"
            | "java.lang.Iterable"
    )
}

fn contains_type_arguments(node: Node<'_>) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "type_arguments" {
            return true;
        }
        stack.extend(current.named_children(&mut current.walk()));
    }
    false
}

fn stdlib_import_is_proven(imports: &[String], name: &str) -> bool {
    let canonical = format!("kotlin.collections.{name}");
    let mut explicit_canonical = false;
    let mut arbitrary_star = false;
    for import in imports {
        let (path, alias) = import
            .split_once(" as ")
            .map_or((import.as_str(), None), |(path, alias)| {
                (path.trim(), Some(alias.trim()))
            });
        if alias == Some(name) && path != canonical.as_str() {
            return false;
        }
        if path.rsplit('.').next() == Some(name) && path != canonical.as_str() {
            return false;
        }
        if path == canonical.as_str() && alias.is_none_or(|alias| alias == name) {
            explicit_canonical = true;
        }
        if path.ends_with(".*") && !matches!(path, "java.util.*" | "java.lang.*") {
            arbitrary_star = true;
        }
    }
    explicit_canonical || !arbitrary_star
}

/// Import nodes exclude comment and string contents in the parsed snapshot.
pub(super) fn imports(source: &str, root: Node<'_>) -> Vec<String> {
    root.named_children(&mut root.walk())
        .filter(|node| node.kind() == "import")
        .filter_map(|node| node.utf8_text(source.as_bytes()).ok())
        .filter_map(|text| text.trim().strip_prefix("import"))
        .map(|text| text.trim().trim_end_matches(';').trim().to_owned())
        .collect()
}

fn has_indexed_extension_shadow(
    call: Node<'_>,
    name: &str,
    source: &str,
    file: &Path,
    extension_symbols: &[SymbolId],
) -> bool {
    let current_package = package_name(source);
    let enclosing_class_path = enclosing_class_path(call, source);
    extension_symbols.iter().any(|symbol| {
        symbol.name == name
            && ((symbol.package == current_package && symbol.owner_path.is_empty())
                || (symbol.file == file
                    && !enclosing_class_path.is_empty()
                    && symbol.owner_path == enclosing_class_path))
    })
}

fn enclosing_class_path(mut node: Node<'_>, source: &str) -> Vec<String> {
    let mut path = Vec::new();
    while let Some(parent) = node.parent() {
        if matches!(parent.kind(), "class_declaration" | "object_declaration")
            && let Some(name) = parent
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source.as_bytes()).ok())
        {
            path.push(name.to_owned());
        }
        node = parent;
    }
    path.reverse();
    path
}

fn has_local_extension_shadow(call: Node<'_>, name: &str, source: &str) -> bool {
    let mut scope = call.parent();
    while let Some(node) = scope {
        if matches!(node.kind(), "function_declaration" | "lambda_literal") {
            let mut stack = vec![node];
            while let Some(candidate) = stack.pop() {
                if candidate.kind() == "function_declaration"
                    && candidate
                        .child_by_field_name("name")
                        .and_then(|name_node| name_node.utf8_text(source.as_bytes()).ok())
                        == Some(name)
                    && has_receiver_type(candidate, source)
                {
                    return true;
                }
                stack.extend(candidate.named_children(&mut candidate.walk()));
            }
        }
        scope = node.parent();
    }
    false
}

fn has_receiver_type(function: Node<'_>, source: &str) -> bool {
    let Some(name) = function.child_by_field_name("name") else {
        return false;
    };
    function.named_children(&mut function.walk()).any(|child| {
        child.kind().contains("type")
            && !child.kind().contains("parameter")
            && child.end_byte() <= name.start_byte()
            && child.utf8_text(source.as_bytes()).is_ok()
    })
}

pub(super) fn supported_lambda_shape<'tree>(
    source: &str,
    call: Node<'tree>,
) -> Option<(Node<'tree>, Node<'tree>, String)> {
    let mut lambdas = Vec::new();
    for child in call
        .named_children(&mut call.walk())
        .filter(|child| child.kind() != "navigation_expression")
    {
        let mut stack = vec![child];
        while let Some(node) = stack.pop() {
            if node.kind() == "lambda_literal" {
                lambdas.push(node);
            } else {
                stack.extend(node.named_children(&mut node.walk()));
            }
        }
    }
    let [lambda] = lambdas.as_slice() else {
        return None;
    };
    let arguments = call
        .named_children(&mut call.walk())
        .find(|child| child.kind() == "value_arguments");
    if let Some(arguments) = arguments {
        let values = arguments
            .named_children(&mut arguments.walk())
            .filter(|argument| argument.kind() == "value_argument")
            .collect::<Vec<_>>();
        match values.as_slice() {
            [] => {}
            [argument] => {
                if argument.child_by_field_name("name").is_some() {
                    return None;
                }
                let mut stack = vec![*argument];
                let mut found_lambda = false;
                while let Some(node) = stack.pop() {
                    if node.kind() == "lambda_literal" {
                        found_lambda = true;
                        continue;
                    }
                    stack.extend(node.named_children(&mut node.walk()));
                }
                if !found_lambda {
                    return None;
                }
            }
            _ => return None,
        }
    }
    let children = lambda
        .named_children(&mut lambda.walk())
        .collect::<Vec<_>>();
    let parameters = children
        .iter()
        .copied()
        .filter(|child| child.kind() == "lambda_parameters")
        .collect::<Vec<_>>();
    let parameter_name = match parameters.as_slice() {
        [] => "it".to_owned(),
        [parameters] => {
            let entries = parameters
                .named_children(&mut parameters.walk())
                .collect::<Vec<_>>();
            let [parameter] = entries.as_slice() else {
                return None;
            };
            if !matches!(parameter.kind(), "identifier" | "simple_identifier")
                && !super::simple_lambda_variable(*parameter, source)
            {
                return None;
            }
            if matches!(parameter.kind(), "identifier" | "simple_identifier") {
                parameter.utf8_text(source.as_bytes()).ok()?.to_owned()
            } else {
                super::node_name(*parameter, source)?
            }
        }
        _ => return None,
    };
    let bodies = children
        .into_iter()
        .filter(|child| child.kind() != "lambda_parameters")
        .collect::<Vec<_>>();
    let [statements] = bodies.as_slice() else {
        return None;
    };
    let body_nodes = if statements.kind() == "statements" {
        statements
            .named_children(&mut statements.walk())
            .collect::<Vec<_>>()
    } else {
        vec![*statements]
    };
    let [body] = body_nodes.as_slice() else {
        return None;
    };
    Some((*lambda, *body, parameter_name))
}

pub(super) fn establish_property_projection<P: SemanticProvider + ?Sized>(
    source: &str,
    body: Node<'_>,
    parameter: &str,
    element: &TypeRef,
    file: &Path,
    provider: &P,
) -> Option<EagerPropertyProjection> {
    if body.kind() != "navigation_expression" {
        return None;
    }
    let parts = body
        .named_children(&mut body.walk())
        .filter(|node| !node.is_extra())
        .collect::<Vec<_>>();
    let [receiver, _member] = parts.as_slice() else {
        return None;
    };
    if !matches!(receiver.kind(), "identifier" | "simple_identifier")
        || receiver.utf8_text(source.as_bytes()).ok()? != parameter
    {
        return None;
    }
    establish_typed_property_projection(source, body, element, false, file, provider)
}

/// Prove one ordinary property hop using a receiver type established by the
/// lambda input or a preceding hop. Platform Java references are allowed as
/// receivers because Kotlin's platform dereference has the same JVM behavior;
/// explicitly nullable Kotlin references remain unsupported.
pub(super) fn establish_typed_property_projection<P: SemanticProvider + ?Sized>(
    source: &str,
    body: Node<'_>,
    receiver_type: &TypeRef,
    receiver_is_platform: bool,
    file: &Path,
    provider: &P,
) -> Option<EagerPropertyProjection> {
    if body.kind() != "navigation_expression" || (receiver_type.nullable && !receiver_is_platform) {
        return None;
    }
    let KotlinType::Named { name, arguments } = &receiver_type.ty else {
        return None;
    };
    if !arguments.is_empty() {
        return None;
    }
    let parts = body
        .named_children(&mut body.walk())
        .filter(|node| !node.is_extra())
        .collect::<Vec<_>>();
    let [receiver, member] = parts.as_slice() else {
        return None;
    };
    if !matches!(member.kind(), "identifier" | "simple_identifier")
        || source.get(receiver.end_byte()..member.start_byte())?.trim() != "."
    {
        return None;
    }
    let property_name = member.utf8_text(source.as_bytes()).ok()?;
    // Keep the bounded proof aligned with the ordinary getX ABI. Boolean
    // isX properties and unusual identifier capitalization need their own ABI.
    if !property_name.is_ascii()
        || !property_name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        || property_name
            .strip_prefix("is")
            .is_some_and(|rest| rest.chars().next().is_some_and(char::is_uppercase))
    {
        return None;
    }
    let FactStatus::Established(owner) =
        provider.resolve_type_in_file(file, &package_name(source), name)
    else {
        return None;
    };
    if !matches!(owner.kind.as_str(), "class" | "interface" | "enum")
        || !matches!(
            provider.is_ordinary_jvm_reference(&owner),
            FactStatus::Established(true)
        )
    {
        return None;
    }
    if owner
        .file
        .extension()
        .is_some_and(|extension| extension == "java")
    {
        let read = provider.java_property_read(&owner, property_name)?;
        return Some(EagerPropertyProjection {
            getter_name: read.method_name.clone(),
            target: read.target.clone(),
            getter_target: None,
            result_type: read.result_type.clone(),
            dependencies: read.dependencies.clone(),
            java_property_read: Some(read),
            java_parameter_name: String::new(),
        });
    }
    if !owner
        .file
        .extension()
        .is_some_and(|extension| extension == "kt" || extension == "kts")
    {
        return None;
    }
    let mut path = owner.owner_path.clone();
    path.push(owner.name.clone());
    let candidates = provider.symbols_named(property_name);
    let properties = candidates
        .into_iter()
        .filter(|symbol| {
            symbol.id.file == owner.file
                && symbol.id.package == owner.package
                && symbol.id.owner_path == path
                && symbol.id.kind == "property"
                && symbol.id.receiver.is_none()
        })
        .collect::<Vec<_>>();
    let [property] = properties.as_slice() else {
        return None;
    };
    let getter_target = if provider.has_plain_instance_property_getter(&property.id) {
        None
    } else {
        Some(provider.equivalent_property_getter(&property.id)?)
    };
    let FactStatus::Established(result_type) = &property.ty else {
        return None;
    };
    // A getter returning a value class has a mangled JVM name even when its
    // declaring class is ordinary. Generic containers erase their arguments;
    // only the returned classifier determines this additional ABI check.
    let mut returned = &result_type.ty;
    while let KotlinType::Nullable(inner) = returned {
        returned = inner;
    }
    if let KotlinType::Named { name, .. } = returned
        && let FactStatus::Established(classifier) =
            provider.resolve_type_in_file(&property.id.file, &property.id.package, name)
        && !matches!(
            provider.is_ordinary_jvm_reference(&classifier),
            FactStatus::Established(true)
        )
    {
        return None;
    }
    // Return types are written in the property's file scope. Canonicalize
    // there before exposing the fact to a consumer, which may not import the
    // declaring file's result type by its source spelling.
    let mut type_dependencies = Vec::new();
    let result_type = super::qualify_type_in_file(
        result_type,
        &property.id.file,
        provider,
        &mut type_dependencies,
    )?;
    let property_source = provider.source_for_file(&property.id.file)?;
    let result_type =
        supported_element_type(&result_type, property_source, &property.id.file, provider)?;
    let mut chars = property_name.chars();
    let getter_name = format!(
        "get{}{}",
        chars.next()?.to_ascii_uppercase(),
        chars.as_str()
    );
    Some(EagerPropertyProjection {
        target: property.id.clone(),
        getter_target,
        java_property_read: None,
        dependencies: type_dependencies,
        getter_name,
        result_type,
        java_parameter_name: String::new(),
    })
}

pub(super) fn has_generic_arguments(ty: &KotlinType) -> bool {
    match ty {
        KotlinType::Named { arguments, .. } => !arguments.is_empty(),
        KotlinType::Nullable(inner) | KotlinType::Array(inner) => has_generic_arguments(inner),
        _ => false,
    }
}

/// Kotlin collection covariance can widen a nested getter result, whereas a
/// Java `Function<E, List<String>>` cannot supply `List<Object>`. Until that
/// conversion has its own plan, require the exact contextual element type.
fn projection_result_context_matches<P: SemanticProvider + ?Sized>(
    source: &str,
    call: Node<'_>,
    file: &Path,
    provider: &P,
    projection: &mut EagerPropertyProjection,
) -> bool {
    let result = &projection.result_type;
    let expected = match super::container_calls::explicit_result_type(source, file, call, provider)
    {
        Ok(Some(expected)) => expected,
        Ok(None) => {
            let accepted = !has_generic_arguments(&result.ty);
            if accepted {
                projection.dependencies.sort();
                projection.dependencies.dedup();
            }
            return accepted;
        }
        Err(()) => return false,
    };
    let mut expected_dependencies = Vec::new();
    let Some(expected_type) = super::qualify_type_in_file(
        &expected.ty,
        &expected.location.file,
        provider,
        &mut expected_dependencies,
    ) else {
        return false;
    };
    let Some(expected_source) = provider.source_for_file(&expected.location.file) else {
        return false;
    };
    projection.dependencies.extend(expected_dependencies);
    let expected_type = flatten_nullable(expected_type.ty, expected_type.nullable);
    if matches!(
        &expected_type.ty,
        KotlinType::Named { name, arguments }
            if arguments.is_empty() && matches!(name.as_str(), "kotlin.Any" | "java.lang.Object")
    ) {
        projection.dependencies.sort();
        projection.dependencies.dedup();
        return true;
    }
    let KotlinType::Named { name, arguments } = &expected_type.ty else {
        return false;
    };
    let Some(canonical) = standard_external_type(expected_source, provider, name, false) else {
        return false;
    };
    if !matches!(
        canonical.as_str(),
        "kotlin.collections.List"
            | "kotlin.collections.MutableList"
            | "kotlin.collections.Collection"
            | "kotlin.collections.MutableCollection"
            | "kotlin.collections.Iterable"
            | "kotlin.collections.MutableIterable"
            | "java.util.List"
            | "java.util.Collection"
            | "java.lang.Iterable"
    ) {
        return false;
    }
    let [argument] = arguments.as_slice() else {
        return false;
    };
    if argument.variance != Variance::Invariant {
        return false;
    }
    let Some(expected_element) = argument.ty.as_ref() else {
        return false;
    };
    let expected_element = flatten_nullable(expected_element.clone(), false);
    let result = flatten_nullable(result.ty.clone(), result.nullable);
    let platform_result = projection
        .java_property_read
        .as_ref()
        .is_some_and(|read| read.nullability == super::ReferenceReturnNullability::Platform);
    if result.nullable && !expected_element.nullable && !platform_result {
        return false;
    }
    let compatible = result.ty == expected_element.ty
        && (expected_element.nullable || !result.nullable || platform_result)
        || projection_types_compatible(
            &TypeRef {
                ty: result.ty.clone(),
                nullable: result.nullable && !platform_result,
            },
            &expected_element,
            &expected.location.file,
            file,
            provider,
            &mut projection.dependencies,
        );
    if compatible {
        projection.dependencies.sort();
        projection.dependencies.dedup();
    }
    compatible
}

fn flatten_nullable(mut ty: KotlinType, mut nullable: bool) -> TypeRef {
    while let KotlinType::Nullable(inner) = ty {
        nullable = true;
        ty = *inner;
    }
    TypeRef { ty, nullable }
}

fn projection_types_compatible<P: SemanticProvider + ?Sized>(
    actual: &TypeRef,
    expected: &TypeRef,
    expected_file: &Path,
    actual_file: &Path,
    provider: &P,
    dependencies: &mut Vec<SymbolId>,
) -> bool {
    if actual.nullable && !expected.nullable {
        return false;
    }
    let actual_name = match &actual.ty {
        KotlinType::Primitive(name) => match name.as_str() {
            "Boolean" => "kotlin.Boolean",
            "Byte" => "kotlin.Byte",
            "Char" => "kotlin.Char",
            "Short" => "kotlin.Short",
            "Int" => "kotlin.Int",
            "Long" => "kotlin.Long",
            "Float" => "kotlin.Float",
            "Double" => "kotlin.Double",
            _ => return false,
        },
        KotlinType::Named { name, arguments } if arguments.is_empty() => name,
        _ => return false,
    };
    let KotlinType::Named {
        name: expected_name,
        arguments: expected_arguments,
    } = &expected.ty
    else {
        return false;
    };
    if !expected_arguments.is_empty() {
        return false;
    }
    if matches!(expected_name.as_str(), "kotlin.Any" | "java.lang.Object") {
        return true;
    }
    let actual_source = provider.source_for_file(actual_file);
    let expected_source = provider.source_for_file(expected_file);
    let (Some(actual_source), Some(expected_source)) = (actual_source, expected_source) else {
        return false;
    };
    let actual =
        match provider.resolve_type_in_file(actual_file, &package_name(actual_source), actual_name)
        {
            FactStatus::Established(id)
                if matches!(
                    provider.is_ordinary_jvm_reference(&id),
                    FactStatus::Established(true)
                ) =>
            {
                id
            }
            _ => return false,
        };
    let expected = match provider.resolve_type_in_file(
        expected_file,
        &package_name(expected_source),
        expected_name,
    ) {
        FactStatus::Established(id)
            if matches!(
                provider.is_ordinary_jvm_reference(&id),
                FactStatus::Established(true)
            ) =>
        {
            id
        }
        _ => return false,
    };
    dependencies.push(actual.clone());
    dependencies.push(expected.clone());
    let Some(path) = super::reified_calls::subtype_path(&actual, &expected, provider) else {
        return false;
    };
    dependencies.extend(path);
    true
}

fn body_returns_value<P: SemanticProvider + ?Sized>(
    source: &str,
    body: Node<'_>,
    parameter: &str,
    file: &Path,
    provider: &P,
    operation: EagerCollectionOperation,
) -> bool {
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "lambda_literal"
                | "return_expression"
                | "break_expression"
                | "continue_expression"
                | "assignment"
                | "throw_expression"
        ) {
            return false;
        }
        if matches!(node.kind(), "prefix_expression" | "postfix_expression")
            && node
                .utf8_text(source.as_bytes())
                .is_ok_and(|text| text.contains("++") || text.contains("--"))
        {
            return false;
        }
        if node.kind() == "call_expression" {
            let Some(callee) = super::call_callee_node(node) else {
                return false;
            };
            let Some(return_type) = source_call_return_type(source, node, callee, file, provider)
            else {
                return false;
            };
            if is_unit_type(&return_type) {
                return false;
            }
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    let Ok(text) = body.utf8_text(source.as_bytes()) else {
        return false;
    };
    let text = text.trim();
    if matches!(operation, EagerCollectionOperation::FilterNot) && matches!(text, "true" | "false")
    {
        return true;
    }
    if matches!(operation, EagerCollectionOperation::Map)
        && text == parameter
        && matches!(body.kind(), "identifier" | "simple_identifier")
    {
        return true;
    }
    if matches!(body.kind(), "string_literal" | "character_literal") {
        return operation == EagerCollectionOperation::Map;
    }
    if matches!(body.kind(), "parenthesized_expression") {
        let children = body.named_children(&mut body.walk()).collect::<Vec<_>>();
        return matches!(children.as_slice(), [inner]
            if body_returns_value(source, *inner, parameter, file, provider, operation));
    }
    if operation == EagerCollectionOperation::Map && body.kind() == "call_expression" {
        return source_call_return_type(
            source,
            body,
            super::call_callee_node(body).unwrap_or(body),
            file,
            provider,
        )
        .is_some();
    }
    if !has_boolean_result_operator(body, source) {
        if operation == EagerCollectionOperation::FilterNot
            && body.kind() == "call_expression"
            && let Some(callee) = super::call_callee_node(body)
            && let Some(return_type) = source_call_return_type(source, body, callee, file, provider)
        {
            return type_is_boolean(&return_type);
        }
        return false;
    }
    operation == EagerCollectionOperation::FilterNot
}

fn has_boolean_result_operator(node: Node<'_>, source: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| !child.is_named())
        .filter_map(|child| child.utf8_text(source.as_bytes()).ok())
        .any(|token| matches!(token, "==" | "!=" | "&&" | "||"))
}

fn source_call_return_type<P: SemanticProvider + ?Sized>(
    source: &str,
    call: Node<'_>,
    callee: Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<TypeRef> {
    let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
    let FactStatus::Established(symbol_id) =
        super::resolve_required_call(source, file, call, callee, name, provider)
    else {
        return None;
    };
    let symbol = provider
        .symbols_named(name)
        .into_iter()
        .find(|symbol| symbol.id == symbol_id)?;
    let FactStatus::Established(return_type) = &symbol.ty else {
        return None;
    };
    Some(return_type.clone())
}

fn is_unit_type(ty: &TypeRef) -> bool {
    match &ty.ty {
        KotlinType::Named { name, .. } => matches!(name.as_str(), "Unit" | "kotlin.Unit" | "void"),
        KotlinType::Nullable(inner) => matches!(
            inner.as_ref(),
            KotlinType::Named { name, .. } if matches!(name.as_str(), "Unit" | "kotlin.Unit" | "void")
        ),
        KotlinType::Unknown(name) => name == "void",
        _ => false,
    }
}

fn type_is_boolean(ty: &TypeRef) -> bool {
    matches!(&ty.ty, KotlinType::Primitive(name) if name == "Boolean")
        || matches!(&ty.ty, KotlinType::Named { name, .. } if matches!(name.as_str(), "Boolean" | "kotlin.Boolean" | "boolean"))
}

fn collection_element_type<P: SemanticProvider + ?Sized>(
    source: &str,
    call: Node<'_>,
    file: &Path,
    provider: &P,
    receiver_spelling: &str,
) -> Option<TypeRef> {
    let type_parameters = super::declaration_type_parameters(source, call);
    let mut parsed = TypeRef::parse_with_type_parameters(receiver_spelling, &type_parameters);
    let mut producer = file;
    let from_factory =
        matches!(&parsed.ty, KotlinType::Named { arguments, .. } if arguments.is_empty());
    if from_factory {
        let receiver = explicit_receiver_node(call, call_callee_node(call)?)?;
        if receiver.kind() != "call_expression" {
            return None;
        }
        let callee = call_callee_node(receiver)?;
        let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
        let FactStatus::Established(id) =
            super::resolve_required_call(source, file, receiver, callee, name, provider)
        else {
            return None;
        };
        let symbol = provider
            .symbols_named(name)
            .into_iter()
            .find(|symbol| symbol.id == id)?;
        let FactStatus::Established(ty) = &symbol.ty else {
            return None;
        };
        parsed = ty.clone();
        producer = &symbol.location.file;
    }
    let KotlinType::Named { arguments, .. } = parsed.ty else {
        return None;
    };
    let [element] = arguments.as_slice() else {
        return None;
    };
    if element.variance != Variance::Invariant {
        return None;
    }
    let element = TypeRef {
        ty: element.ty.clone()?,
        nullable: false,
    };
    if from_factory && !element_has_same_binding(&element.ty, producer, file, provider) {
        return None;
    }
    supported_element_type(&element, source, file, provider)
}

/// Producer-local spellings must denote the same types in the consuming file.
/// Generic factory substitution and platform numeric nullability need richer facts.
fn element_has_same_binding<P: SemanticProvider + ?Sized>(
    ty: &KotlinType,
    producer: &Path,
    consumer: &Path,
    provider: &P,
) -> bool {
    match ty {
        KotlinType::Primitive(name) => {
            let Some(producer_source) = provider.source_for_file(producer) else {
                return false;
            };
            let Some(consumer_source) = provider.source_for_file(consumer) else {
                return false;
            };
            let left = standard_external_type(producer_source, provider, name, false);
            left.is_some() && left == standard_external_type(consumer_source, provider, name, false)
        }
        KotlinType::Nullable(inner) | KotlinType::Array(inner) => {
            element_has_same_binding(inner, producer, consumer, provider)
        }
        KotlinType::Named { name, arguments } => {
            let Some(producer_source) = provider.source_for_file(producer) else {
                return false;
            };
            let Some(consumer_source) = provider.source_for_file(consumer) else {
                return false;
            };
            let same = match (
                provider.resolve_type_in_file(producer, &package_name(producer_source), name),
                provider.resolve_type_in_file(consumer, &package_name(consumer_source), name),
            ) {
                (FactStatus::Established(left), FactStatus::Established(right)) => left == right,
                (FactStatus::Unknown, FactStatus::Unknown) => {
                    let left = standard_external_type(
                        producer_source,
                        provider,
                        name,
                        producer.extension().is_some_and(|ext| ext == "java"),
                    );
                    left.is_some()
                        && left == standard_external_type(consumer_source, provider, name, false)
                }
                _ => false,
            };
            same && arguments.iter().all(|argument| {
                argument
                    .ty
                    .as_ref()
                    .is_some_and(|ty| element_has_same_binding(ty, producer, consumer, provider))
            })
        }
        KotlinType::TypeParameter(_) | KotlinType::Function { .. } | KotlinType::Unknown(_) => {
            false
        }
    }
}

fn supported_element_type<P: SemanticProvider + ?Sized>(
    element: &TypeRef,
    source: &str,
    file: &Path,
    provider: &P,
) -> Option<TypeRef> {
    fn supported<P: SemanticProvider + ?Sized>(
        ty: &KotlinType,
        source: &str,
        file: &Path,
        provider: &P,
    ) -> bool {
        match ty {
            KotlinType::Primitive(name) => {
                standard_external_type(source, provider, name, false).is_some()
            }
            KotlinType::TypeParameter(_) => true,
            KotlinType::Nullable(inner) | KotlinType::Array(inner) => {
                supported(inner, source, file, provider)
            }
            KotlinType::Named { name, arguments } => {
                if provider.has_type_alias_in_package(&package_name(source), name) {
                    return false;
                }
                let known_external =
                    super::standard_external_type(source, provider, name, false).is_some();
                let known_source = matches!(
                    provider.resolve_type_in_file(file, &package_name(source), name),
                    FactStatus::Established(id) if id.kind != "typealias"
                );
                (known_external || known_source)
                    && arguments.iter().all(|argument| {
                        argument
                            .ty
                            .as_ref()
                            .is_some_and(|ty| supported(ty, source, file, provider))
                    })
            }
            KotlinType::Function { .. } | KotlinType::Unknown(_) => false,
        }
    }
    supported(&element.ty, source, file, provider).then(|| element.clone())
}

fn result_element_type<P: SemanticProvider + ?Sized>(
    source: &str,
    root: Node<'_>,
    fact: &EagerCollectionCall,
    file: &Path,
    provider: &P,
) -> Option<TypeRef> {
    if let Some(projection) = &fact.property_projection {
        return Some(projection.result_type.clone());
    }
    if fact.operation == EagerCollectionOperation::FilterNot {
        return fact.element_type.clone();
    }
    let body = source.get(fact.body.start_byte..fact.body.end_byte)?;
    if body.trim() == fact.parameter_name {
        return fact.element_type.clone();
    }
    if matches!(body.trim(), "true" | "false") {
        return Some(TypeRef::parse("Boolean"));
    }
    if body.parse::<i64>().is_ok() {
        return Some(TypeRef::parse("Int"));
    }
    if body.starts_with('"') && body.ends_with('"') {
        return Some(TypeRef::parse("String"));
    }
    let body_node = root.descendant_for_byte_range(fact.body.start_byte, fact.body.end_byte)?;
    let mut stack = vec![body_node];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression" {
            let callee = super::call_callee_node(node)?;
            if source_call_has_uncertain_numeric_result(source, node, callee, file, provider) {
                return None;
            }
            return source_call_return_type(source, node, callee, file, provider);
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    let _ = file;
    None
}

fn captures_modified_local(
    source: &str,
    call: Node<'_>,
    lambda: Node<'_>,
    lambda_parameter: &str,
) -> bool {
    let mut names = BTreeSet::new();
    let mut stack = vec![lambda];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "identifier" | "simple_identifier")
            && let Ok(name) = node.utf8_text(source.as_bytes())
            && name != lambda_parameter
        {
            names.insert(name.to_owned());
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    let mut enclosing = call.parent();
    while let Some(node) = enclosing {
        if matches!(node.kind(), "function_declaration" | "function_literal") {
            let mut declarations = Vec::new();
            let mut assignments = Vec::new();
            let mut stack = vec![node];
            while let Some(candidate) = stack.pop() {
                if candidate.kind() == "variable_declaration"
                    && candidate.start_byte() < lambda.start_byte()
                    && let Some(name) = super::node_name(candidate, source)
                    && names.contains(&name)
                    && lexical_scope_contains(lambda, super::nearest_lexical_scope(candidate))
                {
                    declarations.push((candidate.start_byte(), name));
                }
                if matches!(
                    candidate.kind(),
                    "assignment" | "postfix_expression" | "prefix_expression"
                ) {
                    let text = candidate.utf8_text(source.as_bytes()).unwrap_or("");
                    assignments.push((candidate.start_byte(), text.to_owned()));
                }
                stack.extend(candidate.named_children(&mut candidate.walk()));
            }
            for (declared_at, name) in declarations {
                if assignments.iter().any(|(start, text)| {
                    *start > declared_at
                        && text
                            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                            .any(|part| part == name)
                        && (text.contains('=') || text.contains("++") || text.contains("--"))
                }) {
                    return true;
                }
            }
            return false;
        }
        enclosing = node.parent();
    }
    false
}

fn source_call_has_uncertain_numeric_result<P: SemanticProvider + ?Sized>(
    source: &str,
    call: Node<'_>,
    callee: Node<'_>,
    file: &Path,
    provider: &P,
) -> bool {
    let Some(name) = callee.utf8_text(source.as_bytes()).ok().map(str::trim) else {
        return false;
    };
    let FactStatus::Established(symbol_id) =
        super::resolve_required_call(source, file, call, callee, name, provider)
    else {
        return false;
    };
    if Path::new(&symbol_id.file)
        .extension()
        .and_then(|ext| ext.to_str())
        != Some("java")
    {
        return false;
    }
    provider
        .symbols_named(name)
        .into_iter()
        .find(|symbol| symbol.id == symbol_id)
        .and_then(|symbol| match &symbol.ty {
            FactStatus::Established(ty) => Some(ty),
            _ => None,
        })
        .is_some_and(|ty| match &ty.ty {
            // The syntax provider can normalize boxed Java names to Kotlin
            // primitives without preserving platform nullability. That is
            // insufficient evidence for a subsequent primitive lambda binding.
            KotlinType::Primitive(_) => true,
            KotlinType::Named { name, .. } => matches!(
                name.rsplit('.').next(),
                Some(
                    "Integer"
                        | "Long"
                        | "Short"
                        | "Byte"
                        | "Double"
                        | "Float"
                        | "Boolean"
                        | "Character"
                        | "int"
                        | "long"
                        | "short"
                        | "byte"
                        | "double"
                        | "float"
                        | "boolean"
                        | "char"
                )
            ),
            _ => false,
        })
}

fn lexical_scope_contains(mut node: Node<'_>, scope_id: usize) -> bool {
    loop {
        if node.id() == scope_id {
            return true;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
}
