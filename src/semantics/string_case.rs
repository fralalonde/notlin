//! One typed fact for Kotlin `String.lowercase`/`uppercase` calls, shared by
//! retention planning and Java emission.
//!
//! The direction and the locale contract belong to the *resolved* callee, not
//! to the spelling at the call site. `import kotlin.text.lowercase as fold`
//! makes `value.fold()` the invariant lowercase conversion and must lower to
//! `toLowerCase(java.util.Locale.ROOT)`; a workspace extension that merely
//! spells its name `lowercase` is a different function and must never acquire
//! the JDK member. Track B (K2) supplies that identity when this run has
//! call-site facts; without them the source shape proves the same fact, which
//! is exactly the pre-helper behavior.
//!
//! The lowering implements two erased shapes only — the invariant zero-argument
//! conversion and the explicit `java.util.Locale` overload — so a resolved call
//! of any other shape is retained rather than re-guessed from its spelling.

use super::{
    FactStatus, SemanticProvider, SourceLocation, call_argument_count, direct_trailing_lambda,
    explicit_receiver_node, kotlin_call_site_offset, kotlin_import_lines, resolve_required_call,
};
use crate::facts::protocol::{CallSite, SiteStatus};
use std::path::Path;
use tree_sitter::Node;

/// The stdlib façade class every `String.lowercase`/`uppercase` overload erases
/// to. It is the facade, not a member of it: a same-named function on another
/// owner is a different call.
const STDLIB_OWNER: &str = "kotlin.text.StringsKt";
const STDLIB_RESULT: &str = "kotlin.String";
const LOCALE_PARAMETER: &str = "java.util.Locale";
/// Erased descriptors of the two shapes this lowering implements. The
/// extension receiver is not part of a descriptor, so it carries no medium
/// information.
const INVARIANT_DESCRIPTOR: &str = "()Ljava/lang/String;";
const EXPLICIT_DESCRIPTOR: &str = "(Ljava/util/Locale;)Ljava/lang/String;";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StringCaseDirection {
    Lowercase,
    Uppercase,
}

impl StringCaseDirection {
    fn from_stdlib_name(name: &str) -> Option<Self> {
        match name {
            "lowercase" => Some(Self::Lowercase),
            "uppercase" => Some(Self::Uppercase),
            _ => None,
        }
    }

    pub(crate) fn java_method(self) -> &'static str {
        match self {
            Self::Lowercase => "toLowerCase",
            Self::Uppercase => "toUpperCase",
        }
    }
}

/// Whether the conversion depends on the process default locale (Kotlin's
/// zero-argument form, which is invariant) or on an argument the caller
/// supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StringCaseLocale {
    Invariant,
    Explicit,
}

impl StringCaseLocale {
    /// The value-argument count this contract consumes. The extension receiver
    /// is not an argument.
    pub(crate) fn argument_count(self) -> usize {
        match self {
            Self::Invariant => 0,
            Self::Explicit => 1,
        }
    }
}

/// Where the identity came from. Emission is identical either way; tests and
/// diagnostics use this to keep a resolved call distinct from a source-shape
/// proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StringCaseEvidence {
    /// Track B resolved this exact call site to the stdlib façade.
    CallSite {
        owner: String,
        callee_name: String,
        parameter_types: Vec<String>,
        jvm_descriptor: String,
    },
    /// No call-site fact for this call: the source shape proved it.
    SourceShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StringCaseCall {
    /// Identity of the call expression the fact describes.
    pub(crate) location: SourceLocation,
    /// Identity of the receiver expression, so an emitter can confirm it is
    /// lowering the receiver this fact proved.
    pub(crate) receiver: SourceLocation,
    pub(crate) direction: StringCaseDirection,
    pub(crate) locale: StringCaseLocale,
    #[allow(dead_code)]
    // Evidence is carried for tests and retention diagnostics; the lowering
    // itself depends only on direction and locale contract.
    pub(crate) evidence: StringCaseEvidence,
}

impl StringCaseCall {
    pub(crate) fn argument_count(&self) -> usize {
        self.locale.argument_count()
    }
}

/// The shared proof: `Some` is both "this call is the stdlib conversion" and
/// "here is the Java contract to emit".
pub(crate) fn establish<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    provider: &P,
) -> Option<StringCaseCall> {
    // A Java `method_invocation` keeps the JDK method's own contract: Java's
    // `toLowerCase()` is locale-sensitive by design and is not this fact.
    if call.kind() != "call_expression" {
        return None;
    }
    let callee = super::call_callee_node(call)?;
    let spelling = callee.utf8_text(source.as_bytes()).ok()?.trim();
    let receiver = explicit_receiver_node(call, callee)?;
    if direct_trailing_lambda(call).is_some()
        || call_argument_count(call) > 1
        || safe_access(source, callee, receiver)
    {
        return None;
    }
    // An indexed workspace extension with this spelling owns the call, whatever
    // a stale or unrelated call-site fact says. This keeps the source index's
    // precedence over forwarded facts.
    if provider.symbols_named(spelling).iter().any(|symbol| {
        symbol.id.receiver.is_some() && matches!(symbol.id.kind.as_str(), "function" | "method")
    }) {
        return None;
    }
    let (direction, locale, evidence) = match call_site(source, file, call, provider) {
        Some(site) => {
            // An authoritative fact settles direction and arity. A shape this
            // lowering does not implement is declined here: the spelling must
            // never be resurrected as a fallback.
            let (direction, locale) = stdlib_contract(&site)?;
            (
                direction,
                locale,
                StringCaseEvidence::CallSite {
                    owner: site.callee_owner.clone().unwrap_or_default(),
                    callee_name: site.callee_name.clone().unwrap_or_default(),
                    parameter_types: site.parameter_types.clone(),
                    jvm_descriptor: site.jvm_descriptor.clone().unwrap_or_default(),
                },
            )
        }
        None => syntax_contract(source, file, call, callee, spelling, provider)?,
    };
    if call_argument_count(call) != locale.argument_count() {
        return None;
    }
    if !proven_string_receiver(source, file, call, callee, receiver, provider) {
        return None;
    }
    Some(StringCaseCall {
        location: location(source, file, call),
        receiver: location(source, file, receiver),
        direction,
        locale,
        evidence,
    })
}

/// Whether the call *reads* as a `String` case conversion the lowering would
/// otherwise emit for, even though [`establish`] declined it. The planner uses
/// this to retain a foreign resolution instead of letting the emitter write
/// Kotlin spelling into Java.
///
/// "Reads as" is decided by the *source binding* of the callee selector, not by
/// a table of literal spellings: `import kotlin.text.lowercase as fold` binds
/// `fold` to the stdlib conversion, so a declined `value.fold()` must be
/// retained exactly like a declined `value.lowercase()`. The binding is only an
/// expectation — direction and arity still have to come from the fact in
/// [`establish`] — so a wrong owner, a misshapen descriptor or a foreign
/// function behind the same alias never becomes the JDK member.
pub(crate) fn spelling_claims_string_case<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    callee: Node<'_>,
    name: &str,
    provider: &P,
) -> bool {
    stdlib_case_binding(source, name, call).is_some()
        && call.kind() == "call_expression"
        && direct_trailing_lambda(call).is_none()
        && explicit_receiver_node(call, callee).is_some_and(|receiver| {
            proven_string_receiver(source, file, call, callee, receiver, provider)
        })
}

/// The stdlib case conversion this spelling is *bound* to in this source, or
/// `None` when the selector names something else.
///
/// A canonical spelling is its own binding, as before. An alias binds the new
/// name to the imported path, so only an alias of `kotlin.text.lowercase` /
/// `kotlin.text.uppercase` claims the conversion: `import kotlin.text.fold as
/// fold` or any other alias target stays a plain call.
fn stdlib_case_binding(
    source: &str,
    spelling: &str,
    mut context: Node<'_>,
) -> Option<StringCaseDirection> {
    if let Some(direction) = StringCaseDirection::from_stdlib_name(spelling) {
        return Some(direction);
    }
    while let Some(parent) = context.parent() {
        context = parent;
    }
    // Read import syntax from this call's tree, not line text: comments,
    // whitespace around `as`, and quoted identifiers do not change a binding.
    context
        .named_children(&mut context.walk())
        .filter(|node| node.kind() == "import")
        .find_map(|import| {
            let alias = import
                .named_children(&mut import.walk())
                .find(|node| node.kind() == "identifier")?;
            if alias.utf8_text(source.as_bytes()).ok()?.trim_matches('`')
                != spelling.trim_matches('`')
            {
                return None;
            }
            let path = import
                .named_children(&mut import.walk())
                .find(|node| node.kind() == "qualified_identifier")?;
            let mut cursor = path.walk();
            let mut names = path
                .named_children(&mut cursor)
                .filter(|node| node.kind() == "identifier")
                .map(|node| node.utf8_text(source.as_bytes()).ok());
            let (Some(Some(package)), Some(Some(module)), Some(Some(name)), None) =
                (names.next(), names.next(), names.next(), names.next())
            else {
                return None;
            };
            if package.trim_matches('`') != "kotlin" || module.trim_matches('`') != "text" {
                return None;
            }
            StringCaseDirection::from_stdlib_name(name.trim_matches('`'))
        })
}

/// The hash-guarded Track B answer for this call, or `None`.
///
/// Both keys matter: the offset of the callee selector and the blake3 of the
/// text being planned. The provider refuses a snapshot that is not the text on
/// disk, so a candidate revision never inherits facts.
fn call_site<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    provider: &P,
) -> Option<CallSite> {
    let offset = kotlin_call_site_offset(call)?;
    let site = provider.established_call_site(
        file,
        offset as u64,
        &crate::facts::classpath::source_hash(source),
    )?;
    (site.status == SiteStatus::Established).then_some(site)
}

/// Exactly the two shapes the lowering implements, keyed on the resolved
/// callee's identity rather than on how the call is spelled.
fn stdlib_contract(site: &CallSite) -> Option<(StringCaseDirection, StringCaseLocale)> {
    if site.callee_owner.as_deref() != Some(STDLIB_OWNER)
        || site.result_type.as_deref() != Some(STDLIB_RESULT)
        || site.result_nullable
    {
        return None;
    }
    let direction = StringCaseDirection::from_stdlib_name(site.callee_name.as_deref()?)?;
    let locale = match site.parameter_types.as_slice() {
        [] if site.jvm_descriptor.as_deref() == Some(INVARIANT_DESCRIPTOR) => {
            StringCaseLocale::Invariant
        }
        [parameter]
            if parameter.as_str() == LOCALE_PARAMETER
                && site.jvm_descriptor.as_deref() == Some(EXPLICIT_DESCRIPTOR) =>
        {
            StringCaseLocale::Explicit
        }
        _ => return None,
    };
    Some((direction, locale))
}

/// The source-shape proof: canonical spelling, no shadowing import, no other
/// resolution, and an argument that is a proven `java.util.Locale`.
fn syntax_contract<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    callee: Node<'_>,
    spelling: &str,
    provider: &P,
) -> Option<(StringCaseDirection, StringCaseLocale, StringCaseEvidence)> {
    let direction = StringCaseDirection::from_stdlib_name(spelling)?;
    if shadowing_import(source, spelling) {
        return None;
    }
    if !matches!(
        resolve_required_call(source, file, call, callee, spelling, provider),
        FactStatus::Unknown
    ) {
        // Another declaration owns this call; the case lowering is not it.
        return None;
    }
    let locale = match call_argument_count(call) {
        0 => StringCaseLocale::Invariant,
        1 if super::known_locale_argument(source, file, call, provider) => {
            StringCaseLocale::Explicit
        }
        _ => return None,
    };
    Some((direction, locale, StringCaseEvidence::SourceShape))
}

/// An explicit import of another declaration under this spelling keeps the
/// source precedence: the lowering only implements `kotlin.text.{name}`.
fn shadowing_import(source: &str, spelling: &str) -> bool {
    kotlin_import_lines(source).any(|import| {
        let (path, alias) = import
            .split_once(" as ")
            .map_or((import, None), |(path, alias)| {
                (path.trim(), Some(alias.trim()))
            });
        alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or("")) == spelling
            && path != format!("kotlin.text.{spelling}")
    })
}

/// A non-null `String` receiver: a proven parameter/property/literal type, or
/// the `Object.toString()` call-result fact for a chained conversion.
fn proven_string_receiver<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'_>,
    callee: Node<'_>,
    receiver: Node<'_>,
    provider: &P,
) -> bool {
    if receiver.kind() == "call_expression"
        && let Some(inner) = super::call_callee_node(receiver)
        && super::object_calls::established_object_to_string(
            source, file, receiver, inner, "toString", provider,
        )
        .is_some()
    {
        return true;
    }
    let Some(receiver_type) = super::explicit_receiver_type(source, call, callee, file, provider)
    else {
        return false;
    };
    super::standard_external_type(source, provider, &receiver_type, false).as_deref()
        == Some("java.lang.String")
}

/// `value?.lowercase()` propagates null; a member lowering would not.
fn safe_access(source: &str, callee: Node<'_>, receiver: Node<'_>) -> bool {
    receiver.end_byte() <= callee.start_byte()
        && source[receiver.end_byte()..callee.start_byte()].contains('?')
}

fn location(source: &str, file: &Path, node: Node<'_>) -> SourceLocation {
    SourceLocation {
        file: file.to_path_buf(),
        snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    }
}
