//! Coordinate complete getter families before the ordinary migration fixpoint.
//! Original compilation establishes caller bindings; all source changes remain
//! virtual until the workspace plan passes its usual validation and snapshots.
use super::{LayoutPlan, discover, plan_java_callers, plan_layout};
use crate::cli::Cli;
use crate::java_bindings::{self, JavaAnalysisContext, JavaBindings};
use crate::migration_pipeline::WorkspaceMigrationPlan;
use crate::workspace::{SourceIndex, SourceLanguage, SourceOverlay};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

mod original;

pub(crate) struct Preparation {
    pub overlays: Vec<SourceOverlay>,
    bindings: Vec<JavaBindings>,
    snapshots: Vec<(PathBuf, Option<Vec<u8>>)>,
    families: Vec<(Vec<crate::semantics::SymbolId>, LayoutPlan)>,
}

fn identity(path: &Path) -> PathBuf {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path
    }
}

pub(crate) fn prepare(
    sources: &[(PathBuf, String)],
    cli: &Cli,
    index: &SourceIndex,
    progress: &mut impl FnMut(String),
) -> Result<Option<Preparation>, String> {
    if !cli.migrates_in_place() || cli.out_dir.is_some() {
        return Ok(None);
    }
    let selected = sources
        .iter()
        .map(|(path, _)| identity(path))
        .collect::<HashSet<_>>();
    let families = discover(index)
        .into_iter()
        .filter_map(|family| match plan_layout(index, &family) {
            Ok(layout)
                if layout.edits.iter().all(|edit| {
                    index.source_file(&edit.location.file).is_some_and(|file| {
                        file.language == SourceLanguage::Java
                            || selected.contains(&identity(&file.path))
                    })
                }) =>
            {
                Some((family, layout))
            }
            Ok(_) => None,
            Err(reason) => {
                log::debug!(
                    "getter family {} retained: {reason}",
                    family.root.stable_key()
                );
                None
            }
        })
        .collect::<Vec<_>>();
    if families.is_empty() {
        return Ok(None);
    }
    let mut roots = Vec::new();
    let mut names = Vec::new();
    for (family, _) in &families {
        let mut parts = family.root.owner_path.clone();
        parts.push(family.root.name.clone());
        roots.push(format!(
            "{}{}{}",
            family.root.package,
            if family.root.package.is_empty() {
                ""
            } else {
                "."
            },
            parts.join(".")
        ));
        names.push(family.original_getter.clone());
    }
    roots.sort();
    roots.dedup();
    names.sort();
    names.dedup();
    let has_java = index.java_files().next().is_some();
    let original = if has_java {
        progress("compiling original sources for getter-family Java bindings".into());
        match original::prepare(cli, index, &roots, progress)? {
            Some(original) => Some(original),
            None => {
                progress(
                    "retaining getter families: no supported original compilation context".into(),
                );
                return Ok(None);
            }
        }
    } else {
        None
    };
    let analysis_index = original
        .as_ref()
        .and_then(|original| original.analysis_index.as_ref())
        .unwrap_or(index);
    let mut bindings = Vec::new();
    if let Some(original) = &original {
        for (number, (paths, context)) in original.contexts.iter().enumerate() {
            progress(format!(
                "binding original Java getters: context {} of {}",
                number + 1,
                original.contexts.len()
            ));
            bindings.push(java_bindings::collect(
                analysis_index,
                paths,
                &names,
                context,
                &original.stage.join(format!("java-{number}")),
            )?);
        }
    }
    let mut layouts = Vec::new();
    let expanded = original
        .as_ref()
        .is_some_and(|original| original.analysis_index.is_some())
        .then(|| discover(analysis_index));
    for (family, layout) in families {
        let (family, mut layout) = if let Some(expanded) = &expanded {
            let Some(expanded) = expanded.iter().find(|expanded| {
                expanded.root == family.root && expanded.property == family.property
            }) else {
                continue;
            };
            let Ok(layout) = plan_layout(analysis_index, expanded) else {
                continue;
            };
            (expanded, layout)
        } else {
            (&family, layout)
        };
        let callers = plan_java_callers(analysis_index, family, &bindings)?;
        if !callers.unresolved_calls.is_empty() {
            progress(format!(
                "retaining getter family {}: {} unresolved Java calls",
                family.root.name,
                callers.unresolved_calls.len()
            ));
            continue;
        }
        layout.edits.extend(callers.edits);
        // Compiler-config supporting sources are context, not migration inputs.
        // A family requiring their rewrite must stay on its original layout.
        if layout
            .edits
            .iter()
            .any(|edit| index.source_file(&edit.location.file).is_none())
        {
            progress(format!(
                "retaining getter family {}: a supporting source outside the migration workspace needs editing",
                family.root.name
            ));
            continue;
        }
        progress(format!(
            "normalizing {}.{}: {} property declarations",
            family.root.name,
            family.property,
            family.property_owners.len()
        ));
        layouts.push((family.descendants.clone(), layout));
    }
    if layouts.is_empty() {
        return Ok(None);
    }
    let overlays = combined_overlays(&layouts, index)?;
    Ok(Some(Preparation {
        overlays,
        bindings,
        snapshots: original.map_or_else(Vec::new, |original| original.snapshots),
        families: layouts,
    }))
}

impl Preparation {
    pub fn retain_used_families(
        &mut self,
        plan: &WorkspaceMigrationPlan,
        index: &SourceIndex,
    ) -> Result<bool, String> {
        let owners = plan
            .generated_java
            .iter()
            .flat_map(|output| &output.owners)
            .collect::<HashSet<_>>();
        let before = self.families.len();
        self.families.retain(|(descendants, _)| {
            descendants.iter().any(|descendant| {
                owners.iter().any(|owner| {
                    **owner == *descendant
                        || (!descendant.owner_path.is_empty()
                            && owner.file == descendant.file
                            && owner.owner_path.is_empty()
                            && descendant.owner_path.first() == Some(&owner.name))
                })
            })
        });
        if self.families.len() == before {
            return Ok(false);
        }
        self.overlays = combined_overlays(&self.families, index)?;
        Ok(true)
    }

    pub fn verify_strict_vetoes(
        &self,
        plan: &WorkspaceMigrationPlan,
        strict: bool,
    ) -> Result<(), String> {
        if !strict {
            return Ok(());
        }
        let prepared = self
            .overlays
            .iter()
            .filter_map(|overlay| match overlay {
                SourceOverlay::Replace {
                    path,
                    language: SourceLanguage::Kotlin,
                    ..
                } => Some(identity(path)),
                _ => None,
            })
            .collect::<HashSet<_>>();
        if plan.final_plans.iter().any(|file| {
            prepared.contains(&identity(&file.file)) && (file.errors > 0 || file.warnings > 0)
        }) {
            return Err("getter-family preparation intersects a strict-mode source veto; no migration was applied".into());
        }
        Ok(())
    }
    pub fn kotlin_sources(&self, sources: &[(PathBuf, String)]) -> Vec<(PathBuf, String)> {
        let changed = self
            .overlays
            .iter()
            .filter_map(|overlay| match overlay {
                SourceOverlay::Replace {
                    path,
                    source,
                    language: SourceLanguage::Kotlin,
                } => Some((identity(path), source)),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        sources
            .iter()
            .map(|(path, source)| {
                (
                    path.clone(),
                    changed
                        .get(&identity(path))
                        .map_or(source, |source| *source)
                        .clone(),
                )
            })
            .collect()
    }

    /// Restore original ownership locations and original-to-final proposals.
    /// The preparation adds members inside existing ownership boundaries only.
    pub fn finish(
        self,
        plan: &mut WorkspaceMigrationPlan,
        sources: Vec<(PathBuf, String)>,
        index: &SourceIndex,
        strict: bool,
    ) -> Result<(), String> {
        self.verify_strict_vetoes(plan, strict)?;
        let originals = index
            .kotlin_files()
            .chain(index.java_files())
            .map(|file| {
                (
                    identity(&file.path),
                    (
                        file.path.clone(),
                        Some(file.source_text().as_bytes().to_vec()),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for (path, contents) in &mut plan.original_snapshots {
            if let Some((_, original)) = originals.get(&identity(path)) {
                *contents = original.clone();
            }
        }
        plan.original_snapshots.extend(self.snapshots);
        let generated = plan
            .generated_java
            .iter()
            .map(|output| identity(&output.path))
            .collect::<HashSet<_>>();
        for overlay in self.overlays {
            if let SourceOverlay::Replace {
                path,
                source,
                language: SourceLanguage::Java,
            } = overlay
                && !generated.contains(&identity(&path))
            {
                plan.supporting_java.insert(path, source);
            }
        }
        plan.source_edits.clear();
        plan.migration_proposals.clear();
        for (path, original) in &sources {
            let final_text = plan.final_kotlin_sources.get(path);
            let proposal = match final_text {
                None => crate::migrate::MigrationProposal::Delete,
                Some(text) if text == original => crate::migrate::MigrationProposal::Untouched,
                Some(text) => crate::migrate::MigrationProposal::Rewrite(text.clone()),
            };
            plan.migration_proposals.insert(path.clone(), proposal);
            if let Some(aggregate) = plan.translation_plans.get_mut(path) {
                let tree = crate::transpiler::parse_tree(original);
                let indexed = index
                    .source_file(path)
                    .map_or(path.as_path(), |file| file.path.as_path());
                let original_plan =
                    crate::translation_plan::analyze(original, tree.root_node(), indexed);
                if original_plan.declarations.len() != aggregate.declarations.len() {
                    return Err(format!(
                        "getter preparation changed original ownership boundaries: {}",
                        path.display()
                    ));
                }
                for decision in &mut aggregate.declarations {
                    let initial = original_plan
                        .declarations
                        .iter()
                        .find(|old| old.symbol_id == decision.symbol_id)
                        .ok_or("getter preparation changed an original ownership identity")?;
                    decision.id = initial.id.clone();
                }
                aggregate.source_hash = original_plan.source_hash;
                aggregate.source_edits.clear();
            }
            add_final_edit(
                plan,
                path,
                original,
                final_text.cloned().unwrap_or_default(),
            );
        }
        let supporting = plan.supporting_java.clone();
        for (path, final_text) in supporting {
            let original = index
                .source_file(&path)
                .ok_or("supporting Java original missing")?;
            add_final_edit(plan, &path, original.source_text(), final_text);
        }
        plan.original_sources = sources;
        plan.java_bindings = self.bindings;
        Ok(())
    }
}

fn combined_overlays(
    families: &[(Vec<crate::semantics::SymbolId>, LayoutPlan)],
    index: &SourceIndex,
) -> Result<Vec<SourceOverlay>, String> {
    let Some((_, first)) = families.first() else {
        return Ok(Vec::new());
    };
    let mut combined = LayoutPlan {
        root: first.root.clone(),
        edits: Vec::new(),
        bean_views: Vec::new(),
        java_adapters: Vec::new(),
        caller_migration_required: true,
    };
    for (_, family) in families {
        for edit in &family.edits {
            if !combined.edits.contains(edit) {
                combined.edits.push(edit.clone());
            }
        }
    }
    combined.overlays(index)
}

fn add_final_edit(
    plan: &mut WorkspaceMigrationPlan,
    path: &Path,
    original: &str,
    final_text: String,
) {
    if original == final_text {
        return;
    }
    let edit = crate::translation_plan::PlannedSourceEdit {
        location: crate::semantics::SourceLocation {
            file: path.to_owned(),
            snapshot_hash: *blake3::hash(original.as_bytes()).as_bytes(),
            start_byte: 0,
            end_byte: original.len(),
        },
        replacement: final_text,
        speculative: false,
    };
    if let Some(aggregate) = plan.translation_plans.get_mut(path) {
        aggregate.source_edits = vec![edit.clone()];
    }
    plan.source_edits.push(edit);
}
