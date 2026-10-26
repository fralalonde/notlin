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
//
// The `Dispatch::Constructor` rows below are the **jar-less FALLBACK** for the
// four bounded owner families: they are consulted only when the run's fact
// source has no usable answer for that owner
// (`platform_constructors::fallback_constructor_table`). An owner covered by
// classpath facts is answered from its real `<init>` rows, and "no applicable
// constructor" there is final. Do not extend these rows with overloads the JDK
// has but no test proves: the sourced path is what makes them work.
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

/// Whether a classpath fact source covers an owner for constructor
/// applicability, and what it answered.
///
/// Same exclusivity rule as [`ClasspathAnswer`] for members: an owner the source
/// covers is the whole truth for that owner, so *no applicable constructor* is a
/// final negative rather than a reason to consult the fallback table.
pub(super) enum ConstructorAnswer {
    /// The source has facts for the owner and exactly one constructor matches.
    Established(&'static str),
    /// The source has facts for the owner and none matches (or several do).
    NotApplicable,
    /// The source has no usable facts for the owner: keep the table floor.
    Unavailable,
}

/// Constructor applicability read through the same classpath signature model as
/// instance and static calls.
///
/// `owner` is a canonical JVM binary name the caller has already proven from the
/// call's spelling, imports and shadowing evidence; this function performs no
/// name matching of its own, so it cannot widen the set of owners a caller
/// recognizes. Everything below is the shared resolver: `<init>` rows from the
/// fact source ([`classpath_rows`]), exact proven argument types, exact arity
/// and unique applicability.
///
/// Usable facts are decided **per owner**, not per process: a dead helper, an
/// all-null answer and a type the classpath cannot load all mean "no facts for
/// this owner", and each keeps the caller's pre-helper behavior unchanged.
pub(super) fn constructor_applicable<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    owner: &str,
    provider: &P,
) -> ConstructorAnswer {
    let Some(facts) = provider.classpath_member_facts(owner) else {
        return ConstructorAnswer::Unavailable;
    };
    let rows = member_rows(&facts, owner, "<init>", Dispatch::Constructor);
    match applicable(
        source,
        file,
        call,
        Target {
            owner,
            name: "<init>",
            dispatch: Dispatch::Constructor,
        },
        SignatureSource::Classpath(&rows),
        provider,
    ) {
        Some(result) => ConstructorAnswer::Established(result),
        // A covered owner with no applicable constructor is final. Falling back
        // to the table here would answer from stale guessed rows exactly when
        // the fact source is authoritative — and fake ambiguity or fake
        // applicability depending on which side is narrower.
        None => ConstructorAnswer::NotApplicable,
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
    Some(member_rows(&facts, owner, name, dispatch))
}

/// [`classpath_rows`] over an answer the caller already holds, so a caller that
/// has just checked availability does not ask twice or re-check for a `None`
/// that cannot happen.
///
/// Constructor rows need one semantic mapping: the helper reports a constructor
/// with `<init>` and the JVM `void` result (`result_type: "<void>"`), because a
/// constructor call produces no value at the call instruction. A constructor
/// *expression* still has the constructed class as its Kotlin/JVM type, so the
/// row's result is the owner here. This is a mapping, not a disagreement with
/// the wire, and it is why nothing downstream should expect a constructor row's
/// wire `result_type` to equal the owner.
fn member_rows(
    facts: &[crate::facts::protocol::MemberFacts],
    owner: &str,
    name: &str,
    dispatch: Dispatch,
) -> Vec<ClasspathRow> {
    facts
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
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::protocol::MemberFacts;
    use std::collections::BTreeSet;

    /// One expanded bootstrap case: a table row plus one concrete choice from
    /// each parameter's proven alternative set.
    struct Case {
        row: usize,
        owner: &'static str,
        name: &'static str,
        dispatch: Dispatch,
        parameters: Vec<&'static str>,
        result: &'static str,
    }

    /// One proven case and the native member rows that satisfied it.
    #[derive(Debug)]
    struct CaseEvidence {
        row: usize,
        owner: &'static str,
        name: &'static str,
        dispatch: &'static str,
        parameters: Vec<&'static str>,
        result: &'static str,
        matches: usize,
    }

    fn dispatch_label(dispatch: Dispatch) -> &'static str {
        match dispatch {
            Dispatch::Constructor => "Constructor",
            Dispatch::Static => "Static",
            Dispatch::Instance => "Instance",
        }
    }

    /// Every case the bootstrap table declares, in table order.
    ///
    /// The empty parameter list is one case, not zero: a no-argument row is
    /// exactly where a skipped product would hide.
    fn expanded_cases() -> Vec<Case> {
        let mut cases = Vec::new();
        for (row, signature) in SIGNATURES.iter().enumerate() {
            let mut product: Vec<Vec<&'static str>> = vec![Vec::new()];
            for alternatives in signature.parameters {
                assert!(
                    !alternatives.is_empty(),
                    "{}::{} declares an empty alternative set; its cases would silently vanish",
                    signature.owner,
                    signature.name
                );
                product = product
                    .into_iter()
                    .flat_map(|prefix| {
                        alternatives.iter().map(move |alternative| {
                            let mut extended = prefix.clone();
                            extended.push(*alternative);
                            extended
                        })
                    })
                    .collect();
            }
            for parameters in product {
                cases.push(Case {
                    row,
                    owner: signature.owner,
                    name: signature.name,
                    dispatch: signature.dispatch,
                    parameters,
                    result: signature.result,
                });
            }
        }
        cases
    }

    /// The resolver's own classification of a fact row (`member_rows`):
    /// `<init>` is a constructor whatever `static_member` says, then static,
    /// else instance. The gate has to agree with the resolver it is proving.
    fn fact_dispatch(member: &MemberFacts) -> Dispatch {
        if member.name == "<init>" {
            Dispatch::Constructor
        } else if member.static_member {
            Dispatch::Static
        } else {
            Dispatch::Instance
        }
    }

    /// The semantic result the resolver derives from a fact row, spelled the
    /// way `member_rows` derives it: a constructor row answers the constructed
    /// owner, every other row answers its classified wire result.
    fn semantic_result(owner: &str, member: &MemberFacts) -> String {
        if member.name == "<init>" {
            owner.to_string()
        } else {
            classifier(&member.result_type)
        }
    }

    /// Identity, dispatch and physical parameter agreement — conformance
    /// without the result half.
    fn shape_matches(case: &Case, member: &MemberFacts) -> bool {
        member.name == case.name
            && fact_dispatch(member) == case.dispatch
            && member.parameter_types.len() == case.parameters.len()
            && member
                .parameter_types
                .iter()
                .zip(&case.parameters)
                .all(|(physical, expected)| classifier(physical) == *expected)
    }

    /// Whether one native member row satisfies this declared case.
    ///
    /// Normalization is the resolver's own [`classifier`] mapping, so this is
    /// applicability conformance — not a claim about generics, nullability,
    /// checked exceptions or the Java ABI.
    fn conforms(case: &Case, member: &MemberFacts) -> bool {
        if !shape_matches(case, member) {
            return false;
        }
        // Constructor rows are pinned on both halves: the helper's wire contract
        // (`<init>` reports `<void>`) and the resolver's mapping (the row result
        // is the constructed owner). A helper that reported the owner as the
        // wire result would otherwise pass through the production mapping.
        if case.dispatch == Dispatch::Constructor {
            return matches!(member.result_type.as_str(), "<void>" | "void")
                && case.result == case.owner
                && semantic_result(case.owner, member) == case.result;
        }
        semantic_result(case.owner, member) == case.result
    }

    /// A fixture built through the wire shape of `MemberFacts`, so the controls
    /// cannot drift from fields the helper actually reports.
    fn fact(name: &str, static_member: bool, parameters: &[&str], result: &str) -> MemberFacts {
        let parameters = parameters
            .iter()
            .map(|parameter| format!("\"{parameter}\""))
            .collect::<Vec<_>>()
            .join(",");
        let json = format!(
            r#"{{"name":"{name}","static_member":{static_member},"parameter_types":[{parameters}],"result_type":"{result}","parameter_nullability":[],"result_nullability":"unknown"}}"#
        );
        serde_json::from_str(&json).expect("fixture must deserialize as MemberFacts")
    }

    fn case_of(cases: &[Case], owner: &str, name: &str, parameters: &[&str]) -> usize {
        cases
            .iter()
            .position(|case| {
                case.owner == owner && case.name == name && case.parameters == parameters
            })
            .unwrap_or_else(|| panic!("no declared case {owner}::{name}{parameters:?}"))
    }

    /// Conformance of one owner's declared cases against native member facts.
    ///
    /// Missing or empty facts are **gate failures**, never skips: the bootstrap
    /// row for a covered owner stays unproven, and a silent pass would turn an
    /// unloadable class or a dead helper into a green regression. The table is
    /// not consulted here, so absent evidence cannot be answered from the rows
    /// the gate is supposed to be testing.
    fn evaluate_owner(
        owner: &str,
        cases: &[Case],
        facts: Option<&[MemberFacts]>,
    ) -> Result<Vec<CaseEvidence>, String> {
        let facts = facts.ok_or_else(|| {
            format!(
                "{owner}: native ClassMembers evidence is missing (helper unavailable, query failed, or class not loadable)"
            )
        })?;
        if facts.is_empty() {
            return Err(format!(
                "{owner}: native ClassMembers answer declares no members; nothing proves the bootstrap rows"
            ));
        }
        let mut evidence = Vec::new();
        for case in cases.iter().filter(|case| case.owner == owner) {
            // The resolver chooses applicability before reading the result.
            // A contradictory result must not hide an otherwise ambiguous row.
            let matching = facts
                .iter()
                .filter(|member| shape_matches(case, member))
                .collect::<Vec<_>>();
            let matches = matching.len();
            if matches == 0 {
                return Err(format!(
                    "{owner}::{} {:?} -> {}: no native member conforms",
                    case.name,
                    case.parameters,
                    dispatch_label(case.dispatch),
                ));
            }
            if matches > 1 {
                return Err(format!(
                    "{owner}::{} {:?} -> {}: {matches} native members conform, so the resolver would answer NotApplicable",
                    case.name,
                    case.parameters,
                    dispatch_label(case.dispatch),
                ));
            }
            if !conforms(case, matching[0]) {
                return Err(format!(
                    "{owner}::{} {:?}: unique native shape has a contradictory result",
                    case.name, case.parameters,
                ));
            }
            evidence.push(CaseEvidence {
                row: case.row,
                owner: case.owner,
                name: case.name,
                dispatch: dispatch_label(case.dispatch),
                parameters: case.parameters.clone(),
                result: case.result,
                matches,
            });
        }
        if evidence.is_empty() {
            return Err(format!("{owner}: no bootstrap cases to prove"));
        }
        Ok(evidence)
    }

    #[test]
    fn bootstrap_table_shape_is_21_rows_27_cases_7_owners() {
        let cases = expanded_cases();
        assert_eq!(SIGNATURES.len(), 21, "declared bootstrap rows");
        assert_eq!(cases.len(), 27, "expanded parameter cases");
        let owners = SIGNATURES
            .iter()
            .map(|signature| signature.owner)
            .collect::<BTreeSet<_>>();
        assert_eq!(owners.len(), 7, "declared owners");

        // Every row, and every alternative within it, contributes cases.
        let visited = cases.iter().map(|case| case.row).collect::<BTreeSet<_>>();
        assert_eq!(
            visited,
            (0..SIGNATURES.len()).collect::<BTreeSet<_>>(),
            "every row must be visited"
        );
        for (row, signature) in SIGNATURES.iter().enumerate() {
            let declared: usize = signature
                .parameters
                .iter()
                .map(|alternatives| alternatives.len())
                .product();
            let expanded = cases.iter().filter(|case| case.row == row).count();
            assert_eq!(
                expanded, declared,
                "{}::{} lost or invented cases",
                signature.owner, signature.name
            );
        }

        // No duplicate case: a repeated declaration would fake ambiguity in the
        // resolver and make the uniqueness answer meaningless.
        let unique = cases
            .iter()
            .map(|case| {
                (
                    case.owner,
                    case.name,
                    dispatch_label(case.dispatch),
                    case.parameters.clone(),
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), cases.len(), "duplicate declared case");

        for signature in SIGNATURES {
            assert!(!signature.owner.is_empty() && !signature.name.is_empty());
            assert!(
                !signature.result.is_empty(),
                "{}::{} has no result",
                signature.owner,
                signature.name
            );
            match signature.dispatch {
                Dispatch::Constructor => {
                    assert_eq!(signature.name, "<init>", "constructor rows must be <init>");
                    assert_eq!(
                        signature.result, signature.owner,
                        "a constructor row returns the constructed owner, never the wire <void>"
                    );
                }
                Dispatch::Static | Dispatch::Instance => assert_ne!(
                    signature.name, "<init>",
                    "only Dispatch::Constructor may declare <init>"
                ),
            }
        }
    }

    #[test]
    fn constructor_rows_map_wire_void_to_the_constructed_owner() {
        let cases = expanded_cases();
        let constructors = cases
            .iter()
            .filter(|case| case.dispatch == Dispatch::Constructor)
            .collect::<Vec<_>>();
        assert_eq!(constructors.len(), 9, "constructor cases across 4 owners");
        for case in constructors {
            assert_eq!(case.result, case.owner);
            // The wire reports `<init>` with a void result; the resolver maps it
            // to the constructed owner. Both halves are pinned here.
            assert!(
                conforms(case, &fact("<init>", false, &case.parameters, "<void>")),
                "{}: <init> with wire <void> must conform",
                case.owner
            );
            assert!(
                !conforms(case, &fact("<init>", false, &case.parameters, case.owner)),
                "{}: a constructor wire result equal to the owner is not the helper's contract",
                case.owner
            );
            assert!(
                !conforms(
                    case,
                    &fact("<init>", false, &case.parameters, "kotlin.Unit")
                ),
                "{}: kotlin.Unit is a scalar classifier, not the constructor result",
                case.owner
            );
        }
    }

    #[test]
    fn scalar_spellings_conform_only_through_the_resolver_classifier() {
        // BigDecimal(int) is declared once; the JVM primitive and its boxed
        // alternative are both what `classifier` already maps to kotlin.Int.
        assert_eq!(classifier("int"), "kotlin.Int");
        assert_eq!(classifier("java.lang.Integer"), "kotlin.Int");
        assert_eq!(classifier("java.lang.String"), "java.lang.String");
        let cases = expanded_cases();
        let case = &cases[case_of(&cases, DECIMAL, "<init>", &["kotlin.Int"])];
        assert!(conforms(case, &fact("<init>", false, &["int"], "<void>")));
        assert!(conforms(
            case,
            &fact("<init>", false, &["java.lang.Integer"], "<void>")
        ));
        assert!(!conforms(case, &fact("<init>", false, &["long"], "<void>")));
        assert!(!conforms(
            case,
            &fact("<init>", false, &["java.lang.Long"], "<void>")
        ));
    }

    #[test]
    fn contradictory_member_rows_are_rejected() {
        let cases = expanded_cases();

        let add = &cases[case_of(&cases, DECIMAL, "add", &[DECIMAL])];
        assert!(
            conforms(add, &fact("add", false, &[DECIMAL], DECIMAL)),
            "positive control: the exact native row must conform"
        );
        // Wrong physical result.
        assert!(!conforms(add, &fact("add", false, &[DECIMAL], STRING)));
        assert!(!conforms(
            add,
            &fact("add", false, &[DECIMAL], "kotlin.Int")
        ));
        // Wrong dispatch.
        assert!(!conforms(add, &fact("add", true, &[DECIMAL], DECIMAL)));
        // Wrong parameters, same arity and different arity.
        assert!(!conforms(
            add,
            &fact("add", false, &["kotlin.Long"], DECIMAL)
        ));
        assert!(!conforms(
            add,
            &fact("add", false, &[DECIMAL, DECIMAL], DECIMAL)
        ));
        // Wrong name.
        assert!(!conforms(
            add,
            &fact("subtract", false, &[DECIMAL], DECIMAL)
        ));

        let now = &cases[case_of(&cases, "java.time.Instant", "now", &[])];
        assert!(conforms(now, &fact("now", true, &[], "java.time.Instant")));
        assert!(
            !conforms(now, &fact("now", false, &[], "java.time.Instant")),
            "an instance row cannot satisfy a static case"
        );
        assert!(!conforms(now, &fact("now", true, &[], "kotlin.Unit")));

        let builder = &cases[case_of(&cases, BUILDER, "<init>", &[])];
        assert!(conforms(builder, &fact("<init>", false, &[], "<void>")));
        assert!(!conforms(
            builder,
            &fact("<init>", false, &[STRING], "<void>")
        ));
        assert!(
            !conforms(builder, &fact("toString", false, &[], BUILDER)),
            "a non-<init> member cannot satisfy a constructor case"
        );
    }

    #[test]
    fn a_matching_result_cannot_hide_an_ambiguous_native_shape() {
        let cases = expanded_cases()
            .into_iter()
            .filter(|case| case.owner == BUILDER && case.name == "toString")
            .collect::<Vec<_>>();
        assert_eq!(cases.len(), 1);
        let correct = fact("toString", false, &[], STRING);
        assert!(evaluate_owner(BUILDER, &cases, Some(std::slice::from_ref(&correct))).is_ok());
        // Deliberately contradictory wire evidence: applicability cannot use
        // the desired result to choose between otherwise identical members.
        let contradictory = fact("toString", false, &[], BUILDER);
        let error = evaluate_owner(BUILDER, &cases, Some(&[correct, contradictory]))
            .expect_err("the resolver rejects both shapes before choosing a result");
        assert!(error.contains("NotApplicable"), "{error}");
    }

    #[test]
    fn native_gate_fails_closed_on_missing_unknown_or_ambiguous_evidence() {
        let cases = expanded_cases();
        let builder_facts = vec![
            fact("<init>", false, &[], "<void>"),
            fact("append", false, &["int"], BUILDER),
            fact("append", false, &["long"], BUILDER),
            fact("append", false, &["double"], BUILDER),
            fact("append", false, &[STRING], BUILDER),
            fact("toString", false, &[], STRING),
        ];

        // The table has StringBuilder rows, yet absent native evidence fails:
        // the gate must never answer its own query from the rows under test.
        let missing = evaluate_owner(BUILDER, &cases, None).unwrap_err();
        assert!(missing.contains("missing"), "{missing}");
        let empty = evaluate_owner(BUILDER, &cases, Some(&[])).unwrap_err();
        assert!(empty.contains("no members"), "{empty}");

        // Facts for a different owner are not evidence for this one.
        let foreign = vec![fact("now", true, &[], "java.time.Instant")];
        assert!(evaluate_owner(BUILDER, &cases, Some(&foreign)).is_err());

        let established = evaluate_owner(BUILDER, &cases, Some(&builder_facts)).unwrap();
        assert_eq!(established.len(), 6);
        assert!(established.iter().all(|case| case.matches == 1));

        // Duplicate conforming rows make the resolver answer NotApplicable, so
        // the gate must fail rather than pick one.
        let mut ambiguous = builder_facts.clone();
        ambiguous.push(fact("toString", false, &[], STRING));
        let error = evaluate_owner(BUILDER, &cases, Some(&ambiguous)).unwrap_err();
        assert!(error.contains("NotApplicable"), "{error}");

        // A contradictory result for a declared case is a failure, not a skip.
        let mut wrong_result = builder_facts.clone();
        wrong_result[1] = fact("append", false, &["int"], STRING);
        assert!(evaluate_owner(BUILDER, &cases, Some(&wrong_result)).is_err());

        // An owner with no declared case cannot be "proven" by asking it.
        assert!(evaluate_owner("java.util.UUID", &cases[..0], Some(&builder_facts)).is_err());
    }

    fn required_env(name: &str, hint: &str) -> std::ffi::OsString {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| panic!("{name} must be set: {hint}"))
    }

    fn require_jdk_25(java: &Path) {
        let output = std::process::Command::new(java)
            .arg("-version")
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "NOTLIN_JAVA ({}) must be executable: {error}",
                    java.display()
                )
            });
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains("version \"25."),
            "native conformance is pinned to JDK 25; NOTLIN_JAVA reported: {text}"
        );
    }

    /// Every bootstrap row, and every alternative within it, must be a member
    /// the real JDK25 helper reports — with the constructor `<void>` mapping and
    /// with unique resolver applicability.
    ///
    /// Ignored by default: it needs `NOTLIN_JAVA` (the JDK 25 java binary) and
    /// `NOTLIN_JVM_FACTS_JAR` (the slim helper jar). It panics when they are
    /// absent rather than returning green, and it is meant to be run explicitly:
    ///
    /// ```text
    /// NOTLIN_JAVA=.../25.0.4-tem/bin/java \
    /// NOTLIN_JVM_FACTS_JAR=.../notlin-jvm-facts.jar \
    ///   cargo test --lib -- --ignored --nocapture platform_members
    /// ```
    #[test]
    #[ignore = "native: needs NOTLIN_JAVA (JDK25) and NOTLIN_JVM_FACTS_JAR (slim helper jar)"]
    fn bootstrap_rows_conform_to_real_jdk25_class_members() {
        let java = std::path::PathBuf::from(required_env(
            "NOTLIN_JAVA",
            "the JDK 25 java binary the helper must run on",
        ));
        let jar = std::path::PathBuf::from(required_env(
            "NOTLIN_JVM_FACTS_JAR",
            "the built slim notlin-jvm-facts.jar",
        ));
        require_jdk_25(&java);
        assert!(jar.is_file(), "helper jar missing: {}", jar.display());
        let runtime = crate::facts::runtime::FactsRuntime::start(&jar)
            .expect("the slim helper jar must start under NOTLIN_JAVA");

        let cases = expanded_cases();
        let owners = SIGNATURES
            .iter()
            .map(|signature| signature.owner)
            .collect::<BTreeSet<_>>();
        assert_eq!(owners.len(), 7);

        let mut evidence = Vec::new();
        for owner in &owners {
            let facts = runtime.member_facts(owner);
            let slice = facts.as_ref().map(|facts| facts.as_slice());
            let mut owner_evidence =
                evaluate_owner(owner, &cases, slice).unwrap_or_else(|error| panic!("{error}"));
            // The native answer must be richer than the bootstrap table;
            // otherwise the evidence could be an echo of the rows it proves.
            let facts = facts.expect("checked by evaluate_owner");
            assert!(
                facts.iter().any(|member| !cases
                    .iter()
                    .filter(|case| case.owner == *owner)
                    .any(|case| shape_matches(case, member))),
                "{owner}: native members are indistinguishable from the bootstrap table"
            );
            evidence.append(&mut owner_evidence);
        }

        assert_eq!(evidence.len(), 27, "expanded cases proved");
        assert_eq!(
            evidence
                .iter()
                .map(|case| case.row)
                .collect::<BTreeSet<_>>()
                .len(),
            21,
            "distinct rows proved"
        );
        assert_eq!(
            evidence
                .iter()
                .map(|case| case.owner)
                .collect::<BTreeSet<_>>(),
            owners,
            "owners visited"
        );
        assert!(
            evidence.iter().all(|case| case.matches == 1),
            "every case must have exactly one native match"
        );

        // A class the helper cannot load answers null, which the gate treats as
        // missing evidence. Without this a silently broken helper could look
        // like "nothing to check".
        let absent = runtime.member_facts("java.lang.NotlinFallbackConformanceAbsent");
        assert!(
            absent.is_none(),
            "the helper must answer null for an unloadable class"
        );

        for case in &evidence {
            eprintln!(
                "conforms {owner}::{name} {dispatch} {parameters:?} -> {result} ({matches} native row)",
                owner = case.owner,
                name = case.name,
                dispatch = case.dispatch,
                parameters = case.parameters,
                result = case.result,
                matches = case.matches,
            );
        }
        eprintln!(
            "native conformance: {} rows, {} cases, {} owners, all unique",
            21,
            evidence.len(),
            owners.len()
        );
    }
}
