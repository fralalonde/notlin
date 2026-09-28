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
//!   * receiver-binding for `is` smart-casts: `if (x.data is T)` reads the
//!     underlying Java property twice, and Kotlin's smart-cast does not
//!     flow across getter calls - a local `val` binding fixes it;
//!   * `copy(...)` calls on Kotlin `copy()` of a Java-translated data
//!     class: Java classes have no `copy()`, the fix is the Java copy/
//!     with-constructor form.

use crate::workspace::DeclarationKind;
use std::fs;
use std::path::Path;

const MARKER: &str = "NOTLIN-MANUAL:";

/// Scan the retained .kt text and insert `NOTLIN-MANUAL` comment lines where
/// the workspace shows the referenced declaration is Java-translated.
/// Returns the number of new markers inserted.
pub fn annotate_manual_spots(path: &Path, index: &crate::workspace::SourceIndex) -> usize {
    let Ok(source) = fs::read_to_string(path) else {
        return 0;
    };

    let mut insertions: Vec<(usize, String)> = Vec::new(); // (line_index_before_0, comment)
    let mut copy_rewrites: Vec<(usize, String)> = Vec::new();
    let mut rewrites: Vec<(usize, usize, String)> = Vec::new(); // (condition line, body end, local name)
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
            if let Some(recv) = recv {
                if let Some(ty) = binding.get(recv) {
                    if is_translated_java_data_class(index, ty) {
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
                }
                // Dataflow-free fallback: mark when ANY java data class member
                // property named `recv` exists? Too noisy - skip.
            }
        }

        // P1: `if (<chain> is Type)` where <chain>'s leaf property is a Java
        // getter-derived property of a translated class.
        let is_pos = match trimmed.find(" is ") {
            Some(p) => p,
            None => continue,
        };
        let before = &trimmed[..is_pos];
        let if_pos = match before.rfind("if") {
            Some(p) => p,
            None => continue,
        };
        let chain = before[if_pos + 2..].trim().trim_start_matches('(');
        let Some(dot) = chain.rfind('.') else {
            continue;
        };
        let (base, prop) = (chain[..dot].trim(), chain[dot + 1..].trim());
        if base.is_empty() || prop.is_empty() {
            continue;
        }
        // Binding a repeated property read is semantics-preserving and fixes
        // Kotlin smart-cast invalidation for Java getters. Do this generic
        // syntactic repair even when the lightweight workspace index cannot
        // recover the receiver's erased/generic type.
        // The body reads the same chain again - stale smart-cast territory.
        let chain_lit = chain.to_string();
        let body_repeats = lines
            .iter()
            .skip(i + 1)
            .take(30)
            .take_while(|l| !l.trim_start().starts_with('}'))
            .any(|l| l.contains(&chain_lit));
        if body_repeats {
            let local = prop.to_string();
            let end = i
                + 1
                + lines
                    .iter()
                    .skip(i + 1)
                    .take(30)
                    .take_while(|l| !l.trim_start().starts_with('}'))
                    .count();
            rewrites.push((i, end, local));
        }
    }

    if insertions.is_empty() && copy_rewrites.is_empty() && rewrites.is_empty() {
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
        if let Some((_, _, local)) = rewrites.iter().find(|(at, _, _)| *at == i) {
            let trimmed = line.trim_start();
            if let Some(is_pos) = trimmed.find(" is ") {
                let before = &trimmed[..is_pos];
                if let Some(if_pos) = before.rfind("if") {
                    let chain = before[if_pos + 2..]
                        .trim()
                        .trim_start_matches('(')
                        .trim_end();
                    let indent = &line[..line.len() - trimmed.len()];
                    out.push_str(indent);
                    out.push_str("val ");
                    out.push_str(local);
                    out.push_str(" = ");
                    out.push_str(chain);
                    out.push_str(line_ending);
                    let replacement = trimmed.replacen(chain, local, 1);
                    out.push_str(indent);
                    out.push_str(&replacement);
                    out.push_str(line_ending);
                    continue;
                }
            }
        }
        let line = if let Some((at, _end, local)) =
            rewrites.iter().find(|(at, end, _)| i > *at && i <= *end)
        {
            let condition = lines[*at].trim_start();
            if let Some(pos) = condition.find(" is ") {
                let b = &condition[..pos];
                if let Some(ip) = b.rfind("if") {
                    let chain = b[ip + 2..].trim().trim_start_matches('(').trim_end();
                    let replaced = line.replace(chain, local);
                    // body line rewritten in-place
                    out.push_str(&replaced);
                    out.push_str(line_ending);
                    continue;
                }
            }
            line
        } else {
            line
        };
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
    insertions.len() + copy_rewrites.len() + rewrites.len()
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

/// Does the workspace contain a JAVA declaration named `ty` (a translated
/// Kotlin data class) carrying a property member `prop`?
fn java_class_has_property(index: &crate::workspace::SourceIndex, class: &str, prop: &str) -> bool {
    // Java classes expose the property through bean accessors: `prop` matches
    // property members directly OR `get<T>`/`is<T>` methods (the shape the
    // workspace records for Java sources).
    let getter = format!("get{}", capitalize(prop));
    let isser = format!("is{}", capitalize(prop));
    index.declarations().any(|d| {
        d.name == class
            && d.language == crate::workspace::SourceLanguage::Java
            && d.kind == DeclarationKind::Class
            && d.members.iter().any(|m| {
                (m.kind == crate::workspace::MemberKind::Property && m.name == prop)
                    || (m.kind == crate::workspace::MemberKind::Method
                        && (m.name == getter || m.name == isser))
            })
    })
}

fn java_subtype_has_property(
    index: &crate::workspace::SourceIndex,
    supertype: &str,
    prop: &str,
) -> bool {
    let getter = format!("get{}", capitalize(prop));
    let isser = format!("is{}", capitalize(prop));
    index.declarations().any(|d| {
        d.language == crate::workspace::SourceLanguage::Java
            && d.kind == DeclarationKind::Class
            && d.supertypes
                .iter()
                .any(|ty| ty.rsplit('.').next() == Some(supertype))
            && d.members.iter().any(|m| {
                (m.kind == crate::workspace::MemberKind::Property && m.name == prop)
                    || (m.kind == crate::workspace::MemberKind::Method
                        && (m.name == getter || m.name == isser))
            })
    })
}

fn capitalize(input: &str) -> String {
    let mut chars = input.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
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

/// Any Java-translated class exposing `prop` through a bean getter.
fn any_java_class_has_property(index: &crate::workspace::SourceIndex, prop: &str) -> bool {
    index.declarations().any(|d| {
        d.language == crate::workspace::SourceLanguage::Java
            && d.kind == DeclarationKind::Class
            && d.members.iter().any(|m| {
                m.kind == crate::workspace::MemberKind::Method
                    && (m.name == format!("get{}", capitalize(prop))
                        || m.name == format!("is{}", capitalize(prop)))
            })
    })
}
