//! Ordinary external JVM calls are signature applicability, not lowering rules.
//!
//! These bounded library declarations supply missing classpath facts. The same
//! resolver proves arguments, dispatch and results for every signature; adding
//! a declaration does not add a new translation algorithm.
use super::*;
use crate::facts::classpath::{intern, intern_alternatives};

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

/// One member row materialized from classpath facts for this run.
///
/// Parameter spellings are the Kotlin-side names the resolver already proves
/// (`kotlin.Int` for a JVM `int`/`java.lang.Integer`), because `value_has_type`
/// answers in those spellings. The result is the JVM binary owner name, interned
/// for the process: `establish()` returns `&'static str` at every call site, and
/// a runtime string cannot satisfy that without an arena (see
/// `facts::classpath::intern`).
pub(super) struct ClasspathRow {
    owner: String,
    name: String,
    dispatch: Dispatch,
    parameters: Vec<&'static [&'static str]>,
    result: &'static str,
}

/// One signature row, seen through the same accessors whether it came from the
/// static fallback table or from this run's classpath facts.
///
/// Deliberate shape (versus giving `Signature` `Cow` fields): the table stays a
/// `const` of borrowed literals, no `OnceLock`/lazy init appears in the hot
/// path, and `applicable()` keeps exactly one filter algorithm over one row
/// view instead of two near-duplicates. Classpath rows are owned per decision
/// and dropped with it; only the `&'static str` result needs the arena.
#[derive(Clone, Copy)]
enum Row<'a> {
    Table(&'a Signature),
    Classpath(&'a ClasspathRow),
}

impl Row<'_> {
    fn owner(&self) -> &str {
        match self {
            Self::Table(signature) => signature.owner,
            Self::Classpath(row) => &row.owner,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::Table(signature) => signature.name,
            Self::Classpath(row) => &row.name,
        }
    }

    fn dispatch(&self) -> Dispatch {
        match self {
            Self::Table(signature) => signature.dispatch,
            Self::Classpath(row) => row.dispatch,
        }
    }

    fn parameters(&self) -> &[&'static [&'static str]] {
        match self {
            Self::Table(signature) => signature.parameters,
            Self::Classpath(row) => &row.parameters,
        }
    }

    fn result(&self) -> &'static str {
        match self {
            Self::Table(signature) => signature.result,
            Self::Classpath(row) => row.result,
        }
    }
}

enum RowIter<'a> {
    Table(std::slice::Iter<'static, Signature>),
    Classpath(std::slice::Iter<'a, ClasspathRow>),
}

impl<'a> Iterator for RowIter<'a> {
    type Item = Row<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Table(rows) => rows.next().map(Row::Table),
            Self::Classpath(rows) => rows.next().map(Row::Classpath),
        }
    }
}

/// Where the applicability resolver reads its rows this run.
///
/// The two sources are exclusive per decision, never merged: when classpath
/// facts cover an owner they are the whole truth for it, and the hand-declared
/// rows for that owner are stale bootstrapping, not extra candidates. Merging
/// them would fake unique applicability or fake ambiguity depending on which
/// side happens to be narrower.
pub(super) enum SignatureSource<'a> {
    Table,
    Classpath(&'a [ClasspathRow]),
}

/// The member a decision is about.
#[derive(Clone, Copy)]
pub(super) struct Target<'a> {
    pub(super) owner: &'a str,
    pub(super) name: &'a str,
    pub(super) dispatch: Dispatch,
}

impl<'a> SignatureSource<'a> {
    fn rows(&self) -> RowIter<'a> {
        match self {
            Self::Table => RowIter::Table(SIGNATURES.iter()),
            Self::Classpath(rows) => {
                let rows: &'a [ClasspathRow] = rows;
                RowIter::Classpath(rows.iter())
            }
        }
    }
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
    target: Target<'_>,
    signatures: SignatureSource<'_>,
    provider: &P,
) -> Option<&'static str> {
    let arguments = overloads::positional_arguments(call)?;
    let arity = arguments.len();
    let mut matches = signatures
        .rows()
        .filter(|row| {
            row.owner() == target.owner
                && row.name() == target.name
                && row.dispatch() == target.dispatch
                && row.parameters().len() == arity
        })
        .filter(|row| {
            row.parameters()
                .iter()
                .zip(&arguments)
                .all(|(alternatives, argument)| {
                    alternatives.iter().any(|expected| {
                        value_has_type(source, file, call, *argument, expected, provider)
                            || classpath_value_has_type(
                                source, file, call, *argument, expected, provider,
                            )
                    })
                })
        });
    let selected = matches.next()?;
    matches.next().is_none().then_some(selected.result())
}

/// How a classpath fact source answered one call.
///
/// `NotApplicable` is a *final negative*: the source covers this owner, so the
/// fallback table's rows for the same owner must not be consulted afterwards
/// (they are stale facts, not additional candidates).
enum ClasspathAnswer {
    Established(&'static str),
    NotApplicable,
    Unavailable,
}

pub(super) fn establish<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<&'static str> {
    match establish_from_classpath(source, file, call, provider) {
        ClasspathAnswer::Established(result) => return Some(result),
        ClasspathAnswer::NotApplicable => return None,
        ClasspathAnswer::Unavailable => {}
    }
    establish_from_table(source, file, call, provider)
}

/// Whether no *source* declaration owns this call.
///
/// [`resolve_required_call`] layers Track B call-site facts on top of the source
/// index, so an `Established` answer may be the helper stating that the callee is
/// exactly the external member being asked about. That is not a competing
/// declaration; only an indexed symbol keeps the platform proof away.
fn source_call_is_unknown<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    callee: tree_sitter::Node<'_>,
    name: &str,
    provider: &P,
) -> bool {
    match resolve_required_call(source, file, call, callee, name, provider) {
        FactStatus::Unknown => true,
        FactStatus::Established(target) => crate::facts::is_fact_symbol(&target),
        FactStatus::Inferred(_) | FactStatus::Ambiguous(_) => false,
    }
}

/// The pre-helper path, unchanged: a jar-less run must take exactly this code.
fn establish_from_table<P: SemanticProvider + ?Sized>(
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
        // Only a *source* declaration owns this call. A Track B call-site fact
        // for the same static member (`Locale.getDefault()` resolves to
        // `java.util.Locale` in K2) is another statement of the same external
        // truth, not a competing declaration; refusing it would drop the
        // platform proof exactly when the helper runs.
        if !source_call_is_unknown(source, file, call, callee, name, provider) {
            return None;
        }
        let owner = explicit_receiver_type(source, call, callee, file, provider)?;
        let owner =
            standard_external_type(source, provider, &owner, call.kind() == "method_invocation")?;
        return applicable(
            source,
            file,
            call,
            Target {
                owner: &owner,
                name,
                dispatch: Dispatch::Static,
            },
            SignatureSource::Table,
            provider,
        );
    }
    // Instance dispatch proves the receiver twice over: by the owner identity
    // its declared type resolves to (an import alias names the same class a
    // plain spelling does), and by the platform classifier proofs the table
    // accepts. A receiver typed `Any`, `String` or an unrelated class matches
    // neither, whatever the row's name says.
    let receiver_owner = receiver_owner(source, file, call, receiver, provider);
    for signature in SIGNATURES
        .iter()
        .filter(|signature| signature.name == name && signature.dispatch == Dispatch::Instance)
    {
        if receiver_owner.as_deref() == Some(signature.owner)
            || value_has_type(source, file, call, receiver, signature.owner, provider)
        {
            return applicable(
                source,
                file,
                call,
                Target {
                    owner: signature.owner,
                    name,
                    dispatch: Dispatch::Instance,
                },
                SignatureSource::Table,
                provider,
            );
        }
    }
    None
}

/// The same resolver, reading the owner's classpath facts instead of the table.
///
/// Only owners this file can name as an external JVM type reach the source:
/// a fully-qualified spelling, an exact import, or a platform classifier the
/// syntax layer already canonicalizes. Kotlin owners are deliberately excluded
/// — reflection cannot see extension functions or default bodies, and those
/// callees belong to the Kotlin fact track, not to this one.
fn establish_from_classpath<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> ClasspathAnswer {
    if !provider.has_classpath_facts() {
        return ClasspathAnswer::Unavailable;
    }
    let Some(callee) = call_callee_node(call) else {
        return ClasspathAnswer::Unavailable;
    };
    let Ok(name) = callee.utf8_text(source.as_bytes()) else {
        return ClasspathAnswer::Unavailable;
    };
    let name = name.trim();
    if name.is_empty() {
        return ClasspathAnswer::Unavailable;
    }
    let Some(receiver) = explicit_receiver_node(call, callee) else {
        return ClasspathAnswer::Unavailable;
    };
    let (owner, dispatch) =
        if explicit_receiver_is_type_qualifier(source, call, callee, file, provider) {
            // Same guard as the table path: a call a *source* declaration
            // already resolves is not an external member. Helper facts for the
            // same external member do not disqualify it.
            if !source_call_is_unknown(source, file, call, callee, name, provider) {
                return ClasspathAnswer::Unavailable;
            }
            let Some(spelling) = explicit_receiver_type(source, call, callee, file, provider)
            else {
                return ClasspathAnswer::Unavailable;
            };
            let Some(owner) = classpath_owner(source, provider, &spelling) else {
                return ClasspathAnswer::Unavailable;
            };
            (owner, Dispatch::Static)
        } else {
            let Some(owner) = receiver_owner(source, file, call, receiver, provider) else {
                return ClasspathAnswer::Unavailable;
            };
            (owner, Dispatch::Instance)
        };
    let Some(rows) = classpath_rows(provider, &owner, name, dispatch) else {
        return ClasspathAnswer::Unavailable;
    };
    match applicable(
        source,
        file,
        call,
        Target {
            owner: &owner,
            name,
            dispatch,
        },
        SignatureSource::Classpath(&rows),
        provider,
    ) {
        Some(result) => ClasspathAnswer::Established(result),
        None => ClasspathAnswer::NotApplicable,
    }
}

/// Rows for one `owner`/`name`/`dispatch`, or `None` when the source has no
/// facts for that owner at all. An empty `Some` is a real answer: the owner is
/// known and does not declare that member.
fn classpath_rows<P: SemanticProvider + ?Sized>(
    provider: &P,
    owner: &str,
    name: &str,
    dispatch: Dispatch,
) -> Option<Vec<ClasspathRow>> {
    let facts = provider.classpath_member_facts(owner)?;
    let rows = facts
        .iter()
        .filter_map(|member| {
            let member_dispatch = if member.name == "<init>" {
                Dispatch::Constructor
            } else if member.static_member {
                Dispatch::Static
            } else {
                Dispatch::Instance
            };
            if member.name != name || member_dispatch != dispatch {
                return None;
            }
            let parameters = member
                .parameter_types
                .iter()
                .map(|parameter| {
                    let alternative = classifier(parameter);
                    intern_alternatives(std::slice::from_ref(&alternative))
                })
                .collect();
            let result = if member.name == "<init>" {
                owner.to_string()
            } else {
                classifier(&member.result_type)
            };
            Some(ClasspathRow {
                owner: owner.to_string(),
                name: name.to_string(),
                dispatch,
                parameters,
                result: intern(&result),
            })
        })
        .collect();
    Some(rows)
}

/// JVM classifier name as the resolver's argument types spell it.
fn classifier(name: &str) -> String {
    match name {
        "<void>" => "kotlin.Unit".to_string(),
        "int" | "java.lang.Integer" => "kotlin.Int".to_string(),
        "long" | "java.lang.Long" => "kotlin.Long".to_string(),
        "double" | "java.lang.Double" => "kotlin.Double".to_string(),
        "float" | "java.lang.Float" => "kotlin.Float".to_string(),
        "boolean" | "java.lang.Boolean" => "kotlin.Boolean".to_string(),
        "short" | "java.lang.Short" => "kotlin.Short".to_string(),
        "byte" | "java.lang.Byte" => "kotlin.Byte".to_string(),
        "char" | "java.lang.Character" => "kotlin.Char".to_string(),
        other => other.to_string(),
    }
}

/// The owner a classpath fact source may be asked about for `spelling`.
///
/// Deliberately narrow: a fully-qualified name, an exact import, or a platform
/// classifier the syntax layer canonicalizes. Kotlin packages are never
/// accepted, and a type the index declares itself always wins over an external
/// one with the same name.
fn classpath_owner<P: SemanticProvider + ?Sized>(
    source: &str,
    provider: &P,
    spelling: &str,
) -> Option<String> {
    let spelling = spelling
        .trim()
        .trim_end_matches('?')
        .split('<')
        .next()
        .unwrap_or("")
        .trim();
    if spelling.is_empty() {
        return None;
    }
    let simple = spelling.rsplit('.').next().unwrap_or(spelling);
    if provider
        .symbols_named(simple)
        .iter()
        .any(|symbol| is_type_symbol(&symbol.id))
    {
        return None;
    }
    if is_external_owner(spelling) {
        return Some(spelling.to_string());
    }
    if spelling.contains('.') {
        return None;
    }
    kotlin_import_lines(source)
        .find_map(|import| {
            let (path, alias) = import
                .split_once(" as ")
                .map_or((import, None), |(path, alias)| {
                    (path.trim(), Some(alias.trim()))
                });
            let imported = alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path));
            (imported == simple && is_external_owner(path)).then(|| path.to_string())
        })
        .or_else(|| {
            standard_external_type(source, provider, spelling, false)
                .filter(|owner| is_external_owner(owner))
        })
}

/// A binary name whose members reflection can actually answer for.
fn is_external_owner(path: &str) -> bool {
    path.contains('.')
        && !path.starts_with("kotlin.")
        && path.split('.').all(|segment| !segment.is_empty())
}

/// The owner identity a receiver's *declared* type resolves to.
///
/// This is a spelling-and-imports resolution, not a fact source: `Decimal`
/// imported as an alias for `java.math.BigDecimal` and the plain spelling
/// `BigDecimal` answer the same binary owner, while a foreign class that merely
/// spells `BigDecimal` answers none.
fn receiver_owner<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    receiver: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<String> {
    match receiver.kind() {
        "identifier" | "simple_identifier" => {
            let spelling = receiver.utf8_text(source.as_bytes()).ok()?.trim();
            let ty = container_types::parameter_type(source, file, call, spelling, provider)
                .or_else(|| lexical_parameter_type(source, call, spelling))?;
            if ty.nullable {
                return None;
            }
            let KotlinType::Named { name, arguments } = ty.ty else {
                return None;
            };
            if !arguments.is_empty() {
                return None;
            }
            classpath_owner(source, provider, &name)
        }
        "navigation_expression" => {
            let owner =
                established_direct_property_receiver_type(source, call, receiver, file, provider)?;
            classpath_owner(source, provider, &owner)
        }
        _ => None,
    }
}

/// `value_has_type` for an argument whose declared type is itself external.
///
/// The platform spellings the table accepts are canonical classifiers the
/// syntax layer already knows; a classpath row can name any classpath type, so
/// a parameter typed `com.example.Foo` must be provable from a local's declared
/// type. Both row sources use this, which is what makes an import alias an
/// argument proof in either one: the argument is compared by the owner identity
/// its declared type resolves to, exactly like the receiver.
fn classpath_value_has_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    expression: tree_sitter::Node<'_>,
    expected: &str,
    provider: &P,
) -> bool {
    if !matches!(expression.kind(), "identifier" | "simple_identifier") {
        return false;
    }
    let Ok(spelling) = expression.utf8_text(source.as_bytes()) else {
        return false;
    };
    let spelling = spelling.trim();
    let Some(ty) = container_types::parameter_type(source, file, call, spelling, provider)
        .or_else(|| lexical_parameter_type(source, call, spelling))
    else {
        return false;
    };
    if ty.nullable {
        return false;
    }
    let KotlinType::Named { name, arguments } = ty.ty else {
        return false;
    };
    if !arguments.is_empty() {
        return false;
    }
    classpath_owner(source, provider, &name).as_deref() == Some(expected)
}

/// A bounded external field declaration, shared by dispatch and emission.
/// Returns (canonical JVM owner, field name, result classifier), never a
/// guess based on capitalization. Aliases and shadowing use the same owner
/// identity proof as ordinary external calls.
pub(crate) fn external_static_field_access<P: SemanticProvider + ?Sized>(
    source: &str,
    expression: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<(&'static str, &'static str, &'static str)> {
    if expression.kind() != "navigation_expression" {
        return None;
    }
    let text = expression.utf8_text(source.as_bytes()).ok()?;
    let (qualifier, member) = text.trim().rsplit_once('.')?;
    let root = qualifier.trim().split('.').next().unwrap_or("");
    if is_proven_local_shadow(expression, root, source)
        || provider
            .symbols_named(root)
            .iter()
            .any(|symbol| !is_type_symbol(&symbol.id))
    {
        return None;
    }
    let owner = classpath_owner(source, provider, qualifier.trim())?;
    const FIELDS: &[(&str, &str, &str)] = &[
        (DECIMAL, "ZERO", DECIMAL),
        (DECIMAL, "ONE", DECIMAL),
        (DECIMAL, "TEN", DECIMAL),
    ];
    FIELDS
        .iter()
        .find(|(declaring, name, _)| *declaring == owner && *name == member)
        .copied()
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
            external_static_field_access(source, expression, provider)
                .is_some_and(|(_, _, result)| result == expected)
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
