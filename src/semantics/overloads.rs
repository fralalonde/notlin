//! Exact, deliberately narrow source overload applicability proofs.
//!
//! This is not a Kotlin overload resolver. It only narrows an existing
//! candidate set when every argument and every candidate parameter has a
//! closed, explicit type contract and unrelated types make all rejected
//! candidates provably inapplicable.

use super::{
    KotlinType, SemanticProvider, SymbolId, TypeRef, call_callee_node, declaration_type_parameters,
    lexical_parameter_type, object_calls, standard_external_type,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use tree_sitter::Node;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExactOverloadCall {
    pub location: super::SourceLocation,
    pub target: SymbolId,
    pub result_check: Option<String>,
    #[serde(default)]
    pub return_type: Option<TypeRef>,
}

thread_local! {
    // Cache syntax only. Binding/applicability depends on the current provider
    // and must be recomputed even when this declaration's source is unchanged.
    static TREES: RefCell<VecDeque<(PathBuf, [u8; 32], tree_sitter::Tree)>> = const { RefCell::new(VecDeque::new()) };
}

fn declaration_tree(file: &Path, source: &str, hash: [u8; 32]) -> Option<tree_sitter::Tree> {
    if let Some(tree) = TREES.with(|cache| {
        cache
            .borrow()
            .iter()
            .find(|(path, snapshot, _)| path == file && *snapshot == hash)
            .map(|(_, _, tree)| tree.clone())
    }) {
        return Some(tree);
    }
    let tree = super::parse_source(file, source)?;
    TREES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() == 16 {
            cache.pop_back();
        }
        cache.push_front((file.to_path_buf(), hash, tree.clone()));
    });
    Some(tree)
}

/// Return the unique applicable candidate only when all other candidates are
/// proved incompatible from exact argument types. `None` means that the
/// original candidate set must remain untouched.
pub(super) fn filter_exact_applicable<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    candidates: &[SymbolId],
    provider: &P,
) -> Option<SymbolId> {
    if candidates.len() < 2 || provider.source_for_file(file)? != source {
        return None;
    }
    let args = positional_arguments(call)?;
    let has_java_candidate = candidates.iter().any(|candidate| {
        candidate
            .file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
    });
    let argument_types = args
        .into_iter()
        .map(|argument| exact_argument_type(source, file, call, argument, provider))
        .collect::<Option<Vec<_>>>()?;
    // Java accepts primitive widening and boxing conversions. Keep every
    // numeric/primitive argument unresolved when a Java overload participates.
    if has_java_candidate
        && argument_types
            .iter()
            .any(|ty| matches!(ty, ExactType::Primitive(_)))
    {
        return None;
    }

    let mut applicable = Vec::new();
    for candidate in candidates {
        let parameters = candidate_parameter_types(candidate, provider)?;
        if parameters.len() != argument_types.len() {
            // A default or vararg might still make this candidate applicable;
            // those forms are rejected by `candidate_parameter_types`.
            continue;
        }
        if parameters
            .iter()
            .zip(&argument_types)
            .all(|(parameter, argument)| parameter == argument)
        {
            applicable.push(candidate.clone());
        }
    }
    if applicable.len() != 1 {
        return None;
    }
    let selected = applicable.remove(0);
    if inferred_function_result(call) && closed_return_type(&selected, provider).is_none() {
        return None;
    }
    result_check(source, file, call, &selected, provider).ok()?;
    Some(selected)
}

pub(super) fn establish<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    target: &SymbolId,
    provider: &P,
) -> Option<ExactOverloadCall> {
    if call.kind() != "call_expression" || provider.source_for_file(file)? != source {
        return None;
    }
    let count = provider
        .symbols_named(&target.name)
        .iter()
        .filter(|symbol| {
            symbol.id.file == target.file
                && symbol.id.owner_path == target.owner_path
                && matches!(symbol.id.kind.as_str(), "function" | "method")
        })
        .count();
    if count < 2 {
        return None;
    }
    Some(ExactOverloadCall {
        location: super::SourceLocation {
            file: file.to_path_buf(),
            snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
            start_byte: call.start_byte(),
            end_byte: call.end_byte(),
        },
        result_check: result_check(source, file, call, target, provider).ok()?,
        return_type: closed_return_type(target, provider),
        target: target.clone(),
    })
}

fn inferred_function_result(mut expression: Node<'_>) -> bool {
    while let Some(parent) = expression.parent() {
        match parent.kind() {
            "parenthesized_expression" => expression = parent,
            "function_body" => {
                return parent
                    .children(&mut parent.walk())
                    .any(|child| child.kind() == "=")
                    && parent.parent().is_some_and(|function| {
                        function.kind() == "function_declaration"
                            && super::declaration_type_node(function).is_none()
                    });
            }
            _ => return false,
        }
    }
    false
}

fn closed_return_type<P: SemanticProvider + ?Sized>(
    target: &SymbolId,
    provider: &P,
) -> Option<TypeRef> {
    let symbols = provider.symbols_named(&target.name);
    let mut matches = symbols.into_iter().filter(|symbol| symbol.id == *target);
    let symbol = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    let super::FactStatus::Established(ty) = &symbol.ty else {
        return None;
    };
    let source = provider.source_for_file(&target.file)?;
    if symbol.location.snapshot_hash != *blake3::hash(source.as_bytes()).as_bytes() {
        return None;
    }
    let java = target
        .file
        .extension()
        .is_some_and(|extension| extension == "java");
    let exact = canonical_type(source, &target.file, &ty.ty, java, provider)?;
    Some(TypeRef {
        ty: match exact {
            ExactType::Primitive(name) => KotlinType::Primitive(name.into()),
            ExactType::Reference(name) => KotlinType::Named {
                name: name.into(),
                arguments: Vec::new(),
            },
        },
        nullable: ty.nullable,
    })
}

fn result_check<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    target: &SymbolId,
    provider: &P,
) -> Result<Option<String>, ()> {
    let target_source = provider.source_for_file(&target.file).ok_or(())?;
    let symbols = provider
        .symbols_named(&target.name)
        .into_iter()
        .filter(|symbol| symbol.id == *target)
        .collect::<Vec<_>>();
    let [symbol] = symbols.as_slice() else {
        return Err(());
    };
    if symbol.location.snapshot_hash != *blake3::hash(target_source.as_bytes()).as_bytes() {
        return Err(());
    }
    let tree =
        declaration_tree(&target.file, target_source, symbol.location.snapshot_hash).ok_or(())?;
    let declaration = tree
        .root_node()
        .named_descendant_for_byte_range(symbol.location.start_byte, symbol.location.end_byte)
        .ok_or(())?;
    if !target
        .file
        .extension()
        .is_some_and(|extension| extension == "java")
    {
        return if super::source_calls::declares_checked_throws(target_source, declaration) {
            Err(())
        } else {
            Ok(None)
        };
    }
    if declaration.kind() != "method_declaration"
        || !provider.has_plain_java_package_nullability(&target.package)
        || super::source_calls::java_enclosing_type_has_annotations(declaration)
        || super::source_calls::java_method_declares_throws(target_source, declaration)
    {
        return Err(());
    }
    let ty_node = super::declaration_type_node(declaration).ok_or(())?;
    if matches!(
        ty_node.kind(),
        "integral_type" | "floating_point_type" | "boolean_type" | "void_type"
    ) {
        return Ok(None);
    }
    if ty_node
        .utf8_text(target_source.as_bytes())
        .map_err(|_| ())?
        .contains('@')
    {
        return Err(());
    }
    if !matches!(&symbol.ty, super::FactStatus::Established(TypeRef { ty: KotlinType::Named { arguments, .. }, .. }) if arguments.is_empty())
    {
        return Err(());
    }
    let nullability =
        super::source_calls::java_method_return_nullability(target_source, declaration).ok_or(())?;
    if nullability != super::ReferenceReturnNullability::Platform {
        return Ok(None);
    }
    let expected = super::container_calls::explicit_result_type(source, file, call, provider)?;
    if !expected.is_some_and(|expected| {
        !expected.ty.nullable
            && matches!(
                expected.ty.ty,
                KotlinType::Named { .. } | KotlinType::Array(_) | KotlinType::Function { .. }
            )
    }) {
        return Ok(None);
    }
    if !super::object_calls::java_qualifier_available(source, call, provider) {
        return Err(());
    }
    Ok(Some(format!("{}(...) must not be null", target.name)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExactType {
    Primitive(&'static str),
    Reference(&'static str),
}

fn exact_argument_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    context: Node<'_>,
    mut expression: Node<'_>,
    provider: &P,
) -> Option<ExactType> {
    loop {
        if expression.kind() == "parenthesized_expression" {
            let children = named_children(expression);
            let [inner] = children.as_slice() else {
                return None;
            };
            expression = *inner;
            continue;
        }
        break;
    }

    match expression.kind() {
        "string_literal" => return Some(ExactType::Reference("java.lang.String")),
        "character_literal" | "char_literal" => {
            return Some(ExactType::Primitive("Char"));
        }
        "boolean_literal" => {
            return Some(ExactType::Primitive("Boolean"));
        }
        "identifier" | "simple_identifier" => {
            let name = expression.utf8_text(source.as_bytes()).ok()?.trim();
            let TypeRef {
                ty,
                nullable: false,
            } = lexical_parameter_type(source, context, name)?
            else {
                return None;
            };
            return canonical_type(source, file, &ty, false, provider);
        }
        "call_expression" => {
            let callee = call_callee_node(expression)?;
            let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
            if name == "toString"
                && object_calls::established_object_to_string(
                    source, file, expression, callee, name, provider,
                ) == Some("java.lang.String")
            {
                return Some(ExactType::Reference("java.lang.String"));
            }
        }
        _ => {}
    }
    None
}

fn canonical_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    ty: &KotlinType,
    java_source: bool,
    provider: &P,
) -> Option<ExactType> {
    if provider.source_for_file(file)? != source {
        return None;
    }
    match ty {
        KotlinType::Primitive(name) => (!has_visible_type_alias(source, provider, name)
            && !super::kotlin_import_lines(source).any(|import| {
                let (path, alias) = import.split_once(" as ").unwrap_or((import, ""));
                let visible = if alias.is_empty() {
                    path.rsplit('.').next().unwrap_or("")
                } else {
                    alias.trim()
                };
                (path.ends_with(".*") && !super::is_known_jdk_wildcard(path))
                    || (visible == name && path != format!("kotlin.{name}"))
            })
            && !provider
                .symbols_named(name)
                .iter()
                .any(|symbol| super::is_type_symbol(&symbol.id)))
        .then(|| primitive(name))
        .flatten(),
        KotlinType::Named { name, arguments } if arguments.is_empty() => {
            if !name.contains('.') && has_visible_type_alias(source, provider, name) {
                return None;
            }
            let canonical = standard_external_type(source, provider, name, java_source)?;
            match canonical.as_str() {
                "java.lang.String" => Some(ExactType::Reference("java.lang.String")),
                "java.math.BigDecimal" => Some(ExactType::Reference("java.math.BigDecimal")),
                "java.util.UUID" => Some(ExactType::Reference("java.util.UUID")),
                _ => None,
            }
        }
        _ => None,
    }
}

fn primitive(name: &str) -> Option<ExactType> {
    Some(ExactType::Primitive(match name {
        "Boolean" => "Boolean",
        "Byte" => "Byte",
        "Short" => "Short",
        "Int" => "Int",
        "Long" => "Long",
        "Char" => "Char",
        "Float" => "Float",
        "Double" => "Double",
        _ => return None,
    }))
}

fn candidate_parameter_types<P: SemanticProvider + ?Sized>(
    candidate: &SymbolId,
    provider: &P,
) -> Option<Vec<ExactType>> {
    if !matches!(candidate.kind.as_str(), "function" | "method")
        || candidate.receiver.is_some()
        || !candidate.file.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("kt")
                || extension.eq_ignore_ascii_case("kts")
                || extension.eq_ignore_ascii_case("java")
        })
    {
        return None;
    }
    let source = provider.source_for_file(&candidate.file)?;
    let java_source = candidate
        .file
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("java"));
    let symbols = provider
        .symbols_named(&candidate.name)
        .into_iter()
        .filter(|symbol| symbol.id == *candidate)
        .collect::<Vec<_>>();
    let [symbol] = symbols.as_slice() else {
        return None;
    };
    if symbol.location.snapshot_hash != *blake3::hash(source.as_bytes()).as_bytes() {
        return None;
    }
    let tree = declaration_tree(&candidate.file, source, symbol.location.snapshot_hash)?;
    let declaration = tree
        .root_node()
        .named_descendant_for_byte_range(symbol.location.start_byte, symbol.location.end_byte)?;
    if declaration.start_byte() != symbol.location.start_byte
        || declaration.end_byte() != symbol.location.end_byte
        || declaration.kind()
            != if java_source {
                "method_declaration"
            } else {
                "function_declaration"
            }
        || super::node_name(declaration, source).as_deref() != Some(candidate.name.as_str())
    {
        return None;
    }
    if declaration.has_error()
        || !declaration_type_parameters(source, declaration).is_empty()
        || super::has_type_constraints(declaration)
    {
        return None;
    }
    let parameters = declaration.child_by_field_name("parameters").or_else(|| {
        named_children(declaration).into_iter().find(|node| {
            if java_source {
                node.kind() == "formal_parameters"
            } else {
                node.kind() == "function_value_parameters"
            }
        })
    })?;
    let parameter_nodes = named_children(parameters)
        .into_iter()
        .filter(|node| {
            if java_source {
                matches!(node.kind(), "formal_parameter" | "spread_parameter")
            } else {
                node.kind() == "parameter"
            }
        })
        .collect::<Vec<_>>();
    let mut result = Vec::with_capacity(parameter_nodes.len());
    for parameter in parameter_nodes {
        let signature = super::parameter_signature(source, parameter);
        if signature.is_empty()
            || signature.starts_with("vararg:")
            || !java_source && parameter_has_default(parameter, source)
        {
            return None;
        }
        let type_node = parameter
            .child_by_field_name("type")
            .or_else(|| super::named_type_child(parameter))?;
        let spelling = type_node.utf8_text(source.as_bytes()).ok()?.trim();
        if spelling.ends_with('?')
            || spelling.contains('<')
            || spelling.contains("->")
            || spelling.contains('?')
            || spelling.ends_with("[]")
        {
            return None;
        }
        if java_source {
            let java_primitive = match spelling {
                "boolean" => Some("Boolean"),
                "byte" => Some("Byte"),
                "short" => Some("Short"),
                "int" => Some("Int"),
                "long" => Some("Long"),
                "char" => Some("Char"),
                "float" => Some("Float"),
                "double" => Some("Double"),
                _ => None,
            };
            if let Some(name) = java_primitive {
                result.push(ExactType::Primitive(name));
                continue;
            }
        }
        let TypeRef { ty, nullable } = TypeRef::parse_with_type_parameters(
            spelling,
            &declaration_type_parameters(source, declaration),
        );
        if nullable {
            return None;
        }
        result.push(canonical_type(
            source,
            &candidate.file,
            &ty,
            java_source,
            provider,
        )?);
    }
    Some(result)
}

fn positional_arguments(call: Node<'_>) -> Option<Vec<Node<'_>>> {
    if super::direct_trailing_lambda(call).is_some() {
        return None;
    }
    let arguments = call.child_by_field_name("arguments").or_else(|| {
        named_children(call).into_iter().find(|child| {
            matches!(
                child.kind(),
                "value_arguments" | "argument_list" | "arguments" | "value_argument_list"
            )
        })
    })?;
    let mut values = Vec::new();
    for argument in named_children(arguments)
        .into_iter()
        .filter(|child| matches!(child.kind(), "value_argument" | "argument"))
    {
        if argument
            .children(&mut argument.walk())
            .any(|child| child.kind() == "=")
        {
            return None;
        }
        let expressions = named_children(argument)
            .into_iter()
            .filter(|child| !child.kind().contains("comment"));
        let expressions = expressions.collect::<Vec<_>>();
        let [expression] = expressions.as_slice() else {
            return None;
        };
        values.push(*expression);
    }
    Some(values)
}

fn parameter_has_default(parameter: Node<'_>, source: &str) -> bool {
    if parameter.child_by_field_name("default_value").is_some() {
        return true;
    }
    parameter
        .children(&mut parameter.walk())
        .any(|child| child.kind() == "=" && child.start_byte() < parameter.end_byte())
        || parameter
            .utf8_text(source.as_bytes())
            .is_ok_and(|text| text.contains('='))
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    node.named_children(&mut node.walk()).collect()
}

fn has_visible_type_alias<P: SemanticProvider + ?Sized>(
    source: &str,
    provider: &P,
    spelling: &str,
) -> bool {
    let package = super::package_name(source);
    if provider.has_type_alias_in_package(&package, spelling) {
        return true;
    }
    source
        .lines()
        .filter_map(|line| {
            let import = line.trim().strip_prefix("import ")?;
            let import = import.split(';').next().unwrap_or(import).trim();
            let (path, alias) = import
                .split_once(" as ")
                .map(|(path, alias)| (path.trim(), Some(alias.trim())))
                .unwrap_or((import, None));
            let simple = path.rsplit('.').next().unwrap_or(path);
            let visible = alias.unwrap_or(simple);
            (visible == spelling).then(|| {
                provider
                    .type_aliases()
                    .iter()
                    .any(|((alias_package, alias_name), _)| {
                        alias_name == simple
                            && (path == format!("{alias_package}.{simple}") || alias.is_some())
                    })
            })
        })
        .any(|is_alias| is_alias)
}
