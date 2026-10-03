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

/// Pure decision for how a source file should be migrated.
#[derive(Debug, PartialEq, Eq)]
pub enum MigrationProposal {
    /// Nothing translated — leave the file untouched.
    Untouched,
    /// All useful source was translated — delete the file.
    Delete,
    /// Rewrite with the untranslated residue.
    Rewrite(String),
}

/// Decide the migration result without performing filesystem or logging effects.
pub fn propose_migration(source: &str, coverage: &FileCoverage) -> MigrationProposal {
    propose_migration_with_blockers(source, coverage, true)
}

/// Plan an in-memory speculative rewrite without diagnostic comments. Comments
/// are user-facing output, not Kotlin syntax: inserting them between modifiers
/// can change Tree-sitter's attachment of an annotation block on the next
/// speculative round and make an intrinsically retained declaration appear
/// translatable.
pub fn propose_speculative_migration(source: &str, coverage: &FileCoverage) -> MigrationProposal {
    propose_migration_with_blockers(source, coverage, false)
}

fn propose_migration_with_blockers(
    source: &str,
    coverage: &FileCoverage,
    include_blockers: bool,
) -> MigrationProposal {
    if coverage.translated_spans.is_empty() && (coverage.blockers.is_empty() || !include_blockers) {
        return MigrationProposal::Untouched;
    }
    if !coverage.translated_spans.is_empty() && coverage.is_fully_translated() {
        return MigrationProposal::Delete;
    }

    let stripped = tidy(&strip_translated_with_blockers(
        source,
        coverage,
        include_blockers,
    ));
    if coverage.translated_spans.is_empty() {
        return MigrationProposal::Rewrite(stripped);
    }
    if stripped.trim().is_empty()
        || stripped
            .lines()
            .all(|line| line.trim().is_empty() || line.trim_start().starts_with("//"))
    {
        MigrationProposal::Delete
    } else {
        MigrationProposal::Rewrite(stripped)
    }
}

pub fn apply_migration_proposal(
    path: &Path,
    proposal: &MigrationProposal,
) -> Result<MigrateOutcome, String> {
    match proposal {
        MigrationProposal::Untouched => Ok(MigrateOutcome::Untouched),
        MigrationProposal::Delete => {
            std::fs::remove_file(path)
                .map_err(|e| format!("{}: {e}", crate::paths::display(path)))?;
            Ok(MigrateOutcome::Deleted)
        }
        MigrationProposal::Rewrite(source) => {
            std::fs::write(path, source)
                .map_err(|e| format!("{}: {e}", crate::paths::display(path)))?;
            Ok(MigrateOutcome::Trimmed {
                remaining_bytes: source.len(),
            })
        }
    }
}

/// Strip translated spans from the source, insert `// NOTLIN: …` blocker
/// comments ahead of untranslated residue, return the new text.
pub fn strip_translated(source: &str, coverage: &FileCoverage) -> String {
    strip_translated_with_blockers(source, coverage, true)
}

fn strip_translated_with_blockers(
    source: &str,
    coverage: &FileCoverage,
    include_blockers: bool,
) -> String {
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
    // Blockers arrive in AST-traversal order, not byte-offset order;
    // copy_kept/flush_blockers both rely on ascending offsets.
    let mut blockers: Vec<(usize, String)> = if include_blockers {
        coverage.blockers.to_vec()
    } else {
        Vec::new()
    };
    blockers.sort_by_key(|(offset, _)| *offset);
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
        let indent = &source[line_start..indent_end];
        let existing = format!("{indent}{text}");
        let mut blocker_block_start = line_start;
        while blocker_block_start > from {
            let previous_end = blocker_block_start - 1;
            let previous_start = source[..previous_end]
                .rfind('\n')
                .map(|index| index + 1)
                .unwrap_or(from)
                .max(from);
            let previous_line = &source[previous_start..blocker_block_start];
            if previous_line
                .strip_prefix(indent)
                .is_some_and(|line| line.starts_with("// NOTLIN:"))
            {
                blocker_block_start = previous_start;
            } else {
                break;
            }
        }
        if source[blocker_block_start..line_start].contains(&existing) {
            continue;
        }
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
    match propose_migration(source, coverage) {
        MigrationProposal::Untouched => {
            log::info!(
                "{}: nothing translated; untouched",
                crate::paths::display(kt_path)
            );
            Ok(MigrateOutcome::Untouched)
        }
        MigrationProposal::Delete => {
            std::fs::remove_file(kt_path)
                .map_err(|e| format!("{}: {e}", crate::paths::display(kt_path)))?;
            let message = if coverage.is_fully_translated() {
                "fully translated; deleted"
            } else {
                "fully translated after strip; deleted"
            };
            log::info!("{}: {message}", crate::paths::display(kt_path));
            Ok(MigrateOutcome::Deleted)
        }
        MigrationProposal::Rewrite(stripped) => {
            std::fs::write(kt_path, &stripped)
                .map_err(|e| format!("{}: {e}", crate::paths::display(kt_path)))?;
            log::info!(
                "{}: trimmed to {} bytes (was {})",
                crate::paths::display(kt_path),
                stripped.len(),
                source.len()
            );
            Ok(MigrateOutcome::Trimmed {
                remaining_bytes: stripped.len(),
            })
        }
    }
}
