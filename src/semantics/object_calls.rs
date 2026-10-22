//! Exact `Object.toString()` call-result facts for proven reference receivers.

use super::{
    FactStatus, KotlinType, SemanticProvider, TypeRef, call_argument_count, direct_trailing_lambda,
    explicit_receiver_node, is_proven_local_shadow, is_type_symbol, lexical_parameter_type,
    package_name, resolve_required_call,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tree_sitter::Node;

/// A call whose Java dispatch and result are proven.
/// The receiver identity is evidence; Object dispatch remains virtual.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceStringCall {
    pub location: super::SourceLocation,
    pub receiver: ReferenceStringReceiver,
    pub kind: ReferenceStringCallKind,
}

/// Keep source declarations and external return classifiers distinct. A JDK
/// classifier is not an indexed declaration and must never acquire a fake ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceStringReceiver {
    Indexed(super::SymbolId),
    Standard {
        canonical_type: String,
        expression: super::SourceLocation,
    },
    Returned {
        producer: super::SymbolId,
        producer_location: super::SourceLocation,
        expression: super::SourceLocation,
        canonical_type: String,
        nullability: super::ReferenceReturnNullability,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceStringCallKind {
    ObjectToString,
    InvariantLowercase,
}

pub(super) fn establish<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    provider: &P,
) -> Option<ReferenceStringCall> {
    let callee = super::call_callee_node(call)?;
    let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
    let (receiver, kind) = if name == "toString" {
        (
            object_receiver(source, file, call, callee, name, provider)?,
            ReferenceStringCallKind::ObjectToString,
        )
    } else if let Some(case) = super::string_case::establish(source, file, call, provider)
        && case.direction == super::string_case::StringCaseDirection::Lowercase
        && case.locale == super::string_case::StringCaseLocale::Invariant
    {
        let receiver = explicit_receiver_node(call, callee)?;
        let inner_callee = super::call_callee_node(receiver)?;
        let owner = object_receiver(source, file, receiver, inner_callee, "toString", provider)?;
        (owner, ReferenceStringCallKind::InvariantLowercase)
    } else {
        return None;
    };
    Some(ReferenceStringCall {
        location: super::SourceLocation {
            file: file.to_path_buf(),
            snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
            start_byte: call.start_byte(),
            end_byte: call.end_byte(),
        },
        receiver,
        kind,
    })
}

pub(super) fn established_object_to_string<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    callee: Node<'_>,
    name: &str,
    provider: &P,
) -> Option<&'static str> {
    object_receiver(source, file, call, callee, name, provider).map(|_| "java.lang.String")
}

fn object_receiver<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    callee: Node<'_>,
    name: &str,
    provider: &P,
) -> Option<ReferenceStringReceiver> {
    if call.kind() != "call_expression"
        || name != "toString"
        || callee.utf8_text(source.as_bytes()).ok()?.trim() != "toString"
        || call_argument_count(call) != 0
        || direct_trailing_lambda(call).is_some()
        || provider.source_for_file(file)? != source
    {
        return None;
    }

    let receiver = explicit_receiver_node(call, callee)?;
    if receiver.kind() == "call_expression" {
        return returned_receiver(source, file, call, receiver, provider);
    }
    if receiver_has_nullable_or_unproved_shape(source, receiver) {
        return None;
    }
    if let Some(canonical_type) = standard_object_parameter(source, file, call, receiver, provider)
    {
        return Some(ReferenceStringReceiver::Standard {
            canonical_type: canonical_type.into(),
            expression: super::SourceLocation {
                file: file.to_path_buf(),
                snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
                start_byte: receiver.start_byte(),
                end_byte: receiver.end_byte(),
            },
        });
    }
    let classifier = receiver_classifier(source, call, receiver, file, provider)?;
    // Keep this after structural receiver proof: indexed call-return inference
    // can re-enter this helper for call-shaped receivers, which are excluded
    // above before member resolution is queried.
    if !matches!(
        resolve_required_call(source, file, call, callee, name, provider),
        FactStatus::Unknown
    ) {
        return None;
    }
    if !matches!(
        classifier.kind.as_str(),
        "class" | "interface" | "enum" | "object" | "record"
    ) || !matches!(
        provider.is_ordinary_jvm_reference(&classifier),
        FactStatus::Established(true)
    ) {
        return None;
    }
    let indexed = provider
        .symbols_named(&classifier.name)
        .into_iter()
        .filter(|symbol| symbol.id == classifier)
        .collect::<Vec<_>>();
    let [symbol] = indexed.as_slice() else {
        return None;
    };
    let snapshot = provider.source_for_file(&classifier.file)?;
    (symbol.location.snapshot_hash == *blake3::hash(snapshot.as_bytes()).as_bytes())
        .then_some(ReferenceStringReceiver::Indexed(classifier))
}

fn standard_object_parameter<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    receiver: Node<'_>,
    provider: &P,
) -> Option<&'static str> {
    if !matches!(receiver.kind(), "identifier" | "simple_identifier") {
        return None;
    }
    let parameter = receiver.utf8_text(source.as_bytes()).ok()?.trim();
    if parameter_may_be_refined(source, file, call, parameter, provider) {
        return None;
    }
    let TypeRef {
        ty: KotlinType::Named { name, arguments },
        nullable: false,
    } = lexical_parameter_type(source, call, parameter)?
    else {
        return None;
    };
    if !arguments.is_empty() {
        return None;
    }
    let (simple, canonical) = match name.as_str() {
        "Any" | "kotlin.Any" => ("Any", "kotlin.Any"),
        // An explicit Java Object member has a platform String result;
        // Kotlin inserts a null check that plain Java dispatch cannot erase.
        _ => return None,
    };
    let mut root = call;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let package = root
        .named_children(&mut root.walk())
        .find(|node| node.kind() == "package_header")
        .and_then(|node| {
            node.named_children(&mut node.walk())
                .find(|child| child.kind() == "qualified_identifier")
        })
        .map(|node| node.utf8_text(source.as_bytes()))
        .transpose()
        .ok()?
        .unwrap_or_default()
        .trim();
    // The initial provider still has some textual package inference. Do not
    // use inconsistent namespace evidence to prove a builtin classifier.
    if package != package_name(source) {
        return None;
    }
    if provider.has_type_alias_in_package(package, &name)
        || !matches!(
            provider.resolve_type_in_file(file, package, &name),
            FactStatus::Unknown
        )
        || super::declaration_type_parameters(source, call)
            .iter()
            .any(|parameter| parameter == name.split('.').next().unwrap_or_default())
    {
        return None;
    }
    if name.contains('.') {
        return Some(canonical);
    }

    // Use actual import nodes: text in comments is not binding evidence.
    // Unusual escaped/commented import spellings require stronger resolution.
    let mut explicit = Vec::new();
    let mut unknown_star = false;
    for node in root
        .named_children(&mut root.walk())
        .filter(|node| node.kind() == "import")
    {
        let import = node.utf8_text(source.as_bytes()).ok()?;
        if import.contains('/') || import.contains('`') {
            return None;
        }
        let import = import
            .strip_prefix("import")?
            .trim()
            .trim_end_matches(';')
            .trim();
        let (path, alias) = import.split_once(" as ").unwrap_or((import, ""));
        let visible = if alias.is_empty() {
            path.rsplit('.').next()?
        } else {
            alias.trim()
        };
        if visible == simple {
            explicit.push(path.trim());
        }
        if path.ends_with(".*") && !super::is_known_jdk_wildcard(path) {
            unknown_star = true;
        }
    }
    if !explicit.is_empty() {
        return (explicit.as_slice() == [canonical]).then_some(canonical);
    }
    (!unknown_star).then_some(canonical)
}

fn parameter_may_be_refined<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    parameter: &str,
    provider: &P,
) -> bool {
    let mut callable = call;
    while !matches!(
        callable.kind(),
        "function_declaration" | "secondary_constructor"
    ) {
        let Some(parent) = callable.parent() else {
            return true;
        };
        callable = parent;
    }
    let mut stack = vec![callable];
    while let Some(node) = stack.pop() {
        if node.start_byte() >= call.start_byte() {
            continue;
        }
        if node.kind() == "call_expression" && node.end_byte() <= call.start_byte() {
            let passed_directly = node
                .named_children(&mut node.walk())
                .filter(|child| child.kind() == "value_arguments")
                .any(|arguments| {
                    arguments
                        .named_children(&mut arguments.walk())
                        .filter(|argument| argument.kind() == "value_argument")
                        .any(|argument| {
                            argument
                                .utf8_text(source.as_bytes())
                                .is_ok_and(|text| text.trim() == parameter)
                        })
                });
            if passed_directly {
                let safe = super::call_callee_node(node)
                    .and_then(|callee| {
                        callee
                            .utf8_text(source.as_bytes())
                            .ok()
                            .map(|name| (callee, name))
                    })
                    .is_some_and(|(callee, name)| {
                        let not_kotlin_contract = |target: &super::SymbolId| {
                            is_type_symbol(target)
                                || target
                                    .file
                                    .extension()
                                    .is_some_and(|extension| extension == "java")
                        };
                        match resolve_required_call(source, file, node, callee, name, provider) {
                            FactStatus::Established(target) => not_kotlin_contract(&target),
                            FactStatus::Ambiguous(targets) => {
                                !targets.is_empty() && targets.iter().all(not_kotlin_contract)
                            }
                            _ => false,
                        }
                    });
                if !safe {
                    return true;
                }
            }
        }
        if matches!(node.kind(), "is_expression" | "as_expression")
            && node.child_by_field_name("left").is_some_and(|left| {
                left.utf8_text(source.as_bytes())
                    .is_ok_and(|text| text.trim() == parameter)
            })
        {
            // A simple negative branch leaves the declared Any contract in
            // place. A positive test/cast can select a Java platform method
            // with a different result-null-check contract.
            let negative_branch = node.kind() == "is_expression"
                && node.parent().is_some_and(|parent| {
                    if parent.kind() != "if_expression" || call.end_byte() > parent.end_byte() {
                        return false;
                    }
                    let alternative = parent
                        .children(&mut parent.walk())
                        .find(|child| child.kind() == "else");
                    let in_else =
                        alternative.is_some_and(|token| call.start_byte() > token.end_byte());
                    let negated = node
                        .children(&mut node.walk())
                        .any(|child| child.kind() == "!is");
                    in_else != negated
                });
            if !negative_branch {
                return true;
            }
        }
        if node.kind() == "when_subject"
            && node.named_children(&mut node.walk()).any(|child| {
                child
                    .utf8_text(source.as_bytes())
                    .is_ok_and(|text| text.trim() == parameter)
            })
        {
            return true;
        }
        if node.kind() == "binary_expression"
            && node
                .children(&mut node.walk())
                .any(|child| matches!(child.kind(), "===" | "!=="))
            && node.named_children(&mut node.walk()).any(|child| {
                child
                    .utf8_text(source.as_bytes())
                    .is_ok_and(|text| text.trim() == parameter)
            })
        {
            return true;
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    false
}

fn returned_receiver<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    receiver: Node<'_>,
    provider: &P,
) -> Option<ReferenceStringReceiver> {
    if !direct_delegation_argument(call)
        || call_argument_count(receiver) != 0
        || direct_trailing_lambda(receiver).is_some()
    {
        return None;
    }
    let callee = super::call_callee_node(receiver)?;
    let base = explicit_receiver_node(receiver, callee)?;
    if receiver_has_nullable_or_unproved_shape(source, base) {
        return None;
    }
    let owner = receiver_classifier(source, receiver, base, file, provider)?;
    if !current_reference_classifier(&owner, provider) {
        return None;
    }
    let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
    let FactStatus::Established(producer) = resolve_reference_producer(&owner, name, provider)
    else {
        return None;
    };
    if !provider.is_java_callable_member(&producer) {
        return None;
    }
    let symbols = provider
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| symbol.id == producer)
        .collect::<Vec<_>>();
    let [symbol] = symbols.as_slice() else {
        return None;
    };
    let producer_source = provider.source_for_file(&producer.file)?;
    if symbol.location.snapshot_hash != *blake3::hash(producer_source.as_bytes()).as_bytes() {
        return None;
    }
    let nullability = provider.reference_return_nullability(&producer)?;
    let FactStatus::Established(TypeRef {
        ty: KotlinType::Named { name, arguments },
        ..
    }) = &symbol.ty
    else {
        return None;
    };
    if !arguments.is_empty()
        || name.is_empty()
        || name.starts_with("T:")
        || provider.has_type_alias_in_package(&producer.package, name)
        || super::kotlin_import_lines(producer_source).any(|import| {
            import
                .split_once(" as ")
                .is_some_and(|(_, alias)| alias.trim() == name)
        })
    {
        return None;
    }
    if !matches!(
        provider.resolve_type_in_file(&producer.file, &producer.package, name),
        FactStatus::Unknown
    ) {
        return None;
    }
    let canonical_type = super::standard_external_type(
        producer_source,
        provider,
        name,
        producer
            .file
            .extension()
            .is_some_and(|extension| extension == "java"),
    )?;
    // These final JDK classifiers have the toString return contract exercised
    // by the JVM tests. An arbitrary reference result needs a separate dispatch
    // and return-nullability proof, even if its classifier is resolved.
    if !matches!(
        canonical_type.as_str(),
        "java.util.UUID" | "java.lang.String"
    ) {
        return None;
    }
    // Known UUID/String members win over extensions on platform/nonnull
    // receivers. Only nullable dispatch selects Any?.toString and introduces
    // a Java qualifier: its extension/import/shadowing proof is separate.
    if nullability == super::ReferenceReturnNullability::Nullable
        && (provider
            .symbols_named("toString")
            .iter()
            .any(|symbol| symbol.id.receiver.is_some())
            || super::kotlin_import_lines(source).any(|import| {
                let (path, alias) = import.split_once(" as ").unwrap_or((import, ""));
                path.rsplit('.').next() == Some("toString") || alias.trim() == "toString"
            })
            || !java_qualifier_available(source, call, provider)
            || super::kotlin_import_lines(source)
                .any(|import| import.ends_with(".*") && !super::is_known_jdk_wildcard(import)))
    {
        return None;
    }
    Some(ReferenceStringReceiver::Returned {
        producer,
        producer_location: symbol.location.clone(),
        expression: super::SourceLocation {
            file: file.to_path_buf(),
            snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
            start_byte: receiver.start_byte(),
            end_byte: receiver.end_byte(),
        },
        canonical_type,
        nullability,
    })
}

fn resolve_reference_producer<P: SemanticProvider + ?Sized>(
    owner: &super::SymbolId,
    name: &str,
    provider: &P,
) -> FactStatus<super::SymbolId> {
    if !provider.has_complete_inheritance(owner) {
        return FactStatus::Unknown;
    }
    let mut path = owner.owner_path.clone();
    path.push(owner.name.clone());
    let direct = provider
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| {
            symbol.id.file == owner.file
                && symbol.id.owner_path == path
                && matches!(symbol.id.kind.as_str(), "function" | "method")
        })
        .collect::<Vec<_>>();
    let members = if direct.is_empty() {
        provider.inherited_members(owner, name)
    } else {
        direct
    };
    // The general member lookup intentionally excludes nullable getters. This
    // bounded transformation has a separate public no-arg return contract and
    // preserves nullable dispatch without widening other receiver inference.
    super::fact_from_candidates(
        members
            .into_iter()
            .filter(|member| provider.reference_return_nullability(&member.id).is_some())
            .map(|member| member.id.clone())
            .collect(),
    )
}

fn direct_delegation_argument(call: Node<'_>) -> bool {
    // Kotlin's concat/interpolation lowering can elide a platform receiver's
    // toString call, whereas a direct delegation argument uses virtual dispatch.
    // Keep this fact scoped to the context validated by the JVM comparisons.
    let Some(argument) = call.parent().filter(|node| node.kind() == "value_argument") else {
        return false;
    };
    let expressions = argument
        .named_children(&mut argument.walk())
        .filter(|node| !node.is_extra())
        .collect::<Vec<_>>();
    if !matches!(expressions.as_slice(), [expression] if expression.id() == call.id()) {
        return false;
    }
    argument
        .parent()
        .filter(|node| node.kind() == "value_arguments")
        .and_then(|arguments| arguments.parent())
        .is_some_and(|delegation| delegation.kind() == "constructor_delegation_call")
}

fn current_reference_classifier<P: SemanticProvider + ?Sized>(
    classifier: &super::SymbolId,
    provider: &P,
) -> bool {
    if !matches!(
        provider.is_ordinary_jvm_reference(classifier),
        FactStatus::Established(true)
    ) {
        return false;
    }
    let symbols = provider
        .symbols_named(&classifier.name)
        .into_iter()
        .filter(|symbol| symbol.id == *classifier)
        .collect::<Vec<_>>();
    let [symbol] = symbols.as_slice() else {
        return false;
    };
    provider
        .source_for_file(&classifier.file)
        .is_some_and(|source| {
            symbol.location.snapshot_hash == *blake3::hash(source.as_bytes()).as_bytes()
        })
}

pub(super) fn java_qualifier_available<P: SemanticProvider + ?Sized>(
    source: &str,
    call: Node<'_>,
    provider: &P,
) -> bool {
    !super::lexical_parameter_is_declared(source, call, "java")
        && !super::declaration_type_parameters(source, call)
            .iter()
            .any(|name| name == "java")
        && !super::is_proven_local_shadow(call, "java", source)
        && provider.symbols_named("java").is_empty()
        && !super::kotlin_import_lines(source).any(|import| {
            import.split_once(" as ").map_or_else(
                || import.rsplit('.').next() == Some("java"),
                |(_, alias)| alias.trim() == "java",
            )
        })
}

fn receiver_has_nullable_or_unproved_shape(source: &str, receiver: Node<'_>) -> bool {
    let Ok(spelling) = receiver.utf8_text(source.as_bytes()) else {
        return true;
    };
    spelling.contains("?.")
        || spelling.contains("!!")
        || matches!(
            receiver.kind(),
            "this_expression"
                | "super_expression"
                | "call_expression"
                | "string_literal"
                | "string_template"
                | "integer_literal"
                | "real_literal"
                | "boolean_literal"
                | "null_literal"
                | "parenthesized_expression"
        )
}

fn receiver_classifier<P: SemanticProvider + ?Sized>(
    source: &str,
    call: Node<'_>,
    receiver: Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<super::SymbolId> {
    match receiver.kind() {
        "identifier" | "simple_identifier" => {
            let name = receiver.utf8_text(source.as_bytes()).ok()?.trim();
            if let Some(ty) = lexical_parameter_type(source, call, name) {
                return resolve_nonnullable_reference_type(
                    source,
                    file,
                    &package_name(source),
                    &ty,
                    provider,
                );
            }
            if super::lexical_parameter_is_declared(source, call, name)
                || is_proven_local_shadow(receiver, name, source)
            {
                return None;
            }
            direct_unqualified_property_classifier(source, receiver, file, name, provider)
        }
        "navigation_expression" => {
            direct_qualified_property_classifier(source, call, receiver, file, provider)
        }
        _ => None,
    }
}

fn resolve_nonnullable_reference_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    package: &str,
    ty: &TypeRef,
    provider: &P,
) -> Option<super::SymbolId> {
    let TypeRef {
        ty: KotlinType::Named { name, .. },
        nullable: false,
    } = ty
    else {
        // Type parameters, primitives, nullable references, arrays, and
        // function types do not establish an indexed reference classifier.
        return None;
    };
    if name.is_empty()
        || provider.has_type_alias_in_package(package, name)
        || super::kotlin_import_lines(source).any(|import| {
            import
                .split_once(" as ")
                .is_some_and(|(_, alias)| alias.trim() == name)
        })
    {
        return None;
    }
    match provider.resolve_type_in_file(file, package, name) {
        FactStatus::Established(owner) if is_type_symbol(&owner) && owner.kind != "typealias" => {
            Some(owner)
        }
        FactStatus::Established(_)
        | FactStatus::Unknown
        | FactStatus::Inferred(_)
        | FactStatus::Ambiguous(_) => None,
    }
}

fn direct_unqualified_property_classifier<P: SemanticProvider + ?Sized>(
    source: &str,
    reference: Node<'_>,
    file: &Path,
    name: &str,
    provider: &P,
) -> Option<super::SymbolId> {
    if super::is_proven_local_shadow(reference, name, source) {
        return None;
    }
    let mut scope = reference.parent();
    while let Some(node) = scope {
        if matches!(node.kind(), "for_statement" | "catch_block") {
            return None;
        }
        if matches!(
            node.kind(),
            "class_declaration" | "interface_declaration" | "object_declaration"
        ) {
            break;
        }
        scope = node.parent();
    }
    let owner = super::enclosing_dispatch_owner(source, reference, file, provider)?;
    direct_property_type_owner(&owner, name, provider)
}

fn direct_qualified_property_classifier<P: SemanticProvider + ?Sized>(
    source: &str,
    call: Node<'_>,
    expression: Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<super::SymbolId> {
    let parts = expression
        .named_children(&mut expression.walk())
        .filter(|node| !node.is_extra())
        .collect::<Vec<_>>();
    let [base, property] = parts.as_slice() else {
        return None;
    };
    let property = property.utf8_text(source.as_bytes()).ok()?.trim();
    let owner = if base.utf8_text(source.as_bytes()).ok()?.trim() == "this" {
        super::enclosing_dispatch_owner(source, expression, file, provider)?
    } else if matches!(base.kind(), "identifier" | "simple_identifier") {
        let name = base.utf8_text(source.as_bytes()).ok()?.trim();
        let Some(ty) = lexical_parameter_type(source, call, name) else {
            if super::lexical_parameter_is_declared(source, call, name)
                || is_proven_local_shadow(*base, name, source)
            {
                return None;
            }
            return None;
        };
        resolve_nonnullable_reference_type(source, file, &package_name(source), &ty, provider)?
    } else {
        return None;
    };
    direct_property_type_owner(&owner, property, provider)
}

fn direct_property_type_owner<P: SemanticProvider + ?Sized>(
    owner: &super::SymbolId,
    property: &str,
    provider: &P,
) -> Option<super::SymbolId> {
    let mut owner_path = owner.owner_path.clone();
    owner_path.push(owner.name.clone());
    let properties = provider
        .symbols_named(property)
        .into_iter()
        .filter(|symbol| {
            symbol.id.kind == "property"
                && symbol.id.receiver.is_none()
                && symbol.id.file == owner.file
                && symbol.id.package == owner.package
                && symbol.id.owner_path == owner_path
        })
        .collect::<Vec<_>>();
    let [property_symbol] = properties.as_slice() else {
        return None;
    };
    if !provider.has_plain_instance_property_getter(&property_symbol.id) {
        return None;
    }
    let declaration_source = provider.source_for_file(&property_symbol.id.file)?;
    if property_symbol.location.snapshot_hash
        != *blake3::hash(declaration_source.as_bytes()).as_bytes()
    {
        return None;
    }
    if !source_property_type_is_explicit(declaration_source, property_symbol.location.start_byte) {
        return None;
    }
    let FactStatus::Established(ty) = &property_symbol.ty else {
        return None;
    };
    resolve_nonnullable_reference_type(
        declaration_source,
        &property_symbol.id.file,
        &property_symbol.id.package,
        ty,
        provider,
    )
}

fn source_property_type_is_explicit(source: &str, start_byte: usize) -> bool {
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
        if node.start_byte() == start_byte
            && matches!(node.kind(), "property_declaration" | "class_parameter")
        {
            return super::declaration_type_node(node).is_some()
                || (node.kind() == "class_parameter" && super::named_type_child(node).is_some());
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}
