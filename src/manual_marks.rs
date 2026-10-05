//! Post-migration manual-intervention markers (--in-place).
//!
//! Some Kotin code cannot be auto-fixed by the transpiler but is
//! mechanically descriptible: the migration is blocked on a semantic the
//! transpiler could express as prose, and a small user edit unlocks more
//! translated elements on the NEXT run. Those spots get a grep-able
//!
//!   // NOTLIN-MANUAL: <explanation> - fix unlocks translation on the next run
//!
//! comment inserted into the retained .kt file. The pass is idempotent: a
//! spot already carrying the marker is never re-annotated.
//!
//! Two detections ship today:
//!   * receiver-binding for smart casts over a property whose OWNER this run
//!     translated to Java (`crate::smart_cast`): the property read is bound into
//!     a local, so Kotlin's flow analysis keeps the narrowing across the Java
//!     getter boundary. Applied silently — it is semantics-preserving, not a
//!     request for the user to edit anything;
//!   * `copy(...)` calls on Kotlin `copy()` of a Java-translated data
//!     class: Java classes have no `copy()`, the fix is the Java copy/
//!     with-constructor form. That one is prose, so it becomes a
//!     `NOTLIN-MANUAL` comment.

use crate::workspace::{DeclarationKind, MemberKind, SourceLanguage};
use std::fs;
use std::path::Path;

const MARKER: &str = "NOTLIN-MANUAL:";

/// Scan the retained .kt text and repair what the migration can repair by
/// itself (smart casts over translated property owners), then insert
/// `NOTLIN-MANUAL` comment lines where only a user edit unlocks more.
/// Returns the number of changes made.
pub fn annotate_manual_spots(path: &Path, index: &crate::workspace::SourceIndex) -> usize {
    let Ok(original) = fs::read_to_string(path) else {
        return 0;
    };

    // The smart-cast repair runs first: it inserts binding lines, and every
    // pass below must see the text it produced (its own idempotency included).
    let smart_cast_bindings = index.smart_cast_bindings_for_path(path);
    let (source, smart_cast_rewrites) = crate::smart_cast::rewrite_with_bindings(
        &original,
        &smart_cast_bindings,
        &|owner, property| translated_property_owner(index, owner, property),
    );

    // A named-argument constructor call cannot target a Java constructor at all,
    // and a parameter the caller omits is served by the delegating overload the
    // emitter wrote for that exact pattern (N87CB). Both are lowered to the
    // positional Java shape here — only for a declaration this run translated:
    // one that stayed Kotlin still has nameable parameters and real defaults.
    let (source, ctor_default_rewrites) = crate::ctor_defaults::rewrite(&source, &|callee| {
        translated_constructor_params(index, callee)
    });

    let mut insertions: Vec<(usize, String)> = Vec::new(); // (line_index_before_0, comment)
    let mut copy_rewrites: Vec<(usize, String)> = Vec::new();
    let lines: Vec<&str> = source.lines().collect();

    // In-file binding tables: `val item: OrderLineItem`, `item: OrderLineItem`
    // (ctor param), `val data: WorkflowPosition?` etc. give variable->type
    // reasoning for the receiver chains. Line-lightweight, no regex dep.
    // Chains like `position.data` resolve leaf-by-leaf on the NEXT iteration -
    // bindings only carry plain identifiers.
    let mut binding: std::collections::HashMap<&str, &str> = Default::default();
    for line in &lines {
        let l = line.trim();
        // `val x: T`, `var x: T`, `data x: T?` - and parameter/list shapes
        // like `node: Node` anywhere in the line (ctor params, fun params).
        let name_ty: Vec<(&str, &str)> = decl_pairs(l);
        for (name, ty) in name_ty {
            binding_insert(&mut binding, name, ty);
        }
    }

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        // Earlier migration diagnostics and manual markers can coexist in one
        // retained file. Only suppress a spot that is already immediately
        // annotated; a file-level marker must not hide later independent
        // Java/Kotlin boundary repairs.
        let already_marked = trimmed.contains(MARKER) || (i > 0 && lines[i - 1].contains(MARKER));
        if already_marked {
            continue;
        }

        // P2: `<recv>.copy(` where recv's type is a translated Java class.
        if let Some(copy_at) = trimmed.find(".copy(") {
            let recv = trailing_ident(trimmed, copy_at);
            if let Some(recv) = recv
                && let Some(ty) = binding.get(recv)
                && is_translated_java_data_class(index, ty)
            {
                if let Some(rewritten) = rewrite_copy_call(trimmed, recv, ty, index) {
                    copy_rewrites.push((i, rewritten));
                } else {
                    insertions.push((
                                i,
                                format!(
                                    "{MARKER} `{recv}.copy(...)` calls Kotlin copy() on Java class \
                                     `{ty}` - replace with the Java copy/with constructor; fix unlocks \
                                     translation on the next run\n"
                                ),
                            ));
                }
                continue;
            }
            // Dataflow-free fallback: mark when ANY java data class member
            // property named `recv` exists? Too noisy - skip.
        }
    }

    if insertions.is_empty()
        && copy_rewrites.is_empty()
        && smart_cast_rewrites == 0
        && ctor_default_rewrites == 0
    {
        return 0;
    }

    // Build output: preserve the source's newline convention and terminal newline.
    let mut out = String::with_capacity(source.len() + insertions.len() * 120);
    let line_ending = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let has_terminal_newline = source.ends_with('\n') || source.ends_with('\r');
    for (i, line) in lines.iter().enumerate() {
        if let Some((_, replacement)) = copy_rewrites.iter().find(|(at, _)| *at == i) {
            let indent = &line[..line.len() - line.trim_start().len()];
            out.push_str(indent);
            out.push_str(replacement);
            out.push_str(line_ending);
            continue;
        }
        let line = *line;
        if let Some((_, comment)) = insertions.iter().find(|(line, _)| *line == i) {
            let indent_len = line.len() - line.trim_start().len();
            // Byte-boundary safe: leading whitespace only, but guard anyway.
            let indent = line.get(..indent_len).unwrap_or("                ");
            for comment_line in comment.trim_end().lines() {
                out.push_str(indent);
                out.push_str("// ");
                out.push_str(comment_line);
                out.push_str(line_ending);
            }
        }
        out.push_str(line);
        if i + 1 < lines.len() || has_terminal_newline {
            out.push_str(line_ending);
        }
    }
    fs::write(path, out).ok();
    insertions.len() + copy_rewrites.len() + smart_cast_rewrites + ctor_default_rewrites
}

/// The emitted Java constructor's parameter names for `callee`, when this run
/// translated the declaration. `None` leaves a call site alone, which is what a
/// declaration that stayed Kotlin (or that this pass cannot see) needs.
fn translated_constructor_params(
    index: &crate::workspace::SourceIndex,
    callee: &str,
) -> Option<Vec<String>> {
    index
        .declarations()
        .find(|declaration| {
            declaration.name == callee && declaration.language == SourceLanguage::Java
        })
        .map(|declaration| declaration.ctor_param_names_or_state())
        .filter(|params| !params.is_empty())
}

/// True when `owner` names a declaration this run translated to Java and carries
/// `property` — as a field, or through the getter Kotlin will now resolve the
/// property read to. An owner still in Kotlin needs no repair: its property is
/// still a Kotlin property, which smart-casts as before.
fn translated_property_owner(
    index: &crate::workspace::SourceIndex,
    owner: &str,
    property: &str,
) -> bool {
    let accessor = crate::workspace::property_accessor_name(property);
    let boolean_accessor = format!(
        "is{}{}",
        property.chars().next().unwrap_or('_').to_uppercase(),
        property.chars().skip(1).collect::<String>()
    );
    index.declarations().any(|declaration| {
        declaration.name == owner
            && declaration.language == SourceLanguage::Java
            && declaration.members.iter().any(|member| {
                (matches!(member.kind, MemberKind::Property | MemberKind::Field)
                    && member.name == property)
                    || member.name == accessor
                    || member.name == boolean_accessor
            })
    })
}

/// Rewrite a named single-field copy call only when the generated Java class
/// has an explicit `(Self, changedValue)` constructor. This keeps ordinary
/// Java classes and unsupported copy shapes on the existing manual path.
fn rewrite_copy_call(
    line: &str,
    recv: &str,
    ty: &str,
    index: &crate::workspace::SourceIndex,
) -> Option<String> {
    let start = line.find(".copy(")?;
    let close = line[start..].find(')')? + start;
    let args = line[start + 6..close].trim();
    let (field, expr) = args.split_once('=')?;
    let field = field.trim();
    let expr = expr.trim();
    if !is_ident(field) || expr.is_empty() || expr.contains(',') {
        return None;
    }
    let declaration = index
        .declarations()
        .find(|d| d.name == ty && d.language == crate::workspace::SourceLanguage::Java)?;
    let ctor = declaration
        .members
        .iter()
        .any(|m| m.kind == crate::workspace::MemberKind::Constructor && m.name == ty);
    if !ctor {
        return None;
    }
    let receiver_start = start.checked_sub(recv.len())?;
    let before = &line[..receiver_start];
    let after = &line[close + 1..];
    Some(format!("{before}{ty}({recv}, {expr}){after}"))
}

/// Trailing plain identifier before an offset in a line (for receiver chains).
fn trailing_ident(line: &str, offset: usize) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut start = offset;
    while start > 0 {
        let c = bytes[start - 1] as char;
        if c.is_alphanumeric() || c == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    let ident = &line[start..offset];
    if ident.is_empty()
        || ident.chars().next().unwrap().is_ascii_digit()
        || matches!(
            ident,
            "if" | "while" | "for" | "return" | "null" | "true" | "false"
        )
    {
        return None;
    }
    Some(ident)
}

/// Whitelisted receiver types only - data-class copy ctor existence is not
/// modelled yet, so P2 marks anything whose TYPE is a JAVA class that the
/// workspace knows were formerly Kotlin data classes (kind Class, Java, and
/// members include at least one property).
fn is_translated_java_data_class(index: &crate::workspace::SourceIndex, class: &str) -> bool {
    // Java sources carry fields (Field members); Any field/method evidence
    // identifies a translated Kotlin data class for marking purposes.
    index.declarations().any(|d| {
        d.name == class
            && d.language == crate::workspace::SourceLanguage::Java
            && d.kind == DeclarationKind::Class
            && d.members.iter().any(|m| {
                matches!(
                    m.kind,
                    crate::workspace::MemberKind::Property
                        | crate::workspace::MemberKind::Field
                        | crate::workspace::MemberKind::Method
                        | crate::workspace::MemberKind::Constructor
                )
            })
    })
}

/// Binding-table helper: is this token a plain identifier?
fn is_ident(token: &str) -> bool {
    !token.is_empty()
        && token
            .chars()
            .next()
            .map(|c| c.is_alphabetic() || c == '_')
            .unwrap_or(false)
        && token.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !matches!(token, "if" | "when" | "return" | "var" | "val" | "else")
}

/// Insert `name -> type` into the table; a `T?` type is trimmed to `T`, and
/// generic types (`List<T>`) are unusable for this scan - dropped.
fn binding_insert<'a>(
    binding: &mut std::collections::HashMap<&'a str, &'a str>,
    name: &'a str,
    ty: &'a str,
) -> bool {
    let base = ty.trim().trim_end_matches('?').trim();
    if base.contains('<') || base.contains('(') || base.contains(' ') || base.is_empty() {
        return false;
    }
    if is_ident(base) {
        binding.insert(name, base);
        return true;
    }
    false
}

/// Extract (name, type) pairs for Kotlin bindings in one line of text:
/// matches `val x: T`, alone or in comma lists (`a: A, b: B`), and
/// constructor/parameter decls `x: T`. Returns only plain-ident shapes.
fn decl_pairs(line: &str) -> Vec<(&str, &str)> {
    let mut pairs = Vec::new();
    // Char-safe scan: `line.as_bytes()[i] as char` panics when slicing splits
    // a multi-byte UTF-8 char (em dashes ride inside these source lines).
    let bytes = line.as_bytes();
    let ascii_ident = |b: u8| (b as char).is_alphanumeric() || b == b'_';
    let mut i = 0usize;
    while i < bytes.len() {
        // Only ASCII can start an identifier — never cuts a UTF-8 char.
        if !(bytes[i] < 128 && (bytes[i] as char).is_alphabetic() || bytes[i] == b'_') {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] < 128 && ascii_ident(bytes[i]) {
            i += 1;
        }
        // `i` is now an ASCII/UTF-8 boundary (we only advanced on bytes < 128).
        let token = &line[start..i];
        let mut j = i;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j < bytes.len() && bytes[j] == b':' {
            let mut k = j + 1;
            while k < bytes.len() && (bytes[k] == b' ' || bytes[k] == b'\t') {
                k += 1;
            }
            let tstart = k;
            while k < bytes.len() && bytes[k] < 128 && ascii_ident(bytes[k]) {
                k += 1;
            }
            // k is a boundary — advance through any whitespace after ident.
            let ty = &line[tstart..k];
            if is_ident(token)
                && !matches!(
                    token,
                    "if" | "when"
                        | "return"
                        | "else"
                        | "val"
                        | "var"
                        | "data"
                        | "lateinit"
                        | "override"
                        | "class"
                        | "fun"
                        | "in"
                )
            {
                pairs.push((token, ty));
            }
        }
    }
    pairs
}
