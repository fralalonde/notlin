//! Stable helper identities and collision-free Java spellings, chosen before emission.

use crate::semantics::{EagerCollectionOperation, SemanticProvider, SymbolId};
use crate::translation_plan::{PlannedEagerCollectionHelper, TranslationPlan};
use std::collections::BTreeSet;
use tree_sitter::Node;

pub(super) fn plan_helpers(
    source: &str,
    root: Node<'_>,
    provider: &dyn SemanticProvider,
    plan: &mut TranslationPlan,
) {
    if plan.eager_collection_calls.is_empty()
        && !plan
            .container_calls
            .iter()
            .any(|call| call.lambda_binding.is_some())
    {
        return;
    }
    let mut names = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "identifier" | "type_identifier") {
            names.insert(node.utf8_text(source.as_bytes()).unwrap_or("").to_owned());
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    let parameter_name = loop {
        let name = fresh_name("notlinItem", &mut names, provider);
        let value = format!("{name}Value");
        if !names.contains(&value) && provider.symbols_named(&value).is_empty() {
            names.insert(value);
            break name;
        }
    };
    for fact in &mut plan.eager_collection_calls {
        if let Some(projection) = &mut fact.property_projection {
            projection.java_parameter_name = fresh_name("notlinValue", &mut names, provider);
        }
    }
    for binding in plan
        .container_calls
        .iter_mut()
        .filter_map(|call| call.lambda_binding.as_mut())
    {
        binding.java_parameter_name = fresh_name("notlinElement", &mut names, provider);
        for access in &mut binding.property_accesses {
            access.projection.java_parameter_name = binding.java_parameter_name.clone();
        }
    }
    let origins = plan
        .eager_collection_calls
        .iter()
        .map(|fact| (fact.owner.clone(), operation_kind(fact.operation)))
        .collect::<BTreeSet<_>>();
    for (origin, kind) in origins {
        let (operation, base) = if kind == "eager-collection-map" {
            (EagerCollectionOperation::Map, "notlinMap")
        } else {
            (EagerCollectionOperation::FilterNot, "notlinFilterNot")
        };
        plan.eager_collection_helpers
            .push(PlannedEagerCollectionHelper {
                id: SymbolId::generated(&origin, kind),
                origin,
                name: fresh_name(base, &mut names, provider),
                parameter_name: parameter_name.clone(),
                operation,
            });
    }
}

pub(crate) fn operation_kind(operation: EagerCollectionOperation) -> &'static str {
    match operation {
        EagerCollectionOperation::Map => "eager-collection-map",
        EagerCollectionOperation::FilterNot => "eager-collection-filter-not",
    }
}

fn fresh_name(base: &str, names: &mut BTreeSet<String>, provider: &dyn SemanticProvider) -> String {
    let mut candidate = base.to_owned();
    let mut suffix = 2;
    while names.contains(&candidate) || !provider.symbols_named(&candidate).is_empty() {
        candidate = format!("{base}{suffix}");
        suffix += 1;
    }
    names.insert(candidate.clone());
    candidate
}
