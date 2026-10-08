//! Pure planning for a complete workspace migration.
//!
//! Planning uses virtual Kotlin and Java overlays until the retention and ABI
//! repair fixpoint is stable. It never changes source files; callers can gate
//! the returned plan on validation and then apply it.

use crate::cli::{Cli, UntranslatableMode};
use crate::migrate::{self, MigrationProposal};
use crate::transpiler;
use crate::workspace::{SourceIndex, SourceLanguage, SourceOverlay};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct GeneratedJava {
    /// Canonicalized destination used to detect collisions.
    pub path: PathBuf,
    pub origin: PathBuf,
    pub name: String,
    pub source: String,
}

pub struct WorkspaceMigrationPlan {
    pub original_sources: Vec<(PathBuf, String)>,
    /// Byte-exact snapshots of every Kotlin and Java input consulted by the
    /// plan, including indexed supporting Java files.
    pub original_snapshots: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// Deleted sources are absent from this map.
    pub final_kotlin_sources: HashMap<PathBuf, String>,
    pub generated_java: Vec<GeneratedJava>,
    pub final_plans: Vec<transpiler::fixpoint::FilePlan>,
    /// Per-original-file ownership decisions retained across speculative
    /// rounds, including declarations removed before the final pass.
    pub translation_plans: HashMap<PathBuf, crate::translation_plan::TranslationPlan>,
    /// Final per-original-file write proposal.
    pub migration_proposals: HashMap<PathBuf, MigrationProposal>,
    /// Snapshot-checked edits emitted by the structured translation plans.
    pub source_edits: Vec<crate::translation_plan::PlannedSourceEdit>,
    pub rounds: usize,
}

/// Resolve a workspace fixpoint without writing any files. The callback is
/// called with human-readable progress detail after each speculative round.
pub fn plan_workspace_migration<F>(
    sources: Vec<(PathBuf, String)>,
    cli: &Cli,
    index: &SourceIndex,
    translation_roots: &[PathBuf],
    mut progress: F,
) -> Result<WorkspaceMigrationPlan, String>
where
    F: FnMut(String),
{
    let mut input_ids = HashSet::<PathBuf>::new();
    for (path, _) in &sources {
        if !input_ids.insert(normalized(path)) {
            return Err(format!(
                "overlapping workspace input: {}",
                crate::paths::display(path)
            ));
        }
    }
    let mut virtual_sources = sources.clone();
    let mut cumulative: HashMap<PathBuf, (PathBuf, String, String)> = HashMap::new();
    let mut output_identities = HashMap::<PathBuf, PathBuf>::new();
    let declaration_count = index
        .kotlin_files()
        .map(|f| f.declarations.len())
        .sum::<usize>();
    let limit = declaration_count.saturating_mul(2).max(16);
    let mut converged_plans = None;
    let mut converged_round = 0usize;
    let mut retained_seed: Option<HashSet<crate::semantics::SymbolId>> = None;
    let mut translation_plans = HashMap::<PathBuf, crate::translation_plan::TranslationPlan>::new();
    let mut strict_vetoed_paths = HashSet::<PathBuf>::new();
    let mut callsite_repair = crate::property_callsite::IncrementalRepairState::default();
    // Keep contracts from accepted operations, even once the former property
    // is no longer present in a later speculative source snapshot.
    let mut repaired_property_contracts = Vec::new();
    // Preserve typed ABI contracts when an interface was repaired to explicit
    // Kotlin getter methods in an earlier speculative round. Descendants may
    // only become repairable after their generated Java supertype is accepted.
    let mut persisted_abi_contracts = Vec::<crate::property_abi::PersistedPropertyContract>::new();
    let profile_speculation = std::env::var_os("NOTLIN_PROFILE").is_some();
    // These are immutable input snapshots. Canonicalize each identity once,
    // rather than repeating filesystem queries for every generated output.
    let mut original_java_by_identity = HashMap::new();
    for file in index.java_files() {
        original_java_by_identity
            .entry(normalized(&file.path))
            .or_insert(file);
    }
    let mut original_sources_by_identity = HashMap::new();
    for (path, source) in &sources {
        original_sources_by_identity
            .entry(normalized(path))
            .or_insert(source.as_str());
    }

    for round in 1..=limit {
        let mut phase_started = std::time::Instant::now();
        crate::diagnostics::clear_retention();
        let overlays = speculative_overlays(&sources, &virtual_sources, &cumulative);
        let current_index = index.with_overlays(&overlays)?;
        profile_phase(
            profile_speculation,
            round,
            &mut phase_started,
            "source index",
        );
        let planned = if let Some(seed) = &retained_seed {
            transpiler::fixpoint::plan_workspace_warm(
                &virtual_sources,
                cli,
                &current_index,
                translation_roots,
                cli.max_retention_passes,
                false,
                seed,
            )?
        } else {
            transpiler::fixpoint::plan_workspace_state(
                &virtual_sources,
                cli,
                &current_index,
                translation_roots,
                cli.max_retention_passes,
                false,
            )?
        };
        retained_seed = Some(planned.roots);
        let plans = planned.plans;
        profile_phase(
            profile_speculation,
            round,
            &mut phase_started,
            "ownership planning",
        );
        for file_plan in &plans {
            if file_plan.emission_failed {
                let details = file_plan
                    .translation
                    .diagnostics
                    .iter()
                    .filter(|diagnostic| diagnostic.code == "E001")
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(format!(
                    "Java emission failed for {}; refusing Kotlin migration: {}",
                    crate::paths::display(&file_plan.file),
                    details
                ));
            }
            let original_source = original_sources_by_identity
                .get(&normalized(&file_plan.file))
                .copied();
            merge_translation_plan(
                &mut translation_plans,
                &file_plan.file,
                &file_plan.source,
                original_source,
                &file_plan.translation,
            )?;
        }
        profile_phase(profile_speculation, round, &mut phase_started, "plan merge");
        let mut next = virtual_sources.iter().cloned().collect::<HashMap<_, _>>();
        let cumulative_before = profile_speculation.then(|| cumulative.clone());
        let mut cumulative_changed = false;
        let strict = matches!(cli.untranslatable, UntranslatableMode::Error);
        let mut vetoed_this_round = HashSet::<PathBuf>::new();
        for plan in &plans {
            if strict && (plan.errors > 0 || plan.warnings > 0) {
                if let Some((_, original)) = sources
                    .iter()
                    .find(|(path, _)| normalized(path) == normalized(&plan.file))
                {
                    next.insert(plan.file.clone(), original.clone());
                }
                strict_vetoed_paths.insert(normalized(&plan.file));
                vetoed_this_round.insert(normalized(&plan.file));
                veto_translation_plan(&mut translation_plans, &plan.file);
                let stale = cumulative
                    .iter()
                    .filter(|(_, (origin, _, _))| normalized(origin) == normalized(&plan.file))
                    .map(|(path, _)| path.clone())
                    .collect::<Vec<_>>();
                for path in stale {
                    cumulative.remove(&path);
                    output_identities.remove(&normalized(&path));
                    cumulative_changed = true;
                }
                continue;
            }
            match migrate::propose_speculative_migration(&plan.source, &plan.coverage) {
                MigrationProposal::Untouched => {}
                MigrationProposal::Delete => {
                    next.remove(&plan.file);
                }
                MigrationProposal::Rewrite(text) => {
                    next.insert(plan.file.clone(), text);
                }
            }
            for (name, content) in &plan.java_files {
                let target = cli
                    .out_dir
                    .as_ref()
                    .map(|dir| dir.join(name))
                    .unwrap_or_else(|| plan.file.parent().unwrap_or(Path::new(".")).join(name));
                let identity = normalized(&target);
                let key = target.clone();
                if let Some(existing) = original_java_by_identity.get(&identity) {
                    let origins = [
                        normalized(&plan.file).to_string_lossy().replace('\\', "/"),
                        plan.file.to_string_lossy().replace('\\', "/"),
                    ];
                    let owned = existing.source_text().lines().next().is_some_and(|line| {
                        line.contains("NOTLIN: generated from")
                            && origins
                                .iter()
                                .any(|origin| line.replace('\\', "/").contains(origin))
                    });
                    if !owned {
                        return Err(format!(
                            "generated Java path overlaps an existing source: {}",
                            crate::paths::display(&target)
                        ));
                    }
                }
                if target.exists() && !original_java_by_identity.contains_key(&identity) {
                    let existing = std::fs::read_to_string(&target)
                        .map_err(|e| format!("{}: {e}", crate::paths::display(&target)))?;
                    let origins = [
                        normalized(&plan.file).to_string_lossy().replace('\\', "/"),
                        plan.file.to_string_lossy().replace('\\', "/"),
                    ];
                    let owned = existing.lines().next().is_some_and(|line| {
                        line.contains("NOTLIN: generated from")
                            && origins
                                .iter()
                                .any(|origin| line.replace('\\', "/").contains(origin))
                    });
                    if !owned {
                        return Err(format!(
                            "generated Java path overlaps an existing file: {}",
                            crate::paths::display(&target)
                        ));
                    }
                }
                if let Some(previous_path) = output_identities.get(&identity)
                    && let Some((origin, _old, _)) = cumulative.get(previous_path)
                    && origin != &plan.file
                {
                    return Err(format!(
                        "generated Java path conflict: {}",
                        crate::paths::display(&target)
                    ));
                }
                let value = (plan.file.clone(), content.clone(), name.clone());
                cumulative_changed |= cumulative.get(&key) != Some(&value);
                cumulative.insert(key, value);
                output_identities.insert(identity, target);
            }
        }
        let mut next = next.into_iter().collect::<Vec<_>>();
        profile_phase(
            profile_speculation,
            round,
            &mut phase_started,
            "output acceptance",
        );
        next.sort_by(|left, right| left.0.cmp(&right.0));
        let original_by_path = sources
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut residual_by_path = next
            .iter()
            .filter(|(path, _)| !vetoed_this_round.contains(&normalized(path)))
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        let callsite_report = crate::function_callsite::repair_virtual_calls(
            &original_by_path,
            &plans
                .iter()
                .map(|plan| plan.translation.clone())
                .collect::<Vec<_>>(),
            &mut residual_by_path,
        );
        profile_phase(
            profile_speculation,
            round,
            &mut phase_started,
            "function call-site repair",
        );
        if callsite_report.requires_replan {
            let details = callsite_report
                .diagnostics
                .iter()
                .map(|diagnostic| {
                    format!(
                        "{}: {}",
                        crate::paths::display(&diagnostic.file),
                        diagnostic.message
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!(
                "function call-site repair needs a safer migration plan: {details}"
            ));
        }
        for repair in callsite_report.repairs {
            if vetoed_this_round.contains(&normalized(&repair.target.file)) {
                continue;
            }
            if let Some(path) = plan_owner_path(&translation_plans, &repair.target)
                && let Some(aggregate) = translation_plans.get_mut(&path)
                && !aggregate.repairs.contains(&repair)
            {
                aggregate.repairs.push(repair);
            }
        }
        for (path, original) in &sources {
            if vetoed_this_round.contains(&normalized(path)) {
                residual_by_path.insert(path.clone(), original.clone());
            }
        }
        next = residual_by_path.into_iter().collect();
        let repair_overlays = speculative_overlays(&sources, &next, &cumulative);
        let repair_index = index.with_overlays(&repair_overlays)?;
        profile_phase(
            profile_speculation,
            round,
            &mut phase_started,
            "property repair index",
        );
        let generated_java = cumulative.keys().cloned().collect::<HashSet<_>>();
        let repair_report = crate::property_abi::repair_virtual_sources_planned_with_contracts(
            &repair_index,
            &mut next,
            &generated_java,
            &persisted_abi_contracts,
        );
        repaired_property_contracts.extend(repair_report.callsite_contracts.iter().cloned());
        repaired_property_contracts
            .retain(|(path, _)| !vetoed_this_round.contains(&normalized(path)));
        for contract in &repair_report.abi_contracts {
            if !vetoed_this_round.contains(&normalized(contract.owner_file()))
                && !persisted_abi_contracts.contains(contract)
            {
                persisted_abi_contracts.push(contract.clone());
            }
        }
        merge_property_repairs(&mut translation_plans, repair_report, &vetoed_this_round);
        profile_phase(
            profile_speculation,
            round,
            &mut phase_started,
            "property ABI repair",
        );
        let mut callsite_contracts =
            crate::property_abi::repaired_callsite_contracts_with_persisted(
                &repair_index,
                &generated_java,
                &persisted_abi_contracts,
            );
        callsite_contracts.extend(
            repaired_property_contracts
                .iter()
                .map(|(_, contract)| contract.clone()),
        );
        let full_callsite_sweep = next == virtual_sources && !cumulative_changed;
        let callsite_needs_index =
            callsite_repair.requires_index(&next, &callsite_contracts, full_callsite_sweep);
        let callsite_stats = if callsite_needs_index {
            let callsite_overlays = speculative_overlays(&sources, &next, &cumulative);
            let callsite_index = index.with_overlays(&callsite_overlays)?;
            callsite_repair.rewrite_files(
                &callsite_index,
                &mut next,
                &callsite_contracts,
                full_callsite_sweep,
            )
        } else {
            callsite_repair.rewrite_files(
                &repair_index,
                &mut next,
                &callsite_contracts,
                full_callsite_sweep,
            )
        };
        profile_phase(
            profile_speculation,
            round,
            &mut phase_started,
            "property call-site repair",
        );
        for (path, original) in &sources {
            if vetoed_this_round.contains(&normalized(path))
                && let Some((_, residual)) = next
                    .iter_mut()
                    .find(|(candidate, _)| normalized(candidate) == normalized(path))
            {
                residual.clone_from(original);
            }
        }
        if profile_speculation {
            log_speculative_changes(
                round,
                &virtual_sources,
                &next,
                cumulative_before.as_ref().expect("profile snapshot"),
                &cumulative,
                cli.verbose > 0,
            );
        }
        progress(format!(
            "pass {round} · {} Kotlin sources remain · {} Java outputs ready · {} call-site candidates",
            next.len(),
            cumulative.len(),
            callsite_stats.candidate_files
        ));
        if next == virtual_sources && !cumulative_changed {
            converged_round = round;
            converged_plans = Some(plans);
            break;
        }
        virtual_sources = next;
    }
    let Some(mut final_plans) = converged_plans else {
        return Err(format!(
            "workspace migration did not converge within {limit} speculative rounds"
        ));
    };
    transpiler::fixpoint::qualify_retention_markers(&mut final_plans);
    for file_plan in &mut final_plans {
        if matches!(cli.untranslatable, UntranslatableMode::Error)
            && (file_plan.errors > 0 || file_plan.warnings > 0)
        {
            strict_vetoed_paths.insert(normalized(&file_plan.file));
            veto_translation(&mut file_plan.translation);
            file_plan.java_files.clear();
            file_plan.coverage.translated.clear();
            file_plan.coverage.translated_spans.clear();
            file_plan.coverage.attached_comment_spans.clear();
            file_plan.coverage.untranslated = file_plan
                .translation
                .declarations
                .iter()
                .map(|decision| decision.symbol_id.name.clone())
                .collect();
        }
        for decision in &file_plan.translation.declarations {
            if decision.final_owner == Some(crate::translation_plan::BackendOwner::Kotlin)
                && file_plan
                    .coverage
                    .translated_spans
                    .iter()
                    .any(|(start, end)| {
                        *start < decision.id.end_byte && decision.id.start_byte < *end
                    })
            {
                return Err(format!(
                    "inconsistent final ownership for {}: Kotlin-owned declaration {} overlaps translated source coverage",
                    crate::paths::display(&file_plan.file),
                    decision.symbol_id.stable_key()
                ));
            }
        }
    }
    let mut final_map: HashMap<_, _> = virtual_sources.into_iter().collect();
    for plan in &final_plans {
        if matches!(cli.untranslatable, UntranslatableMode::Error)
            && (plan.errors > 0 || plan.warnings > 0)
        {
            strict_vetoed_paths.insert(normalized(&plan.file));
            continue;
        }
        match migrate::propose_migration(&plan.source, &plan.coverage) {
            MigrationProposal::Untouched => {}
            MigrationProposal::Delete => {
                final_map.remove(&plan.file);
            }
            MigrationProposal::Rewrite(text) => {
                final_map.insert(plan.file.clone(), text);
            }
        }
    }
    let generated_java: Vec<GeneratedJava> = cumulative
        .into_iter()
        .map(|(path, (origin, source, name))| GeneratedJava {
            path,
            origin,
            name,
            source,
        })
        .collect();
    let migration_proposals = sources
        .iter()
        .map(|(path, original)| {
            let proposal = if strict_vetoed_paths.contains(&normalized(path)) {
                MigrationProposal::Untouched
            } else {
                match final_map.get(path) {
                    None => MigrationProposal::Delete,
                    Some(final_text) if final_text == original => MigrationProposal::Untouched,
                    Some(final_text) => MigrationProposal::Rewrite(final_text.clone()),
                }
            };
            (path.clone(), proposal)
        })
        .collect::<HashMap<_, _>>();
    let mut source_edits = Vec::new();
    for (path, original) in &sources {
        let final_text = final_map.get(path).map(String::as_str).unwrap_or("");
        if final_text != original {
            source_edits.push(crate::translation_plan::PlannedSourceEdit {
                location: crate::semantics::SourceLocation {
                    file: path.clone(),
                    snapshot_hash: *blake3::hash(original.as_bytes()).as_bytes(),
                    start_byte: 0,
                    end_byte: original.len(),
                },
                replacement: final_text.to_owned(),
                speculative: false,
            });
        }
    }
    for edit in &source_edits {
        if let Some(aggregate) = translation_plans.get_mut(&edit.location.file) {
            aggregate.source_edits = vec![edit.clone()];
        }
    }
    let mut original_snapshots = sources
        .iter()
        .map(|(p, s)| (p.clone(), Some(s.as_bytes().to_vec())))
        .collect::<Vec<_>>();
    for file in index.kotlin_files() {
        let bytes = file.source_text().as_bytes().to_vec();
        original_snapshots.push((file.path.clone(), Some(bytes)));
    }
    for file in index.java_files() {
        let bytes = file.source_text().as_bytes().to_vec();
        original_snapshots.push((file.path.clone(), Some(bytes)));
    }
    if cli.out_dir.is_none() {
        for (source, _) in &sources {
            let Some(directory) = source.parent() else {
                continue;
            };
            let source_canon = std::fs::canonicalize(source).unwrap_or_else(|_| source.clone());
            let source_marker = source_canon.to_string_lossy().replace('\\', "/");
            let emitted = generated_java
                .iter()
                .filter(|g| g.origin == *source)
                .map(|g| g.name.as_str())
                .collect::<HashSet<_>>();
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("java")
                    || path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| emitted.contains(n))
                {
                    continue;
                }
                let owned = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| text.lines().next().map(str::to_owned))
                    .is_some_and(|line| {
                        line.contains("NOTLIN: generated from") && line.contains(&source_marker)
                    });
                if owned {
                    let bytes = std::fs::read(&path)
                        .map_err(|e| format!("{}: {e}", crate::paths::display(&path)))?;
                    original_snapshots.push((path, Some(bytes)));
                }
            }
        }
    }
    let mut snapshot_seen: HashSet<PathBuf> = HashSet::new();
    original_snapshots.retain(|(p, _)| snapshot_seen.insert(normalized(p)));
    for generated in &generated_java {
        if snapshot_seen.insert(normalized(&generated.path)) {
            let snapshot = match std::fs::read(&generated.path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(format!(
                        "{}: {error}",
                        crate::paths::display(&generated.path)
                    ));
                }
            };
            original_snapshots.push((generated.path.clone(), snapshot));
        }
    }
    Ok(WorkspaceMigrationPlan {
        original_sources: sources,
        original_snapshots,
        final_kotlin_sources: final_map,
        generated_java,
        final_plans,
        translation_plans,
        migration_proposals,
        source_edits,
        rounds: converged_round,
    })
}

fn profile_phase(enabled: bool, round: usize, started: &mut std::time::Instant, phase: &str) {
    if enabled {
        eprintln!(
            "NOTLIN_PROFILE round {round} {phase}: {:.3}s",
            started.elapsed().as_secs_f64()
        );
    }
    *started = std::time::Instant::now();
}

fn merge_translation_plan(
    all: &mut HashMap<PathBuf, crate::translation_plan::TranslationPlan>,
    file: &Path,
    incoming_source: &str,
    original_source: Option<&str>,
    incoming: &crate::translation_plan::TranslationPlan,
) -> Result<(), String> {
    let Some(plan) = all.get_mut(file) else {
        let mut initial = incoming.clone();
        // Intermediate edits refer to their own virtual snapshots. Only the
        // final original-to-result edit is retained below.
        initial.source_edits.clear();
        all.insert(file.to_path_buf(), initial);
        return Ok(());
    };
    for decision in &incoming.declarations {
        if let Some(old) = plan
            .declarations
            .iter_mut()
            .find(|old| old.symbol_id == decision.symbol_id)
        {
            let original_location = old.id.clone();
            *old = decision.clone();
            // A later virtual snapshot may move spans or change the content
            // hash. The aggregate decision remains anchored to its original
            // source identity while ownership and reasons are refreshed.
            old.id = original_location;
        } else {
            let mut mapped_origins = incoming
                .provenance
                .iter()
                .filter(|mapping| mapping.generated == decision.symbol_id)
                .map(|mapping| mapping.origin.clone())
                .collect::<HashSet<_>>();
            let inferred_origin = if mapped_origins.is_empty() {
                original_source.and_then(|original| {
                    unique_parser_recovery_origin(
                        &plan.declarations,
                        &decision.symbol_id,
                        original,
                        incoming_source,
                        &decision.id,
                    )
                })
            } else {
                None
            };
            if let Some((origin, reason)) = inferred_origin {
                mapped_origins.insert(origin.clone());
                let mapping = crate::semantics::OriginMap {
                    generated: decision.symbol_id.clone(),
                    origin,
                    reason,
                };
                if !plan.provenance.contains(&mapping) {
                    plan.provenance.push(mapping);
                }
            }
            if mapped_origins.len() != 1 {
                return Err(format!(
                    "declaration identity changed without an explicit unique origin mapping: {}",
                    decision.symbol_id.stable_key()
                ));
            }
            let origin = mapped_origins
                .into_iter()
                .next()
                .expect("one explicit origin");
            let Some(original) = plan
                .declarations
                .iter_mut()
                .find(|old| old.symbol_id == origin)
            else {
                return Err(format!(
                    "declaration origin is outside the original plan: {}",
                    origin.stable_key()
                ));
            };
            // Transfer current ownership to the original declaration lineage;
            // do not insert a virtual-snapshot span as if it were original.
            original.final_owner = decision.final_owner;
            original.preparation_outcome = decision.preparation_outcome;
            original.retention_reasons = decision.retention_reasons.clone();
            original.candidate_owner = decision.candidate_owner;
        }
    }
    for bridge in &incoming.bridges {
        if !plan.bridges.contains(bridge) {
            plan.bridges.push(bridge.clone());
        }
    }
    for repair in &incoming.repairs {
        if !plan.repairs.contains(repair) {
            plan.repairs.push(repair.clone());
        }
    }
    for output in &incoming.outputs {
        if let Some(old) = plan
            .outputs
            .iter()
            .find(|old| old.path == output.path && old.owner != output.owner)
        {
            return Err(format!(
                "conflicting backend ownership for generated output {} ({:?} vs {:?})",
                output.path.display(),
                old.owner,
                output.owner
            ));
        }
        if !plan.outputs.contains(output) {
            plan.outputs.push(output.clone());
        }
    }
    for origin in &incoming.provenance {
        if plan
            .provenance
            .iter()
            .any(|old| old.generated == origin.generated && old.origin != origin.origin)
        {
            return Err(format!(
                "ambiguous symbol origin mapping for {}",
                origin.generated.stable_key()
            ));
        }
        if !plan.provenance.contains(origin) {
            plan.provenance.push(origin.clone());
        }
    }
    for dependency in &incoming.dependencies {
        if !plan.dependencies.contains(dependency) {
            plan.dependencies.push(dependency.clone());
        }
    }
    for fact in &incoming.type_facts {
        if !plan.type_facts.contains(fact) {
            plan.type_facts.push(fact.clone());
        }
    }
    Ok(())
}

/// Match a declaration that was obscured by Tree-sitter recovery or was
/// assigned a new anonymous offset after translated siblings were removed.
/// The key deliberately excludes byte positions and only succeeds for one
/// structural candidate in the same source, package, owner, and signature.
fn unique_parser_recovery_origin(
    original: &[crate::translation_plan::DeclarationDecision],
    generated: &crate::semantics::SymbolId,
    original_source: &str,
    incoming_source: &str,
    generated_location: &crate::translation_plan::DeclarationId,
) -> Option<(crate::semantics::SymbolId, String)> {
    let anonymous = |name: &str| {
        name.strip_prefix("<anonymous@")
            .and_then(|offset| offset.strip_suffix('>'))
            .is_some_and(|offset| {
                !offset.is_empty() && offset.bytes().all(|byte| byte.is_ascii_digit())
            })
    };
    let generated_text =
        incoming_source.get(generated_location.start_byte..generated_location.end_byte)?;
    let generated_text = generated_text.trim_end_matches(char::is_whitespace);
    let candidates = original
        .iter()
        .filter(|candidate| {
            let old = &candidate.symbol_id;
            let name_matches =
                old.name == generated.name || (anonymous(&old.name) && anonymous(&generated.name));
            let kind_matches = old.kind == generated.kind
                || (matches!(
                    old.kind.as_str(),
                    "annotation" | "class" | "interface" | "enum" | "object"
                ) && matches!(
                    generated.kind.as_str(),
                    "annotation" | "class" | "interface" | "enum" | "object"
                ));
            normalized(&old.file) == normalized(&generated.file)
                && old.module == generated.module
                && old.package == generated.package
                && old.owner_path == generated.owner_path
                && name_matches
                && kind_matches
                && old.receiver == generated.receiver
                && old.parameters == generated.parameters
                // Parser recovery can alter a node's kind or anonymous byte
                // offset without altering its source. Require the complete
                // declaration text to survive byte-for-byte before treating
                // that as identity continuity; this prevents dropped modifiers
                // (for example `annotation`) from being hidden by lineage.
                && original_source.get(candidate.id.start_byte..candidate.id.end_byte)
                    .map(|text| text.trim_end_matches(char::is_whitespace))
                    == Some(generated_text)
        })
        .map(|candidate| candidate.symbol_id.clone())
        .collect::<HashSet<_>>();
    if candidates.len() != 1 {
        return None;
    }
    let origin = candidates.into_iter().next()?;
    let reason = if origin.name != generated.name {
        "unique declaration header matched after parser recovery changed its anonymous byte offset"
    } else if origin.kind != generated.kind {
        "unique declaration header matched after parser recovery changed its declaration kind"
    } else {
        "unique declaration header matched after parser recovery changed its semantic identity"
    };
    Some((origin, reason.into()))
}

fn veto_translation_plan(
    plans: &mut HashMap<PathBuf, crate::translation_plan::TranslationPlan>,
    file: &Path,
) {
    if let Some(plan) = plans.get_mut(file) {
        veto_translation(plan);
        return;
    }
    let identity = normalized(file);
    let Some(path) = plans
        .keys()
        .find(|path| normalized(path) == identity)
        .cloned()
    else {
        return;
    };
    let Some(plan) = plans.get_mut(&path) else {
        return;
    };
    veto_translation(plan);
}

fn veto_translation(plan: &mut crate::translation_plan::TranslationPlan) {
    for decision in &mut plan.declarations {
        decision.final_owner = Some(crate::translation_plan::BackendOwner::Kotlin);
        decision.preparation_outcome = Some(crate::translation_plan::PreparationOutcome::Retained);
        let reason = crate::translation_plan::RetentionReason::PreparationBlocker {
            kind: crate::translation_plan::PreparationBlockerKind::CompatibilityRule,
            message: "strict mode vetoed this source because it has diagnostics; no outputs were accepted".into(),
        };
        if !decision.retention_reasons.contains(&reason) {
            decision.retention_reasons.push(reason);
        }
    }
    plan.outputs.clear();
    plan.bridges.clear();
    plan.repairs.clear();
    plan.source_edits.clear();
}

fn merge_property_repairs(
    plans: &mut HashMap<PathBuf, crate::translation_plan::TranslationPlan>,
    report: crate::property_abi::PlannedPropertyRepairs,
    excluded_files: &HashSet<PathBuf>,
) {
    for repair in report.repairs {
        if excluded_files.contains(&normalized(&repair.target.file)) {
            continue;
        }
        if let Some(path) = plan_owner_path(plans, &repair.target)
            && let Some(plan) = plans.get_mut(&path)
            && !plan.repairs.contains(&repair)
        {
            plan.repairs.push(repair);
        }
    }
    for bridge in report.bridges {
        if excluded_files.contains(&normalized(&bridge.origin.file)) {
            continue;
        }
        let owner =
            plan_owner_path(plans, &bridge.origin).or_else(|| plan_owner_path(plans, &bridge.id));
        if let Some(path) = owner
            && let Some(plan) = plans.get_mut(&path)
            && !plan.bridges.contains(&bridge)
        {
            plan.bridges.push(bridge);
        }
    }
    for origin in report.provenance {
        if excluded_files.contains(&normalized(&origin.origin.file)) {
            continue;
        }
        let owner = plan_owner_path(plans, &origin.origin)
            .or_else(|| plan_owner_path(plans, &origin.generated));
        if let Some(path) = owner
            && let Some(plan) = plans.get_mut(&path)
            && !plan.provenance.contains(&origin)
        {
            plan.provenance.push(origin);
        }
    }
}

fn plan_owner_path(
    plans: &HashMap<PathBuf, crate::translation_plan::TranslationPlan>,
    id: &crate::semantics::SymbolId,
) -> Option<PathBuf> {
    if let Some((path, _)) = plans.get_key_value(&id.file) {
        return Some(path.clone());
    }
    let identity = normalized(&id.file);
    plans
        .keys()
        .find(|path| normalized(path) == identity)
        .cloned()
}

/// Refuse to apply a plan if any source read during planning changed.
pub fn verify_original_snapshots(plan: &WorkspaceMigrationPlan) -> Result<(), String> {
    let profile = std::env::var_os("NOTLIN_PROFILE").is_some();
    let started = std::time::Instant::now();
    let result = verify_original_snapshots_inner(plan);
    if profile {
        eprintln!(
            "NOTLIN_PROFILE snapshot verification: {:.3}s",
            started.elapsed().as_secs_f64()
        );
    }
    result
}

fn verify_original_snapshots_inner(plan: &WorkspaceMigrationPlan) -> Result<(), String> {
    for (path, expected) in &plan.original_snapshots {
        let current = match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "cannot verify source snapshot {}: {error}",
                    crate::paths::display(path)
                ));
            }
        };
        if &current != expected {
            return Err(format!(
                "source changed since planning: {}",
                crate::paths::display(path)
            ));
        }
    }
    // Snapshot paths and edit paths can use different spellings of the same
    // file. Normalize each snapshot once and retain the first one, matching
    // the old ordered `.find` behavior when aliases occur more than once.
    let mut snapshots_by_identity = HashMap::with_capacity(plan.original_snapshots.len());
    for (path, snapshot) in &plan.original_snapshots {
        snapshots_by_identity
            .entry(normalized(path))
            .or_insert(snapshot.as_deref());
    }
    let mut edit_ranges = HashMap::<PathBuf, Vec<(usize, usize)>>::new();
    let mut snapshot_hashes = HashMap::<PathBuf, [u8; 32]>::new();
    for edit in &plan.source_edits {
        let identity = normalized(&edit.location.file);
        let Some(Some(bytes)) = snapshots_by_identity.get(&identity).copied() else {
            return Err(format!(
                "source edit has no original snapshot: {}",
                edit.location.file.display()
            ));
        };
        let snapshot_hash = *snapshot_hashes
            .entry(identity.clone())
            .or_insert_with(|| *blake3::hash(bytes).as_bytes());
        if snapshot_hash != edit.location.snapshot_hash {
            return Err(format!(
                "source edit snapshot hash mismatch: {}",
                edit.location.file.display()
            ));
        }
        if edit.location.start_byte > edit.location.end_byte || edit.location.end_byte > bytes.len()
        {
            return Err(format!(
                "source edit range is invalid: {}",
                edit.location.file.display()
            ));
        }
        edit_ranges
            .entry(identity)
            .or_default()
            .push((edit.location.start_byte, edit.location.end_byte));
    }
    for (file, ranges) in &mut edit_ranges {
        ranges.sort_unstable();
        for pair in ranges.windows(2) {
            if pair[1].0 < pair[0].1 || pair[1].0 == pair[0].0 {
                return Err(format!(
                    "overlapping planned source edits: {}",
                    crate::paths::display(file)
                ));
            }
        }
    }
    Ok(())
}

/// Compile the whole planned source set in an isolated temporary workspace.
/// Files named by the validation config outside the migrated workspace are
/// copied into the stage as additional module sources. No runtime is launched.
pub fn validate_workspace_plan(
    plan: &WorkspaceMigrationPlan,
    indexed_kotlin: &[PathBuf],
    indexed_java: &[PathBuf],
    config_path: &Path,
) -> Result<(), String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
    let stage =
        std::env::temp_dir().join(format!("notlin-validation-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&stage)
        .map_err(|e| format!("validation staging directory {}: {e}", stage.display()))?;
    let result = (|| {
        let config =
            crate::jvm_validation::load_validation_config(config_path, stage.join("classes"))?;
        let source_root = stage.join("sources");
        std::fs::create_dir_all(&source_root)
            .map_err(|e| format!("{}: {e}", source_root.display()))?;
        let mut kotlin = Vec::new();
        let mut java = Vec::new();
        let input_paths: HashSet<PathBuf> = plan
            .original_sources
            .iter()
            .map(|(p, _)| normalized(p))
            .collect();
        let generated_paths: HashSet<PathBuf> = plan
            .generated_java
            .iter()
            .map(|g| normalized(&g.path))
            .collect();
        let indexed_source_paths: HashSet<PathBuf> = indexed_kotlin
            .iter()
            .chain(indexed_java.iter())
            .map(|p| normalized(p))
            .collect();
        let mut config_source_snapshots = Vec::<(PathBuf, Vec<u8>)>::new();
        for path in config
            .kotlin_sources
            .iter()
            .chain(config.java_sources.iter())
        {
            let key = normalized(path);
            if input_paths.contains(&key)
                || generated_paths.contains(&key)
                || indexed_source_paths.contains(&key)
            {
                continue;
            }
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            config_source_snapshots.push((path.clone(), bytes));
        }
        let mut serial = 0usize;
        let mut stage_text =
            |original: &Path, text: &str, extension: &str| -> Result<PathBuf, String> {
                serial += 1;
                let unit_dir = source_root.join(format!("unit_{serial}"));
                std::fs::create_dir_all(&unit_dir)
                    .map_err(|e| format!("{}: {e}", unit_dir.display()))?;
                let fallback = if extension == "kt" {
                    "Source.kt"
                } else {
                    "Source.java"
                };
                let basename = original
                    .file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new(fallback));
                let target = unit_dir.join(basename);
                std::fs::write(&target, text).map_err(|e| format!("{}: {e}", target.display()))?;
                Ok(target)
            };
        for (path, _) in &plan.original_sources {
            if let Some(text) = plan.final_kotlin_sources.get(path) {
                kotlin.push(stage_text(path, text, "kt")?);
            }
        }
        let snapshot_text = |path: &Path| -> Result<String, String> {
            let snapshot = plan
                .original_snapshots
                .iter()
                .find(|(candidate, _)| normalized(candidate) == normalized(path))
                .and_then(|(_, contents)| contents.as_ref())
                .ok_or_else(|| {
                    format!(
                        "validation source has no planning snapshot: {}",
                        path.display()
                    )
                })?;
            String::from_utf8(snapshot.clone())
                .map_err(|e| format!("{} is not UTF-8: {e}", path.display()))
        };
        let indexed_kotlin_paths: HashSet<PathBuf> =
            indexed_kotlin.iter().map(|p| normalized(p)).collect();
        for path in indexed_kotlin {
            let key = normalized(path);
            if input_paths.contains(&key) {
                continue;
            }
            let text = snapshot_text(path)?;
            kotlin.push(stage_text(path, &text, "kt")?);
        }
        for generated in &plan.generated_java {
            java.push(stage_text(&generated.path, &generated.source, "java")?);
        }
        // Preserve Java sources indexed from the workspace unless a generated
        // output replaces that exact path.
        let indexed_paths = indexed_java
            .iter()
            .map(|p| normalized(p))
            .collect::<HashSet<_>>();
        for path in indexed_java {
            let key = normalized(path);
            if generated_paths.contains(&key) {
                continue;
            }
            let text = snapshot_text(path)?;
            java.push(stage_text(path, &text, "java")?);
        }
        // Explicit module sources can include supporting sources outside the
        // indexed workspace. Avoid adding a stale on-disk copy of migrated or
        // generated files; their planned versions above are authoritative.
        for (paths, targets, ext) in [
            (&config.kotlin_sources, &mut kotlin, "kt"),
            (&config.java_sources, &mut java, "java"),
        ] {
            for path in paths {
                let key = normalized(path);
                if input_paths.contains(&key)
                    || generated_paths.contains(&key)
                    || indexed_paths.contains(&key)
                    || indexed_kotlin_paths.contains(&key)
                {
                    continue;
                }
                let text = captured_config_source_text(&config_source_snapshots, path)?;
                targets.push(stage_text(path, &text, ext)?);
            }
        }
        let mut staged_config = config;
        staged_config.kotlin_sources = kotlin;
        staged_config.java_sources = java;
        staged_config.run_main_class = None;
        crate::jvm_validation::validate_jvm_sources(&staged_config).map_err(|e| e.to_string())?;
        for (path, expected) in &config_source_snapshots {
            let current = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            if &current != expected {
                return Err(format!(
                    "validation source changed during JVM validation: {}",
                    path.display()
                ));
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

fn captured_config_source_text(
    snapshots: &[(PathBuf, Vec<u8>)],
    path: &Path,
) -> Result<String, String> {
    let bytes = snapshots
        .iter()
        .find(|(candidate, _)| normalized(candidate) == normalized(path))
        .map(|(_, bytes)| bytes)
        .ok_or_else(|| {
            format!(
                "validation source has no captured snapshot: {}",
                path.display()
            )
        })?;
    String::from_utf8(bytes.clone()).map_err(|e| format!("{} is not UTF-8: {e}", path.display()))
}

fn normalized(path: &Path) -> PathBuf {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().to_ascii_lowercase())
    } else {
        path
    }
}

fn speculative_overlays(
    original: &[(PathBuf, String)],
    current: &[(PathBuf, String)],
    generated: &HashMap<PathBuf, (PathBuf, String, String)>,
) -> Vec<SourceOverlay> {
    let original_by_path = original
        .iter()
        .map(|(p, s)| (p, s))
        .collect::<HashMap<_, _>>();
    let current_paths = current.iter().map(|(p, _)| p).collect::<HashSet<_>>();
    let mut overlays = Vec::with_capacity(original.len() + generated.len());
    overlays.extend(
        original
            .iter()
            .filter(|(p, _)| !current_paths.contains(p))
            .map(|(p, _)| SourceOverlay::Delete { path: p.clone() }),
    );
    overlays.extend(
        current
            .iter()
            .filter(|(p, s)| original_by_path.get(p).is_none_or(|old| *old != s))
            .map(|(p, s)| SourceOverlay::Replace {
                path: p.clone(),
                language: SourceLanguage::Kotlin,
                source: s.clone(),
            }),
    );
    overlays.extend(
        generated
            .iter()
            .map(|(p, (_, s, _))| SourceOverlay::Replace {
                path: p.clone(),
                language: SourceLanguage::Java,
                source: s.clone(),
            }),
    );
    overlays
}

fn log_speculative_changes(
    round: usize,
    before_kotlin: &[(PathBuf, String)],
    after_kotlin: &[(PathBuf, String)],
    before_java: &HashMap<PathBuf, (PathBuf, String, String)>,
    after_java: &HashMap<PathBuf, (PathBuf, String, String)>,
    verbose: bool,
) {
    let before_kotlin: HashMap<_, _> = before_kotlin
        .iter()
        .map(|(p, s)| (p, s.as_bytes()))
        .collect();
    let after_kotlin: HashMap<_, _> = after_kotlin
        .iter()
        .map(|(p, s)| (p, s.as_bytes()))
        .collect();
    let (mut ka, mut kr, mut kc) = (Vec::new(), Vec::new(), Vec::new());
    for (p, s) in &after_kotlin {
        match before_kotlin.get(p) {
            None => ka.push((*p).clone()),
            Some(old) if *old != *s => kc.push((*p).clone()),
            _ => {}
        }
    }
    for p in before_kotlin.keys() {
        if !after_kotlin.contains_key(p) {
            kr.push((*p).clone());
        }
    }
    let (mut ja, mut jr, mut jc) = (Vec::new(), Vec::new(), Vec::new());
    for (p, (_, s, _)) in after_java {
        match before_java.get(p) {
            None => ja.push(p.clone()),
            Some((_, old, _)) if old != s => jc.push(p.clone()),
            _ => {}
        }
    }
    for p in before_java.keys() {
        if !after_java.contains_key(p) {
            jr.push(p.clone());
        }
    }
    eprintln!(
        "NOTLIN_PROFILE speculative round {round}: Kotlin added={} removed={} byte-changed={}; cumulative Java added={} removed={} content-changed={}",
        ka.len(),
        kr.len(),
        kc.len(),
        ja.len(),
        jr.len(),
        jc.len()
    );
    if verbose {
        for (category, mut paths) in [
            ("Kotlin added", ka),
            ("Kotlin removed", kr),
            ("Kotlin byte-changed", kc),
            ("Java added", ja),
            ("Java removed", jr),
            ("Java content-changed", jc),
        ] {
            paths.sort();
            let samples = paths
                .iter()
                .take(20)
                .map(|p| crate::paths::display(p).to_string())
                .collect::<Vec<_>>();
            eprintln!(
                "NOTLIN_PROFILE {category} samples: [{}]",
                samples.join(", ")
            );
        }
    }
}

#[cfg(test)]
mod validation_snapshot_tests {
    use super::captured_config_source_text;
    use std::fs;

    #[test]
    fn config_source_staging_uses_captured_bytes_after_disk_changes() {
        let path = std::env::temp_dir().join(format!(
            "notlin-validation-snapshot-{}.kt",
            std::process::id()
        ));
        fs::write(&path, b"class Captured\n").unwrap();
        let snapshots = vec![(path.clone(), fs::read(&path).unwrap())];
        fs::write(&path, b"class Changed\n").unwrap();
        let text = captured_config_source_text(&snapshots, &path).unwrap();
        assert_eq!(text, "class Captured\n");
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod lineage_tests {
    use super::*;

    fn analyze(source: &str) -> crate::translation_plan::TranslationPlan {
        let tree = crate::transpiler::parse_tree(source);
        crate::translation_plan::analyze(source, tree.root_node(), Path::new("Definitions.kt"))
    }

    #[test]
    fn annotated_interface_lineage_survives_removed_siblings() {
        let original = "package sample\ninterface Plain\ninterface Named { val name: String }\nenum class State { READY, STOPPED }\n@Read(using = Reader::class)\n@Write(using = Writer::class)\ninterface Contract {\n val prefix: String\n val suffix: String\n fun key(): String {\n if (suffix == \"\") { return prefix }\n return \"$prefix/$suffix\".lowercase(java.util.Locale.getDefault());\n }\n}\ninterface Dated { val created: java.time.Instant? }\n";
        let residual = "package sample\n@Read(using = Reader::class)\n@Write(using = Writer::class)\ninterface Contract {\n val prefix: String\n val suffix: String\n fun key(): String {\n if (suffix == \"\") { return prefix }\n return \"$prefix/$suffix\".lowercase(java.util.Locale.getDefault());\n }\n}\n";
        let mut all = HashMap::new();
        let initial = analyze(original);
        let incoming = analyze(residual);
        let original_contract = initial
            .declarations
            .iter()
            .find(|d| d.symbol_id.name == "Contract")
            .expect("annotated interface has a stable named identity");
        assert_eq!(original_contract.symbol_id.kind, "interface");
        assert_eq!(
            incoming.declarations[0].symbol_id,
            original_contract.symbol_id
        );
        merge_translation_plan(
            &mut all,
            Path::new("Definitions.kt"),
            original,
            Some(original),
            &initial,
        )
        .unwrap();
        merge_translation_plan(
            &mut all,
            Path::new("Definitions.kt"),
            residual,
            Some(original),
            &incoming,
        )
        .unwrap();
        let repaired = residual.replace("val prefix: String", "fun getPrefix(): String");
        merge_translation_plan(
            &mut all,
            Path::new("Definitions.kt"),
            &repaired,
            Some(original),
            &analyze(&repaired),
        )
        .unwrap();
        let aggregate = &all[Path::new("Definitions.kt")];
        let contract = aggregate
            .declarations
            .iter()
            .find(|d| d.symbol_id.name == "Contract")
            .unwrap();
        assert_eq!(contract.id, original_contract.id);
    }

    #[test]
    fn genuine_declaration_kind_changes_still_require_an_origin_mapping() {
        let original = "package sample\ninterface Contract\n";
        let changed = "package sample\nclass Contract\n";
        let mut all = HashMap::new();
        merge_translation_plan(
            &mut all,
            Path::new("Definitions.kt"),
            original,
            Some(original),
            &analyze(original),
        )
        .unwrap();
        let error = merge_translation_plan(
            &mut all,
            Path::new("Definitions.kt"),
            changed,
            Some(original),
            &analyze(changed),
        )
        .unwrap_err();
        assert!(error.contains("explicit unique origin mapping"), "{error}");
    }
}
