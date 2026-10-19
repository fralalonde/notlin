//! Bounded, source-proven payloads for JDK containers and their exact lambdas.
//! Receiver chains are inspected backwards; source returns are never inferred
//! from callable bodies, and bindings never escape their owning lambda.

use super::{
    FactStatus, KotlinType, SemanticProvider, SourceLocation, SymbolId, TypeRef, Variance,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tree_sitter::Node;

const MAX_CHAIN_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerLambdaBinding {
    pub location: SourceLocation,
    pub body: SourceLocation,
    pub parameter_name: String,
    pub java_parameter_name: String,
    pub input_type: TypeRef,
    pub property_accesses: Vec<ContainerPropertyAccess>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerPropertyAccess {
    pub location: SourceLocation,
    /// Exact receiver expression for this hop (for example `entry.data`).
    pub receiver: SourceLocation,
    /// Receiver type established from the lambda input and preceding getters.
    pub receiver_type: TypeRef,
    /// Ordered JVM getters from the lambda parameter through this hop,
    /// including the current getter (for example `getData`, `getType`).
    pub getter_path: Vec<String>,
    pub projection: super::EagerPropertyProjection,
}

#[derive(Clone)]
pub(super) struct Payload {
    pub classifier: String,
    pub element: TypeRef,
    pub dependencies: Vec<SymbolId>,
}

pub(super) struct CallTypes {
    pub receiver: Payload,
    pub result_element: Option<TypeRef>,
    pub lambda: Option<ContainerLambdaBinding>,
    pub dependencies: Vec<SymbolId>,
}

pub(super) fn establish<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    provider: &P,
) -> Option<CallTypes> {
    let callee = super::call_callee_node(call)?;
    let name = callee.utf8_text(source.as_bytes()).ok()?;
    if !super::container_calls::supported_member_name(name)
        || provider.source_for_file(file)? != source
    {
        return None;
    }
    // Recursive receivers share this established, immutable provider snapshot.
    call_types(source, file, call, provider, 0)
}

fn call_types<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    provider: &P,
    depth: usize,
) -> Option<CallTypes> {
    if depth >= MAX_CHAIN_DEPTH {
        return None;
    }
    let callee = super::call_callee_node(call)?;
    let name = callee.utf8_text(source.as_bytes()).ok()?;
    if !super::container_calls::supported_member_name(name) {
        return None;
    }
    let receiver_node = super::explicit_receiver_node(call, callee)?;
    if !super::container_calls::plain_call_arguments(call, callee)
        || source
            .get(receiver_node.end_byte()..callee.start_byte())?
            .trim()
            != "."
    {
        return None;
    }
    let receiver = expression_payload(source, file, receiver_node, call, provider, depth + 1)?;
    let arity = super::call_argument_count(call);
    let mut dependencies = receiver.dependencies.clone();
    let mut lambda = None;
    let result_element = match (receiver.classifier.as_str(), name, arity) {
        (ty, "stream", 0) if collection(ty) => Some(receiver.element.clone()),
        ("java.util.stream.Stream" | "java.util.Optional", "filter" | "map", 1) => {
            let (lambda_node, body, parameter) =
                super::eager_collections::supported_lambda_shape(source, call)?;
            let mut accesses = Vec::new();
            let mut pending = vec![body];
            while let Some(node) = pending.pop() {
                if node.kind() == "lambda_literal" {
                    // Nested receiver scopes require another independent proof.
                    return None;
                }
                if node.kind() == "navigation_expression"
                    && !navigation_is_call_callee(node)
                    && contains_parameter_reference(source, node, &parameter)
                {
                    let mut chain = Vec::new();
                    PropertyChainContext {
                        source,
                        file,
                        parameter: &parameter,
                        input_type: &receiver.element,
                        provider,
                    }
                    .prove(node, 0, &mut chain)?;
                    for mut access in chain {
                        dependencies.push(access.projection.target.clone());
                        dependencies.extend(access.projection.getter_target.iter().cloned());
                        dependencies.extend(access.projection.dependencies.iter().cloned());
                        if let Some(read) = &access.projection.java_property_read {
                            dependencies.extend(read.dependencies.iter().cloned());
                        }
                        access.receiver_type =
                            qualify_type(&access.receiver_type, file, provider, &mut dependencies)?;
                        access.projection.result_type = qualify_type(
                            &access.projection.result_type,
                            file,
                            provider,
                            &mut dependencies,
                        )?;
                        if !accesses.iter().any(|existing: &ContainerPropertyAccess| {
                            existing.location.start_byte == access.location.start_byte
                                && existing.location.end_byte == access.location.end_byte
                        }) {
                            accesses.push(access);
                        }
                    }
                    // The proof already visited every receiver hop. A pure
                    // property chain has no other lambda expressions to bind.
                    continue;
                }
                pending.extend(node.named_children(&mut node.walk()));
            }
            accesses.sort_by_key(|access| (access.location.start_byte, access.location.end_byte));
            if (super::eager_collections::nullable_floating_point(&receiver.element)
                || accesses.iter().any(|access| {
                    super::eager_collections::nullable_floating_point(
                        &access.projection.result_type,
                    )
                }))
                && super::eager_collections::contains_structural_equality(body)
            {
                return None;
            }
            if (super::eager_collections::primitive_type(&receiver.element)
                || accesses.iter().any(|access| {
                    super::eager_collections::primitive_type(&access.projection.result_type)
                }))
                && super::eager_collections::contains_referential_equality(body)
            {
                return None;
            }
            let output = if name == "filter"
                || body.utf8_text(source.as_bytes()).ok() == Some(parameter.as_str())
            {
                Some(receiver.element.clone())
            } else if matches!(body.kind(), "string_literal" | "string_template") {
                Some(TypeRef::parse("java.lang.String"))
            } else if let Some(access) = accesses.iter().find(|access| {
                access.location.start_byte == body.start_byte()
                    && access.location.end_byte == body.end_byte()
            }) {
                let result = qualify_type(
                    &access.projection.result_type,
                    file,
                    provider,
                    &mut dependencies,
                )?;
                if super::eager_collections::has_generic_arguments(&result.ty)
                    && !exact_projection_context(
                        source,
                        file,
                        call,
                        provider,
                        &receiver.classifier,
                        &result,
                    )
                {
                    // Nested Kotlin covariance cannot become a Java invariant
                    // payload without a separately planned conversion.
                    return None;
                }
                Some(result)
            } else {
                None
            };
            lambda = Some(ContainerLambdaBinding {
                location: location(source, file, lambda_node),
                body: location(source, file, body),
                parameter_name: parameter,
                java_parameter_name: String::new(),
                input_type: receiver.element.clone(),
                property_accesses: accesses,
            });
            output
        }
        ("java.util.stream.Stream", "findAny" | "findFirst", 0) => Some(receiver.element.clone()),
        ("java.util.Optional", "get" | "orElseThrow", 0)
        | ("java.util.Optional", "orElse" | "orElseGet", 1) => Some(receiver.element.clone()),
        _ => return None,
    };
    dependencies.sort();
    dependencies.dedup();
    Some(CallTypes {
        receiver,
        result_element,
        lambda,
        dependencies,
    })
}

fn navigation_is_call_callee(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "call_expression"
            && parent
                .named_child(0)
                .is_some_and(|callee| callee.id() == node.id())
    })
}

fn is_parameter_identifier(source: &str, node: Node<'_>, parameter: &str) -> bool {
    matches!(node.kind(), "identifier" | "simple_identifier")
        && node.utf8_text(source.as_bytes()).ok() == Some(parameter)
}

fn contains_parameter_reference(source: &str, expression: Node<'_>, parameter: &str) -> bool {
    let mut pending = vec![expression];
    while let Some(node) = pending.pop() {
        if is_parameter_identifier(source, node, parameter) {
            return true;
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    false
}

struct PropertyChainContext<'a, P: SemanticProvider + ?Sized> {
    source: &'a str,
    file: &'a Path,
    parameter: &'a str,
    input_type: &'a TypeRef,
    provider: &'a P,
}

impl<P: SemanticProvider + ?Sized> PropertyChainContext<'_, P> {
    fn prove(
        &self,
        node: Node<'_>,
        depth: usize,
        accesses: &mut Vec<ContainerPropertyAccess>,
    ) -> Option<(TypeRef, bool, Vec<String>)> {
        if depth >= MAX_CHAIN_DEPTH {
            return None;
        }
        if node.kind() == "parenthesized_expression" {
            let children = node.named_children(&mut node.walk()).collect::<Vec<_>>();
            let [inner] = children.as_slice() else {
                return None;
            };
            return self.prove(*inner, depth + 1, accesses);
        }
        if is_parameter_identifier(self.source, node, self.parameter) {
            return Some((self.input_type.clone(), false, Vec::new()));
        }
        if node.kind() != "navigation_expression" {
            return None;
        }
        let parts = node
            .named_children(&mut node.walk())
            .filter(|child| !child.is_extra())
            .collect::<Vec<_>>();
        let [receiver, member] = parts.as_slice() else {
            return None;
        };
        if !matches!(member.kind(), "identifier" | "simple_identifier")
            || self
                .source
                .get(receiver.end_byte()..member.start_byte())?
                .trim()
                != "."
        {
            return None;
        }
        let (receiver_type, receiver_is_platform, mut getter_path) =
            if is_parameter_identifier(self.source, *receiver, self.parameter) {
                (self.input_type.clone(), false, Vec::new())
            } else {
                self.prove(*receiver, depth + 1, accesses)?
            };
        let projection = super::eager_collections::establish_typed_property_projection(
            self.source,
            node,
            &receiver_type,
            receiver_is_platform,
            self.file,
            self.provider,
        )?;
        getter_path.push(projection.getter_name.clone());
        let is_platform = projection
            .java_property_read
            .as_ref()
            .is_some_and(|read| read.nullability == super::ReferenceReturnNullability::Platform);
        let result_type = projection.result_type.clone();
        accesses.push(ContainerPropertyAccess {
            location: location(self.source, self.file, node),
            receiver: location(self.source, self.file, *receiver),
            receiver_type: receiver_type.clone(),
            getter_path: getter_path.clone(),
            projection,
        });
        Some((result_type, is_platform, getter_path))
    }
}

fn exact_projection_context<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    provider: &P,
    classifier: &str,
    result: &TypeRef,
) -> bool {
    let Ok(Some(expected)) =
        super::container_calls::explicit_result_type(source, file, call, provider)
    else {
        return false;
    };
    let Some(payload) =
        declared_payload(&expected.ty, &expected.location.file, provider, Vec::new())
    else {
        return false;
    };
    payload.classifier == classifier && payload.element == *result
}

fn expression_payload<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    expression: Node<'_>,
    context: Node<'_>,
    provider: &P,
    depth: usize,
) -> Option<Payload> {
    if depth >= MAX_CHAIN_DEPTH {
        return None;
    }
    if expression.kind() == "call_expression" {
        if let Some(inner) = call_types(source, file, expression, provider, depth + 1) {
            let name = super::call_callee_node(expression)?
                .utf8_text(source.as_bytes())
                .ok()?;
            let classifier = match (inner.receiver.classifier.as_str(), name) {
                (ty, "stream") if collection(ty) => "java.util.stream.Stream",
                ("java.util.stream.Stream", "filter" | "map") => "java.util.stream.Stream",
                ("java.util.stream.Stream", "findAny" | "findFirst") => "java.util.Optional",
                ("java.util.Optional", "filter" | "map") => "java.util.Optional",
                _ => return None,
            };
            return Some(Payload {
                classifier: classifier.into(),
                element: inner.result_element?,
                dependencies: inner.dependencies,
            });
        }
        let callee = super::call_callee_node(expression)?;
        // Do not ask the general resolver to rediscover an unsupported JDK
        // chain: it consults these facts itself. Direct source returns use an
        // ordinary local dispatch receiver and never infer from a chain body.
        if super::explicit_receiver_node(expression, callee)
            .is_some_and(|receiver| receiver.kind() != "this_expression")
        {
            return None;
        }
        let name = callee.utf8_text(source.as_bytes()).ok()?;
        let FactStatus::Established(target) =
            super::resolve_required_call(source, file, expression, callee, name, provider)
        else {
            return None;
        };
        let ty = provider.explicit_nongeneric_kotlin_return(&target)?;
        return declared_payload(&ty, &target.file, provider, vec![target.clone()]);
    }
    if !matches!(expression.kind(), "identifier" | "simple_identifier") {
        return None;
    }
    let name = expression.utf8_text(source.as_bytes()).ok()?;
    if let Some(ty) = super::lexical_parameter_type(source, context, name) {
        return declared_payload(&ty, file, provider, Vec::new());
    }
    if super::is_proven_local_shadow(expression, name, source) {
        return None;
    }
    let owner = super::enclosing_dispatch_owner(source, expression, file, provider)?;
    let mut path = owner.owner_path.clone();
    path.push(owner.name.clone());
    let candidates = provider.symbols_named(name);
    let properties = candidates
        .into_iter()
        .filter(|symbol| {
            symbol.id.file == owner.file
                && symbol.id.owner_path == path
                && symbol.id.package == owner.package
                && symbol.id.kind == "property"
                && symbol.id.receiver.is_none()
        })
        .collect::<Vec<_>>();
    let [property] = properties.as_slice() else {
        return None;
    };
    let FactStatus::Established(ty) = &property.ty else {
        return None;
    };
    declared_payload(
        ty,
        &property.id.file,
        provider,
        vec![property.id.clone(), owner],
    )
}

fn declared_payload<P: SemanticProvider + ?Sized>(
    ty: &TypeRef,
    producer: &Path,
    provider: &P,
    mut dependencies: Vec<SymbolId>,
) -> Option<Payload> {
    if ty.nullable {
        return None;
    }
    let KotlinType::Named { name, arguments } = &ty.ty else {
        return None;
    };
    let name =
        super::standard_external_type(provider.source_for_file(producer)?, provider, name, false)?;
    if !collection(&name)
        && !matches!(
            name.as_str(),
            "java.util.Optional" | "java.util.stream.Stream"
        )
    {
        return None;
    }
    let [argument] = arguments.as_slice() else {
        return None;
    };
    if argument.variance != Variance::Invariant {
        return None;
    }
    let ty = qualify_type(
        &TypeRef {
            ty: argument.ty.clone()?,
            nullable: false,
        },
        producer,
        provider,
        &mut dependencies,
    )?
    .ty;
    if matches!(
        ty,
        KotlinType::Nullable(_) | KotlinType::TypeParameter(_) | KotlinType::Unknown(_)
    ) {
        return None;
    }
    Some(Payload {
        classifier: name,
        element: TypeRef {
            ty,
            nullable: false,
        },
        dependencies,
    })
}

pub(super) fn lambda_requires_binding(source: &str, call: Node<'_>) -> bool {
    let Some((_, body, parameter)) = super::eager_collections::supported_lambda_shape(source, call)
    else {
        return false;
    };
    let mut pending = vec![body];
    while let Some(node) = pending.pop() {
        if node.kind() == "navigation_expression"
            && node
                .named_child(0)
                .is_some_and(|base| contains_parameter_reference(source, base, &parameter))
        {
            return true;
        }
        if node.children(&mut node.walk()).any(|child| {
            matches!(
                child.kind(),
                "==" | "!=" | "===" | "!==" | "+" | "-" | "*" | "/" | "<" | ">" | "<=" | ">="
            )
        }) {
            return true;
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    false
}

pub(crate) fn qualify_type<P: SemanticProvider + ?Sized>(
    ty: &TypeRef,
    producer: &Path,
    provider: &P,
    dependencies: &mut Vec<SymbolId>,
) -> Option<TypeRef> {
    let source = provider.source_for_file(producer)?;
    let qualify = |ty: &KotlinType, dependencies: &mut Vec<SymbolId>| {
        qualify_type(
            &TypeRef {
                ty: ty.clone(),
                nullable: false,
            },
            producer,
            provider,
            dependencies,
        )
        .map(|ty| ty.ty)
    };
    let qualified = match &ty.ty {
        KotlinType::Primitive(name) => {
            super::standard_external_type(source, provider, name, false)?;
            ty.ty.clone()
        }
        KotlinType::Named { name, arguments } => {
            let name =
                match provider.resolve_type_in_file(producer, &super::package_name(source), name) {
                    FactStatus::Established(id)
                        if id.kind != "typealias"
                            && matches!(
                                provider.is_ordinary_jvm_reference(&id),
                                FactStatus::Established(true)
                            ) =>
                    {
                        let name = super::qualified_source_type_name(&id);
                        dependencies.push(id);
                        name
                    }
                    FactStatus::Unknown => super::standard_external_type(
                        source,
                        provider,
                        name,
                        producer.extension().is_some_and(|ext| ext == "java"),
                    )?,
                    _ => return None,
                };
            let arguments = arguments
                .iter()
                .map(|argument| {
                    (argument.variance == Variance::Invariant).then_some(())?;
                    Some(super::TypeArgument {
                        variance: argument.variance.clone(),
                        ty: Some(qualify(argument.ty.as_ref()?, dependencies)?),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            KotlinType::Named { name, arguments }
        }
        KotlinType::Nullable(inner) => {
            KotlinType::Nullable(Box::new(qualify(inner, dependencies)?))
        }
        KotlinType::Array(inner) => KotlinType::Array(Box::new(qualify(inner, dependencies)?)),
        _ => return None,
    };
    Some(TypeRef {
        ty: qualified,
        nullable: ty.nullable,
    })
}

fn collection(classifier: &str) -> bool {
    matches!(
        classifier,
        "java.util.List"
            | "java.util.Set"
            | "java.util.Collection"
            | "kotlin.collections.List"
            | "kotlin.collections.MutableList"
            | "kotlin.collections.Set"
            | "kotlin.collections.MutableSet"
            | "kotlin.collections.Collection"
            | "kotlin.collections.MutableCollection"
    )
}

fn location(source: &str, file: &Path, node: Node<'_>) -> SourceLocation {
    SourceLocation {
        file: file.to_path_buf(),
        snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    }
}

/// Query only the nearest proven lambda. An unresolved or nested scope must
/// never fall back to an outer same-spelled parameter or an implicit `it`.
pub(super) fn parameter_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    reference: Node<'_>,
    name: &str,
    provider: &P,
) -> Option<TypeRef> {
    let binding = binding_at(source, file, reference, provider)?;
    if binding.parameter_name != name {
        return None;
    }
    // Any earlier local with this name makes the binding uncertain.
    let mut node = reference.parent();
    while let Some(parent) = node {
        if parent.kind() == "lambda_literal" {
            break;
        }
        if parent.named_children(&mut parent.walk()).any(|child| {
            child.start_byte() < reference.start_byte()
                && child.kind() == "property_declaration"
                && super::declaration_identifier(child, source)
                    .is_some_and(|id| id.utf8_text(source.as_bytes()).ok() == Some(name))
        }) {
            return None;
        }
        node = parent.parent();
    }
    Some(binding.input_type)
}

pub(super) fn binding_at<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    reference: Node<'_>,
    provider: &P,
) -> Option<ContainerLambdaBinding> {
    let mut ancestor = reference.parent();
    while let Some(node) = ancestor {
        if matches!(
            node.kind(),
            "lambda_literal" | "function_literal" | "anonymous_function"
        ) {
            let mut call = node.parent()?;
            while matches!(
                call.kind(),
                "annotated_lambda" | "value_argument" | "value_arguments"
            ) {
                call = call.parent()?;
            }
            if call.kind() != "call_expression" {
                return None;
            }
            let binding = establish(source, file, call, provider)?.lambda?;
            return (binding.location.start_byte == node.start_byte()
                && binding.location.end_byte == node.end_byte())
            .then_some(binding);
        }
        if matches!(
            node.kind(),
            "function_declaration" | "class_declaration" | "object_declaration"
        ) {
            return None;
        }
        ancestor = node.parent();
    }
    None
}

pub(super) fn property_access<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    reference: Node<'_>,
    provider: &P,
) -> Option<ContainerPropertyAccess> {
    binding_at(source, file, reference, provider)?
        .property_accesses
        .into_iter()
        .find(|access| {
            access.location.start_byte == reference.start_byte()
                && access.location.end_byte == reference.end_byte()
        })
}
