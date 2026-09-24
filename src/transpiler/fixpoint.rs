//! Workspace-level retention fixpoint.
//!
//! Retention is order-dependent: a Kotlin subtype retained for an intrinsic
//! reason (declaration annotation, enum-`entries` ABI, KClass usage...)
//! cannot implement a translated-away supertype, so its supertypes must
//! retain too. A single pass over files in arbitrary order cannot know which
//! subtypes will remain Kotlin; probing greedily produced 5499 kotlinc
//! errors of exactly that shape.
//!
//! The conservative fix: compute the retained set by least-fixpoint
//! iteration BEFORE writing anything. Each pass probe-translates every
//! selected file in memory (no diagnostics printed, no files written) and
//! collects the declarations that stayed in Kotlin (`coverage.untranslated`
//! plus every declaration name the file index knows about that did NOT
//! translate). The subtype rule consumes that set through
//! `Unit::retained_hint`, so a hub interface only retains when one of its
//! Kotlin subtypes is itself retained. Seeds are monotone — an intrinsically
//! tainted declaration never un-taints — so the iteration reaches the least
//! fixpoint and terminates (bounded by the number of declarations).

use crate::cli::Cli;
use crate::transpiler::transpile_with_workspace_hint;
use crate::workspace::SourceIndex;
use std::collections::HashSet;
use std::path::PathBuf;

/// Per-file fixpoint result: exactly what `run()` needs to write once.
pub struct FilePlan {
    pub file: PathBuf,
    pub source: String,
    pub java_files: Vec<(String, String)>,
    pub errors: usize,
    pub warnings: usize,
    pub coverage: crate::diagnostics::FileCoverage,
}

/// Probe-translate every file and iterate the retained set to the least
/// fixpoint; return the final pass' plans (diagnostics NOT printed — the
/// caller prints/serializes its own summary).
pub fn plan_workspace(
    files: &[(PathBuf, String)],
    cli: &Cli,
    index: &SourceIndex,
    translation_roots: &[PathBuf],
    max_passes: usize,
) -> Vec<FilePlan> {
    let mut retained: HashSet<String> = HashSet::new();
    let mut plans = Vec::with_capacity(files.len());
    let mut stable_pass_seen = false;
    for pass in 0..max_passes {
        let mut grew = false;
        plans.clear();
        for (file, source) in files {
            let (java_files, errors, warnings, coverage) = transpile_with_workspace_hint(
                source,
                file,
                cli,
                Some(index),
                translation_roots,
                Some(retained.clone()),
                /*silent=*/ true,
            );
            // Declarations still in Kotlin after this pass: every declaration
            // the index knows for this file that did NOT appear in
            // `coverage.translated`. NOTE: index entries are workspace-wide;
            // filter to this file's entries via its declarations list.
            let file_decls = index
                .source_file(file)
                .map(|sf| {
                    sf.declarations
                        .iter()
                        .map(|d| d.name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for name in file_decls {
                if !coverage.translated.iter().any(|t| t == &name) && !retained.contains(&name) {
                    retained.insert(name);
                    grew = true;
                }
            }
            plans.push(FilePlan {
                file: file.clone(),
                source: source.clone(),
                java_files,
                errors,
                warnings,
                coverage,
            });
        }
        log::debug!(
            "fixpoint pass {}: retained set {} names{}",
            pass + 1,
            retained.len(),
            if grew { " (grew)" } else { " (stable)" }
        );
        if !grew {
            stable_pass_seen = true;
            break;
        }
    }
    // The stable pass ran silently (probe mode), so its diagnostics were
    // never printed. Re-run it once with printing enabled — the retained set
    // is unchanged, so the plans are identical; only the console output
    // differs. Single file: the final pass IS the first pass.
    if stable_pass_seen {
        plans.clear();
        for (file, source) in files {
            let (java_files, errors, warnings, coverage) = transpile_with_workspace_hint(
                source,
                file,
                cli,
                Some(index),
                translation_roots,
                Some(retained.clone()),
                /*silent=*/ false,
            );
            plans.push(FilePlan {
                file: file.clone(),
                source: source.clone(),
                java_files,
                errors,
                warnings,
                coverage,
            });
        }
    }
    plans
}

/// Convenience: count of java files in a plan list (for summary lines).
pub fn total_java_files(plans: &[FilePlan]) -> usize {
    plans.iter().map(|plan| plan.java_files.len()).sum()
}
