//! Small, evidence-backed source changes that may unblock Java ownership.
//! This is advisory only: it never changes eligibility, source, or ownership.
use crate::diagnostics::{RetentionKind, RetentionSite};
use crate::semantics::{FactStatus, SymbolId};
use crate::translation_plan::{BackendOwner, TranslationPlan};
use crate::transpiler::fixpoint::FilePlan;
use crate::workspace::SourceIndex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationAdvice {
    pub code: String,
    /// Declaration to edit, which can differ from the retained root.
    pub target: SymbolId,
    pub file: PathBuf,
    /// Location in the original indexed Kotlin snapshot.
    pub line: usize,
    pub title: String,
    pub evidence: Vec<String>,
    pub explanation: String,
    pub suggested_change: String,
    pub retained_roots: Vec<SymbolId>,
    /// Exact, resolved dependencies only. This is not a verified unlock count.
    pub related_retained_declarations: usize,
}

/// Inspect only the final ownership result. No name-based dependency graph or
/// uncertain semantic fact is used to generate or rank recommendations.
pub fn analyze(
    files: &[FilePlan],
    plans: &HashMap<PathBuf, TranslationPlan>,
    index: &SourceIndex,
    roots: &[PathBuf],
    sites: &[RetentionSite],
) -> Vec<TranslationAdvice> {
    let retained: HashSet<_> = plans
        .values()
        .flat_map(|plan| &plan.declarations)
        .filter(|decision| decision.final_owner == Some(BackendOwner::Kotlin))
        .map(|decision| decision.symbol_id.clone())
        .collect();
    let mut advice = BTreeMap::<(SymbolId, String), TranslationAdvice>::new();
    let mut locations = HashMap::new();
    let uncertain_symbols: HashSet<_> = plans
        .values()
        .flat_map(|plan| &plan.declarations)
        .filter(|decision| uncertain(&decision.retention_reasons))
        .map(|decision| &decision.symbol_id)
        .collect();
    for file in index.kotlin_files() {
        for declaration in &file.declarations {
            let provider = crate::semantics::workspace_symbol(index, declaration);
            if !retained.contains(&provider) {
                continue;
            }
            if uncertain_symbols.contains(&provider) {
                continue;
            }
            let Some(conflict) =
                index.default_property_getter_conflict(declaration, &retained, roots)
            else {
                continue;
            };
            if uncertain_symbols.contains(&conflict.implementation)
                || uncertain_symbols.contains(&conflict.competing_owner)
            {
                continue;
            }
            let (Some(narrow), Some(wide)) = (&conflict.provider_type, &conflict.competing_type)
            else {
                continue;
            };
            // Generic substitution, inferred returns and same-spelled contracts
            // are semantic-analysis work, not a reason to ask for source edits.
            if narrow == wide || narrow.contains(['<', '(', '?']) || wide.contains(['<', '(', '?'])
            {
                continue;
            }
            let Some(competing_file) = index.source_file(&conflict.competing_owner.file) else {
                continue;
            };
            let (Some(narrow_decl), Some(wide_decl)) = (
                index.resolve_type(file, narrow),
                index.resolve_type(competing_file, wide),
            ) else {
                continue;
            };
            if !narrow_decl.type_params.is_empty() || !wide_decl.type_params.is_empty() {
                continue;
            }
            if !proven_subtype(index, narrow_decl, wide_decl) {
                continue;
            }
            let Some(implementation_file) = index.source_file(&conflict.implementation.file) else {
                continue;
            };
            let Some(implementation) = implementation_file.declarations.iter().find(|item| {
                crate::semantics::workspace_symbol(index, item) == conflict.implementation
            }) else {
                continue;
            };
            let key = (conflict.implementation.clone(), conflict.property.clone());
            let Some(implementation_line) =
                indexed_line(index, &conflict.implementation, &mut locations)
            else {
                continue;
            };
            let Some(provider_line) = indexed_line(index, &provider, &mut locations) else {
                continue;
            };
            let entry = advice.entry(key).or_insert_with(|| TranslationAdvice {
                code: "U001".into(), target: conflict.implementation.clone(),
                file: implementation_file.path.clone(), line: implementation_line,
                title: format!("competing inherited getters for `{}`", conflict.property),
                evidence: Vec::new(),
                explanation: "The retained Kotlin implementation relies on an inherited default getter. Moving its provider to Java can remove the physical covariant getter bridge; annotation processors can then select the broader accessor. This is a mixed-language interoperability constraint, not invalid Kotlin.".into(),
                suggested_change: format!("Explicitly override `{}` on `{}` with return type `{narrow}`, delegating to the existing default implementation. Preserve its value, evaluation timing and annotations; do not add separate storage. Replan to check whether this clears the getter blocker.", conflict.property, implementation.name),
                retained_roots: Vec::new(), related_retained_declarations: 0,
            });
            entry.evidence.push(format!(
                "{}.{}: {} at {}:{}; competing {}.{}: {} at {}",
                provider.name,
                conflict.property,
                narrow,
                crate::paths::display(&file.path),
                provider_line,
                conflict.competing_owner.name,
                conflict.property,
                wide,
                crate::paths::display(&competing_file.path)
            ));
            entry.retained_roots.push(provider);
        }
    }
    // Constructor collision evidence comes from the actual overload planner,
    // matched to an exact final declaration location, not its simple name.
    const COLLISION: &str =
        "the delegating overloads its callers need collide after type erasure: ";
    for file in files {
        for decision in &file.translation.declarations {
            if decision.final_owner != Some(BackendOwner::Kotlin) {
                continue;
            }
            if uncertain(&decision.retention_reasons) {
                continue;
            }
            let Some(prefix) = file.source.get(..decision.id.start_byte) else {
                continue;
            };
            let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
            let Some(site) = sites.iter().find(|site| {
                site.file == crate::paths::display(&file.file)
                    && site.line == line
                    && site.kind == RetentionKind::MiddleDefaultParameter
                    && site.blockers.is_empty()
            }) else {
                continue;
            };
            let Some(evidence) = site
                .params
                .iter()
                .find(|message| message.starts_with(COLLISION))
            else {
                continue;
            };
            let Some(original) = index.source_file(&decision.symbol_id.file) else {
                continue;
            };
            if !known_constructor_types(index, original, &decision.symbol_id) {
                continue;
            }
            let Some(original_line) = indexed_line(index, &decision.symbol_id, &mut locations)
            else {
                continue;
            };
            let mut constructor_evidence = vec![evidence.clone()];
            constructor_evidence.extend(omitting_callers(index, &decision.symbol_id));
            advice.insert((decision.symbol_id.clone(), "constructor".into()), TranslationAdvice {
                code: "U002".into(), target: decision.symbol_id.clone(), file: original.path.clone(), line: original_line,
                title: "default-argument constructor overloads collide".into(),
                evidence: constructor_evidence,
                explanation: "Distinct Kotlin omission patterns require Java constructors with the same erased parameter signature. Java cannot distinguish them by parameter names or generic arguments.".into(),
                suggested_change: "At callers using these omission patterns, supply the omitted arguments explicitly, preserving Kotlin's default evaluation order and side effects. Alternatively introduce distinctly named factories and update those callers. Replan: removing the need for the colliding overloads may allow this class to translate.".into(),
                retained_roots: vec![decision.symbol_id.clone()], related_retained_declarations: 0,
            });
        }
    }
    let mut reverse = BTreeMap::<SymbolId, BTreeSet<SymbolId>>::new();
    for dependency in plans.values().flat_map(|plan| &plan.dependencies) {
        if retained.contains(&dependency.from)
            && let FactStatus::Established(target) = &dependency.resolution
        {
            reverse
                .entry(target.clone())
                .or_default()
                .insert(dependency.from.clone());
        }
    }
    let mut result: Vec<_> = advice.into_values().collect();
    for item in &mut result {
        item.evidence.sort();
        item.evidence.dedup();
        item.retained_roots.sort();
        item.retained_roots.dedup();
        item.related_retained_declarations = related_count(&item.retained_roots, &reverse);
    }
    result.sort_by(|a, b| {
        b.related_retained_declarations
            .cmp(&a.related_retained_declarations)
            .then(a.file.cmp(&b.file))
            .then(a.line.cmp(&b.line))
            .then(a.code.cmp(&b.code))
    });
    result
}

fn uncertain(reasons: &[crate::translation_plan::RetentionReason]) -> bool {
    reasons.iter().any(|reason| {
        matches!(
            reason,
            crate::translation_plan::RetentionReason::ParseError { .. }
                | crate::translation_plan::RetentionReason::UnknownRequiredFact { .. }
                | crate::translation_plan::RetentionReason::AmbiguousRequiredFact { .. }
        )
    })
}

fn omitting_callers(index: &SourceIndex, target: &SymbolId) -> Vec<String> {
    let Some(owner) = index.source_file(&target.file).and_then(|file| {
        file.declarations
            .iter()
            .find(|declaration| crate::semantics::workspace_symbol(index, declaration) == *target)
    }) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for file in index.kotlin_files() {
        for call in file.ctor_calls() {
            if call.callee != target.name
                || call.unknown
                || call.positional > owner.constructor_param_names.len()
            {
                continue;
            }
            let Some(resolved) = index.resolve_type(file, &call.callee) else {
                continue;
            };
            if crate::semantics::workspace_symbol(index, resolved) != *target {
                continue;
            }
            let mut supplied: HashSet<_> = (0..call.positional).collect();
            let mut known = true;
            for name in &call.named {
                if let Some(position) = owner
                    .constructor_param_names
                    .iter()
                    .position(|parameter| parameter == name)
                {
                    supplied.insert(position);
                } else {
                    known = false;
                }
            }
            if !known {
                continue;
            }
            let omitted: Vec<_> = owner
                .constructor_param_names
                .iter()
                .enumerate()
                .filter(|(position, _)| {
                    !supplied.contains(position)
                        && owner
                            .constructor_param_defaults
                            .get(*position)
                            .and_then(Option::as_deref)
                            .is_some_and(|default| {
                                !crate::ctor_defaults::is_neutral_literal(default)
                            })
                })
                .map(|(_, name)| name.as_str())
                .collect();
            if !omitted.is_empty() {
                result.push(format!(
                    "caller {}:{} omits {}",
                    crate::paths::display(&file.path),
                    call.line,
                    omitted.join(", ")
                ));
            }
        }
    }
    result
}

fn known_constructor_types(
    index: &SourceIndex,
    file: &crate::workspace::SourceFile,
    symbol: &SymbolId,
) -> bool {
    let aliases = index.type_aliases_for(&file.path);
    let tree = crate::transpiler::parse_tree(file.source_text());
    let mut pending = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        if crate::semantics::declaration_node_kind(node.kind())
            && crate::semantics::symbol_id_for_node(file.source_text(), node, &file.path) == *symbol
        {
            if node.has_error() {
                return false;
            }
            let types = crate::ctor_defaults::class_param_types(node, file.source_text());
            return !types.is_empty()
                && types.iter().all(|ty| {
                    let base = ty.split('<').next().unwrap_or(ty).trim();
                    // Nullability and aliases can change primitive boxing or the
                    // erased type; do not turn the legacy erasure heuristic into
                    // a source-change recommendation in those cases.
                    !ty.contains(['?', '(', '*'])
                        && !aliases.iter().any(|(alias, _)| alias == base)
                        && (index.resolve_type(file, base).is_some()
                            || matches!(
                                base,
                                "String"
                                    | "Int"
                                    | "Long"
                                    | "Short"
                                    | "Byte"
                                    | "Boolean"
                                    | "Char"
                                    | "Double"
                                    | "Float"
                                    | "List"
                                    | "MutableList"
                                    | "Set"
                                    | "MutableSet"
                                    | "Map"
                                    | "MutableMap"
                                    | "Collection"
                                    | "MutableCollection"
                            ))
                });
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

fn indexed_line(
    index: &SourceIndex,
    symbol: &SymbolId,
    cache: &mut HashMap<PathBuf, HashMap<SymbolId, Option<usize>>>,
) -> Option<usize> {
    if !cache.contains_key(&symbol.file) {
        let file = index.source_file(&symbol.file)?;
        let tree = crate::transpiler::parse_tree(file.source_text());
        let mut locations = HashMap::new();
        let mut pending = vec![tree.root_node()];
        while let Some(node) = pending.pop() {
            if crate::semantics::declaration_node_kind(node.kind()) {
                let id = crate::semantics::symbol_id_for_node(file.source_text(), node, &file.path);
                locations
                    .entry(id)
                    .and_modify(|line| *line = None)
                    .or_insert(Some(node.start_position().row + 1));
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        cache.insert(symbol.file.clone(), locations);
    }
    cache.get(&symbol.file)?.get(symbol).copied().flatten()
}

fn proven_subtype(
    index: &SourceIndex,
    narrow: &crate::workspace::Declaration,
    wide: &crate::workspace::Declaration,
) -> bool {
    let target = crate::semantics::workspace_symbol(index, wide);
    if crate::semantics::workspace_symbol(index, narrow) == target {
        return false;
    }
    let mut pending = vec![narrow];
    let mut seen = HashSet::new();
    while let Some(declaration) = pending.pop() {
        let symbol = crate::semantics::workspace_symbol(index, declaration);
        if symbol == target {
            return true;
        }
        if !seen.insert(symbol) {
            continue;
        }
        let Some(file) = index.declaration_source_file(declaration) else {
            continue;
        };
        for supertype in &declaration.supertypes {
            if !supertype.contains('<')
                && let Some(parent) = index.resolve_type(file, supertype)
            {
                pending.push(parent);
            }
        }
    }
    false
}

fn related_count(roots: &[SymbolId], reverse: &BTreeMap<SymbolId, BTreeSet<SymbolId>>) -> usize {
    let mut seen: BTreeSet<_> = roots.iter().cloned().collect();
    let mut pending = roots.to_vec();
    while let Some(root) = pending.pop() {
        for dependent in reverse.get(&root).into_iter().flatten() {
            if seen.insert(dependent.clone()) {
                pending.push(dependent.clone());
            }
        }
    }
    seen.len().saturating_sub(roots.len())
}

pub fn render(advice: &[TranslationAdvice], limit: usize) -> String {
    if advice.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nnotlin: targeted source changes that may unlock translation\n");
    for item in advice.iter().take(limit) {
        out.push_str(&format!(
            "\n  {} {}:{} — {}\n",
            item.code,
            crate::paths::display(&item.file),
            item.line,
            item.title
        ));
        for evidence in item.evidence.iter().take(6) {
            out.push_str(&format!("    evidence: {evidence}\n"));
        }
        if item.evidence.len() > 6 {
            out.push_str(&format!(
                "    +{} further evidence location(s) in the structured report\n",
                item.evidence.len() - 6
            ));
        }
        out.push_str(&format!(
            "    why: {}\n    suggested change: {}\n",
            item.explanation, item.suggested_change
        ));
        out.push_str(&format!("    retained roots: {}; {} other retained declaration(s) linked by resolved references\n", item.retained_roots.iter().map(|root| format!("{}.{}", root.package, root.name).trim_start_matches('.').to_owned()).collect::<Vec<_>>().join(", "), item.related_retained_declarations));
    }
    if advice.len() > limit {
        out.push_str(&format!(
            "\n  {} further recommendation(s) omitted from the console report.\n",
            advice.len() - limit
        ));
    }
    out.push_str("\n  Suggestions require manual review. Linked declarations are not a verified unlock count; replan and validate after editing. Locations refer to the input snapshots.\n");
    out
}
