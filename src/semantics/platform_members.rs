//! Ordinary external JVM calls are signature applicability, not lowering rules.
//!
//! These bounded library declarations supply missing classpath facts. The same
//! resolver proves arguments, dispatch and results for every signature; adding
//! a declaration does not add a new translation algorithm.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Dispatch {
    Constructor,
    Static,
    Instance,
}

struct Signature {
    owner: &'static str,
    name: &'static str,
    dispatch: Dispatch,
    parameters: &'static [&'static [&'static str]],
    result: &'static str,
}

const DECIMAL: &str = "java.math.BigDecimal";
const BUILDER: &str = "java.lang.StringBuilder";
const STRING: &str = "java.lang.String";

macro_rules! signature {
    ($owner:expr, $name:expr, $dispatch:ident, $parameters:expr, $result:expr) => {
        Signature {
            owner: $owner,
            name: $name,
            dispatch: Dispatch::$dispatch,
            parameters: $parameters,
            result: $result,
        }
    };
}

// Parameter alternatives describe the currently proven overload subset, not
// arbitrary Java assignability. Unknown, nullable and generic values fail closed.
const SIGNATURES: &[Signature] = &[
    signature!(
        DECIMAL,
        "<init>",
        Constructor,
        &[&["kotlin.Int", "kotlin.Long", "kotlin.Double", STRING]],
        DECIMAL
    ),
    signature!(BUILDER, "<init>", Constructor, &[], BUILDER),
    signature!(
        "java.lang.IllegalArgumentException",
        "<init>",
        Constructor,
        &[],
        "java.lang.IllegalArgumentException"
    ),
    signature!(
        "java.lang.IllegalArgumentException",
        "<init>",
        Constructor,
        &[&[STRING]],
        "java.lang.IllegalArgumentException"
    ),
    signature!(
        "java.lang.IllegalStateException",
        "<init>",
        Constructor,
        &[],
        "java.lang.IllegalStateException"
    ),
    signature!(
        "java.lang.IllegalStateException",
        "<init>",
        Constructor,
        &[&[STRING]],
        "java.lang.IllegalStateException"
    ),
    signature!(DECIMAL, "add", Instance, &[&[DECIMAL]], DECIMAL),
    signature!(DECIMAL, "subtract", Instance, &[&[DECIMAL]], DECIMAL),
    signature!(DECIMAL, "multiply", Instance, &[&[DECIMAL]], DECIMAL),
    signature!(DECIMAL, "divide", Instance, &[&[DECIMAL]], DECIMAL),
    signature!(DECIMAL, "remainder", Instance, &[&[DECIMAL]], DECIMAL),
    signature!(DECIMAL, "compareTo", Instance, &[&[DECIMAL]], "kotlin.Int"),
    signature!(DECIMAL, "abs", Instance, &[], DECIMAL),
    signature!(DECIMAL, "negate", Instance, &[], DECIMAL),
    signature!(DECIMAL, "stripTrailingZeros", Instance, &[], DECIMAL),
    signature!(DECIMAL, "toPlainString", Instance, &[], STRING),
    signature!(
        BUILDER,
        "append",
        Instance,
        &[&["kotlin.Int", "kotlin.Long", "kotlin.Double", STRING]],
        BUILDER
    ),
    signature!(BUILDER, "toString", Instance, &[], STRING),
    signature!(
        "java.util.UUID",
        "randomUUID",
        Static,
        &[],
        "java.util.UUID"
    ),
    signature!(
        "java.util.Locale",
        "getDefault",
        Static,
        &[],
        "java.util.Locale"
    ),
    signature!("java.time.Instant", "now", Static, &[], "java.time.Instant"),
];

pub(super) fn applicable<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    owner: &str,
    name: &str,
    dispatch: Dispatch,
    provider: &P,
) -> Option<&'static str> {
    let arguments = overloads::positional_arguments(call)?;
    let arity = arguments.len();
    let mut matches = SIGNATURES
        .iter()
        .filter(|signature| {
            signature.owner == owner
                && signature.name == name
                && signature.dispatch == dispatch
                && signature.parameters.len() == arity
        })
        .filter(|signature| {
            signature
                .parameters
                .iter()
                .zip(&arguments)
                .all(|(alternatives, argument)| {
                    alternatives.iter().any(|expected| {
                        value_has_type(source, file, call, *argument, expected, provider)
                    })
                })
        });
    let selected = matches.next()?;
    matches.next().is_none().then_some(selected.result)
}

pub(super) fn establish<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<&'static str> {
    let callee = call_callee_node(call)?;
    let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
    // A negative prefilter avoids resolving unrelated expressions recursively.
    if !SIGNATURES.iter().any(|signature| signature.name == name) {
        return None;
    }
    let receiver = explicit_receiver_node(call, callee)?;
    let static_call = explicit_receiver_is_type_qualifier(source, call, callee, file, provider);
    if static_call {
        if !matches!(
            resolve_required_call(source, file, call, callee, name, provider),
            FactStatus::Unknown
        ) {
            return None;
        }
        let owner = explicit_receiver_type(source, call, callee, file, provider)?;
        let owner =
            standard_external_type(source, provider, &owner, call.kind() == "method_invocation")?;
        return applicable(source, file, call, &owner, name, Dispatch::Static, provider);
    }
    for signature in SIGNATURES
        .iter()
        .filter(|signature| signature.name == name && signature.dispatch == Dispatch::Instance)
    {
        if value_has_type(source, file, call, receiver, signature.owner, provider) {
            return applicable(
                source,
                file,
                call,
                signature.owner,
                name,
                Dispatch::Instance,
                provider,
            );
        }
    }
    None
}

fn value_has_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    context: tree_sitter::Node<'_>,
    expression: tree_sitter::Node<'_>,
    expected: &str,
    provider: &P,
) -> bool {
    let scalar = argument_kind(source, context, expression, provider).map(|kind| match kind {
        "Int" => "kotlin.Int",
        "Long" => "kotlin.Long",
        "Double" => "kotlin.Double",
        "String" => STRING,
        _ => "",
    });
    if scalar.is_some() {
        return scalar == Some(expected);
    }
    match expression.kind() {
        "identifier" | "simple_identifier" => {
            let Ok(spelling) = expression.utf8_text(source.as_bytes()) else {
                return false;
            };
            let parameter = lexical_parameter_type(source, context, spelling.trim()).is_some_and(|ty| {
                !ty.nullable && matches!(ty.ty, KotlinType::Named { name, arguments }
                    if arguments.is_empty() && standard_external_type(source, provider, &name, false).as_deref() == Some(expected))
            });
            parameter
                || platform_constructors::established_local_platform_classifier(
                    source, expression, file, provider,
                ) == Some(expected)
                || unqualified_instance_property_classifier(
                    source,
                    expression,
                    file,
                    spelling.trim(),
                    provider,
                )
                .as_deref()
                    == Some(expected)
        }
        "navigation_expression" => {
            if established_direct_property_receiver_type(
                source, context, expression, file, provider,
            )
            .as_deref()
                == Some(expected)
            {
                return true;
            }
            let Ok(text) = expression.utf8_text(source.as_bytes()) else {
                return false;
            };
            let Some((qualifier, member)) = text.trim().rsplit_once('.') else {
                return false;
            };
            // Source-backed fields use the property path above. External fields
            // require declarations too; their names alone never prove a type.
            const FIELDS: &[(&str, &str, &str)] = &[
                (DECIMAL, "ZERO", DECIMAL),
                (DECIMAL, "ONE", DECIMAL),
                (DECIMAL, "TEN", DECIMAL),
            ];
            let root = qualifier.trim().split('.').next().unwrap_or("");
            !is_proven_local_shadow(expression, root, source)
                && !provider
                    .symbols_named(root)
                    .iter()
                    .any(|symbol| !is_type_symbol(&symbol.id))
                && standard_external_type(source, provider, qualifier.trim(), false)
                    .is_some_and(|owner| FIELDS.contains(&(owner.as_str(), member, expected)))
        }
        "call_expression" => {
            established_platform_call_return_type(source, file, expression, provider)
                == Some(expected)
        }
        _ => false,
    }
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
