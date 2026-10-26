//! Call-scoped proof that one helper-established call is an ordinary Java
//! *instance* member call the emitter may write verbatim.
//!
//! A fact `SymbolId` is a call-site identity, not a declaration: it carries no
//! receiver shape, no source-argument count and no per-call descriptor. This
//! module therefore reads the hash-guarded [`CallSite`](crate::facts::protocol::CallSite)
//! for *this call* and composes it with Track A `ClassMembers` rows. Every step
//! fails closed with `None`, which keeps today's retention/approximation path.
//!
//! Slice 1 boundary (deliberately narrow):
//! * owner is a loadable JDK `java.*` binary name (no `javax.*`, no Kotlin
//!   facades, no third-party jars, no `kotlin.*`);
//! * instance dispatch only — a type-qualified receiver needs the separate
//!   canonical-static/alias emission proof and stays retained;
//! * zero declared value parameters *and* zero actual source arguments, with no
//!   trailing lambda;
//! * the receiver is a simple identifier whose source-declared type is itself
//!   the same Java reference classifier (arguments erased), proved from the
//!   declaration and from the source tree's import bindings — never from the
//!   helper's resolved owner alone, and never from line text. A Kotlin logical
//!   type that merely erases to a Java type is *not* accepted here;
//! * that declared spelling is bound by exactly one unaliased, non-wildcard
//!   import of a `java.*` owner (or written fully qualified). Comments bind
//!   nothing, an alias is retained because the emitted Java keeps the source
//!   spelling, and duplicate/conflicting bindings are retained because Kotlin
//!   itself rejects them;
//! * exactly one zero-parameter instance row under the same physical name,
//!   whose reconstructed JVM descriptor is byte-equal to the K2 descriptor
//!   (one documented normalization: non-null `kotlin.Unit` ⇒ `V`);
//! * `checked_exceptions == Some([])` — Unknown (`None`, old jar) rejects.
//!
//! [`checked_throws`] is the sibling probe for the *other* question the same
//! evidence answers: what the callee physically declares. It does not widen
//! this slice (no emission path consumes it) and it is not a name table: one
//! call at a time, composed from that call's hash-guarded site and the
//! physical rows, with the complete descriptor as member identity.
//!
//! Nothing here answers for `FactsProvider::is_java_callable_member`, adds a
//! memo, or widens the broad receiver fallbacks used by platform rows.
use super::*;
use crate::facts::protocol::{MemberFacts, SiteStatus};

/// Sign the shared lowering receipt for `call`, or `None` when this slice has
/// no proof.
///
/// `None` is not a negative fact: it means no builder covers this call site, so
/// the declaration-wide contract records a missing operation instead of
/// letting any older Established/platform path emit the call.
///
/// The returned [`CallReceipt`] is the *whole* rendering/evaluation/result
/// contract every consumer reads: emission writes `receiver.name` from it, type
/// inference reads its [`JavaRepresentation`], and the whole-declaration
/// inventory refuses the declaration when the receipt's use context would
/// observe a payload this receipt cannot prove.
pub(crate) fn propose<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<CallReceipt> {
    let callee = call_callee_node(call)?;
    let receiver = explicit_receiver_node(call, callee)?;
    // Instance-only. A type-qualified receiver can name a static member or an
    // object's instance reached through its class name; distinguishing those
    // belongs to the static/alias emission slice.
    if explicit_receiver_is_type_qualifier(source, call, callee, file, provider) {
        return None;
    }

    // Zero *actual* source arguments and no trailing lambda. This is the
    // emitter-shape gate: the verbatim form is exactly `receiver.name()`.
    // `positional_arguments` refuses named arguments and trailing lambdas;
    // the explicit checks keep the gate readable when a future refactor
    // changes that helper.
    let arguments = overloads::positional_arguments(call)?;
    if !arguments.is_empty() || direct_trailing_lambda(call).is_some() {
        return None;
    }

    // Same hash-guarded site the required-call resolver used, re-read for this
    // call. `established_call_site` refuses when the planned snapshot is not
    // the bytes on disk, so an overlay never inherits disk facts.
    let offset = kotlin_call_site_offset(call)? as u64;
    let site = provider.established_call_site(
        file,
        offset,
        &crate::facts::classpath::source_hash(source),
    )?;
    if site.status != SiteStatus::Established {
        return None;
    }

    // Zero *declared* value parameters. This rejects defaulted parameters,
    // varargs and extension value parameters even though `Established` proved
    // the source call applicable: the audit's `defaulted()` counterexample has
    // zero source arguments but a declared `Int` parameter.
    if !site.parameter_types.is_empty() {
        return None;
    }

    let owner = site.callee_owner.as_deref()?;
    if !is_slice_owner(owner) {
        return None;
    }
    let name = site.callee_name.as_deref()?;
    if !is_jvm_member_name(name) {
        return None;
    }
    // The emitter writes the *source* selector, so a contradictory site whose
    // resolved name differs from the callee text must not be trusted: proving
    // `sorted` exists says nothing about the emitted `distinct()`.
    let source_name = callee.utf8_text(source.as_bytes()).ok()?.trim();
    if source_name != name {
        return None;
    }

    // Receiver boundary: a simple source binding whose *declared* classifier
    // is the same Java reference type as the resolved owner. This is where a
    // Kotlin logical type that merely erases to a Java type is rejected.
    let declared_owner = declared_receiver_owner(source, call, receiver, provider)?;
    if declared_owner != owner {
        return None;
    }

    let facts = provider.classpath_member_facts(owner)?;
    let mut matching = facts
        .iter()
        .filter(|row| row.name == name && !row.static_member && row.parameter_types.is_empty());
    let row = matching.next()?;
    if matching.next().is_some() {
        // Contradictory evidence: two physical zero-parameter instance rows
        // under one name cannot both be the callee.
        return None;
    }

    // Proved no checked exceptions. `None` is Unknown (old jar), which is a
    // rejection, never an empty list.
    match row.checked_exceptions.as_deref() {
        Some([]) => {}
        _ => return None,
    }

    // The load-bearing identity: the physical row's descriptor must match the
    // K2 descriptor byte for byte, with the single documented Kotlin-Unit
    // normalization. Descriptor inequality is what rejects generic
    // substitution (`Gen<String>.read()`: K2 String vs physical Object) and
    // Java varargs shapes.
    let expected = expected_descriptor(&site)?;
    if full_row_descriptor(row).as_deref() != Some(expected.as_str()) {
        return None;
    }

    // The result representation is physical output, so it comes from the row,
    // never from the K2/source result type: a substituted `Stream<Int>` in the
    // source is still an erased `java.util.stream.Stream` in the emitted call.
    let outcome = representation_from_row(&row.result_type, &row.result_nullability)?;

    let snapshot_hash = crate::facts::classpath::source_hash(source);
    Some(CallReceipt {
        location: SourceLocation {
            file: file.to_path_buf(),
            snapshot_hash,
            start_byte: call.start_byte(),
            end_byte: call.end_byte(),
        },
        selector: SourceLocation {
            file: file.to_path_buf(),
            snapshot_hash,
            start_byte: callee.start_byte(),
            end_byte: callee.end_byte(),
        },
        receiver: Some(SourceLocation {
            file: file.to_path_buf(),
            snapshot_hash,
            start_byte: receiver.start_byte(),
            end_byte: receiver.end_byte(),
        }),
        callee: CalleeIdentity {
            owner: owner.to_string(),
            name: name.to_string(),
            descriptor: expected,
        },
        dispatch: Dispatch::Instance,
        arguments: ArgumentMapping::Exact(0),
        outcome,
        effects: CheckedEffects::None,
        behavior: Behavior::Exact,
        evidence: ReceiptEvidence::PhysicalMember,
    })
}

/// Call-scoped physical checked-exception evidence for the member one call
/// denotes.
///
/// Exactly one variant is a verdict:
///
/// * [`CheckedThrows::Checked`] — a unique physical member for *this* call
///   declares these checked exceptions;
/// * [`CheckedThrows::None`] — a unique physical member proved to declare
///   none (`checked_exceptions: []`);
/// * [`CheckedThrows::Unknown`] — a unique physical member whose metadata is
///   absent (old helper/jar shape). Unknown is never "proved none";
/// * [`CheckedThrows::Unidentified`] — this slice proved no unique physical
///   member for the call. It is neither a throws nor a no-throws claim, so a
///   caller must keep today's answer for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckedThrows {
    Unidentified,
    Unknown,
    None,
    Checked(Vec<String>),
}

/// Compose this call's hash-guarded site with the physical `ClassMembers` rows
/// of its owner and report what the *callee* declares.
///
/// The composition is deliberately the same evidence `establish` consumes, but
/// this probe answers a different question and therefore must not inherit
/// `establish`'s narrow emission slice:
///
/// * dispatch shape (type-qualified receiver ⇒ static member, expression
///   receiver ⇒ instance member, receiver-less call spelled as the owner's
///   simple name ⇒ constructor) comes from the source tree, so `Thread.sleep`
///   and `FileInputStream(path)` are reachable without a spelling table;
/// * member identity is the *complete* JVM descriptor including every
///   parameter, so a same-named or same-arity overload is not the same member;
/// * a contradiction (no unique row, or a row whose descriptor is not this
///   call's) is `Unidentified`, never a guess.
///
/// Nothing here is keyed on a method name: `read` is retention-worthy only
/// because the site and the physical row agree that this call is
/// `InputStream.read()` and that row declares `java.io.IOException`.
pub(crate) fn checked_throws<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> CheckedThrows {
    use CheckedThrows::Unidentified;

    let Some(callee) = call_callee_node(call) else {
        return Unidentified;
    };
    let Some(offset) = kotlin_call_site_offset(call) else {
        return Unidentified;
    };
    let Some(site) = provider.established_call_site(
        file,
        offset as u64,
        &crate::facts::classpath::source_hash(source),
    ) else {
        return Unidentified;
    };
    if site.status != SiteStatus::Established {
        return Unidentified;
    }
    let Some(owner) = site.callee_owner.as_deref() else {
        return Unidentified;
    };
    if !is_slice_owner(owner) {
        return Unidentified;
    }
    let Some(site_name) = site.callee_name.as_deref() else {
        return Unidentified;
    };
    let callee_text = callee
        .utf8_text(source.as_bytes())
        .ok()
        .map(str::trim)
        .unwrap_or_default();
    // A type-qualified callee (`java.lang.Thread.sleep`) carries the receiver
    // in the same node; the selector is its last segment.
    let selector = callee_text.rsplit('.').next().unwrap_or(callee_text);
    if selector.is_empty() {
        return Unidentified;
    }
    let receiver = explicit_receiver_node(call, callee);
    let type_qualified = receiver.is_some()
        && explicit_receiver_is_type_qualifier(source, call, callee, file, provider);
    // `FileInputStream(path)` / `java.io.FileInputStream(path)` are spelled
    // like calls of the class name: K2 reports the constructed class as the
    // site owner and that spelling as the callee, while the physical row is
    // `<init>`. A method that happens to share its class name stays reachable
    // through the same filter and a two-row match stays `Unidentified`.
    let constructor_form = selector == simple_owner_name(owner) && selector == site_name;
    if !constructor_form && (selector != site_name || !is_jvm_member_name(site_name)) {
        return Unidentified;
    }
    let Some(expected) = expected_descriptor(&site) else {
        return Unidentified;
    };
    let Some(facts) = provider.classpath_member_facts(owner) else {
        return Unidentified;
    };
    let receiver_is_plain_value = matches!(
        receiver.map(|node| node.kind()),
        Some("identifier" | "simple_identifier")
    );
    let mut matching = facts.iter().filter(|row| {
        if full_row_descriptor(row).as_deref() != Some(expected.as_str()) {
            return false;
        }
        if row.name == "<init>" {
            return constructor_form;
        }
        if row.name != site_name {
            return false;
        }
        // Dispatch shape is only enforced where the source proves it. The
        // capitalization heuristic behind `explicit_receiver_is_type_qualifier`
        // sees a root segment, not the whole path (`java.lang.Thread.sleep` is
        // a type), so an unproved dotted receiver falls back to the unique
        // owner+name+descriptor row instead of guessing.
        if type_qualified {
            return row.static_member;
        }
        if receiver_is_plain_value {
            return !row.static_member;
        }
        true
    });
    let Some(row) = matching.next() else {
        return Unidentified;
    };
    if matching.next().is_some() {
        // Two physical members for one site is contradiction, not evidence.
        return Unidentified;
    }
    match row.checked_exceptions.as_deref() {
        None => CheckedThrows::Unknown,
        Some([]) => CheckedThrows::None,
        Some(checked) => CheckedThrows::Checked(checked.to_vec()),
    }
}

fn simple_owner_name(owner: &str) -> &str {
    owner.rsplit('.').next().unwrap_or(owner)
}

/// A JDK `java.*` binary name this slice may load through the platform loader.
///
/// `java.` (not `javax.`) is the slice boundary: it structurally excludes
/// Kotlin facades, value classes, companion/object shapes and third-party
/// jars, none of which the JDK loader can misrepresent as ordinary Java.
fn is_slice_owner(owner: &str) -> bool {
    owner.starts_with("java.")
        && owner.contains('.')
        && owner.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        })
}

/// A JVM method name, never an initializer or a mangled Kotlin ABI spelling.
fn is_jvm_member_name(name: &str) -> bool {
    if name.is_empty() || matches!(name, "<init>" | "<clinit>") {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_' || first == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// The Java reference classifier a simple receiver binding is *declared* with.
///
/// Only a formal parameter with an explicit type is accepted. The spelling
/// must resolve to a `java.*` owner either by full qualification or by exactly
/// one unaliased import under that name; a Kotlin logical type such as
/// `kotlin.collections.List` is rejected even though it erases to
/// `java.util.List`. A source type declaration with the same simple name, a
/// vararg parameter, a shadowing local, a lambda-inferred type, or a
/// nullable/type-parameter declaration all reject.
fn declared_receiver_owner<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    receiver: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<String> {
    if !matches!(receiver.kind(), "identifier" | "simple_identifier") {
        return None;
    }
    let spelling = receiver.utf8_text(source.as_bytes()).ok()?.trim();
    if spelling.is_empty() {
        return None;
    }
    let ty = lexical_parameter_type(source, call, spelling)?;
    if ty.nullable {
        return None;
    }
    let KotlinType::Named { name, .. } = ty.ty else {
        return None;
    };
    let simple = name.rsplit('.').next()?.trim();
    if simple.is_empty() {
        return None;
    }
    // A workspace type of that simple name always wins over an external
    // spelling; accepting it here would guess the emitted receiver type.
    if provider
        .symbols_named(simple)
        .iter()
        .any(|symbol| is_type_symbol(&symbol.id) || symbol.id.kind == "typealias")
    {
        return None;
    }
    if is_slice_owner(&name) {
        return Some(name);
    }
    if name.contains('.') {
        // Any other qualified spelling (`kotlin.*`, `javax.*`, third-party)
        // is outside this slice.
        return None;
    }
    // Simple spelling: accept only an exact, unique, unaliased import that
    // names a `java.*` owner. No wildcard import guessing, no alias.
    imported_slice_owner(source, call, &name)
}

/// One `import` header, read structurally from the source tree.
///
/// `path` is the dotted `qualified_identifier`; `alias` is the `as` name with
/// backticks removed (they escape the identifier, they are not part of the
/// spelled name); `wildcard` records a `.*` import, which binds no specific
/// name.
pub(crate) struct ImportHeader {
    pub(crate) path: String,
    pub(crate) alias: Option<String>,
    pub(crate) wildcard: bool,
}

impl ImportHeader {
    fn binds(&self, name: &str) -> bool {
        match self.alias.as_deref() {
            Some(alias) => alias == name,
            None if self.wildcard => false,
            None => self.path.rsplit('.').next() == Some(name),
        }
    }
}

pub(crate) fn import_header(source: &str, import: tree_sitter::Node<'_>) -> Option<ImportHeader> {
    let mut path = None;
    let mut alias = None;
    let mut wildcard = false;
    let mut cursor = import.walk();
    for child in import.children(&mut cursor) {
        match child.kind() {
            // Comments are trivia: `import java.util.stream.Stream /* x */`
            // binds exactly like the uncommented header.
            kind if kind.ends_with("comment") => {}
            // `import`, `as`, `.` and `;` are punctuation.
            "import" | "as" | "." | ";" => {}
            "*" => wildcard = true,
            "qualified_identifier" => {
                if path.is_some() {
                    return None;
                }
                path = Some(qualified_identifier_path(source, child)?);
            }
            "identifier" => {
                if alias.is_some() {
                    return None;
                }
                alias = Some(
                    child
                        .utf8_text(source.as_bytes())
                        .ok()?
                        .trim_matches('`')
                        .to_string(),
                );
            }
            // An unexpected import shape is not proof of anything.
            _ => return None,
        }
    }
    Some(ImportHeader {
        path: path?,
        alias,
        wildcard,
    })
}

fn qualified_identifier_path(source: &str, path: tree_sitter::Node<'_>) -> Option<String> {
    let mut segments = Vec::new();
    let mut cursor = path.walk();
    for segment in path.named_children(&mut cursor) {
        if segment.kind() != "identifier" {
            return None;
        }
        segments.push(segment.utf8_text(source.as_bytes()).ok()?);
    }
    (!segments.is_empty()).then(|| segments.join("."))
}

/// The `java.*` owner bound to a simple receiver spelling, proved from the
/// source tree's imports.
///
/// The binding comes from the call's own syntax tree — the root's `import`
/// nodes, their `qualified_identifier` segments and an optional `as` alias —
/// never from line text. A commented-out `import java.util.stream.Stream`
/// inside a block comment is not an `import` node, so it binds nothing.
///
/// Exactly one import must bind the spelled name. Kotlin itself reports
/// `conflicting import: imported name … is ambiguous` for two bindings (even
/// two identical headers), so a non-unique binding never proves an owner.
/// The unique binding must also be unaliased and non-wildcard:
///
/// * an alias changes the emitted Java type spelling without changing the
///   Kotlin one (`S<Integer>` is written verbatim) and the emitter drops
///   aliased Kotlin imports, so the generated Java does not resolve; retention
///   is the honest floor until emission canonicalises aliases;
/// * a wildcard import neither binds this name nor proves which declaration it
///   denotes.
fn imported_slice_owner(source: &str, call: tree_sitter::Node<'_>, name: &str) -> Option<String> {
    let mut root = call;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut bound = Vec::new();
    let mut cursor = root.walk();
    for import in root.named_children(&mut cursor) {
        if import.kind() != "import" {
            continue;
        }
        let header = import_header(source, import)?;
        if header.binds(name) {
            bound.push(header);
        }
    }
    let [header] = bound.as_slice() else {
        return None;
    };
    if header.alias.is_some() || header.wildcard {
        return None;
    }
    is_slice_owner(&header.path).then(|| header.path.clone())
}

/// The JVM descriptor a K2 site claims for the member it resolved.
///
/// One documented normalization: a Java `void` method is `kotlin.Unit` on the
/// Kotlin side, and K2 renders that as either `…V` or `…Lkotlin/Unit;`. Any
/// other K2 shape with a Unit result is contradictory and must not be
/// normalized away — a descriptor is the member identity, not a suggestion.
fn expected_descriptor(site: &crate::facts::protocol::CallSite) -> Option<String> {
    let descriptor = site.jvm_descriptor.as_deref()?.trim();
    if !descriptor.starts_with('(') || !descriptor.contains(')') {
        return None;
    }
    if site.result_type.as_deref() == Some("kotlin.Unit") && !site.result_nullable {
        if let Some(head) = descriptor.strip_suffix("Lkotlin/Unit;") {
            return Some(format!("{head}V"));
        }
        if !descriptor.ends_with('V') {
            return None;
        }
    }
    Some(descriptor.to_string())
}

/// Reconstruct the physical JVM descriptor of a `MemberFacts` row.
///
/// `MemberFacts` carries erased binary names (`java.lang.String`), JVM
/// primitive spellings and `<void>`, in declaration order; the descriptor is
/// therefore `(parameters…)result`, including every parameter so that
/// overloads are distinguishable by shape rather than by name and count.
fn full_row_descriptor(row: &MemberFacts) -> Option<String> {
    let mut descriptor = String::from("(");
    for parameter in &row.parameter_types {
        descriptor.push_str(&jvm_type_descriptor(parameter)?);
    }
    descriptor.push(')');
    descriptor.push_str(&jvm_type_descriptor(&row.result_type)?);
    Some(descriptor)
}

fn jvm_type_descriptor(name: &str) -> Option<String> {
    match name {
        "<void>" => Some("V".to_string()),
        "int" => Some("I".to_string()),
        "long" => Some("J".to_string()),
        "double" => Some("D".to_string()),
        "float" => Some("F".to_string()),
        "boolean" => Some("Z".to_string()),
        "char" => Some("C".to_string()),
        "byte" => Some("B".to_string()),
        "short" => Some("S".to_string()),
        other => reference_descriptor(other),
    }
}

/// `Class.getName()` reference spelling as a JVM field descriptor.
///
/// Object names become `Lowner;` with `/` separators (`$` preserved); arrays
/// keep their leading `[` and primitive code, or convert the `L...;` element.
fn reference_descriptor(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if let Some(rest) = name.strip_prefix('[') {
        if rest.is_empty() {
            return None;
        }
        if let Some(object) = rest.strip_prefix('L').and_then(|n| n.strip_suffix(';')) {
            if object.is_empty()
                || !object
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '$')
            {
                return None;
            }
            return Some(format!("[L{};", object.replace('.', "/")));
        }
        let mut chars = rest.chars();
        let first = chars.next()?;
        if chars.next().is_some() || !matches!(first, 'I' | 'J' | 'D' | 'F' | 'Z' | 'C' | 'B' | 'S')
        {
            return None;
        }
        return Some(format!("[{first}"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '$')
        || !name.contains('.')
    {
        return None;
    }
    Some(format!("L{};", name.replace('.', "/")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::decorator::FactsProvider;
    use crate::facts::runtime::FactsRuntime;
    use crate::semantics::SyntaxSemanticProvider;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "notlin-java-member-calls-{}-{id}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A scripted helper that answers `Ping`, echoes the asked file for
    /// `ResolveCalls` with `SITES`, and answers `ClassMembers` for one `OWNER`
    /// with `MEMBERS` (every other owner is `members: null`, the helper's
    /// Unknown).
    fn scripted_helper(sites: String, owner: &str, members: String) -> Command {
        let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"Ping"'*)
      printf '{"version":1,"id":%s,"answer":{"Pong":{"version":1}}}\n' "$id"
      ;;
    *'"ResolveCalls"'*)
      file=$(printf '%s' "$line" | sed -n 's/.*"file":"\([^"]*\)".*/\1/p')
      printf '{"version":1,"id":%s,"answer":{"CallSites":{"file":"%s","sites":%s}}}\n' "$id" "$file" "$SITES"
      ;;
    *'"ClassMembers"'*)
      asked=$(printf '%s' "$line" | sed -n 's/.*"owner":"\([^"]*\)".*/\1/p')
      if [ "$asked" = "$OWNER" ]; then
        printf '{"version":1,"id":%s,"answer":{"Members":{"owner":"%s","members":%s}}}\n' "$id" "$asked" "$MEMBERS"
      else
        printf '{"version":1,"id":%s,"answer":{"Members":{"owner":"%s","members":null}}}\n' "$id" "$asked"
      fi
      ;;
  esac
done"#;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .env("SITES", sites)
            .env("OWNER", owner)
            .env("MEMBERS", members);
        command
    }

    fn member(name: &str, statik: bool, params: &str, result: &str, throws: &str) -> String {
        format!(
            r#"{{"name":"{name}","static_member":{statik},"parameter_types":{params},"result_type":"{result}","parameter_nullability":[],"result_nullability":"unknown","checked_exceptions":{throws}}}"#
        )
    }

    fn site(
        offset: u64,
        owner: &str,
        name: &str,
        params: &str,
        result: &str,
        nullable: bool,
        descriptor: &str,
    ) -> String {
        format!(
            r#"[{{"byte_offset":{offset},"text":"{name}()","status":"Established","callee_owner":"{owner}","callee_name":"{name}","parameter_types":{params},"result_type":"{result}","result_nullable":{nullable},"jvm_descriptor":"{descriptor}"}}]"#
        )
    }

    /// The call node whose selector text is `name`.
    fn call_named<'tree>(
        root: tree_sitter::Node<'tree>,
        source: &str,
        name: &str,
    ) -> tree_sitter::Node<'tree> {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression"
                && let Some(callee) = call_callee_node(node)
                && callee.utf8_text(source.as_bytes()).ok() == Some(name)
            {
                return node;
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        panic!("no `{name}` call in fixture");
    }

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    fn provider_for(
        file: &Path,
        source: &str,
        sites: String,
        owner: &str,
        members: String,
    ) -> FactsProvider {
        let runtime = FactsRuntime::with_command(scripted_helper(sites, owner, members)).unwrap();
        FactsProvider::with_runtime(
            SyntaxSemanticProvider::new([(file.to_path_buf(), source.to_owned())]),
            runtime,
        )
    }

    /// Run the real producer (`propose`) on `source`.
    ///
    /// The emission gate is deliberately *not* used here: after the
    /// declaration-contract redesign, emission answers only from the planned
    /// receipts, and these tests pin what the producer signs.
    fn accepted(source: &str, sites: String, owner: &str, members: String, method: &str) -> bool {
        let scratch = Scratch::new();
        let file = scratch.0.join("Fixture.kt");
        fs::write(&file, source).unwrap();
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, method);
        let provider = provider_for(&file, source, sites, owner, members);
        propose(source, &file, call, &provider).is_some()
    }

    /// The emission gate must stay source-only: a helper fact identity plus a
    /// complete physical proof is *not* an unconditional positive outside a
    /// planned receipt.
    fn emission_gate(
        source: &str,
        sites: String,
        owner: &str,
        members: String,
        method: &str,
    ) -> bool {
        let scratch = Scratch::new();
        let file = scratch.0.join("Fixture.kt");
        fs::write(&file, source).unwrap();
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, method);
        let provider = provider_for(&file, source, sites, owner, members);
        crate::semantics::established_java_callable_member(source, &file, call, &provider)
    }

    fn stream_source(declaration: &str) -> String {
        format!("package fixture\n\nfun f(s: {declaration}) = s.sorted()\n")
    }

    fn sorted_site(source: &str, descriptor: &str, result: &str, nullable: bool) -> String {
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "sorted");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        site(
            offset,
            "java.util.stream.Stream",
            "sorted",
            "[]",
            result,
            nullable,
            descriptor,
        )
    }

    const STREAM_ROWS: fn(&str) -> String = |throws: &str| {
        format!(
            "[{}]",
            member("sorted", false, "[]", "java.util.stream.Stream", throws)
        )
    };

    #[test]
    fn accepts_a_jdk_instance_call_with_empty_checked_throws() {
        let source = stream_source("java.util.stream.Stream<String>");
        let sites = sorted_site(
            &source,
            "()Ljava/util/stream/Stream;",
            "java.util.stream.Stream",
            false,
        );
        assert!(accepted(
            &source,
            sites,
            "java.util.stream.Stream",
            STREAM_ROWS(r#"[]"#),
            "sorted"
        ));
    }

    #[test]
    fn accepts_exactly_one_unaliased_import_binding_under_the_declared_name() {
        // The binding is read from the call's own syntax tree, so trivia is
        // irrelevant: a tab after `import` or a comment inside the header does
        // not change which declaration `Stream` denotes.
        for source in [
            "package fixture\n\nimport java.util.stream.Stream\n\nfun f(s: Stream<String>) = s.sorted()\n",
            "package fixture\n\nimport\tjava.util.stream.Stream\n\nfun f(s: Stream<String>) = s.sorted()\n",
            "package fixture\n\nimport java.util.stream.Stream /* keep */\n\nfun f(s: Stream<String>) = s.sorted()\n",
        ] {
            let sites = sorted_site(
                source,
                "()Ljava/util/stream/Stream;",
                "java.util.stream.Stream",
                false,
            );
            assert!(
                accepted(
                    source,
                    sites,
                    "java.util.stream.Stream",
                    STREAM_ROWS(r#"[]"#),
                    "sorted"
                ),
                "rejected {source}"
            );
        }
    }

    #[test]
    fn rejects_an_aliased_import_binding() {
        // Emitted Java keeps the *source* spelling (`S<Integer>`), and the
        // emitter drops aliased Kotlin imports, so a proved call would emit
        // Java that javac cannot resolve. Retention is the honest floor until
        // an alias-canonicalising emission gate exists. The redundant
        // `as Stream` spelling is rejected too: only the unconditional
        // `java.util.stream.Stream` header import made that one shape resolve,
        // which is not a general canonicalisation proof.
        for source in [
            "package fixture\n\nimport java.util.stream.Stream as S\n\nfun f(s: S<String>) = s.sorted()\n",
            "package fixture\n\nimport java.util.stream.Stream as Stream\n\nfun f(s: Stream<String>) = s.sorted()\n",
            "package fixture\n\nimport\tjava.util.stream.Stream\tas\tS\n\nfun f(s: S<String>) = s.sorted()\n",
            "package fixture\n\nimport java.util.stream.Stream /* c */ as S\n\nfun f(s: S<String>) = s.sorted()\n",
        ] {
            let sites = sorted_site(
                source,
                "()Ljava/util/stream/Stream;",
                "java.util.stream.Stream",
                false,
            );
            assert!(
                !accepted(
                    source,
                    sites,
                    "java.util.stream.Stream",
                    STREAM_ROWS(r#"[]"#),
                    "sorted"
                ),
                "accepted aliased {source}"
            );
        }
    }

    #[test]
    fn rejects_a_block_comment_that_spells_a_java_import() {
        // The declared `Stream` binds `com.example.Stream`; the commented-out
        // import must not become the receiver's owner. Scripted mutation with
        // a deliberately contradictory Java owner: this is not native wire
        // evidence, it pins the source-binding gate.
        let source = "package fixture\n\nimport com.example.Stream\n\n/*\nimport java.util.stream.Stream\n*/\n\nfun f(s: Stream<String>) = s.sorted()\n";
        let sites = sorted_site(
            source,
            "()Ljava/util/stream/Stream;",
            "java.util.stream.Stream",
            false,
        );
        assert!(!accepted(
            source,
            sites,
            "java.util.stream.Stream",
            STREAM_ROWS(r#"[]"#),
            "sorted"
        ));
    }

    #[test]
    fn rejects_duplicate_or_conflicting_import_bindings() {
        // Kotlin itself refuses every one of these (`conflicting import:
        // imported name 'Stream' is ambiguous`), so the binding is not unique
        // and cannot prove an owner. The scripted Java owner is contradictory
        // on purpose: only the source gate is under test.
        for source in [
            // Duplicate identical import.
            "package fixture\n\nimport java.util.stream.Stream\nimport java.util.stream.Stream\n\nfun f(s: Stream<String>) = s.sorted()\n",
            // Two owners for the same simple name.
            "package fixture\n\nimport java.util.stream.Stream\nimport com.example.Stream\n\nfun f(s: Stream<String>) = s.sorted()\n",
            // An alias competing with the plain import.
            "package fixture\n\nimport com.example.Stream as Stream\nimport java.util.stream.Stream\n\nfun f(s: Stream<String>) = s.sorted()\n",
            // A backticked alias binds the same spelling.
            "package fixture\n\nimport com.example.Stream as `Stream`\nimport java.util.stream.Stream\n\nfun f(s: Stream<String>) = s.sorted()\n",
        ] {
            let sites = sorted_site(
                source,
                "()Ljava/util/stream/Stream;",
                "java.util.stream.Stream",
                false,
            );
            assert!(
                !accepted(
                    source,
                    sites,
                    "java.util.stream.Stream",
                    STREAM_ROWS(r#"[]"#),
                    "sorted"
                ),
                "accepted ambiguous {source}"
            );
        }
    }

    #[test]
    fn rejects_local_loop_and_catch_shadowed_receivers() {
        // `lexical_parameter_type` refuses a formal parameter when an earlier
        // local, for-loop variable or catch parameter can bind the spelling.
        // Each call's *declared* type would otherwise be the java.* Stream.
        for source in [
            // Local val in a nested block (`List<String>.sorted()`).
            "package fixture\n\nfun f(values: java.util.stream.Stream<String>): java.util.stream.Stream<String> {\n    if (values.hashCode() != 0) {\n        val values = listOf(\"shadow\")\n        values.sorted()\n    }\n    return values\n}\n",
            // For-loop variable over a list of lists.
            "package fixture\n\nfun f(values: java.util.stream.Stream<String>): Any {\n    for (values in listOf(listOf(\"shadow\"))) {\n        return values.sorted()\n    }\n    return values\n}\n",
            // Catch parameter (a Throwable with its own `sorted()`).
            "package fixture\n\nclass ShadowError : Exception() {\n    fun sorted(): List<Int> = listOf(1)\n}\n\nfun f(values: java.util.stream.Stream<String>): Any {\n    try {\n        return values\n    } catch (values: ShadowError) {\n        return values.sorted()\n    }\n}\n",
        ] {
            let sites = sorted_site(
                source,
                "()Ljava/util/stream/Stream;",
                "java.util.stream.Stream",
                false,
            );
            assert!(
                !accepted(
                    source,
                    sites,
                    "java.util.stream.Stream",
                    STREAM_ROWS(r#"[]"#),
                    "sorted"
                ),
                "accepted shadowed {source}"
            );
        }
    }

    #[test]
    fn accepts_the_documented_kotlin_unit_void_normalization() {
        // The native shape: K2 reports `java.util.ArrayList.clear()` as
        // `kotlin.Unit` with descriptor `()Lkotlin/Unit;` while the physical
        // row is `<void>`. Using the owner K2 actually reports keeps the
        // normalization test free of a contradictory owner claim.
        let source = "package fixture\n\nfun f(l: java.util.ArrayList<String>) = l.clear()\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "clear");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.util.ArrayList",
            "clear",
            "[]",
            "kotlin.Unit",
            false,
            "()Lkotlin/Unit;",
        );
        assert!(accepted(
            source,
            sites,
            "java.util.ArrayList",
            format!("[{}]", member("clear", false, "[]", "<void>", "[]")),
            "clear"
        ));
    }

    #[test]
    fn rejects_the_base_stream_owner_of_close_without_widening_the_scope() {
        // `Stream` inherits `close()` from `java.util.stream.BaseStream`, and
        // that is the owner K2 reports. The exact declared-classifier gate
        // therefore rejects it: the receiver is declared as `Stream`, not as
        // the interface that actually declares the member. This is a
        // documented boundary, not a reason to widen the owner match.
        let source = "package fixture\n\nfun f(s: java.util.stream.Stream<String>) = s.close()\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "close");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.util.stream.BaseStream",
            "close",
            "[]",
            "kotlin.Unit",
            false,
            "()Lkotlin/Unit;",
        );
        assert!(!accepted(
            source,
            sites,
            "java.util.stream.BaseStream",
            format!("[{}]", member("close", false, "[]", "<void>", "[]")),
            "close"
        ));
    }

    #[test]
    fn rejects_unknown_checked_exceptions() {
        let source = stream_source("java.util.stream.Stream<String>");
        let sites = sorted_site(
            &source,
            "()Ljava/util/stream/Stream;",
            "java.util.stream.Stream",
            false,
        );
        // Old-shape row: no field at all.
        let old_shape = r#"[{"name":"sorted","static_member":false,"parameter_types":[],"result_type":"java.util.stream.Stream","parameter_nullability":[],"result_nullability":"unknown"}]"#;
        assert!(!accepted(
            &source,
            sites,
            "java.util.stream.Stream",
            old_shape.to_string(),
            "sorted"
        ));
        // Explicit null is the same Unknown.
        assert!(!accepted(
            &source,
            sorted_site(
                &source,
                "()Ljava/util/stream/Stream;",
                "java.util.stream.Stream",
                false
            ),
            "java.util.stream.Stream",
            STREAM_ROWS("null"),
            "sorted"
        ));
    }

    #[test]
    fn rejects_a_declaration_that_throws_a_checked_exception() {
        let source = stream_source("java.util.stream.Stream<String>");
        let sites = sorted_site(
            &source,
            "()Ljava/util/stream/Stream;",
            "java.util.stream.Stream",
            false,
        );
        assert!(!accepted(
            &source,
            sites,
            "java.util.stream.Stream",
            STREAM_ROWS(r#"["java.io.IOException"]"#),
            "sorted"
        ));
    }

    #[test]
    fn rejects_a_descriptor_that_differs_from_the_physical_row() {
        let source = stream_source("java.util.stream.Stream<String>");
        let sites = sorted_site(
            &source,
            "()Ljava/util/stream/Stream;",
            "java.util.stream.Stream",
            false,
        );
        let rows = format!(
            "[{}]",
            member("sorted", false, "[]", "java.lang.String", "[]")
        );
        assert!(!accepted(
            &source,
            sites,
            "java.util.stream.Stream",
            rows,
            "sorted"
        ));
    }

    #[test]
    fn rejects_contradictory_duplicate_rows() {
        let source = stream_source("java.util.stream.Stream<String>");
        let sites = sorted_site(
            &source,
            "()Ljava/util/stream/Stream;",
            "java.util.stream.Stream",
            false,
        );
        let rows = format!(
            "[{},{}]",
            member("sorted", false, "[]", "java.util.stream.Stream", "[]"),
            member("sorted", false, "[]", "java.util.stream.Stream", "[]")
        );
        assert!(!accepted(
            &source,
            sites,
            "java.util.stream.Stream",
            rows,
            "sorted"
        ));
    }

    #[test]
    fn rejects_a_static_row_for_a_value_receiver() {
        let source = stream_source("java.util.stream.Stream<String>");
        let sites = sorted_site(
            &source,
            "()Ljava/util/stream/Stream;",
            "java.util.stream.Stream",
            false,
        );
        let rows = format!(
            "[{}]",
            member("sorted", true, "[]", "java.util.stream.Stream", "[]")
        );
        assert!(!accepted(
            &source,
            sites,
            "java.util.stream.Stream",
            rows,
            "sorted"
        ));
    }

    #[test]
    fn rejects_declared_parameters_even_when_the_site_claims_none() {
        // Contradictory script: K2 declares zero parameters but the physical
        // row takes one. The descriptor comparison alone would reject this;
        // the declared-parameter gate rejects it first.
        let source = stream_source("java.util.stream.Stream<String>");
        let sites = sorted_site(
            &source,
            "()Ljava/util/stream/Stream;",
            "java.util.stream.Stream",
            false,
        );
        let rows = format!(
            "[{}]",
            member(
                "sorted",
                false,
                "[\"java.util.Comparator\"]",
                "java.util.stream.Stream",
                "[]"
            )
        );
        assert!(!accepted(
            &source,
            sites,
            "java.util.stream.Stream",
            rows,
            "sorted"
        ));

        // The same field non-empty on the site.
        let tree = parse(&source);
        let call = call_named(tree.root_node(), &source, "sorted");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let params_site = site(
            offset,
            "java.util.stream.Stream",
            "sorted",
            "[\"kotlin.Comparator\"]",
            "java.util.stream.Stream",
            false,
            "()Ljava/util/stream/Stream;",
        );
        assert!(!accepted(
            &source,
            params_site,
            "java.util.stream.Stream",
            STREAM_ROWS("[]"),
            "sorted"
        ));
    }

    #[test]
    fn rejects_actual_source_arguments_even_with_an_empty_k2_parameter_list() {
        // Contradictory script: the call passes one argument but the helper
        // claims a zero-parameter declaration. The emitter-shape gate is the
        // actual source argument count, not the helper's answer.
        let source = "package fixture\n\nfun f(s: java.util.stream.Stream<String>) = s.sorted(java.util.Comparator.naturalOrder())\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "sorted");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.util.stream.Stream",
            "sorted",
            "[]",
            "java.util.stream.Stream",
            false,
            "()Ljava/util/stream/Stream;",
        );
        assert!(!accepted(
            source,
            sites,
            "java.util.stream.Stream",
            STREAM_ROWS("[]"),
            "sorted"
        ));
    }

    #[test]
    fn rejects_a_trailing_lambda() {
        let source =
            "package fixture\n\nfun f(s: java.util.stream.Stream<String>) = s.forEach { }\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "forEach");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.util.stream.Stream",
            "forEach",
            "[]",
            "kotlin.Unit",
            false,
            "()V",
        );
        assert!(!accepted(
            source,
            sites,
            "java.util.stream.Stream",
            format!("[{}]", member("forEach", false, "[]", "<void>", "[]")),
            "forEach"
        ));
    }

    #[test]
    fn rejects_owners_outside_the_jdk_java_slice() {
        for owner in [
            "kotlin.collections.List",
            "javax.swing.JFrame",
            "com.example.Thing",
        ] {
            let source = stream_source("java.util.stream.Stream<String>");
            let tree = parse(&source);
            let call = call_named(tree.root_node(), &source, "sorted");
            let offset = kotlin_call_site_offset(call).unwrap() as u64;
            let sites = site(
                offset,
                owner,
                "sorted",
                "[]",
                "java.util.stream.Stream",
                false,
                "()Ljava/util/stream/Stream;",
            );
            assert!(
                !accepted(&source, sites, owner, STREAM_ROWS("[]"), "sorted"),
                "accepted owner {owner}"
            );
        }
    }

    #[test]
    fn rejects_a_kotlin_logical_receiver_type_even_when_it_erases_to_java() {
        // `kotlin.collections.List` erases to `java.util.List`; the helper's
        // contradictory owner claim must not turn that logical type into a
        // proved Java receiver.
        let source = "package fixture\n\nfun f(l: kotlin.collections.List<String>) = l.sorted()\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "sorted");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.util.List",
            "sorted",
            "[]",
            "java.util.List",
            false,
            "()Ljava/util/List;",
        );
        assert!(!accepted(
            source,
            sites,
            "java.util.List",
            format!(
                "[{}]",
                member("sorted", false, "[]", "java.util.List", "[]")
            ),
            "sorted"
        ));
    }

    #[test]
    fn rejects_receivers_that_are_not_a_simple_declared_binding() {
        // Call result.
        let call_result = "package fixture\n\nfun make(): java.util.stream.Stream<String> = TODO()\n\nfun f() = make().sorted()\n";
        let tree = parse(call_result);
        let call = call_named(tree.root_node(), call_result, "sorted");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.util.stream.Stream",
            "sorted",
            "[]",
            "java.util.stream.Stream",
            false,
            "()Ljava/util/stream/Stream;",
        );
        assert!(!accepted(
            call_result,
            sites,
            "java.util.stream.Stream",
            STREAM_ROWS("[]"),
            "sorted"
        ));

        // Property chain receiver.
        let property = "package fixture\n\nclass Holder(val s: java.util.stream.Stream<String>)\n\nfun f(h: Holder) = h.s.sorted()\n";
        let tree = parse(property);
        let call = call_named(tree.root_node(), property, "sorted");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.util.stream.Stream",
            "sorted",
            "[]",
            "java.util.stream.Stream",
            false,
            "()Ljava/util/stream/Stream;",
        );
        assert!(!accepted(
            property,
            sites,
            "java.util.stream.Stream",
            STREAM_ROWS("[]"),
            "sorted"
        ));
    }

    #[test]
    fn rejects_a_smart_cast_only_receiver() {
        // The declared type is `Any`; the smart cast proves the call in Kotlin
        // but not in the emitted Java identifier declaration.
        let source = "package fixture\n\nfun f(o: Any) = if (o is java.util.stream.Stream<*>) o.sorted() else null\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "sorted");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.util.stream.Stream",
            "sorted",
            "[]",
            "java.util.stream.Stream",
            false,
            "()Ljava/util/stream/Stream;",
        );
        assert!(!accepted(
            source,
            sites,
            "java.util.stream.Stream",
            STREAM_ROWS("[]"),
            "sorted"
        ));
    }

    #[test]
    fn rejects_a_bare_simple_receiver_type_without_import_proof() {
        let source = "package fixture\n\nfun f(s: Stream<String>) = s.sorted()\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "sorted");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.util.stream.Stream",
            "sorted",
            "[]",
            "java.util.stream.Stream",
            false,
            "()Ljava/util/stream/Stream;",
        );
        assert!(!accepted(
            source,
            sites,
            "java.util.stream.Stream",
            STREAM_ROWS("[]"),
            "sorted"
        ));
    }

    #[test]
    fn rejects_a_stale_source_hash_without_answering_from_disk_facts() {
        let disk = stream_source("java.util.stream.Stream<String>");
        let scratch = Scratch::new();
        let file = scratch.0.join("Fixture.kt");
        fs::write(&file, &disk).unwrap();
        // Same offsets, different bytes: the planned snapshot is not on disk.
        let planned = format!("{disk}// speculative revision\n");
        let tree = parse(&planned);
        let call = call_named(tree.root_node(), &planned, "sorted");
        let sites = sorted_site(
            &planned,
            "()Ljava/util/stream/Stream;",
            "java.util.stream.Stream",
            false,
        );
        let provider = provider_for(
            &file,
            &planned,
            sites,
            "java.util.stream.Stream",
            STREAM_ROWS("[]"),
        );
        assert!(!crate::semantics::established_java_callable_member(
            &planned, &file, call, &provider
        ));
    }

    #[test]
    fn rejects_an_import_spelling_shadowed_by_a_source_typealias() {
        // A typealias with the imported simple name can change the declared
        // receiver type; the import alone is not proof.
        let source = "package fixture\n\nimport java.util.stream.Stream\n\ntypealias Stream<T> = kotlin.collections.List<T>\n\nfun f(s: Stream<String>) = s.sorted()\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "sorted");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.util.stream.Stream",
            "sorted",
            "[]",
            "java.util.stream.Stream",
            false,
            "()Ljava/util/stream/Stream;",
        );
        assert!(!accepted(
            source,
            sites,
            "java.util.stream.Stream",
            STREAM_ROWS("[]"),
            "sorted"
        ));
    }

    #[test]
    fn rejects_a_site_whose_resolved_name_differs_from_the_emitted_selector() {
        // Contradictory callee: the source spells `distinct` but the site
        // claims the resolved member is `sorted`. The emitter writes the
        // source selector, so the proof must not transfer.
        let source =
            "package fixture\n\nfun f(s: java.util.stream.Stream<String>) = s.distinct()\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "distinct");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.util.stream.Stream",
            "sorted",
            "[]",
            "java.util.stream.Stream",
            false,
            "()Ljava/util/stream/Stream;",
        );
        assert!(!accepted(
            source,
            sites,
            "java.util.stream.Stream",
            STREAM_ROWS("[]"),
            "distinct"
        ));
    }

    #[test]
    fn rejects_unit_normalization_for_a_nullable_or_contradictory_descriptor() {
        let source = "package fixture\n\nfun f(l: java.util.ArrayList<String>) = l.clear()\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "clear");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let rows = format!("[{}]", member("clear", false, "[]", "<void>", "[]"));

        // `kotlin.Unit?` has no proved physical mapping.
        let nullable_unit = site(
            offset,
            "java.util.ArrayList",
            "clear",
            "[]",
            "kotlin.Unit",
            true,
            "()Lkotlin/Unit;",
        );
        assert!(!accepted(
            source,
            nullable_unit,
            "java.util.ArrayList",
            rows.clone(),
            "clear"
        ));

        // A non-Unit K2 descriptor with a Unit result is contradictory.
        let contradictory = site(
            offset,
            "java.util.ArrayList",
            "clear",
            "[]",
            "kotlin.Unit",
            false,
            "()Ljava/lang/String;",
        );
        assert!(!accepted(
            source,
            contradictory,
            "java.util.ArrayList",
            rows,
            "clear"
        ));
    }

    // ---------------------------------------------------------------------
    // `checked_throws`: the same call-scoped evidence, asked what the callee
    // declares. Only `None` is a safety claim; `Unidentified` is not a verdict.
    // ---------------------------------------------------------------------

    /// Run the real probe on the first `method` call of `source`.
    fn throws(
        source: &str,
        sites: String,
        owner: &str,
        members: String,
        method: &str,
    ) -> CheckedThrows {
        let scratch = Scratch::new();
        let file = scratch.0.join("Fixture.kt");
        fs::write(&file, source).unwrap();
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, method);
        let provider = provider_for(&file, source, sites, owner, members);
        checked_throws(source, &file, call, &provider)
    }

    fn stream_throws_source() -> String {
        "package fixture\n\nfun f(input: java.io.InputStream): Int = input.read()\n".to_string()
    }

    fn read_site(source: &str) -> String {
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "read");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        site(
            offset,
            "java.io.InputStream",
            "read",
            "[]",
            "kotlin.Int",
            false,
            "()I",
        )
    }

    const READ_ROWS: fn(&str) -> String =
        |throws: &str| format!("[{}]", member("read", false, "[]", "int", throws));

    #[test]
    fn a_checked_physical_row_is_reported_as_checked() {
        let source = stream_throws_source();
        assert_eq!(
            throws(
                &source,
                read_site(&source),
                "java.io.InputStream",
                READ_ROWS(r#"["java.io.IOException"]"#),
                "read"
            ),
            CheckedThrows::Checked(vec!["java.io.IOException".to_string()])
        );
    }

    #[test]
    fn an_empty_row_list_is_proved_none_and_absent_metadata_is_unknown() {
        let source = stream_throws_source();
        assert_eq!(
            throws(
                &source,
                read_site(&source),
                "java.io.InputStream",
                READ_ROWS("[]"),
                "read"
            ),
            CheckedThrows::None
        );
        let old_shape = r#"[{"name":"read","static_member":false,"parameter_types":[],"result_type":"int","parameter_nullability":[],"result_nullability":"unknown"}]"#;
        assert_eq!(
            throws(
                &source,
                read_site(&source),
                "java.io.InputStream",
                old_shape.to_string(),
                "read"
            ),
            CheckedThrows::Unknown
        );
    }

    /// `Thread.sleep(1L)` is a *static* member with a parameter: identity is
    /// the complete descriptor, not the spelling or the arity.
    #[test]
    fn a_static_member_with_arguments_is_identified_by_descriptor() {
        let source = "package fixture\n\nfun pause() {\n    java.lang.Thread.sleep(1L)\n}\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "sleep");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.lang.Thread",
            "sleep",
            r#"["kotlin.Long"]"#,
            "kotlin.Unit",
            false,
            "(J)V",
        );
        let rows = format!(
            "[{},{}]",
            member(
                "sleep",
                true,
                r#"["long"]"#,
                "<void>",
                r#"["java.lang.InterruptedException"]"#
            ),
            member(
                "sleep",
                true,
                r#"["long","int"]"#,
                "<void>",
                r#"["java.lang.InterruptedException"]"#
            )
        );
        assert_eq!(
            throws(source, sites, "java.lang.Thread", rows, "sleep"),
            CheckedThrows::Checked(vec!["java.lang.InterruptedException".to_string()])
        );
    }

    /// A constructor site: K2 spells the class name, the physical row is
    /// `<init>`, and the checked exception lives on that row.
    #[test]
    fn a_constructor_site_matches_its_init_row() {
        let source = "package fixture\n\nfun open(): java.io.FileInputStream = java.io.FileInputStream(\"data.txt\")\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "FileInputStream");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "java.io.FileInputStream",
            "FileInputStream",
            r#"["kotlin.String"]"#,
            "java.io.FileInputStream",
            false,
            "(Ljava/lang/String;)V",
        );
        let rows = format!(
            "[{},{}]",
            member(
                "<init>",
                false,
                r#"["java.lang.String"]"#,
                "<void>",
                r#"["java.io.FileNotFoundException"]"#
            ),
            member("<init>", false, r#"["java.io.File"]"#, "<void>", "[]")
        );
        assert_eq!(
            throws(
                source,
                sites,
                "java.io.FileInputStream",
                rows,
                "FileInputStream"
            ),
            CheckedThrows::Checked(vec!["java.io.FileNotFoundException".to_string()])
        );
    }

    /// A same-named row with another parameter list is a different member: the
    /// probe must not import its throws clause into this call.
    #[test]
    fn an_overload_with_another_descriptor_is_unidentified() {
        let source = stream_throws_source();
        let rows = format!(
            "[{}]",
            member(
                "read",
                false,
                r#"["[B"]"#,
                "int",
                r#"["java.io.IOException"]"#
            )
        );
        assert_eq!(
            throws(
                &source,
                read_site(&source),
                "java.io.InputStream",
                rows,
                "read"
            ),
            CheckedThrows::Unidentified
        );
    }

    /// Two physical rows for one site is contradiction, not evidence.
    #[test]
    fn two_matching_rows_are_unidentified() {
        let source = stream_throws_source();
        let rows = format!(
            "[{},{}]",
            member("read", false, "[]", "int", "[]"),
            member("read", false, "[]", "int", r#"["java.io.IOException"]"#)
        );
        assert_eq!(
            throws(
                &source,
                read_site(&source),
                "java.io.InputStream",
                rows,
                "read"
            ),
            CheckedThrows::Unidentified
        );
    }

    /// A site that names another callee than the source selector proves
    /// nothing about this call.
    #[test]
    fn a_site_naming_another_selector_is_unidentified() {
        let source = stream_throws_source();
        let tree = parse(&source);
        let call = call_named(tree.root_node(), &source, "read");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let contradictory = site(
            offset,
            "java.io.InputStream",
            "close",
            "[]",
            "kotlin.Unit",
            false,
            "()V",
        );
        assert_eq!(
            throws(
                &source,
                contradictory,
                "java.io.InputStream",
                READ_ROWS(r#"["java.io.IOException"]"#),
                "read"
            ),
            CheckedThrows::Unidentified
        );
    }

    /// A non-JDK owner is outside the loadable slice: no verdict either way.
    #[test]
    fn a_non_jdk_owner_is_unidentified() {
        let source = stream_throws_source();
        let tree = parse(&source);
        let call = call_named(tree.root_node(), &source, "read");
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let sites = site(
            offset,
            "org.example.Input",
            "read",
            "[]",
            "kotlin.Int",
            false,
            "()I",
        );
        assert_eq!(
            throws(
                &source,
                sites,
                "org.example.Input",
                READ_ROWS(r#"["java.io.IOException"]"#),
                "read"
            ),
            CheckedThrows::Unidentified
        );
    }

    /// The emission gate must not admit a helper-proved call from the fact
    /// identity alone. The positive lives in the planned receipt; keeping a
    /// fact-symbol positive here is what let a `Stream.sorted()` proof leak
    /// unproved sibling lowerings out of the same declaration.
    #[test]
    fn the_emission_gate_is_source_only_and_the_receipt_is_the_positive() {
        let source = stream_source("java.util.stream.Stream<String>");
        let sites = sorted_site(
            &source,
            "()Ljava/util/stream/Stream;",
            "java.util.stream.Stream",
            false,
        );
        let rows = STREAM_ROWS(r#"[]"#);
        assert!(
            accepted(
                &source,
                sites.clone(),
                "java.util.stream.Stream",
                rows.clone(),
                "sorted"
            ),
            "the producer must sign a fully proved call"
        );
        assert!(
            !emission_gate(&source, sites, "java.util.stream.Stream", rows, "sorted"),
            "the emission gate must stay source-only: a helper fact is not an unconditional \
             positive"
        );
    }

    /// The receipt is the shared object: physical callee identity, argument
    /// mapping, result representation and effects, all snapshot-bound.
    #[test]
    fn the_receipt_records_the_physical_representation() {
        let source = stream_source("java.util.stream.Stream<String>");
        let tree = parse(&source);
        let call = call_named(tree.root_node(), &source, "sorted");
        let scratch = Scratch::new();
        let file = scratch.0.join("Fixture.kt");
        fs::write(&file, &source).unwrap();
        let provider = provider_for(
            &file,
            &source,
            sorted_site(
                &source,
                "()Ljava/util/stream/Stream;",
                "java.util.stream.Stream",
                false,
            ),
            "java.util.stream.Stream",
            STREAM_ROWS(r#"[]"#),
        );
        let receipt = propose(&source, &file, call, &provider).expect("proved receipt");
        assert_eq!(receipt.callee.owner, "java.util.stream.Stream");
        assert_eq!(receipt.callee.name, "sorted");
        assert_eq!(receipt.callee.descriptor, "()Ljava/util/stream/Stream;");
        assert_eq!(receipt.dispatch, Dispatch::Instance);
        assert_eq!(receipt.arguments, ArgumentMapping::Exact(0));
        assert_eq!(receipt.effects, CheckedEffects::None);
        assert_eq!(receipt.behavior, Behavior::Exact);
        assert_eq!(receipt.evidence, ReceiptEvidence::PhysicalMember);
        match &receipt.outcome {
            JavaOutcome::Value(value) => {
                // The K2 source type is `Stream<String>`; the physical row is
                // an erased reference, and the payload is *not* claimed.
                assert_eq!(value.erased, "java.util.stream.Stream");
                assert!(matches!(value.kind, ReprKind::ErasedReference));
                assert_eq!(value.nullable, Nullability::Unknown);
                assert!(!value.payload_complete());
            }
            other => panic!("expected a value outcome, got {other:?}"),
        }
        // `values.sorted()`: the selector is `sorted`, the receiver `values`.
        assert_eq!(
            &source[receipt.selector.start_byte..receipt.selector.end_byte],
            "sorted"
        );
        let receiver = receipt.receiver.as_ref().expect("explicit receiver");
        assert_eq!(&source[receiver.start_byte..receiver.end_byte], "s");
        assert_eq!(
            receipt.location.snapshot_hash,
            crate::facts::classpath::source_hash(&source)
        );
    }

    /// A physically `void` member is `Void`, never a Kotlin `Unit` value.
    #[test]
    fn a_void_member_receipt_has_no_value_representation() {
        let source = "package fixture\n\nfun f(l: java.util.ArrayList<String>) = l.clear()\n";
        let tree = parse(source);
        let call = call_named(tree.root_node(), source, "clear");
        let scratch = Scratch::new();
        let file = scratch.0.join("Fixture.kt");
        fs::write(&file, source).unwrap();
        let offset = kotlin_call_site_offset(call).unwrap() as u64;
        let provider = provider_for(
            &file,
            source,
            site(offset, "java.util.ArrayList", "clear", "[]", "kotlin.Unit", false, "()V"),
            "java.util.ArrayList",
            r#"[{"name":"clear","static_member":false,"parameter_types":[],"result_type":"<void>","parameter_nullability":[],"result_nullability":"unknown","checked_exceptions":[]}]"#.to_string(),
        );
        let receipt = propose(source, &file, call, &provider).expect("proved receipt");
        assert_eq!(receipt.outcome, JavaOutcome::Void);
    }
}
