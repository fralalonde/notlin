//! In-place migration: after transpiling a .kt file, strip the declarations
//! that produced Java output from the .kt source. Fully-translated files are
//! deleted; partially-translated files are rewritten with the leftovers,
//! each blocking construct annotated with a `// NOTLIN: <code> <message>`
//! stub that mirrors the console diagnostic.
use crate::diagnostics::FileCoverage;
use std::path::Path;

/// Result of a migration step for one file.
#[derive(Debug)]
pub enum MigrateOutcome {
    /// Nothing translated — file untouched.
    Untouched,
    /// File rewritten with only untranslatable declarations remaining.
    Trimmed { remaining_bytes: usize },
    /// Everything translated — the .kt file was deleted.
    Deleted,
}

/// Strip translated spans from the source, insert `// NOTLIN: …` blocker
/// comments ahead of untranslated residue, return the new text.
pub fn strip_translated(source: &str, coverage: &FileCoverage) -> String {
    // Collect non-overlapping byte ranges to remove, sorted.
    let mut spans: Vec<(usize, usize)> = coverage.translated_spans.to_vec();
    spans.sort();
    spans.dedup();

    // Expand each span to swallow trailing whitespace/newline on its line.
    let bytes = source.as_bytes();
    let mut expanded: Vec<(usize, usize)> = Vec::new();
    for (start, end) in spans {
        let mut e = end;
        // swallow trailing spaces then one newline
        while e < bytes.len() && (bytes[e] == b' ' || bytes[e] == b'\t') {
            e += 1;
        }
        if e < bytes.len() && bytes[e] == b'\n' {
            e += 1;
        }
        expanded.push((start, e));
    }

    // Merge overlapping/adjacent spans.
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for s in expanded {
        match merged.last_mut() {
            Some(last) if s.0 <= last.1 => last.1 = last.1.max(s.1),
            _ => merged.push(s),
        }
    }

    // Build the result by skipping merged spans. Blockers are keyed by byte
    // offset in SOURCE coordinates. A blocker whose anchor falls inside a
    // stripped span lands right where that code used to start; a blocker
    // anchored inside kept (untranslated) residue is inserted at its exact
    // offset — immediately above the offending element — so residue comments
    // never drift to the head of the file or the top of a kept segment.
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    let mut blockers: Vec<(usize, String)> = coverage.blockers.to_vec();
    for (start, end) in merged {
        copy_kept(&mut blockers, source, cursor, start, &mut out);
        // Blockers anchored inside the stripped span: emit at its start,
        // where the removed code used to be.
        flush_blockers(&mut blockers, start, end, &mut out);
        cursor = end;
    }
    copy_kept(&mut blockers, source, cursor, source.len(), &mut out);
    out
}

/// Copy `source[from..to]` verbatim, inserting each blocker whose anchor
/// offset falls inside the kept region as a comment line directly above the
/// line that contains its element (exact line insertion, in offset order —
/// the element's own indentation is preserved because the full line is
/// re-copied after the comment).
fn copy_kept(
    blockers: &mut Vec<(usize, String)>,
    source: &str,
    from: usize,
    to: usize,
    out: &mut String,
) {
    let mut p = from;
    for (offset, text) in blockers.iter() {
        if *offset < from || *offset >= to {
            continue;
        }
        // Back up to the start of the line holding the element, then insert
        // the comment AFTER that line's leading whitespace so the comment
        // shares the element's indentation; re-copy the whitespace + content
        // after it (p rewinds to the line start).
        let line_start = source[..*offset]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
            .max(from);
        let indent_end = source[line_start..*offset]
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count()
            + line_start;
        out.push_str(&source[p..indent_end]);
        out.push_str(text);
        p = line_start;
    }
    out.push_str(&source[p..to]);
    blockers.retain(|(o, _)| !(*o >= from && *o < to));
}

/// Emit (and remove) blockers with `from <= offset < to`, in offset order.
fn flush_blockers(blockers: &mut Vec<(usize, String)>, from: usize, to: usize, out: &mut String) {
    blockers.retain(|(offset, text)| {
        if *offset >= from && *offset < to {
            out.push_str(text);
            false
        } else {
            true
        }
    });
}

/// Collapse more than one consecutive blank line into one and trim leading
/// and trailing blank lines. Keeps the trimmed .kt readable.
pub fn tidy(source: &str) -> String {
    let mut out = String::new();
    let mut blank_run = 0usize;
    for line in source.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    // Drop leading and trailing blank lines.
    let trimmed = out.trim_matches('\n');
    trimmed.to_string() + if trimmed.is_empty() { "" } else { "\n" }
}

/// Migrate one .kt file in place according to its coverage.
pub fn migrate(
    kt_path: &Path,
    source: &str,
    coverage: &FileCoverage,
) -> Result<MigrateOutcome, String> {
    if coverage.translated_spans.is_empty() {
        log::info!("{}: nothing translated; untouched", kt_path.display());
        return Ok(MigrateOutcome::Untouched);
    }

    if coverage.is_fully_translated() {
        std::fs::remove_file(kt_path).map_err(|e| format!("{}: {e}", kt_path.display()))?;
        log::info!("{}: fully translated; deleted", kt_path.display());
        return Ok(MigrateOutcome::Deleted);
    }

    // Partially translated: rewrite with only untranslated code (plus any
    // blocker comments explaining what could not translate).
    let stripped = tidy(&strip_translated(source, coverage));
    if stripped.trim().is_empty()
        || stripped
            .lines()
            .all(|l| l.trim().is_empty() || l.trim_start().starts_with("//"))
    {
        // Only comment stubs remained — treat as fully translated.
        std::fs::remove_file(kt_path).map_err(|e| format!("{}: {e}", kt_path.display()))?;
        log::info!(
            "{}: fully translated after strip; deleted",
            kt_path.display()
        );
        return Ok(MigrateOutcome::Deleted);
    }
    std::fs::write(kt_path, &stripped).map_err(|e| format!("{}: {e}", kt_path.display()))?;
    log::info!(
        "{}: trimmed to {} bytes (was {})",
        kt_path.display(),
        stripped.len(),
        source.len()
    );
    Ok(MigrateOutcome::Trimmed {
        remaining_bytes: stripped.len(),
    })
}
