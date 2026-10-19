//! Applicable JDK constructors, established independently of Java rendering.
use super::*;

pub(crate) fn established_platform_constructor<P: SemanticProvider + ?Sized>(
    source: &str,
    _file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<&'static str> {
    if call.kind() != "call_expression" || direct_trailing_lambda(call).is_some() {
        return None;
    }
    let mut cursor = call.walk();
    let target = call.named_children(&mut cursor).next()?;
    let spelling = target.utf8_text(source.as_bytes()).ok()?.trim();
    let canonical = match spelling {
        "BigDecimal" | "java.math.BigDecimal" => "java.math.BigDecimal",
        "StringBuilder" | "java.lang.StringBuilder" => "java.lang.StringBuilder",
        "IllegalArgumentException" | "java.lang.IllegalArgumentException" => {
            "java.lang.IllegalArgumentException"
        }
        "IllegalStateException" | "java.lang.IllegalStateException" => {
            "java.lang.IllegalStateException"
        }
        _ => return None,
    };
    if standard_external_type(source, provider, spelling, false).as_deref() != Some(canonical) {
        return None;
    }
    let root = spelling.split('.').next()?;
    if spelling.contains('.') {
        if is_proven_local_shadow(target, root, source)
            || !provider.symbols_named(root).is_empty()
            || provider.has_type_alias_in_package(&package_name(source), root)
            || kotlin_import_lines(source).any(|import| {
                let (path, alias) = import
                    .split_once(" as ")
                    .map_or((import, None), |(path, alias)| {
                        (path.trim(), Some(alias.trim()))
                    });
                alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == root
                    || path.ends_with(".*") && !is_known_jdk_wildcard(path) && path != "java.math.*"
            })
        {
            return None;
        }
    } else {
        // A constructor's classifier spelling does not prove that a callable
        // value, local/member function, or implicit receiver is absent.
        if same_package_factory_name_is_shadowed(source, call, spelling, provider)
            || is_proven_constructor_property_shadow(target, spelling, source)
        {
            return None;
        }
        // Explicitly imported JDK classifiers take precedence over wildcard
        // namespaces. Do not guess BigDecimal from wildcard imports alone.
        if canonical == "java.math.BigDecimal" {
            let imports = kotlin_import_lines(source)
                .filter(|import| import.rsplit('.').next() == Some(spelling))
                .collect::<Vec<_>>();
            if imports != [canonical] {
                return None;
            }
        }
    }
    match (canonical, call_argument_count(call)) {
        ("java.lang.StringBuilder", 0) => Some(canonical),
        ("java.lang.IllegalArgumentException" | "java.lang.IllegalStateException", 0) => {
            Some(canonical)
        }
        ("java.lang.IllegalArgumentException" | "java.lang.IllegalStateException", 1) => {
            (argument_kind(source, call, single_positional_argument(call)?, provider)
                == Some("String"))
            .then_some(canonical)
        }
        ("java.math.BigDecimal", 1) => {
            let argument = single_positional_argument(call)?;
            matches!(
                argument_kind(source, call, argument, provider),
                Some("Int" | "Long" | "Double" | "String")
            )
            .then_some(canonical)
        }
        _ => None,
    }
}

pub(super) fn established_builder_call<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<&'static str> {
    let callee = call_callee_node(call)?;
    let name = callee.utf8_text(source.as_bytes()).ok()?;
    if !matches!(name, "append" | "toString")
        || explicit_receiver_is_type_qualifier(source, call, callee, file, provider)
    {
        return None;
    }
    let receiver = explicit_receiver_type(source, call, callee, file, provider)?;
    if standard_external_type(source, provider, &receiver, false).as_deref()
        != Some("java.lang.StringBuilder")
    {
        return None;
    }
    match (name, call_argument_count(call)) {
        ("toString", 0) => Some("java.lang.String"),
        ("append", 1)
            if argument_kind(source, call, single_positional_argument(call)?, provider)
                .is_some() =>
        {
            Some("java.lang.StringBuilder")
        }
        _ => None,
    }
}

/// An immutable local keeps a proven initializer classifier within its block.
/// Search only direct preceding declarations: nested blocks, lambdas and fields
/// must not leak a same-spelled binding into this fact.
pub(super) fn established_local_platform_classifier<P: SemanticProvider + ?Sized>(
    source: &str,
    reference: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<&'static str> {
    let name = reference.utf8_text(source.as_bytes()).ok()?.trim();
    let mut child = reference;
    let block = loop {
        let parent = child.parent()?;
        if matches!(parent.kind(), "block" | "statements") {
            break parent;
        }
        if matches!(
            parent.kind(),
            "lambda_literal"
                | "function_literal"
                | "anonymous_function"
                | "function_declaration"
                | "class_declaration"
                | "object_declaration"
                | "object_literal"
                | "anonymous_object"
                | "companion_object"
                | "for_statement"
                | "when_expression"
                | "catch_block"
                | "catch_clause"
        ) {
            return None;
        }
        child = parent;
    };
    let mut cursor = block.walk();
    let declarations = block
        .named_children(&mut cursor)
        .filter(|candidate| {
            candidate.end_byte() <= child.start_byte() && candidate.kind() == "property_declaration"
        })
        .collect::<Vec<_>>();
    for declaration in declarations.into_iter().rev() {
        let mut cursor = declaration.walk();
        let children = declaration.children(&mut cursor).collect::<Vec<_>>();
        let equals = children.iter().position(|child| child.kind() == "=")?;
        let mut lhs = children[..equals].to_vec();
        let mut mentions_name = false;
        while let Some(node) = lhs.pop() {
            mentions_name |= matches!(node.kind(), "identifier" | "simple_identifier")
                && node.utf8_text(source.as_bytes()).ok() == Some(name);
            lhs.extend(node.named_children(&mut node.walk()));
        }
        if !mentions_name {
            continue;
        }
        if !children.iter().any(|child| child.kind() == "val")
            || declaration_identifier(declaration, source)
                .and_then(|node| node.utf8_text(source.as_bytes()).ok())
                != Some(name)
        {
            return None;
        }
        let initializer = children[equals + 1..]
            .iter()
            .find(|child| child.is_named())?;
        let classifier =
            established_platform_call_return_type(source, file, *initializer, provider)?;
        let variable = children
            .iter()
            .find(|child| child.kind() == "variable_declaration")?;
        if let Some(ty) = named_type_child(*variable) {
            let ty = TypeRef::parse(ty.utf8_text(source.as_bytes()).ok()?);
            let KotlinType::Named { name, arguments } = ty.ty else {
                return None;
            };
            if ty.nullable
                || !arguments.is_empty()
                || standard_external_type(source, provider, &name, false).as_deref()
                    != Some(classifier)
            {
                return None;
            }
        }
        return Some(classifier);
    }
    None
}

fn argument_kind<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    expression: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<&'static str> {
    match expression.kind() {
        "parenthesized_expression" | "prefix_expression" | "unary_expression" => {
            let mut cursor = expression.walk();
            let children = expression.named_children(&mut cursor).collect::<Vec<_>>();
            let [inner] = children.as_slice() else {
                return None;
            };
            if expression.kind() != "parenthesized_expression" {
                let text = expression.utf8_text(source.as_bytes()).ok()?.trim();
                if !text.starts_with(['-', '+']) {
                    return None;
                }
            }
            let kind = argument_kind(source, call, *inner, provider)?;
            (expression.kind() == "parenthesized_expression" || kind != "String").then_some(kind)
        }
        "string_literal" => Some("String"),
        "number_literal" | "float_literal" | "real_literal" | "long_literal" => {
            let text = expression
                .utf8_text(source.as_bytes())
                .ok()?
                .replace('_', "");
            if let Some(value) = text.strip_suffix('L') {
                value.parse::<i64>().ok().map(|_| "Long")
            } else if text.contains(['.', 'e', 'E']) && !text.ends_with(['f', 'F']) {
                text.parse::<f64>().ok().map(|_| "Double")
            } else {
                text.parse::<i32>().ok().map(|_| "Int")
            }
        }
        "identifier" | "simple_identifier" => {
            let name = expression.utf8_text(source.as_bytes()).ok()?.trim();
            let ty = lexical_parameter_type(source, call, name)?;
            if ty.nullable {
                return None;
            }
            match ty.ty {
                KotlinType::Primitive(ref name)
                    if matches!(name.as_str(), "Int" | "Long" | "Double") =>
                {
                    // Primitive spellings in source type positions may also be
                    // imported classifiers or aliases. Literal types above do
                    // not depend on those spellings.
                    if !provider.symbols_named(name).is_empty()
                        || provider.has_type_alias_in_package(&package_name(source), name)
                        || kotlin_import_lines(source).any(|import| {
                            let (path, alias) = import
                                .split_once(" as ")
                                .map_or((import, None), |(path, alias)| {
                                    (path.trim(), Some(alias.trim()))
                                });
                            alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == name
                                && path != format!("kotlin.{name}")
                                || path.ends_with(".*")
                                    && !is_known_jdk_wildcard(path)
                                    && path != "kotlin.*"
                        })
                    {
                        return None;
                    }
                    match name.as_str() {
                        "Int" => Some("Int"),
                        "Long" => Some("Long"),
                        "Double" => Some("Double"),
                        _ => None,
                    }
                }
                KotlinType::Named {
                    ref name,
                    ref arguments,
                } if arguments.is_empty()
                    && standard_external_type(source, provider, name, false).as_deref()
                        == Some("java.lang.String") =>
                {
                    Some("String")
                }
                _ => None,
            }
        }
        _ => None,
    }
}
