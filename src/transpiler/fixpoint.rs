//! Workspace ownership fixpoint over stable declaration symbols.
//!
//! Each pass re-prepares snapshots whose recorded ownership queries changed.
//! Exact ownership is recomputed so speculative repairs can
//! release declarations; repeated sets and a pass limit prevent oscillation.
//! Nothing is written before convergence and accepted Java preparation.

use crate::cli::Cli;
use crate::transpiler::{PlannedTranslation, WorkspaceScope, parse_tree, plan_with_tree_hint};
use crate::workspace::SourceIndex;
use rayon::prelude::*;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;

/// Per-file fixpoint result: exactly what `run()` needs to write once.
pub struct FilePlan {
    pub emission_failed: bool,
    pub file: PathBuf,
    pub source: String,
    pub java_files: Vec<(String, String)>,
    pub errors: usize,
    pub warnings: usize,
    pub coverage: crate::diagnostics::FileCoverage,
    pub translation: crate::translation_plan::TranslationPlan,
}

/// Workspace plans together with the stable retained declaration names.
pub struct WorkspacePlan {
    pub plans: Vec<FilePlan>,
    pub retained: HashSet<crate::semantics::SymbolId>,
    /// Intrinsic retained declarations. Unlike the full retained closure, these
    /// cannot form self-supporting cascade cycles when reused after overlays.
    pub roots: HashSet<crate::semantics::SymbolId>,
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

fn planner_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(silent_jobs())
            .stack_size(worker_stack_bytes())
            .thread_name(|index| format!("notlin-{index}"))
            .build()
            .expect("build the translation worker pool")
    })
}

pub(crate) fn install_parallel<R: Send>(operation: impl FnOnce() -> R + Send) -> R {
    planner_pool().install(operation)
}

struct SilentPlanner<'a> {
    semantic_provider: &'a dyn crate::semantics::SemanticProvider,
    function_callsite_retention: &'a BTreeMap<
        crate::semantics::SymbolId,
        Vec<crate::function_callsite::CallsiteRepairDiagnostic>,
    >,
    files: &'a [(PathBuf, String)],
    trees: &'a [tree_sitter::Tree],
    workspace_files: &'a [Option<PathBuf>],
    cli: &'a Cli,
    index: &'a SourceIndex,
    translation_roots: &'a [PathBuf],
}

struct CachedFilePlan {
    plan: FilePlan,
    reads: super::retention_queries::RetentionReads,
}

impl SilentPlanner<'_> {
    fn translate_one(
        &self,
        index: usize,
        retained: &HashSet<crate::semantics::SymbolId>,
        forced_retained: &BTreeMap<crate::semantics::SymbolId, usize>,
    ) -> CachedFilePlan {
        let (file, source) = &self.files[index];
        let probe = super::retention_queries::Probe::start();
        let profile = self.cli.verbose > 0 && std::env::var_os("NOTLIN_PROFILE").is_some();
        let started = std::time::Instant::now();
        if profile {
            eprintln!("profile: planning {}", file.display());
        }
        let PlannedTranslation {
            java_files,
            errors,
            warnings,
            coverage,
            plan: translation,
            emission_failed,
            ..
        } = plan_with_tree_hint(
            source,
            &self.trees[index],
            file,
            self.cli,
            WorkspaceScope {
                index: Some(self.index),
                roots: self.translation_roots,
                retained_hint: Some(retained),
                forced_retained: Some(forced_retained),
                indexed_path: self.workspace_files[index].as_deref(),
                semantic_provider: Some(self.semantic_provider),
                function_callsite_retention: Some(self.function_callsite_retention),
            },
            true,
        );
        if profile {
            eprintln!(
                "profile: planned {} in {:.3}s ({} Java files, {} retained declarations)",
                file.display(),
                started.elapsed().as_secs_f64(),
                java_files.len(),
                coverage.untranslated.len(),
            );
        }
        let plan = FilePlan {
            emission_failed,
            file: file.clone(),
            source: source.clone(),
            java_files,
            errors,
            warnings,
            coverage,
            translation,
        };
        CachedFilePlan {
            plan,
            reads: probe.finish(),
        }
    }

    fn translate_indices(
        &self,
        indices: &[usize],
        retained: &HashSet<crate::semantics::SymbolId>,
        forced_retained: &BTreeMap<crate::semantics::SymbolId, usize>,
    ) -> Result<Vec<(usize, CachedFilePlan)>, String> {
        let jobs = silent_jobs().min(indices.len().max(1));
        if jobs <= 1 || indices.len() < 2 {
            return Ok(indices
                .iter()
                .map(|index| {
                    (
                        *index,
                        self.translate_one(*index, retained, forced_retained),
                    )
                })
                .collect());
        }
        Ok(planner_pool().install(|| {
            indices
                .par_iter()
                .map(|index| {
                    (
                        *index,
                        self.translate_one(*index, retained, forced_retained),
                    )
                })
                .collect()
        }))
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
    seed: &HashSet<crate::semantics::SymbolId>,
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
    seed: &HashSet<crate::semantics::SymbolId>,
) -> Result<WorkspacePlan, String> {
    plan_workspace_cold_mode(
        files,
        cli,
        index,
        translation_roots,
        max_passes,
        emit_diagnostics,
        seed,
        BTreeMap::new(),
        true,
    )
    .map(|(plan, _)| plan)
}

// The extra switch supplies the full-replanning reference implementation for tests.
#[allow(clippy::too_many_arguments)]
fn plan_workspace_cold_mode(
    files: &[(PathBuf, String)],
    cli: &Cli,
    index: &SourceIndex,
    translation_roots: &[PathBuf],
    max_passes: usize,
    emit_diagnostics: bool,
    seed: &HashSet<crate::semantics::SymbolId>,
    mut forced_retained: BTreeMap<crate::semantics::SymbolId, usize>,
    incremental: bool,
) -> Result<(WorkspacePlan, usize), String> {
    let trees: Vec<_> = files.iter().map(|(_, source)| parse_tree(source)).collect();
    let workspace_files: Vec<_> = files
        .iter()
        .map(|(file, _)| std::fs::canonicalize(file).ok())
        .collect();
    let semantic_provider = crate::semantics::SyntaxSemanticProvider::new(
        index
            .files
            .iter()
            .map(|file| (file.path.clone(), file.source_text().to_owned())),
    );
    use crate::semantics::SemanticProvider;
    let snapshots = index
        .files
        .iter()
        .map(|file| (file.path.clone(), file.source_text().to_owned()))
        .collect();
    let function_callsite_retention =
        crate::function_callsite::functions_requiring_kotlin_retention_with_provider(
            &snapshots,
            &semantic_provider
                .symbols()
                .iter()
                .map(|symbol| symbol.id.clone())
                .collect::<Vec<_>>(),
            &semantic_provider,
        );
    let planner = SilentPlanner {
        semantic_provider: &semantic_provider,
        function_callsite_retention: &function_callsite_retention,
        files,
        trees: &trees,
        workspace_files: &workspace_files,
        cli,
        index,
        translation_roots,
    };
    let mut indices = (0..files.len()).collect::<Vec<_>>();
    let mut cache: Vec<Option<CachedFilePlan>> = (0..files.len()).map(|_| None).collect();
    let mut preparations = 0;
    let mut retained = seed.clone();
    retained.extend(forced_retained.keys().cloned());
    let mut seen = BTreeMap::new();
    let mut history = vec![retained.clone()];
    seen.insert(sorted_key(&retained), 0usize);
    for _pass in 0..max_passes {
        preparations += indices.len();
        for (position, plan) in planner.translate_indices(&indices, &retained, &forced_retained)? {
            cache[position] = Some(plan);
        }
        let exact: HashSet<_> = cache
            .iter()
            .filter_map(Option::as_ref)
            .flat_map(|cached| &cached.plan.translation.declarations)
            .filter(|decision| {
                decision.final_owner == Some(crate::translation_plan::BackendOwner::Kotlin)
            })
            .map(|decision| decision.symbol_id.clone())
            .collect();
        if std::env::var_os("NOTLIN_PROFILE").is_some() {
            eprintln!(
                "profile: ownership pass {} prepared {} files, reused {}, retained {} symbols",
                _pass + 1,
                indices.len(),
                files.len() - indices.len(),
                exact.len()
            );
        }
        if exact == retained {
            let mut plans: Vec<_> = cache
                .into_iter()
                .map(|cached| cached.expect("prepared file").plan)
                .collect();
            if emit_diagnostics {
                crate::diagnostics::clear_retention();
                plans.clear();
                for (((file, source), tree), indexed_path) in
                    files.iter().zip(&trees).zip(&workspace_files)
                {
                    let PlannedTranslation {
                        java_files,
                        errors,
                        warnings,
                        coverage,
                        plan: translation,
                        emission_failed,
                        ..
                    } = plan_with_tree_hint(
                        source,
                        tree,
                        file,
                        cli,
                        WorkspaceScope {
                            index: Some(index),
                            roots: translation_roots,
                            retained_hint: Some(&retained),
                            forced_retained: Some(&forced_retained),
                            indexed_path: indexed_path.as_deref(),
                            semantic_provider: Some(&semantic_provider),
                            function_callsite_retention: Some(&function_callsite_retention),
                        },
                        false,
                    );
                    plans.push(FilePlan {
                        emission_failed,
                        file: file.clone(),
                        source: source.clone(),
                        java_files,
                        errors,
                        warnings,
                        coverage,
                        translation,
                    });
                }
                qualify_retention_markers(&mut plans);
            }
            let roots = intrinsic_roots(&retained, &plans);
            return Ok((
                WorkspacePlan {
                    plans,
                    retained,
                    roots,
                },
                preparations,
            ));
        }
        let key = sorted_key(&exact);
        if let Some(cycle_start) = seen.get(&key).copied() {
            let cycle_states = &history[cycle_start..];
            let cycle_members = cycle_varying_symbols(cycle_states);
            let newly_forced = cycle_members
                .into_iter()
                .filter(|symbol| !forced_retained.contains_key(symbol))
                .collect::<HashSet<_>>();
            if newly_forced.is_empty() {
                return Err("workspace retention entered a repeated retained symbol set".into());
            }
            let period = history.len().saturating_sub(cycle_start).max(1);
            if std::env::var_os("NOTLIN_PROFILE").is_some() {
                eprintln!(
                    "profile: ownership cycle of {period} passes; retaining {} unstable declarations",
                    newly_forced.len()
                );
            }
            for symbol in &newly_forced {
                forced_retained.insert(symbol.clone(), period);
            }
            let mut next = exact;
            next.extend(forced_retained.keys().cloned());
            let delta = retained.symmetric_difference(&next).cloned().collect();
            indices = cache
                .iter()
                .enumerate()
                .filter_map(|(position, cached)| {
                    let owns_cycle_member = cached.as_ref().is_some_and(|cached| {
                        cached
                            .plan
                            .translation
                            .declarations
                            .iter()
                            .any(|decision| newly_forced.contains(&decision.symbol_id))
                    });
                    (!incremental
                        || cached
                            .as_ref()
                            .is_none_or(|cached| cached.reads.affected_by(&delta))
                        || owns_cycle_member)
                        .then_some(position)
                })
                .collect();
            retained = next;
            history.clear();
            history.push(retained.clone());
            seen.clear();
            seen.insert(sorted_key(&retained), 0);
            continue;
        }
        seen.insert(key, history.len());
        history.push(exact.clone());
        let delta = retained.symmetric_difference(&exact).cloned().collect();
        indices = cache
            .iter()
            .enumerate()
            .filter_map(|(position, cached)| {
                (!incremental
                    || cached
                        .as_ref()
                        .is_none_or(|cached| cached.reads.affected_by(&delta)))
                .then_some(position)
            })
            .collect();
        retained = exact;
    }
    Err(format!(
        "workspace retention did not converge within {max_passes} probe passes; increase --max-retention-passes above {max_passes}"
    ))
}
pub fn qualify_retention_markers(plans: &mut [FilePlan]) {
    qualify_retention_markers_at(plans);
}

pub fn qualify_retention_markers_at(plans: &mut [FilePlan]) {
    let sites = crate::diagnostics::retention_sites();
    let qualified_names = sites
        .iter()
        .filter_map(|site| {
            let plan = plans
                .iter()
                .find(|plan| crate::paths::display(&plan.file) == site.file)?;
            let package = plan.source.lines().map(str::trim).find_map(|line| {
                line.strip_prefix("package ")
                    .map(|package| package.trim_end_matches(';').trim())
            })?;
            Some((
                (site.file.clone(), site.name.clone()),
                format!("{package}.{}", site.name),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let sites_by_name = sites
        .iter()
        .fold(BTreeMap::<&str, Vec<_>>::new(), |mut map, site| {
            map.entry(&site.name).or_default().push(site);
            map
        });
    for plan in plans {
        for (offset, marker) in &mut plan.coverage.blockers {
            let line = source_line(&plan.source, *offset);
            let Some((original_code, original_message)) = marker
                .trim_end()
                .strip_prefix("// NOTLIN: ")
                .and_then(|rest| rest.split_once(' '))
            else {
                continue;
            };
            if !original_message.starts_with("retained ") {
                *marker = crate::retention_docs::kdoc(original_code, original_message, &[]);
                continue;
            }
            let file = crate::paths::display(&plan.file);
            let Some(site) = sites
                .iter()
                .find(|site| site.file == file && site.line == line)
            else {
                continue;
            };
            let links = site
                .blockers
                .iter()
                .flat_map(|name| sites_by_name.get(name.as_str()).into_iter().flatten())
                .filter_map(|blocker| {
                    qualified_names
                        .get(&(blocker.file.clone(), blocker.name.clone()))
                        .cloned()
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let reason = if links.is_empty() {
                site.kind.detail(&site.params)
            } else {
                format!(
                    "{} because the linked declarations remain Kotlin.",
                    site.kind.detail(&site.params)
                )
            };
            *marker = crate::retention_docs::kdoc(&site.kind.code(), &reason, &links);
        }
    }
}

fn source_line(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn intrinsic_roots(
    retained: &HashSet<crate::semantics::SymbolId>,
    plans: &[FilePlan],
) -> HashSet<crate::semantics::SymbolId> {
    let sites = crate::diagnostics::retention_sites();
    retained
        .iter()
        .filter(|symbol| {
            !sites.iter().any(|site| {
                site.name == symbol.name
                    && !site.blockers.is_empty()
                    && plans.iter().any(|plan| {
                        crate::paths::display(&plan.file) == site.file
                            && plan
                                .translation
                                .declarations
                                .iter()
                                .any(|decision| &decision.symbol_id == *symbol)
                    })
            })
        })
        .cloned()
        .collect()
}

fn sorted_key(set: &HashSet<crate::semantics::SymbolId>) -> Vec<crate::semantics::SymbolId> {
    let mut key = set.iter().cloned().collect::<Vec<_>>();
    key.sort();
    key
}

fn cycle_varying_symbols(
    states: &[HashSet<crate::semantics::SymbolId>],
) -> HashSet<crate::semantics::SymbolId> {
    let Some(first) = states.first() else {
        return HashSet::new();
    };
    let mut union = HashSet::new();
    let mut intersection = first.clone();
    for state in states {
        union.extend(state.iter().cloned());
        intersection.retain(|symbol| state.contains(symbol));
    }
    union.difference(&intersection).cloned().collect()
}
/// Convenience: count of java files in a plan list (for summary lines).
pub fn total_java_files(plans: &[FilePlan]) -> usize {
    plans.iter().map(|plan| plan.java_files.len()).sum()
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use clap::Parser;

    fn symbol(name: &str) -> crate::semantics::SymbolId {
        crate::semantics::SymbolId {
            module: "test".into(),
            package: "sample".into(),
            file: PathBuf::from("sample.kt"),
            owner_path: Vec::new(),
            kind: "class".into(),
            name: name.into(),
            receiver: None,
            parameters: Vec::new(),
        }
    }

    #[test]
    fn three_state_cycle_pins_every_membership_varying_symbol() {
        let stable = symbol("Stable");
        let first_only = symbol("FirstOnly");
        let middle_only = symbol("MiddleOnly");
        let last_only = symbol("LastOnly");
        let states = [
            HashSet::from([stable.clone(), first_only.clone()]),
            HashSet::from([stable.clone(), middle_only.clone()]),
            HashSet::from([stable, first_only.clone(), last_only.clone()]),
        ];

        assert_eq!(
            cycle_varying_symbols(&states),
            HashSet::from([first_only, middle_only, last_only])
        );
    }

    #[test]
    fn cycle_pin_retains_eligible_declaration_without_blocking_unrelated_java() {
        use crate::semantics::workspace_symbol;

        let root = std::env::temp_dir().join(format!(
            "notlin-cycle-pin-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("Types.kt");
        let source = "package sample\nclass Forced\nclass Independent\n";
        std::fs::write(&path, source).unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        let forced = workspace_symbol(
            &index,
            index
                .declarations()
                .find(|declaration| declaration.name == "Forced")
                .unwrap(),
        );
        let cli = Cli::parse_from(["notlin", "--annotations", "none"]);
        let files = [(path.clone(), source.to_owned())];
        let forced_set = BTreeMap::from([(forced.clone(), 3usize)]);
        let (planned, _) = plan_workspace_cold_mode(
            &files,
            &cli,
            &index,
            std::slice::from_ref(&root),
            8,
            false,
            &HashSet::new(),
            forced_set,
            true,
        )
        .unwrap();

        assert!(planned.retained.contains(&forced));
        let forced_decision = planned.plans[0]
            .translation
            .declarations
            .iter()
            .find(|decision| decision.symbol_id == forced)
            .unwrap();
        assert_eq!(
            forced_decision.candidate_owner,
            crate::translation_plan::BackendOwner::Java
        );
        assert_eq!(
            forced_decision.final_owner,
            Some(crate::translation_plan::BackendOwner::Kotlin)
        );
        assert!(forced_decision.retention_reasons.iter().any(|reason| {
            matches!(
                reason,
                crate::translation_plan::RetentionReason::OwnershipCycle { period: 3 }
            )
        }));
        assert!(
            planned.plans[0]
                .java_files
                .iter()
                .any(|(name, _)| name.ends_with("Independent.java"))
        );
        assert!(
            !planned.plans[0]
                .java_files
                .iter()
                .any(|(name, _)| name.ends_with("Forced.java"))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn incremental_ownership_matches_full_replanning_and_reuses_unrelated_files() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("plan-cache-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut sources = vec![
            (
                "Leaf.kt".to_owned(),
                "package sample\nclass Leaf : Contract { override fun value(): Int = 1; override fun base(): Int = 2; suspend fun boundary() {} }\n".to_owned(),
            ),
            (
                "Contract.kt".to_owned(),
                "package sample\ninterface Contract : Parent { fun value(): Int }\n".to_owned(),
            ),
            (
                "Parent.kt".to_owned(),
                "package sample\ninterface Parent { fun base(): Int }\n".to_owned(),
            ),
        ];
        for index in 0..24 {
            sources.push((
                format!("Stable{index}.kt"),
                format!("package sample\nclass Stable{index}(val value: Int)\n"),
            ));
        }
        for (name, source) in &sources {
            std::fs::write(root.join(name), source).unwrap();
        }
        let index = SourceIndex::discover(&root).unwrap();
        let files: Vec<_> = sources
            .into_iter()
            .map(|(name, source)| (std::fs::canonicalize(root.join(name)).unwrap(), source))
            .collect();
        let cli = Cli::parse_from(["notlin", "--annotations", "none"]);
        let (incremental, incremental_work) = plan_workspace_cold_mode(
            &files,
            &cli,
            &index,
            std::slice::from_ref(&root),
            64,
            false,
            &HashSet::new(),
            BTreeMap::new(),
            true,
        )
        .unwrap();
        let (full, full_work) = plan_workspace_cold_mode(
            &files,
            &cli,
            &index,
            std::slice::from_ref(&root),
            64,
            false,
            &HashSet::new(),
            BTreeMap::new(),
            false,
        )
        .unwrap();
        assert_eq!(incremental.retained, full.retained);
        assert_eq!(incremental.roots, full.roots);
        for (left, right) in incremental.plans.iter().zip(&full.plans) {
            assert_eq!(left.java_files, right.java_files);
            assert_eq!(left.translation, right.translation);
            assert_eq!(left.coverage.untranslated, right.coverage.untranslated);
        }
        assert!(
            incremental_work < full_work,
            "{incremental_work} incremental vs {full_work} full preparations"
        );
        eprintln!("ownership preparations: {incremental_work} incremental vs {full_work} full");
        std::fs::remove_dir_all(root).unwrap();
    }
}
