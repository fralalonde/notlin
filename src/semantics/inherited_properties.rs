//! Source-graph proofs for inherited Kotlin property getter contracts.
//!
//! This module intentionally resolves only properties whose receiver ancestry,
//! source declaration, getter ABI, and value classifier are all indexed.

use super::{
    FactStatus, KotlinType, SemanticProvider, SymbolId, SyntaxSemanticProvider, TypeRef,
    kotlin_import_lines,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InheritedPropertyAccess {
    pub source_property: SymbolId,
    pub getter_contract: SymbolId,
    pub contract_owner: SymbolId,
    pub value_owner: SymbolId,
}

#[derive(Clone)]
struct PropertyFact {
    symbol: SymbolId,
    owner: SymbolId,
    value_owner: SymbolId,
    plain_getter: bool,
    readable_override: bool,
}

pub(super) fn establish(
    provider: &SyntaxSemanticProvider,
    owner: &SymbolId,
    name: &str,
) -> FactStatus<InheritedPropertyAccess> {
    if !provider
        .symbols_named(name)
        .iter()
        .any(|symbol| symbol.id.kind == "property")
    {
        return FactStatus::Unknown;
    }
    if !is_indexed_type(provider, owner) || !provider.has_complete_inheritance(owner) {
        return FactStatus::Unknown;
    }
    let Some(graph) = complete_ancestry(provider, owner) else {
        return FactStatus::Unknown;
    };
    if has_java_conflict(provider, &graph, name) {
        return FactStatus::Unknown;
    }

    let Some(mut properties) = direct_properties(provider, &graph, name) else {
        return FactStatus::Unknown;
    };
    if properties.is_empty() {
        return FactStatus::Unknown;
    }
    // Generic substitution and covariant property returns need a separate
    // proof. This capability requires one identical canonical value type.
    if properties
        .iter()
        .any(|property| property.value_owner != properties[0].value_owner)
    {
        return FactStatus::Unknown;
    }
    properties.sort_by(|left, right| left.symbol.cmp(&right.symbol));

    // A same-name declaration on a descendant only shadows an ancestor when
    // its source explicitly establishes a readable override. Do not select a
    // shallowest BFS match: sibling branches can be equally near the owner.
    for descendant in &properties {
        for ancestor in &properties {
            if descendant.owner != ancestor.owner
                && provider.inherits_type(&descendant.owner, &ancestor.owner)
                && !descendant.readable_override
            {
                return FactStatus::Unknown;
            }
        }
    }
    let maximal = properties
        .iter()
        .filter(|candidate| {
            !properties.iter().any(|other| {
                candidate.owner != other.owner
                    && provider.inherits_type(&other.owner, &candidate.owner)
                    && other.readable_override
            })
        })
        .cloned()
        .collect::<Vec<_>>();
    let visible = match maximal.as_slice() {
        [visible] => visible.clone(),
        _ => {
            // A concrete class implementation can be the unique readable
            // implementation of an interface contract even when another
            // inherited interface explicitly re-declares that same contract.
            // Accept this only when every declaration reduces to the same
            // unannotated root getter and exactly one concrete class override
            // supplies the value.
            let concrete = properties
                .iter()
                .filter(|candidate| {
                    candidate.owner.kind == "class"
                        && candidate.readable_override
                        && property_location(provider, &candidate.symbol).is_some_and(|location| {
                            provider
                                .concrete_kotlin_override_property_locations
                                .contains(&(candidate.symbol.file.clone(), location))
                        })
                })
                .cloned()
                .collect::<Vec<_>>();
            let [concrete] = concrete.as_slice() else {
                return FactStatus::Unknown;
            };
            let mut shared_root: Option<SymbolId> = None;
            for candidate in &properties {
                if candidate.value_owner != concrete.value_owner {
                    return FactStatus::Unknown;
                }
                if candidate.owner.kind != "interface"
                    && candidate.owner != concrete.owner
                    && !provider.inherits_type(&concrete.owner, &candidate.owner)
                {
                    return FactStatus::Unknown;
                }
                let roots = reduced_contracts(provider, candidate, &properties);
                let [root] = roots.as_slice() else {
                    return FactStatus::Unknown;
                };
                if shared_root
                    .as_ref()
                    .is_some_and(|existing| existing != &root.symbol)
                {
                    return FactStatus::Unknown;
                }
                shared_root = Some(root.symbol.clone());
            }
            concrete.clone()
        }
    };
    if !visible.plain_getter && !visible.readable_override {
        return FactStatus::Unknown;
    }

    let roots = reduced_contracts(provider, &visible, &properties);

    let accesses = roots
        .iter()
        .map(|contract| InheritedPropertyAccess {
            source_property: visible.symbol.clone(),
            getter_contract: contract.symbol.clone(),
            contract_owner: contract.owner.clone(),
            value_owner: visible.value_owner.clone(),
        })
        .collect::<Vec<_>>();
    match accesses.as_slice() {
        [access] => FactStatus::Established(access.clone()),
        [] => FactStatus::Unknown,
        _ => FactStatus::Ambiguous(accesses),
    }
}

fn reduced_contracts(
    provider: &SyntaxSemanticProvider,
    property: &PropertyFact,
    properties: &[PropertyFact],
) -> Vec<PropertyFact> {
    let mut contracts = BTreeMap::<SymbolId, PropertyFact>::new();
    for contract in properties {
        if contract.owner.kind != "interface"
            || !contract.plain_getter
            || contract.value_owner != property.value_owner
        {
            continue;
        }
        let compatible = property.symbol == contract.symbol
            || (property.readable_override
                && provider.inherits_type(&property.owner, &contract.owner));
        if compatible {
            contracts
                .entry(contract.symbol.clone())
                .or_insert_with(|| contract.clone());
        }
    }
    let contract_facts = contracts.values().cloned().collect::<Vec<_>>();
    contract_facts
        .iter()
        .filter(|candidate| {
            !contract_facts.iter().any(|other| {
                candidate.owner != other.owner
                    && provider.inherits_type(&candidate.owner, &other.owner)
                    && candidate.readable_override
            })
        })
        .cloned()
        .collect()
}

fn property_location(provider: &SyntaxSemanticProvider, property: &SymbolId) -> Option<usize> {
    let matches = provider
        .symbols_named(&property.name)
        .into_iter()
        .filter(|symbol| symbol.id == *property)
        .collect::<Vec<_>>();
    let [symbol] = matches.as_slice() else {
        return None;
    };
    Some(symbol.location.start_byte)
}

fn is_indexed_type<P: SemanticProvider + ?Sized>(provider: &P, owner: &SymbolId) -> bool {
    matches!(owner.kind.as_str(), "class" | "interface")
        && provider
            .symbols_named(&owner.name)
            .iter()
            .filter(|symbol| symbol.id == *owner)
            .count()
            == 1
}

fn complete_ancestry<P: SemanticProvider + ?Sized>(
    provider: &P,
    owner: &SymbolId,
) -> Option<Vec<SymbolId>> {
    fn visit<P: SemanticProvider + ?Sized>(
        provider: &P,
        current: &SymbolId,
        active: &mut BTreeSet<SymbolId>,
        visited: &mut BTreeSet<SymbolId>,
        ordered: &mut Vec<SymbolId>,
    ) -> bool {
        if active.contains(current) || !is_indexed_type(provider, current) {
            return false;
        }
        if visited.contains(current) {
            return true;
        }
        if !provider.has_complete_inheritance(current) {
            return false;
        }
        active.insert(current.clone());
        let FactStatus::Established(parents) = provider.instance_contract_parents(current) else {
            return false;
        };
        let mut unique = BTreeSet::new();
        if parents.iter().any(|parent| !unique.insert(parent.clone())) {
            return false;
        }
        for parent in parents {
            if !visit(provider, &parent, active, visited, ordered) {
                return false;
            }
        }
        active.remove(current);
        visited.insert(current.clone());
        ordered.push(current.clone());
        true
    }

    let mut active = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut ordered = Vec::new();
    visit(provider, owner, &mut active, &mut visited, &mut ordered).then_some(ordered)
}

fn has_java_conflict(
    provider: &SyntaxSemanticProvider,
    ancestry: &[SymbolId],
    property: &str,
) -> bool {
    if !property.is_ascii() {
        return true;
    }
    let mut characters = property.chars();
    let Some(first) = characters.next() else {
        return true;
    };
    let suffix = format!("{}{}", first.to_ascii_uppercase(), characters.as_str());
    let mut possible_getters = BTreeSet::from([format!("get{suffix}")]);
    if property
        .strip_prefix("is")
        .is_some_and(|rest| rest.chars().next().is_some_and(char::is_uppercase))
    {
        possible_getters.insert(property.to_owned());
    }

    for owner in ancestry {
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        if provider.java_visible_fields.contains(&(
            owner.file.clone(),
            path.clone(),
            property.to_owned(),
        )) {
            return true;
        }
        for getter in &possible_getters {
            if provider.symbols_named(getter).iter().any(|symbol| {
                symbol.id.file == owner.file
                    && symbol.id.package == owner.package
                    && symbol.id.owner_path == path
                    && matches!(symbol.id.kind.as_str(), "method" | "function")
                    && owner
                        .file
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
            }) {
                return true;
            }
        }
    }
    false
}

fn direct_properties(
    provider: &SyntaxSemanticProvider,
    ancestry: &[SymbolId],
    name: &str,
) -> Option<Vec<PropertyFact>> {
    let mut facts = Vec::new();
    for owner in ancestry {
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        let matches = provider
            .symbols_named(name)
            .into_iter()
            .filter(|symbol| {
                symbol.id.name == name
                    && symbol.id.file == owner.file
                    && symbol.id.package == owner.package
                    && symbol.id.owner_path == path
                    && symbol.id.kind == "property"
                    && symbol.id.receiver.is_none()
                    && symbol.id.file.extension().is_some_and(|extension| {
                        extension.eq_ignore_ascii_case("kt")
                            || extension.eq_ignore_ascii_case("kts")
                    })
            })
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return None;
        }
        let Some(symbol) = matches.first() else {
            continue;
        };
        let value_owner = property_value_owner(provider, &symbol.id, &symbol.ty)?;
        if !provider.has_complete_inheritance(&value_owner)
            || provider.is_jvm_value_class(&value_owner)
            || provider.is_jvm_value_class(owner)
        {
            return None;
        }
        let plain_getter = provider.has_plain_instance_property_getter(&symbol.id);
        let readable_override = provider
            .readable_kotlin_override_property_locations
            .contains(&(symbol.id.file.clone(), symbol.location.start_byte));
        facts.push(PropertyFact {
            symbol: symbol.id.clone(),
            owner: owner.clone(),
            value_owner,
            plain_getter,
            readable_override,
        });
    }
    Some(facts)
}

fn property_value_owner(
    provider: &SyntaxSemanticProvider,
    property: &SymbolId,
    ty: &FactStatus<TypeRef>,
) -> Option<SymbolId> {
    let FactStatus::Established(TypeRef {
        ty: KotlinType::Named { name, arguments },
        nullable: false,
    }) = ty
    else {
        return None;
    };
    if !arguments.is_empty() {
        return None;
    }
    let source = provider.source_for_file(&property.file)?;
    if provider.has_type_alias_in_package(&property.package, name)
        || kotlin_import_lines(source).any(|import| {
            import
                .split_once(" as ")
                .is_some_and(|(_, alias)| alias.trim() == name.as_str())
        })
    {
        return None;
    }
    let FactStatus::Established(value_owner) =
        provider.resolve_type_in_file(&property.file, &property.package, name)
    else {
        return None;
    };
    (matches!(value_owner.kind.as_str(), "class" | "interface")
        && !provider.is_jvm_value_class(&value_owner))
    .then_some(value_owner)
}
