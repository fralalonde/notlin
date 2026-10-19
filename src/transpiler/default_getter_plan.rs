use crate::semantics::SymbolId;
use crate::translation_plan::{
    BackendOwner, PlannedDefaultGetterBridge, PreparationBlockerKind, RetentionReason,
    TranslationPlan,
};
use crate::workspace::SourceIndex;
use std::collections::HashSet;
use std::path::Path;

pub(super) fn plan_default_getters(
    index: &SourceIndex,
    file: &Path,
    retained: &HashSet<SymbolId>,
    roots: &[std::path::PathBuf],
    plan: &mut TranslationPlan,
) {
    let Some(source_file) = index.source_file(file) else {
        return;
    };
    let java_providers = |id: &SymbolId| {
        id.file
            .extension()
            .is_some_and(|extension| extension == "java")
            || (index.is_selected(&id.file, roots)
                && !retained.contains(id)
                && plan
                    .declarations
                    .iter()
                    .find(|decision| &decision.symbol_id == id)
                    .is_none_or(|decision| decision.candidate_owner == BackendOwner::Java))
    };
    let mut results = Vec::new();
    for declaration in &source_file.declarations {
        let owner = crate::semantics::workspace_symbol(index, declaration);
        if !plan.declarations.iter().any(|decision| {
            decision.symbol_id == owner && decision.candidate_owner == BackendOwner::Java
        }) {
            continue;
        }
        let result = index
            .covariant_default_getter_bridges(file, &declaration.name)
            .and_then(|bridges| {
                if bridges.iter().any(|bridge| {
                    !java_providers(&bridge.provider_owner) || !java_providers(&bridge.target)
                }) {
                    Err("inherited covariant getter needs a concrete Java bridge, but its default provider remains Kotlin".to_owned())
                } else {
                    Ok(bridges)
                }
            });
        results.push((owner, result));
    }
    for (owner, result) in results {
        match result {
            Ok(bridges) => {
                for bridge in bridges {
                    let decision = plan
                        .declarations
                        .iter()
                        .find(|decision| decision.symbol_id == owner)
                        .expect("bridge owner belongs to this plan");
                    let location = crate::semantics::SourceLocation {
                        file: file.to_path_buf(),
                        snapshot_hash: plan.source_hash,
                        start_byte: decision.id.start_byte,
                        end_byte: decision.id.end_byte,
                    };
                    plan.record_generated_bridge(
                        &owner,
                        format!("covariant-default-getter:{}", bridge.method_name),
                        "preserve the concrete covariant getter seen by Java annotation processors",
                    );
                    plan.default_getter_bridges
                        .push(PlannedDefaultGetterBridge {
                            owner: bridge.owner,
                            location,
                            target: bridge.target,
                            provider: bridge.provider_owner,
                            method_name: bridge.method_name,
                            return_type: bridge.return_type,
                        });
                }
            }
            Err(message) => {
                let Some(decision) = plan
                    .declarations
                    .iter_mut()
                    .find(|decision| decision.symbol_id == owner)
                else {
                    continue;
                };
                decision.candidate_owner = BackendOwner::Kotlin;
                decision
                    .retention_reasons
                    .push(RetentionReason::PreparationBlocker {
                        kind: PreparationBlockerKind::CompatibilityRule,
                        message,
                    });
            }
        }
    }
}
