//! In-place migration: after transpiling a .kt file, strip the declarations
//! that produced Java output from the .kt source. Fully-translated files are
//! deleted; partially-translated files are rewritten with the leftovers.
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

/// Strip translated spans from the source, tidy whitespace, return the new text.
pub fn strip_translated(source: &str, coverage: &FileCoverage) -> String {
    // Collect non-overlapping byte ranges to remove, sorted.
    let mut spans: Vec<(usize, usize)> = coverage.translated_spans.iter().copied().collect();
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

    // Build the result by skipping merged spans.
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (start, end) in merged {
        out.push_str(&source[cursor..start]);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    out
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

    // Partially translated: rewrite with only untranslated code.
    let stripped = tidy(&strip_translated(source, coverage));
    if stripped.trim().is_empty() {
        // Nothing meaningful remained — treat as fully translated.
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
