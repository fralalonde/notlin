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
use crate::transpiler::{parse_tree, transpile_with_tree_hint_selection};
use crate::workspace::SourceIndex;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Debug hook: `NOTLIN_TRACE_RETENTION=<substr>[,<substr>...]` prints one line
/// per probe file per pass, listing the declarations that stayed Kotlin.
/// An empty value traces every file. Returning `None` disables the trace.
fn retention_trace_filter() -> Option<Vec<String>> {
    let raw = std::env::var("NOTLIN_TRACE_RETENTION").ok()?;
    Some(
        raw.split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn retention_traced(filter: &Option<Vec<String>>, file: &Path) -> bool {
    match filter {
        None => false,
        Some(parts) if parts.is_empty() => true,
        Some(parts) => {
            let text = file.to_string_lossy().replace('\\', "/");
            parts.iter().any(|part| text.contains(part.as_str()))
        }
    }
}

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
    let profile = std::env::var_os("NOTLIN_PROFILE").is_some();
    let trace = retention_trace_filter();
    let profile_start = Instant::now();
    let mut probe_transpile = Duration::ZERO;
    let mut probe_retention = Duration::ZERO;
    let mut final_transpile = Duration::ZERO;
    let mut probe_passes = 0usize;
    let mut probe_files_per_pass = Vec::new();
    let mut retained: HashSet<String> = HashSet::new();
    let mut retained_delta: HashSet<String> = HashSet::new();
    let all_kotlin_selected = index.all_kotlin_selected(translation_roots);
    // Trees are immutable. Parsing each source once avoids re-parsing it for
    // every retained-set probe and the final emitting translation.
    let trees: Vec<_> = files.iter().map(|(_, source)| parse_tree(source)).collect();
    let workspace_files: Vec<_> = files
        .iter()
        .map(|(file, _)| std::fs::canonicalize(file).ok())
        .collect();
    let mut plans = Vec::with_capacity(files.len());
    let mut stable_pass_seen = false;
    for pass in 0..max_passes {
        probe_passes += 1;
        // The first pass discovers intrinsic blockers exhaustively. Later
        // passes only need declarations linked to names newly retained by the
        // preceding pass; every other coverage result is retention-stable.
        let probe_all = pass == 0 || pass + 1 == max_passes;
        let preserve_probe_plans = pass + 1 == max_passes;
        let mut next_retained_delta = HashSet::new();
        let mut probed_files = 0usize;
        let mut grew = false;
        plans.clear();
        for (((file, source), tree), workspace_file) in
            files.iter().zip(&trees).zip(&workspace_files)
        {
            if !probe_all && !index.retained_delta_can_affect(file, &retained_delta) {
                continue;
            }
            probed_files += 1;
            let started = Instant::now();
            let (java_files, errors, warnings, coverage) = transpile_with_tree_hint_selection(
                source,
                tree,
                file,
                cli,
                Some(index),
                translation_roots,
                Some(&retained),
                Some(all_kotlin_selected),
                workspace_file.as_deref(),
                /*silent=*/ true,
            );
            if retention_traced(&trace, file) {
                eprintln!(
                    "notlin retention probe pass={} file={} translated={:?} untranslated={:?}",
                    pass + 1,
                    crate::paths::display(file),
                    coverage.translated,
                    coverage.untranslated,
                );
            }
            probe_transpile += started.elapsed();
            // Declarations still in Kotlin after this pass: every declaration
            // the index knows for this file that did NOT appear in
            // `coverage.translated`. NOTE: index entries are workspace-wide;
            // filter to this file's entries via its declarations list.
            let started = Instant::now();
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
                if !coverage.translated.iter().any(|t| t == &name) && retained.insert(name.clone())
                {
                    next_retained_delta.insert(name);
                    grew = true;
                }
            }
            probe_retention += started.elapsed();
            if preserve_probe_plans {
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
        retained_delta = next_retained_delta;
        if profile {
            probe_files_per_pass.push(probed_files);
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
    // Probe plans are always speculative: while a pass is walking files, an
    // earlier file can be translated before a later file grows `retained`.
    // Re-run all files from the complete retained set before writing, whether
    // the fixpoint stabilized or the pass bound was reached. This removes
    // file-order dependent output and prevents the final saved probe plans
    // from being emitted with an incomplete retained hint.
    //
    // The probe passes prune files that the current delta cannot affect, so a
    // retention rule that consults the retained set can keep a declaration in
    // Kotlin on a pass that did not re-probe every file. The emitting pass
    // therefore feeds its own findings back and repeats until the retained set
    // stops growing: every unit's output then comes from the same complete
    // hint. (Seeds are monotone, so this terminates; `FINAL_PASS_LIMIT` is the
    // safety bound, and hitting it is reported.)
    const FINAL_PASS_LIMIT: usize = 4;
    if stable_pass_seen || !plans.is_empty() {
        let mut converged = false;
        for _ in 0..FINAL_PASS_LIMIT {
            plans.clear();
            let mut grew = false;
            for (((file, source), tree), workspace_file) in
                files.iter().zip(&trees).zip(&workspace_files)
            {
                let started = Instant::now();
                let (java_files, errors, warnings, coverage) = transpile_with_tree_hint_selection(
                    source,
                    tree,
                    file,
                    cli,
                    Some(index),
                    translation_roots,
                    Some(&retained),
                    Some(all_kotlin_selected),
                    workspace_file.as_deref(),
                    /*silent=*/ false,
                );
                if retention_traced(&trace, file) {
                    eprintln!(
                        "notlin retention final file={} translated={:?} untranslated={:?}",
                        crate::paths::display(file),
                        coverage.translated,
                        coverage.untranslated,
                    );
                }
                for name in index
                    .source_file(file)
                    .map(|sf| {
                        sf.declarations
                            .iter()
                            .map(|declaration| declaration.name.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
                {
                    if !coverage.translated.contains(&name) && retained.insert(name) {
                        grew = true;
                    }
                }
                final_transpile += started.elapsed();
                plans.push(FilePlan {
                    file: file.clone(),
                    source: source.clone(),
                    java_files,
                    errors,
                    warnings,
                    coverage,
                });
            }
            if !grew {
                converged = true;
                break;
            }
        }
        if !converged {
            eprintln!(
                "warning: retention did not converge within {FINAL_PASS_LIMIT} emitting passes; \
                 output may retain less than the final pass decided"
            );
        }
    }
    if profile {
        let translation = crate::transpiler::translation_profile();
        eprintln!(
            "notlin profile: workspace fixpoint total={:?}; probe passes={probe_passes}; probe files/pass={probe_files_per_pass:?}; \
             probe transpile={:?}; retention bookkeeping={:?}; final transpile={:?}; files={}; retained={}",
            profile_start.elapsed(),
            probe_transpile,
            probe_retention,
            final_transpile,
            files.len(),
            retained.len(),
        );
        eprintln!(
            "notlin profile: translation internals translations={}; parse={:?}; unit={:?}; diagnostics={:?}; java-output={:?}",
            translation.translations,
            translation.parses,
            translation.units,
            translation.diagnostics,
            translation.java_output,
        );
    }
    plans
}

/// Convenience: count of java files in a plan list (for summary lines).
pub fn total_java_files(plans: &[FilePlan]) -> usize {
    plans.iter().map(|plan| plan.java_files.len()).sum()
}
