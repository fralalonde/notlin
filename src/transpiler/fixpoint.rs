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
use crate::transpiler::{WorkspaceScope, parse_tree, transpile_with_tree_hint_selection};
use crate::workspace::SourceIndex;
use std::collections::{BTreeMap, HashSet};
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

/// Workspace plans together with the stable retained declaration names.
pub struct WorkspacePlan {
    pub plans: Vec<FilePlan>,
    pub retained: HashSet<String>,
    /// Intrinsic retained declarations. Unlike the full retained closure, these
    /// cannot form self-supporting cascade cycles when reused after overlays.
    pub roots: HashSet<String>,
}

fn silent_jobs() -> usize {
    std::env::var("NOTLIN_JOBS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|jobs| *jobs > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
        })
        .min(8)
}

fn worker_stack_bytes() -> usize {
    std::env::var("NOTLIN_STACK_MB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|size| *size > 0)
        .unwrap_or(512)
        .saturating_mul(1024 * 1024)
}

struct SilentPlanner<'a> {
    files: &'a [(PathBuf, String)],
    trees: &'a [tree_sitter::Tree],
    workspace_files: &'a [Option<PathBuf>],
    cli: &'a Cli,
    index: &'a SourceIndex,
    translation_roots: &'a [PathBuf],
}

impl SilentPlanner<'_> {
    fn translate_one(&self, index: usize, retained: &HashSet<String>) -> FilePlan {
        let (file, source) = &self.files[index];
        let (java_files, errors, warnings, coverage) = transpile_with_tree_hint_selection(
            source,
            &self.trees[index],
            file,
            self.cli,
            WorkspaceScope {
                index: Some(self.index),
                roots: self.translation_roots,
                retained_hint: Some(retained),
                indexed_path: self.workspace_files[index].as_deref(),
            },
            true,
        );
        FilePlan {
            file: file.clone(),
            source: source.clone(),
            java_files,
            errors,
            warnings,
            coverage,
        }
    }

    fn translate_indices(
        &self,
        indices: &[usize],
        retained: &HashSet<String>,
    ) -> Result<Vec<(usize, FilePlan)>, String> {
        let configured_jobs = silent_jobs();
        let jobs = configured_jobs.min(indices.len().max(1));
        if jobs <= 1 || indices.len() < 2 {
            return Ok(indices
                .iter()
                .map(|index| (*index, self.translate_one(*index, retained)))
                .collect());
        }
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(jobs);
            for worker in 0..jobs {
                let assigned = indices
                    .iter()
                    .copied()
                    .skip(worker)
                    .step_by(jobs)
                    .collect::<Vec<_>>();
                handles.push(
                    std::thread::Builder::new()
                        .name(format!("notlin-{worker}"))
                        .stack_size(worker_stack_bytes())
                        .spawn_scoped(scope, move || {
                            assigned
                                .into_iter()
                                .map(|index| (index, self.translate_one(index, retained)))
                                .collect::<Vec<_>>()
                        })
                        .map_err(|error| format!("spawn translation worker: {error}"))?,
                );
            }
            let mut translated = Vec::with_capacity(indices.len());
            for handle in handles {
                translated.extend(
                    handle
                        .join()
                        .map_err(|_| "translation worker panicked".to_string())?,
                );
            }
            translated.sort_by_key(|(index, _)| *index);
            Ok(translated)
        })
    }
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
    emit_diagnostics: bool,
) -> Result<Vec<FilePlan>, String> {
    Ok(plan_workspace_state(
        files,
        cli,
        index,
        translation_roots,
        max_passes,
        emit_diagnostics,
    )?
    .plans)
}

pub fn plan_workspace_state(
    files: &[(PathBuf, String)],
    cli: &Cli,
    index: &SourceIndex,
    translation_roots: &[PathBuf],
    max_passes: usize,
    emit_diagnostics: bool,
) -> Result<WorkspacePlan, String> {
    plan_workspace_cold(
        files,
        cli,
        index,
        translation_roots,
        max_passes,
        emit_diagnostics,
        &HashSet::new(),
    )
}

pub fn plan_workspace_warm(
    files: &[(PathBuf, String)],
    cli: &Cli,
    index: &SourceIndex,
    translation_roots: &[PathBuf],
    max_passes: usize,
    emit_diagnostics: bool,
    seed: &HashSet<String>,
) -> Result<WorkspacePlan, String> {
    plan_workspace_cold(
        files,
        cli,
        index,
        translation_roots,
        max_passes,
        emit_diagnostics,
        seed,
    )
}

fn plan_workspace_cold(
    files: &[(PathBuf, String)],
    cli: &Cli,
    index: &SourceIndex,
    translation_roots: &[PathBuf],
    max_passes: usize,
    emit_diagnostics: bool,
    seed: &HashSet<String>,
) -> Result<WorkspacePlan, String> {
    let profile = std::env::var_os("NOTLIN_PROFILE").is_some();
    let trace = retention_trace_filter();
    let profile_start = Instant::now();
    let trees: Vec<_> = files.iter().map(|(_, source)| parse_tree(source)).collect();
    let workspace_files: Vec<_> = files
        .iter()
        .map(|(file, _)| std::fs::canonicalize(file).ok())
        .collect();
    let planner = SilentPlanner {
        files,
        trees: &trees,
        workspace_files: &workspace_files,
        cli,
        index,
        translation_roots,
    };
    if profile {
        eprintln!("notlin profile: fixpoint jobs={}", silent_jobs());
    }
    let mut retained = seed.clone();
    let mut retained_delta = retained.clone();
    let mut probe_transpile = Duration::ZERO;
    let mut probe_retention = Duration::ZERO;
    let mut probe_files_per_pass = Vec::new();
    let mut plans = Vec::with_capacity(files.len());
    let mut stable = false;
    let mut force_full_probe = false;

    for pass in 0..max_passes {
        let pass_started = Instant::now();
        let probe_all = pass == 0 || force_full_probe || pass + 1 == max_passes;
        force_full_probe = false;
        let mut next_delta = HashSet::new();
        let mut grew = false;
        let indices = files
            .iter()
            .enumerate()
            .filter_map(|(position, (file, _))| {
                (probe_all || index.retained_delta_can_affect(file, &retained_delta))
                    .then_some(position)
            })
            .collect::<Vec<_>>();
        let probed_files = indices.len();
        let started = Instant::now();
        let translated = planner.translate_indices(&indices, &retained)?;
        probe_transpile += started.elapsed();
        let mut pass_plans = Vec::with_capacity(translated.len());
        for (position, plan) in translated {
            let file = &files[position].0;
            if retention_traced(&trace, file) {
                eprintln!(
                    "notlin retention probe pass={} file={} translated={:?} untranslated={:?} blockers={:?}",
                    pass + 1,
                    crate::paths::display(file),
                    plan.coverage.translated,
                    plan.coverage.untranslated,
                    plan.coverage.blockers
                );
            }
            let started = Instant::now();
            for name in index
                .source_file(file)
                .map(|source_file| {
                    source_file
                        .declarations
                        .iter()
                        .map(|declaration| declaration.name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
            {
                if !plan.coverage.translated.contains(&name) && retained.insert(name.clone()) {
                    next_delta.insert(name);
                    grew = true;
                }
            }
            probe_retention += started.elapsed();
            pass_plans.push(plan);
        }
        retained_delta = next_delta;
        if profile {
            probe_files_per_pass.push(probed_files);
            eprintln!(
                "notlin profile: fixpoint probe pass={} full={} probed_files={} retained_grew={} retained_total={} translation={:?} bookkeeping={:?} total={:?}",
                pass + 1,
                probe_all,
                probed_files,
                grew,
                retained.len(),
                probe_transpile,
                probe_retention,
                pass_started.elapsed()
            );
        }
        if !grew {
            if probe_all {
                plans = pass_plans;
                stable = true;
                break;
            }
            // The reverse-dependency index is an optimization, not a proof of
            // global stability. Verify every selective no-growth result with a
            // complete workspace pass before declaring convergence.
            force_full_probe = true;
        }
    }
    if !stable {
        return Err(format!(
            "workspace retention did not converge within {max_passes} probe passes; increase --max-retention-passes above {max_passes} if this workspace has a deeper valid dependency chain"
        ));
    }

    // The monotone probe above deliberately grows an upper bound, but a
    // provisional retained name can disappear from the final plans (for
    // example, an enum that can safely implement a retained Kotlin interface).
    // Re-plan from the exact final coverage until the hint and the plans agree;
    // otherwise released names leak into cascade provenance and make the next
    // physical migration rewrite blocker comments.
    let all_indices = (0..files.len()).collect::<Vec<_>>();
    let mut refinement_seen = HashSet::new();
    refinement_seen.insert(sorted_key(&retained));
    let mut exact_stable = false;
    for _ in 0..max_passes {
        let exact = retained_from_plans(index, &plans);
        if exact == retained {
            exact_stable = true;
            break;
        }
        let key = sorted_key(&exact);
        if !refinement_seen.insert(key) {
            return Err("workspace retention refinement entered a repeated retained set".into());
        }
        retained = exact;
        plans = planner
            .translate_indices(&all_indices, &retained)?
            .into_iter()
            .map(|(_, plan)| plan)
            .collect();
    }
    if !exact_stable {
        return Err(format!(
            "workspace retention refinement did not converge within {max_passes} passes; increase --max-retention-passes above {max_passes}"
        ));
    }

    if emit_diagnostics {
        crate::diagnostics::clear_retention();
        plans.clear();
        for (((file, source), tree), workspace_file) in
            files.iter().zip(&trees).zip(&workspace_files)
        {
            let (java_files, errors, warnings, coverage) = transpile_with_tree_hint_selection(
                source,
                tree,
                file,
                cli,
                WorkspaceScope {
                    index: Some(index),
                    roots: translation_roots,
                    retained_hint: Some(&retained),
                    indexed_path: workspace_file.as_deref(),
                },
                false,
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
        qualify_retention_markers(&mut plans);
    }

    let exact_retained = retained_from_plans(index, &plans);
    if profile {
        let translation = crate::transpiler::translation_profile();
        eprintln!(
            "notlin profile: workspace fixpoint total={:?}; probe files/pass={probe_files_per_pass:?}; probe transpile={probe_transpile:?}; retention bookkeeping={probe_retention:?}; files={}; retained={}",
            profile_start.elapsed(),
            files.len(),
            exact_retained.len()
        );
        eprintln!(
            "notlin profile: translation internals translations={}; parse={:?}; unit={:?}; diagnostics={:?}; java-output={:?}",
            translation.translations,
            translation.parses,
            translation.units,
            translation.diagnostics,
            translation.java_output
        );
    }
    let roots = intrinsic_roots(&exact_retained);
    Ok(WorkspacePlan {
        plans,
        retained: exact_retained,
        roots,
    })
}

pub fn qualify_retention_markers(plans: &mut [FilePlan]) {
    let display_root = common_plan_root(plans)
        .map(|root| format!("{}/", crate::paths::display(&root).trim_end_matches('/')));
    let mut intrinsic: crate::diagnostics::IntrinsicRetentionMarkers = BTreeMap::new();
    for plan in plans.iter() {
        let file = crate::paths::display(&plan.file);
        let mut retention_anchors = plan
            .coverage
            .blockers
            .iter()
            .filter_map(|(offset, marker)| {
                marker
                    .trim_start()
                    .strip_prefix("// NOTLIN: ")
                    .and_then(|rest| rest.split_once(' '))
                    .filter(|(_, message)| message.starts_with("retained "))
                    .map(|_| (*offset, source_line(&plan.source, *offset)))
            })
            .collect::<Vec<_>>();
        retention_anchors.sort_unstable();
        for (offset, marker) in &plan.coverage.blockers {
            let Some(rest) = marker.trim_end().strip_prefix("// NOTLIN: ") else {
                continue;
            };
            let Some((code, message)) = rest.split_once(' ') else {
                continue;
            };
            if message.starts_with("retained ") {
                continue;
            }
            let owner_line = retention_anchors
                .iter()
                .rev()
                .find(|(anchor, _)| anchor <= offset)
                .or_else(|| retention_anchors.first())
                .map(|(_, line)| *line)
                .unwrap_or_else(|| source_line(&plan.source, *offset));
            intrinsic
                .entry((file.clone(), owner_line))
                .or_default()
                .push((code.to_string(), message.to_string()));
        }
    }
    for plan in plans {
        for (offset, marker) in &mut plan.coverage.blockers {
            let line = source_line(&plan.source, *offset);
            let Some((code, message)) =
                crate::diagnostics::retention_source_message(&plan.file, line, &intrinsic)
            else {
                continue;
            };
            let message = display_root
                .as_deref()
                .map(|prefix| message.replace(prefix, ""))
                .unwrap_or(message);
            let prefix = format!("// NOTLIN: {code} retained");
            if marker.starts_with(&prefix) {
                *marker = format!("// NOTLIN: {code} {message}\n");
            }
        }
    }
}

fn common_plan_root(plans: &[FilePlan]) -> Option<PathBuf> {
    let mut root = plans.first()?.file.parent()?.to_path_buf();
    while !plans.iter().all(|plan| plan.file.starts_with(&root)) {
        if !root.pop() {
            return None;
        }
    }
    Some(root)
}

fn source_line(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn intrinsic_roots(retained: &HashSet<String>) -> HashSet<String> {
    crate::diagnostics::retention_sites()
        .into_iter()
        .filter(|site| site.blockers.is_empty() && retained.contains(&site.name))
        .map(|site| site.name)
        .collect()
}

fn retained_from_plans(index: &SourceIndex, plans: &[FilePlan]) -> HashSet<String> {
    let mut retained = HashSet::new();
    for plan in plans {
        for name in index
            .source_file(&plan.file)
            .map(|source_file| {
                source_file
                    .declarations
                    .iter()
                    .map(|declaration| declaration.name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
        {
            if !plan.coverage.translated.contains(&name) {
                retained.insert(name);
            }
        }
    }
    retained
}

#[allow(dead_code)]
fn plan_workspace_warm_impl(
    files: &[(PathBuf, String)],
    cli: &Cli,
    index: &SourceIndex,
    translation_roots: &[PathBuf],
    max_passes: usize,
    emit_diagnostics: bool,
    mut retained: HashSet<String>,
) -> Result<WorkspacePlan, String> {
    let profile = std::env::var_os("NOTLIN_PROFILE").is_some();
    let trace = retention_trace_filter();
    let profile_start = Instant::now();
    let probe_transpile = Duration::ZERO;
    let probe_retention = Duration::ZERO;
    let final_transpile = Duration::ZERO;
    let mut probe_passes = 0usize;
    let mut probe_files_per_pass = Vec::new();
    let mut seen = HashSet::new();
    seen.insert(sorted_key(&retained));
    // Trees are immutable. Parsing each source once avoids re-parsing it for
    // every retained-set probe and the final emitting translation.
    let trees: Vec<_> = files.iter().map(|(_, source)| parse_tree(source)).collect();
    let workspace_files: Vec<_> = files
        .iter()
        .map(|(file, _)| std::fs::canonicalize(file).ok())
        .collect();
    let planner = SilentPlanner {
        files,
        trees: &trees,
        workspace_files: &workspace_files,
        cli,
        index,
        translation_roots,
    };
    if profile {
        eprintln!("notlin profile: fixpoint jobs={}", silent_jobs());
    }
    let all_indices = (0..files.len()).collect::<Vec<_>>();
    let mut plans = Vec::with_capacity(files.len());
    let mut stable_pass_seen = false;
    for pass in 0..max_passes {
        probe_passes += 1;
        let pass_started = Instant::now();
        let mut next_retained = HashSet::new();
        let translated = planner.translate_indices(&all_indices, &retained)?;
        let mut pass_plans = Vec::with_capacity(files.len());
        for (position, plan) in translated {
            let file = &files[position].0;
            if retention_traced(&trace, file) {
                eprintln!(
                    "notlin retention probe pass={} file={} translated={:?} untranslated={:?} blockers={:?}",
                    pass + 1,
                    crate::paths::display(file),
                    plan.coverage.translated,
                    plan.coverage.untranslated,
                    plan.coverage.blockers
                );
            }
            for name in index
                .source_file(file)
                .map(|sf| {
                    sf.declarations
                        .iter()
                        .map(|d| d.name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
            {
                if !plan.coverage.translated.contains(&name) {
                    next_retained.insert(name);
                }
            }
            pass_plans.push(plan);
        }
        if next_retained == retained {
            plans = pass_plans;
            stable_pass_seen = true;
            break;
        }
        let key = sorted_key(&next_retained);
        if !seen.insert(key) {
            return Err("workspace retention entered a repeated retained set".into());
        }
        retained = next_retained;
        if profile {
            probe_files_per_pass.push(files.len());
            eprintln!(
                "notlin profile: fixpoint probe pass={} probed_files={} retained_total={} translation={:?} bookkeeping={:?} total={:?}",
                pass + 1,
                files.len(),
                retained.len(),
                probe_transpile,
                probe_retention,
                pass_started.elapsed()
            );
        }
    }
    if !stable_pass_seen {
        return Err(format!(
            "workspace retention did not converge within {max_passes} probe passes"
        ));
    }
    if emit_diagnostics {
        plans.clear();
        for (((file, source), tree), workspace_file) in
            files.iter().zip(&trees).zip(&workspace_files)
        {
            let (java_files, errors, warnings, coverage) = transpile_with_tree_hint_selection(
                source,
                tree,
                file,
                cli,
                WorkspaceScope {
                    index: Some(index),
                    roots: translation_roots,
                    retained_hint: Some(&retained),
                    indexed_path: workspace_file.as_deref(),
                },
                false,
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
    if profile {
        let translation = crate::transpiler::translation_profile();
        eprintln!(
            "notlin profile: workspace fixpoint total={:?}; probe passes={probe_passes}; probe files/pass={probe_files_per_pass:?}; probe transpile={probe_transpile:?}; retention bookkeeping={probe_retention:?}; final transpile={final_transpile:?}; files={}; retained={}",
            profile_start.elapsed(),
            files.len(),
            retained.len()
        );
        eprintln!(
            "notlin profile: translation internals translations={}; parse={:?}; unit={:?}; diagnostics={:?}; java-output={:?}",
            translation.translations,
            translation.parses,
            translation.units,
            translation.diagnostics,
            translation.java_output
        );
    }
    let roots = intrinsic_roots(&retained);
    Ok(WorkspacePlan {
        plans,
        retained,
        roots,
    })
}

fn sorted_key(set: &HashSet<String>) -> Vec<String> {
    let mut key = set.iter().cloned().collect::<Vec<_>>();
    key.sort();
    key
}

/// Convenience: count of java files in a plan list (for summary lines).
pub fn total_java_files(plans: &[FilePlan]) -> usize {
    plans.iter().map(|plan| plan.java_files.len()).sum()
}
