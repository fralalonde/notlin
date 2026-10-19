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
    pub emitted_owners: Vec<crate::semantics::SymbolId>,
    pub emitted_file_owners: Vec<(String, Vec<crate::semantics::SymbolId>)>,
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
    top_level_property_callsite_retention: &'a crate::top_level_property_callsite::RetentionMap,
    files: &'a [(PathBuf, String)],
    trees: &'a [tree_sitter::Tree],
    workspace_files: &'a [Option<PathBuf>],
    cli: &'a Cli,
    index: &'a SourceIndex,
    translation_roots: &'a [PathBuf],
    static_facts: Option<&'a [super::StaticPlanFacts]>,
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
        let started = std::time::Instant::now();
        log::debug!("planning {}", crate::paths::display(file));
        let scope = WorkspaceScope {
            index: Some(self.index),
            roots: self.translation_roots,
            retained_hint: Some(retained),
            forced_retained: Some(forced_retained),
            indexed_path: self.workspace_files[index].as_deref(),
            semantic_provider: Some(self.semantic_provider),
            function_callsite_retention: Some(self.function_callsite_retention),
            top_level_property_callsite_retention: Some(self.top_level_property_callsite_retention),
        };
        let planned = if let Some(static_facts) = self.static_facts {
            super::plan_with_static_facts_hint(
                source,
                &self.trees[index],
                file,
                self.cli,
                scope,
                true,
                &static_facts[index],
            )
        } else {
            plan_with_tree_hint(source, &self.trees[index], file, self.cli, scope, true)
        };
        let PlannedTranslation {
            java_files,
            emitted_owners,
            emitted_file_owners,
            errors,
            warnings,
            coverage,
            plan: translation,
            emission_failed,
            ..
        } = planned;
        log::debug!(
            "planned {} in {:.3}s ({} Java files, {} retained declarations)",
            crate::paths::display(file),
            started.elapsed().as_secs_f64(),
            java_files.len(),
            coverage.untranslated.len(),
        );
        let plan = FilePlan {
            emission_failed,
            file: file.clone(),
            source: source.clone(),
            java_files,
            emitted_owners,
            emitted_file_owners,
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
    cache_static_facts: bool,
) -> Result<(WorkspacePlan, usize), String> {
    let setup_started = std::time::Instant::now();
    let trees: Vec<_> = files.iter().map(|(_, source)| parse_tree(source)).collect();
    let workspace_files: Vec<_> = files
        .iter()
        .map(|(file, _)| std::fs::canonicalize(file).ok())
        .collect();
    let mut semantic_provider = crate::semantics::SyntaxSemanticProvider::new(
        index
            .files
            .iter()
            .map(|file| (file.path.clone(), file.source_text().to_owned())),
    );
    semantic_provider.enrich_workspace_type_facts(index);
    use crate::semantics::SemanticProvider;
    if std::env::var_os("NOTLIN_PROFILE").is_some() {
        eprintln!(
            "NOTLIN_PROFILE semantic provider setup: {:.3}s, {} symbols",
            setup_started.elapsed().as_secs_f64(),
            semantic_provider.symbols().len()
        );
    }
    let callsite_started = std::time::Instant::now();
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
    let top_level_property_callsite_retention =
        crate::top_level_property_callsite::properties_requiring_kotlin_retention_in_selection(
            &snapshots,
            &semantic_provider
                .symbols()
                .iter()
                .map(|s| s.id.clone())
                .collect::<Vec<_>>(),
            &semantic_provider,
            &files
                .iter()
                .map(|(path, _)| {
                    index
                        .source_file(path)
                        .map(|file| file.path.clone())
                        .unwrap_or_else(|| path.clone())
                })
                .collect(),
        );
    if std::env::var_os("NOTLIN_PROFILE").is_some() {
        eprintln!(
            "NOTLIN_PROFILE call-site eligibility: {:.3}s",
            callsite_started.elapsed().as_secs_f64()
        );
    }
    let static_facts_started = std::time::Instant::now();
    let static_facts = cache_static_facts.then(|| {
        planner_pool().install(|| {
            files
                .par_iter()
                .enumerate()
                .map(|(file_index, (file, source))| {
                    crate::transpiler::prepare_static_plan_facts(
                        source,
                        &trees[file_index],
                        file,
                        cli,
                        WorkspaceScope {
                            index: Some(index),
                            roots: translation_roots,
                            semantic_provider: Some(&semantic_provider),
                            indexed_path: workspace_files[file_index].as_deref(),
                            ..WorkspaceScope::default()
                        },
                    )
                })
                .collect::<Vec<_>>()
        })
    });
    if cache_static_facts && std::env::var_os("NOTLIN_PROFILE").is_some() {
        eprintln!(
            "NOTLIN_PROFILE round static plan facts: {:.3}s",
            static_facts_started.elapsed().as_secs_f64()
        );
    }
    let planner = SilentPlanner {
        semantic_provider: &semantic_provider,
        function_callsite_retention: &function_callsite_retention,
        top_level_property_callsite_retention: &top_level_property_callsite_retention,
        files,
        trees: &trees,
        workspace_files: &workspace_files,
        cli,
        index,
        translation_roots,
        static_facts: static_facts.as_deref(),
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
                for (file_index, (((file, source), tree), indexed_path)) in
                    files.iter().zip(&trees).zip(&workspace_files).enumerate()
                {
                    let scope = WorkspaceScope {
                        index: Some(index),
                        roots: translation_roots,
                        retained_hint: Some(&retained),
                        forced_retained: Some(&forced_retained),
                        indexed_path: indexed_path.as_deref(),
                        semantic_provider: Some(&semantic_provider),
                        function_callsite_retention: Some(&function_callsite_retention),
                        top_level_property_callsite_retention: Some(
                            &top_level_property_callsite_retention,
                        ),
                    };
                    let planned = if let Some(static_facts) =
                        static_facts.as_ref().map(|facts| &facts[file_index])
                    {
                        super::plan_with_static_facts_hint(
                            source,
                            tree,
                            file,
                            cli,
                            scope,
                            false,
                            static_facts,
                        )
                    } else {
                        plan_with_tree_hint(source, tree, file, cli, scope, false)
                    };
                    let PlannedTranslation {
                        java_files,
                        emitted_owners,
                        emitted_file_owners,
                        errors,
                        warnings,
                        coverage,
                        plan: translation,
                        emission_failed,
                        ..
                    } = planned;
                    plans.push(FilePlan {
                        emission_failed,
                        file: file.clone(),
                        source: source.clone(),
                        java_files,
                        emitted_owners,
                        emitted_file_owners,
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
            true,
        )
        .unwrap();
        let (uncached, _) = plan_workspace_cold_mode(
            &files,
            &cli,
            &index,
            std::slice::from_ref(&root),
            64,
            false,
            &HashSet::new(),
            BTreeMap::new(),
            true,
            false,
        )
        .unwrap();
        assert_eq!(incremental.retained, full.retained);
        assert_eq!(incremental.roots, full.roots);
        assert_eq!(incremental.retained, uncached.retained);
        assert_eq!(incremental.roots, uncached.roots);
        for ((left, right), no_cache) in incremental
            .plans
            .iter()
            .zip(&full.plans)
            .zip(&uncached.plans)
        {
            assert_eq!(left.java_files, right.java_files);
            assert_eq!(left.translation, right.translation);
            assert_eq!(left.coverage.untranslated, right.coverage.untranslated);
            assert_eq!(left.java_files, no_cache.java_files);
            assert_eq!(left.translation, no_cache.translation);
            assert_eq!(left.coverage.untranslated, no_cache.coverage.untranslated);
        }
        assert!(
            incremental_work < full_work,
            "{incremental_work} incremental vs {full_work} full preparations"
        );
        eprintln!("ownership preparations: {incremental_work} incremental vs {full_work} full");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn static_plan_facts_follow_provider_and_approximation_round_context() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("static-facts-provider-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let consumer_path = root.join("Consumer.kt");
        let consumer_source =
            "package sample\nclass Consumer { fun read(): String = Producer().message() }\n";
        std::fs::write(&consumer_path, consumer_source).unwrap();

        let assert_cached_matches_uncached =
            |files: &[(PathBuf, String)], index: &SourceIndex, cli: &Cli| {
                let cached = plan_workspace_cold_mode(
                    files,
                    cli,
                    index,
                    std::slice::from_ref(&root),
                    16,
                    false,
                    &HashSet::new(),
                    BTreeMap::new(),
                    true,
                    true,
                )
                .unwrap()
                .0;
                let uncached = plan_workspace_cold_mode(
                    files,
                    cli,
                    index,
                    std::slice::from_ref(&root),
                    16,
                    false,
                    &HashSet::new(),
                    BTreeMap::new(),
                    true,
                    false,
                )
                .unwrap()
                .0;
                assert_eq!(cached.retained, uncached.retained);
                assert_eq!(cached.roots, uncached.roots);
                for (cached, uncached) in cached.plans.iter().zip(&uncached.plans) {
                    assert_eq!(cached.java_files, uncached.java_files);
                    assert_eq!(cached.translation, uncached.translation);
                    assert_eq!(cached.coverage.untranslated, uncached.coverage.untranslated);
                }
            };

        let mut cli = Cli::parse_from(["notlin", "--annotations", "none"]);
        for allow_approximations in [false, true] {
            cli.allow_approximations = allow_approximations;
            let before_index = SourceIndex::discover(&root).unwrap();
            let before_files = [(consumer_path.clone(), consumer_source.to_owned())];
            assert_cached_matches_uncached(&before_files, &before_index, &cli);

            let producer_path = root.join("Producer.kt");
            let producer_source =
                "package sample\nclass Producer { fun message(): String = \"ready\" }\n";
            std::fs::write(&producer_path, producer_source).unwrap();
            let after_index = SourceIndex::discover(&root).unwrap();
            let after_files = [
                (consumer_path.clone(), consumer_source.to_owned()),
                (producer_path.clone(), producer_source.to_owned()),
            ];
            assert_cached_matches_uncached(&after_files, &after_index, &cli);
            std::fs::remove_file(producer_path).unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn static_plan_facts_are_invalidated_for_a_new_source_index_round() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("static-facts-invalidation-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("Snapshot.kt");
        let first_source = "package sample\ndata class Snapshot(val first: String)\n";
        std::fs::write(&path, first_source).unwrap();
        let first_index = SourceIndex::discover(&root).unwrap();
        let first_files = [(path.clone(), first_source.to_owned())];
        let cli = Cli::parse_from(["notlin", "--annotations", "none"]);
        let first = plan_workspace_cold_mode(
            &first_files,
            &cli,
            &first_index,
            std::slice::from_ref(&root),
            16,
            false,
            &HashSet::new(),
            BTreeMap::new(),
            true,
            true,
        )
        .unwrap()
        .0;

        let second_source = "package sample\ndata class Snapshot(val second: Int)\n";
        std::fs::write(&path, second_source).unwrap();
        let second_index = SourceIndex::discover(&root).unwrap();
        let second_files = [(path.clone(), second_source.to_owned())];
        let second = plan_workspace_cold_mode(
            &second_files,
            &cli,
            &second_index,
            std::slice::from_ref(&root),
            16,
            false,
            &HashSet::new(),
            BTreeMap::new(),
            true,
            true,
        )
        .unwrap()
        .0;
        let first_java = first.plans[0]
            .java_files
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let second_java = second.plans[0]
            .java_files
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(first_java.contains("first"), "{first_java}");
        assert!(second_java.contains("second"), "{second_java}");
        assert!(
            !second_java.contains("first"),
            "stale plan fact reused: {second_java}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
