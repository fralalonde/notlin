//! Bridge retained Kotlin implementations of translated abstract-property
//! interfaces. Kotlin does not treat a Java `getX()` as an overridable Kotlin
//! `val x`; implementations need a field for Kotlin property syntax and an
//! explicit Java-style getter (and setter for `var`).

use crate::property_callsite::PropertyAccessorContract;
use crate::workspace::{
    Declaration, DeclarationKind, MemberKind, SourceFile, SourceIndex, SourceLanguage,
};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::Node;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PropertyContract {
    name: String,
    type_name: String,
    type_source: std::path::PathBuf,
    getter: String,
    setter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedPropertyContract {
    owner_file: std::path::PathBuf,
    owner_name: String,
    owner_package: Option<String>,
    owner_kind: DeclarationKind,
    contract: PropertyContract,
}

impl PersistedPropertyContract {
    pub(crate) fn owner_file(&self) -> &Path {
        &self.owner_file
    }
}

#[cfg(test)]
fn persisted_owner_matches(
    persisted: &PersistedPropertyContract,
    file: &SourceFile,
    declaration: &Declaration,
) -> bool {
    persisted.owner_file == file.path
        && persisted.owner_name == declaration.name
        && persisted.owner_package == declaration.package
        && persisted.owner_kind == declaration.kind
        // Declaration currently carries no syntax span or SymbolId. Refuse
        // to attach a persisted contract to a same-named nested declaration
        // in the same file instead of guessing which owner produced it.
        && file
            .declarations
            .iter()
            .filter(|candidate| {
                candidate.name == declaration.name
                    && candidate.package == declaration.package
                    && candidate.kind == declaration.kind
            })
            .count()
            == 1
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PersistedOwnerKey {
    file: std::path::PathBuf,
    name: String,
    package: Option<String>,
    kind: u8,
}

impl PersistedOwnerKey {
    fn new(file: &Path, name: &str, package: Option<&str>, kind: DeclarationKind) -> Self {
        Self {
            file: file.to_path_buf(),
            name: name.to_string(),
            package: package.map(str::to_string),
            kind: kind as u8,
        }
    }
}

/// Exact owner lookup for operation-derived ABI facts. Persisted contracts
/// are accumulated across rounds, so filtering the full vector at every
/// inheritance edge turns ABI repair into declarations × contracts work.
/// This index preserves the original uniqueness guard while paying for it
/// once per overlay snapshot.
struct PersistedContractIndex<'a> {
    by_owner: HashMap<PersistedOwnerKey, Vec<&'a PersistedPropertyContract>>,
}

impl<'a> PersistedContractIndex<'a> {
    fn empty() -> Self {
        Self {
            by_owner: HashMap::new(),
        }
    }

    fn new(index: &SourceIndex, contracts: &'a [PersistedPropertyContract]) -> Self {
        let mut declaration_counts = HashMap::<PersistedOwnerKey, usize>::new();
        for file in &index.files {
            for declaration in &file.declarations {
                *declaration_counts
                    .entry(PersistedOwnerKey::new(
                        &file.path,
                        &declaration.name,
                        declaration.package.as_deref(),
                        declaration.kind,
                    ))
                    .or_default() += 1;
            }
        }

        let mut by_owner = HashMap::<PersistedOwnerKey, Vec<_>>::new();
        for persisted in contracts {
            let key = PersistedOwnerKey::new(
                &persisted.owner_file,
                &persisted.owner_name,
                persisted.owner_package.as_deref(),
                persisted.owner_kind,
            );
            if declaration_counts.get(&key) == Some(&1) {
                by_owner.entry(key).or_default().push(persisted);
            }
        }
        Self { by_owner }
    }

    fn for_owner(
        &self,
        file: &SourceFile,
        declaration: &Declaration,
    ) -> &[&'a PersistedPropertyContract] {
        let key = PersistedOwnerKey::new(
            &file.path,
            &declaration.name,
            declaration.package.as_deref(),
            declaration.kind,
        );
        self.by_owner.get(&key).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Operation-derived facts for speculative Kotlin property ABI repairs.
/// These are accumulated while the exact property node is rewritten; they are
/// not reconstructed by comparing source snapshots after the fact.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlannedPropertyRepairs {
    pub count: usize,
    pub repairs: Vec<crate::translation_plan::PlannedRepair>,
    pub bridges: Vec<crate::translation_plan::PlannedBridge>,
    pub provenance: Vec<crate::semantics::OriginMap>,
    pub callsite_contracts: Vec<(std::path::PathBuf, PropertyAccessorContract)>,
    pub(crate) abi_contracts: Vec<PersistedPropertyContract>,
}

impl PlannedPropertyRepairs {
    fn append(&mut self, mut other: Self) {
        self.count += other.count;
        self.repairs.append(&mut other.repairs);
        self.bridges.append(&mut other.bridges);
        self.provenance.append(&mut other.provenance);
        self.callsite_contracts
            .append(&mut other.callsite_contracts);
        self.abi_contracts.append(&mut other.abi_contracts);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PropertyBridgeFailure {
    pub declaration: String,
    pub reason: String,
}

pub(crate) fn is_property_interface_candidate(target: &Declaration) -> bool {
    target.kind == DeclarationKind::Interface
        && target
            .members
            .iter()
            .any(|member| member.kind == MemberKind::Property)
        && target.members.iter().all(|member| {
            member.kind == MemberKind::Method
                || (member.kind == MemberKind::Property
                    && !member.is_static
                    && (!member.has_body || !member.is_mutable)
                    && !member.has_unsupported_property_shape
                    && !member.has_unsupported_property_annotations)
        })
}

/// A getter-only interface already has Java-compatible method signatures.
/// Retained Kotlin descendants can implement those methods directly without
/// converting their properties or changing a JVM descriptor.
pub(crate) fn is_getter_method_interface_candidate(target: &Declaration) -> bool {
    target.kind == DeclarationKind::Interface
        && !target.members.is_empty()
        && target.members.iter().all(|member| {
            member.kind == MemberKind::Method
                && !member.is_static
                && member.parameter_types.is_empty()
                && getter_property(&member.name).is_some()
                && member
                    .type_name
                    .as_deref()
                    .is_some_and(is_jvm_getter_return_type)
                && !member.has_unsupported_property_shape
                && !member.has_unsupported_property_annotations
        })
}

/// Return a retained Kotlin property ancestor whose inherited property shape
/// can become an invalid fake override after translating `target`. This also
/// covers memberless interfaces: a Java getter path can join an abstract
/// Kotlin property path through a diamond even when the target declares no
/// property of its own.
pub(crate) fn retained_kotlin_property_ancestor(
    index: &SourceIndex,
    target: &Declaration,
    retained: &HashSet<crate::semantics::SymbolId>,
) -> Option<String> {
    if target.kind != DeclarationKind::Interface {
        return None;
    }
    let target_file = index.declaration_source_file(target)?;
    let properties = target
        .members
        .iter()
        .filter(|member| member.kind == MemberKind::Property && !member.is_static)
        .collect::<Vec<_>>();
    let mut pending = target
        .supertypes
        .iter()
        .cloned()
        .map(|supertype| (target_file.path.clone(), supertype))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while let Some((context_path, supertype)) = pending.pop() {
        let context = index.source_file(&context_path)?;
        let Some(parent) = index.resolve_type(context, &supertype) else {
            continue;
        };
        let Some(parent_file) = index.declaration_source_file(parent) else {
            continue;
        };
        let key = format!("{}:{}", parent_file.path.display(), parent.name);
        if !visited.insert(key) {
            continue;
        }
        if parent.language == SourceLanguage::Kotlin
            && retained.contains(&crate::semantics::workspace_symbol(index, parent))
            && parent.members.iter().any(|ancestor_property| {
                if ancestor_property.kind != MemberKind::Property || ancestor_property.is_static {
                    return false;
                }
                if properties.is_empty() {
                    return true;
                }
                properties.iter().any(|property| {
                    ancestor_property.name == property.name
                        && property.type_name.as_deref().is_some_and(|child_type| {
                            ancestor_property
                                .type_name
                                .as_deref()
                                .is_some_and(|parent_type| {
                                    index.property_getter_return_compatible_in_files(
                                        &target_file.path,
                                        child_type,
                                        &parent_file.path,
                                        parent_type,
                                    )
                                })
                        })
                })
            })
        {
            return Some(parent.name.clone());
        }
        pending.extend(
            parent
                .supertypes
                .iter()
                .cloned()
                .map(|parent_supertype| (parent_file.path.clone(), parent_supertype)),
        );
    }
    None
}

/// Whether every retained Kotlin subtype can be repaired before this simple
/// property interface is translated. A missing declaration, unselected file,
/// custom accessor, or property shape we cannot rewrite keeps the old ABI.
#[cfg(test)]
pub(crate) fn retained_subtypes_repairable(
    index: &SourceIndex,
    target: &Declaration,
    retained: &HashSet<crate::semantics::SymbolId>,
    translation_roots: &[std::path::PathBuf],
) -> bool {
    retained_subtypes_bridge(index, target, retained, translation_roots).is_ok()
}

pub(crate) fn retained_subtypes_bridge(
    index: &SourceIndex,
    target: &Declaration,
    retained: &HashSet<crate::semantics::SymbolId>,
    translation_roots: &[std::path::PathBuf],
) -> Result<(), PropertyBridgeFailure> {
    let Some(properties) = source_contract_properties(index, target) else {
        return Err(bridge_rejected(
            index,
            target,
            target,
            "the root interface is not a simple property contract",
        ));
    };
    if properties.is_empty() {
        return Err(bridge_rejected(
            index,
            target,
            target,
            "the root contract is empty",
        ));
    }
    let mut visited = HashSet::new();
    retained_descendants_repairable(
        index,
        target,
        target,
        &properties,
        retained,
        translation_roots,
        &mut visited,
    )
}

fn retained_descendants_repairable(
    index: &SourceIndex,
    target: &Declaration,
    contract_owner: &Declaration,
    properties: &[PropertyContract],
    retained: &HashSet<crate::semantics::SymbolId>,
    translation_roots: &[std::path::PathBuf],
    visited: &mut HashSet<String>,
) -> Result<(), PropertyBridgeFailure> {
    if index.has_unselected_kotlin_subtype(target, translation_roots) {
        return Err(bridge_rejected(
            index,
            contract_owner,
            target,
            "a Kotlin subtype is outside the selected translation roots",
        ));
    }
    let matches = index
        .direct_subtypes(target)
        .into_iter()
        .filter(|declaration| {
            declaration.language == SourceLanguage::Kotlin
                && index.declaration_retained(declaration, retained)
        })
        .collect::<Vec<_>>();
    for subtype in matches {
        let Some(file) = index.declaration_source_file(subtype) else {
            return Err(bridge_rejected(
                index,
                contract_owner,
                subtype,
                "the retained subtype has no indexed source file",
            ));
        };
        let key = format!("{}:{}", file.path.display(), subtype.name);
        if !visited.insert(key) {
            continue;
        }
        if !index.is_selected(&file.path, translation_roots) {
            return Err(bridge_rejected(
                index,
                contract_owner,
                subtype,
                "the retained subtype is outside the selected translation roots",
            ));
        }
        let Some(subtype_properties) =
            specialize_contract_properties(index, target, subtype, properties)
        else {
            return Err(bridge_rejected(
                index,
                contract_owner,
                subtype,
                "the generic supertype arguments cannot be resolved safely",
            ));
        };
        if matches!(
            subtype.kind,
            DeclarationKind::Class | DeclarationKind::Object
        ) && subtype.members.iter().any(|member| {
            member.kind == MemberKind::Property
                && subtype_properties
                    .iter()
                    .any(|property| property.name == member.name)
                && inherits_retained_kotlin_property(index, subtype, &member.name, Some(retained))
        }) {
            return Err(bridge_rejected(
                index,
                contract_owner,
                subtype,
                "a constructor or class property also overrides a retained Kotlin property",
            ));
        }
        let covers_properties =
            subtype_covers_properties(index, contract_owner, subtype, &subtype_properties);
        let covers_or_inherits_properties = subtype_properties.iter().all(|property| {
            subtype_covers_properties(
                index,
                contract_owner,
                subtype,
                std::slice::from_ref(property),
            ) || inherited_repairable_property_methods(
                index,
                contract_owner,
                subtype,
                std::slice::from_ref(property),
            )
        });
        // A class field bridge removes virtual property dispatch. Even a
        // source class that appears final may have Java/Kotlin descendants
        // elsewhere in the workspace, so only apply this check when this
        // class actually owns the bridge. An abstract intermediate class may
        // defer the Java getter contract to its concrete descendants.
        if matches!(
            subtype.kind,
            DeclarationKind::Class | DeclarationKind::Object
        ) && covers_properties
            && index.has_any_subtype(&subtype.name)
            && !class_descendants_inherit_contracts(
                index,
                subtype,
                &subtype_properties,
                translation_roots,
                &mut HashSet::new(),
            )
        {
            return Err(bridge_rejected(
                index,
                contract_owner,
                subtype,
                "a class property bridge would be inherited by another indexed subtype",
            ));
        }
        if matches!(
            subtype.kind,
            DeclarationKind::Class | DeclarationKind::Object
        ) && !class_supertype_properties_compatible(
            index,
            contract_owner,
            subtype,
            &subtype_properties,
            translation_roots,
        ) {
            return Err(bridge_rejected(
                index,
                contract_owner,
                subtype,
                "another Kotlin supertype declares an incompatible property contract",
            ));
        }
        // A retained intermediate interface may inherit the Java getter
        // contract without redeclaring it. If it does redeclare properties,
        // the repair below converts those members to bridge functions.
        let interface_repairable = subtype.kind == DeclarationKind::Interface
            && subtype
                .members
                .iter()
                .filter(|m| m.kind == MemberKind::Property)
                .all(|member| {
                    subtype_properties
                        .iter()
                        .find(|property| property.name == member.name)
                        .is_none_or(|property| {
                            interface_member_repairable(
                                index,
                                contract_owner,
                                subtype,
                                member,
                                property,
                            )
                        })
                });
        let abstract_intermediate = subtype.kind == DeclarationKind::Class
            && subtype.is_abstract
            && !covers_properties
            && !subtype.members.iter().any(|member| {
                subtype_properties.iter().any(|property| {
                    (member.kind == MemberKind::Property && member.name == property.name)
                        || (member.kind == MemberKind::Method
                            && (member.name == property.getter
                                || property.setter.as_deref() == Some(member.name.as_str())))
                })
            });
        if !(interface_repairable || covers_or_inherits_properties) && !abstract_intermediate {
            return Err(bridge_rejected(
                index,
                contract_owner,
                subtype,
                "the property is missing or uses a shape that cannot be bridged safely",
            ));
        }
        retained_descendants_repairable(
            index,
            subtype,
            contract_owner,
            &subtype_properties,
            retained,
            translation_roots,
            visited,
        )?;
    }
    Ok(())
}

fn class_descendants_inherit_contracts(
    index: &SourceIndex,
    owner: &Declaration,
    properties: &[PropertyContract],
    translation_roots: &[std::path::PathBuf],
    visited: &mut HashSet<String>,
) -> bool {
    if index.declaration_source_file(owner).is_none() {
        return false;
    }
    let children = index.direct_subtypes(owner);
    if children.is_empty() {
        return !index.has_unresolved_supertype(owner);
    }
    children.into_iter().all(|child| {
        let Some(child_file) = index.declaration_source_file(child) else {
            return false;
        };
        let key = format!("{}:{}", child_file.path.display(), child.name);
        if !visited.insert(key) {
            return true;
        }
        child.language == SourceLanguage::Kotlin
            && index.is_selected(&child_file.path, translation_roots)
            && !child.members.iter().any(|member| {
                properties.iter().any(|property| {
                    (member.kind == MemberKind::Property && member.name == property.name)
                        || (member.kind == MemberKind::Method
                            && (member.name == property.getter
                                || property.setter.as_deref() == Some(member.name.as_str())
                                || member.name == setter_name(&property.name)))
                })
            })
            && class_descendants_inherit_contracts(
                index,
                child,
                properties,
                translation_roots,
                visited,
            )
    })
}

fn interface_member_repairable(
    index: &SourceIndex,
    contract_owner: &Declaration,
    owner: &Declaration,
    member: &crate::workspace::Member,
    property: &PropertyContract,
) -> bool {
    let setter = bridged_setter_name(property, member.is_mutable);
    property_type_matches(index, contract_owner, owner, member, property)
        && (!member.has_body || !member.is_mutable)
        && !member.has_unsupported_property_shape
        && !member.has_unsupported_property_annotations
        && (property.setter.is_none() || member.is_mutable)
        && !owner.members.iter().any(|candidate| {
            candidate.kind == MemberKind::Method
                && (candidate.name == property.getter
                    || setter.as_deref() == Some(candidate.name.as_str()))
        })
}

/// A concrete subtype need not redeclare a property when a retained Kotlin
/// interface in the same contract hierarchy supplies it. That interface is
/// repaired to an explicit Java getter, which the concrete subtype inherits.
fn inherited_repairable_property_methods(
    index: &SourceIndex,
    contract_owner: &Declaration,
    subtype: &Declaration,
    properties: &[PropertyContract],
) -> bool {
    if !matches!(
        subtype.kind,
        DeclarationKind::Class | DeclarationKind::Enum | DeclarationKind::Object
    ) {
        return false;
    }
    let Some(source_file) = index.declaration_source_file(subtype) else {
        return false;
    };
    properties.iter().all(|property| {
        let mut pending = subtype
            .supertypes
            .iter()
            .cloned()
            .map(|supertype| (source_file.path.clone(), supertype))
            .collect::<Vec<_>>();
        let mut visited = HashSet::new();
        while let Some((context_path, supertype)) = pending.pop() {
            let Some(context) = index.source_file(&context_path) else {
                return false;
            };
            let Some(parent) = index.resolve_type(context, &supertype) else {
                continue;
            };
            let Some(parent_file) = index.declaration_source_file(parent) else {
                continue;
            };
            let key = format!("{}:{}", parent_file.path.display(), parent.name);
            if !visited.insert(key) {
                continue;
            }
            if parent.language == SourceLanguage::Kotlin {
                let supplies_method = match parent.kind {
                    DeclarationKind::Interface => parent.members.iter().any(|member| {
                        member.kind == MemberKind::Property
                            && member.name == property.name
                            && member.has_body
                            && interface_member_repairable(
                                index,
                                contract_owner,
                                parent,
                                member,
                                property,
                            )
                            || (member.kind == MemberKind::Method
                                && member.name == property.getter
                                && member.parameter_types.is_empty()
                                && member.has_body
                                && !member.is_static
                                && !member.has_unsupported_property_annotations
                                && !member.has_unsupported_property_shape
                                && property_type_matches(
                                    index,
                                    contract_owner,
                                    parent,
                                    member,
                                    property,
                                ))
                    }),
                    DeclarationKind::Class | DeclarationKind::Object => subtype_covers_properties(
                        index,
                        contract_owner,
                        parent,
                        std::slice::from_ref(property),
                    ),
                    _ => false,
                };
                if declaration_is_subtype_of(index, parent, contract_owner) && supplies_method {
                    return true;
                }
                pending.extend(
                    parent
                        .supertypes
                        .iter()
                        .cloned()
                        .map(|supertype| (parent_file.path.clone(), supertype)),
                );
            }
        }
        false
    })
}

/// A Kotlin property ancestor is part of the source-level override contract
/// when it appears on the same supertype path as the Java getter. Keep the
/// descendant's property syntax in that case: a Kotlin `fun getX()` cannot
/// override the inherited Kotlin `val x`.
fn inherits_kotlin_property_on_java_getter_path(
    index: &SourceIndex,
    declaration: &Declaration,
    property_name: &str,
    getter_name: &str,
) -> bool {
    let Some(source_file) = index.declaration_source_file(declaration) else {
        return false;
    };
    let mut pending = declaration
        .supertypes
        .iter()
        .cloned()
        .map(|supertype| (source_file.path.clone(), supertype, false, false))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while let Some((context_path, supertype, mut saw_java_getter, mut saw_kotlin_property)) =
        pending.pop()
    {
        let Some(context) = index.source_file(&context_path) else {
            continue;
        };
        let Some(parent) = index.resolve_type(context, &supertype) else {
            continue;
        };
        let Some(parent_file) = index.declaration_source_file(parent) else {
            continue;
        };
        if !visited.insert((
            parent_file.path.clone(),
            parent.name.clone(),
            saw_java_getter,
            saw_kotlin_property,
        )) {
            continue;
        }
        if parent.language == SourceLanguage::Java {
            saw_java_getter |= parent.members.iter().any(|member| {
                member.kind == MemberKind::Method
                    && member.name == getter_name
                    && member.parameter_types.is_empty()
                    && !member.is_static
                    && member.type_name.as_deref() != Some("void")
            });
        } else {
            // A property below the Java getter is the declaration that needs
            // repair. Only a Kotlin property above that getter can shadow it.
            saw_kotlin_property |= saw_java_getter
                && parent.members.iter().any(|member| {
                    member.kind == MemberKind::Property && member.name == property_name
                });
        }
        if saw_java_getter && saw_kotlin_property {
            return true;
        }
        pending.extend(parent.supertypes.iter().cloned().map(|next| {
            (
                parent_file.path.clone(),
                next,
                saw_java_getter,
                saw_kotlin_property,
            )
        }));
    }
    false
}

/// A Kotlin property override must keep its declaration-site `override`
/// modifier while a Kotlin superinterface still owns that property. JavaBean
/// repair can otherwise rewrite a constructor property into a field plus
/// getter function, which no longer overrides the retained Kotlin property.
fn inherits_retained_kotlin_property(
    index: &SourceIndex,
    declaration: &Declaration,
    property_name: &str,
    retained: Option<&HashSet<crate::semantics::SymbolId>>,
) -> bool {
    let Some(source_file) = index.declaration_source_file(declaration) else {
        return false;
    };
    let mut pending = declaration
        .supertypes
        .iter()
        .cloned()
        .map(|supertype| (source_file.path.clone(), supertype))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while let Some((context_path, supertype)) = pending.pop() {
        let Some(context) = index.source_file(&context_path) else {
            continue;
        };
        let Some(parent) = index.resolve_type(context, &supertype) else {
            continue;
        };
        let Some(parent_file) = index.declaration_source_file(parent) else {
            continue;
        };
        if !visited.insert((parent_file.path.clone(), parent.name.clone())) {
            continue;
        }
        if parent.language == SourceLanguage::Kotlin
            && matches!(
                parent.kind,
                DeclarationKind::Class | DeclarationKind::Object
            )
            && retained.is_none_or(|retained| index.declaration_retained(parent, retained))
            && parent.members.iter().any(|member| {
                member.kind == MemberKind::Property
                    && !member.is_static
                    && member.name == property_name
            })
        {
            return true;
        }
        pending.extend(
            parent
                .supertypes
                .iter()
                .cloned()
                .map(|next| (parent_file.path.clone(), next)),
        );
    }
    false
}

fn bridge_rejected(
    index: &SourceIndex,
    contract_owner: &Declaration,
    declaration: &Declaration,
    reason: &str,
) -> PropertyBridgeFailure {
    let owner_file = index
        .declaration_source_file(contract_owner)
        .map(|file| file.path.display().to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    let declaration_file = index
        .declaration_source_file(declaration)
        .map(|file| file.path.display().to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    log::debug!(
        "property ABI bridge for {} ({}) rejected at {} ({}): {}",
        contract_owner.name,
        owner_file,
        declaration.name,
        declaration_file,
        reason
    );
    PropertyBridgeFailure {
        declaration: declaration.name.clone(),
        reason: reason.to_string(),
    }
}

fn source_contract_properties(
    index: &SourceIndex,
    target: &Declaration,
) -> Option<Vec<PropertyContract>> {
    if target.kind != DeclarationKind::Interface {
        return None;
    }
    let type_source = index.declaration_source_file(target)?.path.clone();
    let mut properties = Vec::new();
    for member in &target.members {
        if member.kind == MemberKind::Method {
            continue;
        }
        if member.kind != MemberKind::Property
            || member.is_static
            || (member.has_body && member.is_mutable)
            || member.has_unsupported_property_annotations
            || member
                .type_name
                .as_deref()
                .is_none_or(|ty| ty.contains("->") || ty.contains(['{', '}']))
        {
            return None;
        }
        properties.push(PropertyContract {
            name: member.name.clone(),
            type_name: member.type_name.clone()?,
            type_source: type_source.clone(),
            getter: crate::workspace::property_accessor_name(&member.name),
            setter: member.is_mutable.then(|| setter_name(&member.name)),
        });
    }
    Some(properties)
}

/// Re-express the current contract's property types in a direct subtype's
/// type-parameter environment. The source index records resolved declarations
/// for subtype edges, while the edge spelling retains the generic arguments
/// needed here (for example `IObjectEvent<Foo, String>`).
fn specialize_contract_properties(
    index: &SourceIndex,
    contract: &Declaration,
    subtype: &Declaration,
    properties: &[PropertyContract],
) -> Option<Vec<PropertyContract>> {
    let subtype_file = index.declaration_source_file(subtype)?;
    let contract_file = index.declaration_source_file(contract)?;
    let mut matching_edge = None;
    for edge in &subtype.supertypes {
        let (base, arguments) = split_type_arguments(edge);
        if index
            .resolve_type(subtype_file, base)
            .is_some_and(|resolved| {
                resolved.name == contract.name
                    && index
                        .declaration_source_file(resolved)
                        .is_some_and(|file| file.path == contract_file.path)
            })
        {
            matching_edge = Some(arguments);
            break;
        }
    }
    let arguments = matching_edge?;
    if arguments.len() != contract.type_params.len()
        || arguments.iter().any(|argument| {
            argument.is_empty()
                || *argument == "*"
                || argument.starts_with("in ")
                || argument.starts_with("out ")
        })
    {
        return None;
    }
    let substitutions = contract
        .type_params
        .iter()
        .cloned()
        .zip(arguments.into_iter().map(str::to_string))
        .collect::<HashMap<_, _>>();
    properties
        .iter()
        .map(|property| {
            let type_name = substitute_type_parameters(&property.type_name, &substitutions)?;
            Some(PropertyContract {
                name: property.name.clone(),
                type_source: if type_name == property.type_name {
                    property.type_source.clone()
                } else {
                    subtype_file.path.clone()
                },
                type_name,
                getter: property.getter.clone(),
                setter: property.setter.clone(),
            })
        })
        .collect()
}

fn substitute_type_parameters(
    type_name: &str,
    substitutions: &HashMap<String, String>,
) -> Option<String> {
    let mut output = String::with_capacity(type_name.len());
    let mut identifier = String::new();
    let flush_identifier = |identifier: &mut String, output: &mut String| {
        if !identifier.is_empty() {
            output.push_str(
                substitutions
                    .get(identifier)
                    .map_or(identifier.as_str(), String::as_str),
            );
            identifier.clear();
        }
    };
    for character in type_name.chars() {
        if character.is_alphanumeric() || character == '_' {
            identifier.push(character);
        } else {
            flush_identifier(&mut identifier, &mut output);
            output.push(character);
        }
    }
    flush_identifier(&mut identifier, &mut output);
    // A star or a variance projection in the resulting property type is not
    // sufficient evidence that a Java getter override remains source-safe.
    Some(output)
}

fn type_mentions_any_parameter(type_name: &str, parameters: &[String]) -> bool {
    let mut identifier = String::new();
    let mut mentions = false;
    for character in type_name.chars().chain(std::iter::once(' ')) {
        if character.is_alphanumeric() || character == '_' {
            identifier.push(character);
        } else {
            if parameters.iter().any(|parameter| parameter == &identifier) {
                mentions = true;
                break;
            }
            identifier.clear();
        }
    }
    mentions
}

fn subtype_covers_properties(
    index: &SourceIndex,
    contract_owner: &Declaration,
    subtype: &Declaration,
    properties: &[PropertyContract],
) -> bool {
    if !matches!(
        subtype.kind,
        DeclarationKind::Interface
            | DeclarationKind::Class
            | DeclarationKind::Enum
            | DeclarationKind::Object
    ) {
        return false;
    }
    properties.iter().all(|contract| {
        let matching = subtype
            .members
            .iter()
            .find(|member| member.kind == MemberKind::Property && member.name == contract.name);
        let property_covers = matching.is_some_and(|member| {
            let setter = bridged_setter_name(contract, member.is_mutable);
            !member.is_static
                && !member.is_jvm_field
                && property_type_matches(index, contract_owner, subtype, member, contract)
                && (subtype.kind != DeclarationKind::Interface
                    || (!member.has_body && !member.is_constructor_property))
                && (!matches!(
                    subtype.kind,
                    DeclarationKind::Class | DeclarationKind::Object
                ) || !member.has_custom_accessor
                    || member.is_constructor_property
                    || computed_class_property_bridgeable(index, subtype))
                && !member.has_unsupported_property_annotations
                && !member.has_unsupported_property_shape
                && member.visibility.as_deref() != Some("private")
                && member.visibility.as_deref() != Some("protected")
                && (!contract.setter.is_some() || member.is_mutable)
                && !subtype.members.iter().any(|candidate| {
                    candidate.kind == MemberKind::Method
                        && (candidate.name == contract.getter
                            || setter.as_deref() == Some(candidate.name.as_str()))
                })
        });
        let method_covers = subtype
            .members
            .iter()
            .find(|member| member.kind == MemberKind::Method && member.name == contract.getter)
            .is_some_and(|member| {
                contract.setter.is_none()
                    && member.parameter_types.is_empty()
                    && !member.is_static
                    && member.visibility.as_deref() != Some("private")
                    && member.visibility.as_deref() != Some("protected")
                    && !member.has_unsupported_property_annotations
                    && !member.has_unsupported_property_shape
                    && property_type_matches(index, contract_owner, subtype, member, contract)
            });
        property_covers
            || method_covers
            || (matches!(
                subtype.kind,
                DeclarationKind::Class | DeclarationKind::Object
            ) && persisted_class_property_bridge(index, contract_owner, subtype, contract))
    })
}

/// Recognize a class property already rewritten by an earlier migration
/// round. The generated `@JvmField` stores the Kotlin property while its
/// explicit getter implements the JavaBean contract; this is equivalent to
/// the original override once the remaining Kotlin interface is translated.
fn persisted_class_property_bridge(
    index: &SourceIndex,
    contract_owner: &Declaration,
    subtype: &Declaration,
    contract: &PropertyContract,
) -> bool {
    if contract.setter.is_some() {
        return false;
    }
    let Some(field) = subtype.members.iter().find(|member| {
        matches!(member.kind, MemberKind::Property | MemberKind::Field)
            && member.name == contract.name
    }) else {
        return false;
    };
    let Some(getter) = subtype.members.iter().find(|member| {
        member.kind == MemberKind::Method
            && member.name == contract.getter
            && member.parameter_types.is_empty()
    }) else {
        return false;
    };
    field.is_jvm_field
        && !field.is_mutable
        && !field.is_static
        && !field.has_unsupported_property_annotations
        && !field.has_unsupported_property_shape
        && !getter.is_static
        && getter.visibility.as_deref() != Some("private")
        && getter.visibility.as_deref() != Some("protected")
        && property_type_matches(index, contract_owner, subtype, field, contract)
        && property_type_matches(index, contract_owner, subtype, getter, contract)
}

fn computed_class_property_bridgeable(index: &SourceIndex, owner: &Declaration) -> bool {
    let Some(file) = index.declaration_source_file(owner) else {
        return false;
    };
    let Ok(source) = std::fs::read_to_string(&file.path) else {
        return false;
    };
    // Kotlin all-open plugins reject @JvmName on the virtual accessor. Until
    // component-wide call-site rewriting replaces computed properties with
    // methods, keep those annotated classes on the Kotlin side of the ABI.
    !["@Entity", "@MappedSuperclass", "@Embeddable"]
        .iter()
        .any(|annotation| source.contains(annotation))
}

fn property_type_matches(
    index: &SourceIndex,
    _contract_owner: &Declaration,
    subtype: &Declaration,
    member: &crate::workspace::Member,
    contract: &PropertyContract,
) -> bool {
    let Some(member_type) = member.type_name.as_deref() else {
        return false;
    };
    if member_type.contains(['{', '}'])
        || member_type.contains("->")
        || contract.type_name.contains(['{', '}'])
        || contract.type_name.contains("->")
    {
        return false;
    }
    if !member.is_mutable
        && !member_type.trim().ends_with('?')
        && contract.type_name.trim().ends_with('?')
        && is_scalar_reference_type(member_type)
        && is_scalar_reference_type(&contract.type_name)
    {
        let Some(subtype_file) = index.declaration_source_file(subtype) else {
            return false;
        };
        let widened_contract = contract.type_name.trim().trim_end_matches('?').trim();
        if index.property_getter_return_compatible_in_files(
            &subtype_file.path,
            member_type,
            &contract.type_source,
            widened_contract,
        ) {
            return true;
        }
    }
    if member_type.contains(['<', '>']) || contract.type_name.contains(['<', '>']) {
        let Some(contract_file) = index.source_file(&contract.type_source) else {
            return false;
        };
        let Some(subtype_file) = index.declaration_source_file(subtype) else {
            return false;
        };
        if exact_type_tree_matches(
            index,
            contract_file,
            &contract.type_name,
            subtype_file,
            member_type,
        ) {
            return true;
        }
        return member_type != contract.type_name
            && index.property_getter_return_compatible_in_files(
                &subtype_file.path,
                member_type,
                &contract.type_source,
                &contract.type_name,
            );
    }
    if member_type != contract.type_name {
        let Some(subtype_file) = index.declaration_source_file(subtype) else {
            return false;
        };
        return index.property_getter_return_compatible_in_files(
            &subtype_file.path,
            member_type,
            &contract.type_source,
            &contract.type_name,
        );
    }
    let simple = member_type.trim_end_matches('?');
    if matches!(
        simple,
        "Any"
            | "Nothing"
            | "Unit"
            | "String"
            | "Boolean"
            | "Byte"
            | "Short"
            | "Int"
            | "Long"
            | "Float"
            | "Double"
            | "Char"
    ) || simple.contains('.')
    {
        return true;
    }
    let Some(contract_file) = index.source_file(&contract.type_source) else {
        return false;
    };
    let Some(subtype_file) = index.declaration_source_file(subtype) else {
        return false;
    };
    let contract_resolved = index.resolve_type(contract_file, simple);
    let subtype_resolved = index.resolve_type(subtype_file, simple);
    match (contract_resolved, subtype_resolved) {
        (Some(_), Some(_)) => index.property_getter_return_compatible_in_files(
            &subtype_file.path,
            simple,
            &contract_file.path,
            simple,
        ),
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => {
            unresolved_type_identity_matches(contract_file, simple, subtype_file, simple)
                || index.property_getter_return_compatible_in_files(
                    &subtype_file.path,
                    simple,
                    &contract_file.path,
                    simple,
                )
        }
    }
}

fn is_scalar_reference_type(type_name: &str) -> bool {
    let ty = type_name.trim().trim_end_matches('?').trim();
    !ty.is_empty()
        && !ty.contains(['<', '>', '*', ' ', '[', ']', '{', '}'])
        && !ty.contains("->")
        && !matches!(
            ty,
            "Any"
                | "Nothing"
                | "Unit"
                | "String"
                | "Boolean"
                | "Byte"
                | "Short"
                | "Int"
                | "Long"
                | "Float"
                | "Double"
                | "Char"
        )
}

fn is_jvm_getter_return_type(type_name: &str) -> bool {
    is_scalar_reference_type(type_name)
        || matches!(
            type_name.trim().trim_end_matches('?').trim(),
            "String" | "Boolean" | "Byte" | "Short" | "Int" | "Long" | "Float" | "Double" | "Char"
        )
}

fn qualify_simple_type_argument(
    index: &SourceIndex,
    context: &SourceFile,
    argument: &str,
) -> String {
    let argument = argument.trim();
    let (name, nullable) = argument
        .strip_suffix('?')
        .map_or((argument, false), |name| (name.trim(), true));
    if name.contains(['<', '>', '.', ' ', '*', '[', ']']) {
        return argument.to_string();
    }
    let Some(declaration) = index.resolve_type(context, name) else {
        return argument.to_string();
    };
    let qualified = declaration
        .package
        .as_ref()
        .map(|package| format!("{package}.{}", declaration.name))
        .unwrap_or_else(|| declaration.name.clone());
    if nullable {
        format!("{qualified}?")
    } else {
        qualified
    }
}

fn adapt_parallel_readonly_contracts(
    index: &SourceIndex,
    declaration: &Declaration,
    contracts: Vec<PropertyContract>,
) -> Vec<PropertyContract> {
    if declaration.kind != DeclarationKind::Interface {
        return contracts;
    }
    let Some(source_file) = index.declaration_source_file(declaration) else {
        return contracts;
    };
    contracts
        .into_iter()
        .map(|mut contract| {
            let Some(member) = declaration
                .members
                .iter()
                .find(|member| member.kind == MemberKind::Property && member.name == contract.name)
            else {
                return contract;
            };
            let Some(local_type) = member.type_name.as_deref() else {
                return contract;
            };
            let local_is_type_parameter = declaration
                .type_params
                .iter()
                .any(|parameter| parameter == local_type.trim().trim_end_matches('?').trim());
            if member.is_mutable
                || !is_jvm_getter_return_type(local_type)
                || !is_jvm_getter_return_type(&contract.type_name)
                || (!local_is_type_parameter
                    && !index.property_getter_return_compatible_in_files(
                        &contract.type_source,
                        &contract.type_name,
                        &source_file.path,
                        local_type,
                    ))
            {
                return contract;
            }
            // Preserve the parallel interface's own getter descriptor. A
            // broader local return can coexist with the narrowed generated
            // contract when concrete implementations satisfy both.
            contract.type_name = local_type.to_string();
            contract.type_source = source_file.path.clone();
            contract.setter = None;
            contract
        })
        .collect()
}

/// A descendant's generated Java getter may be carried back to a parallel
/// Kotlin interface only when its own read-only property can safely retain
/// the broader local descriptor. Type parameters are not widened from one
/// concrete descendant: their instantiations must be proven at the interface
/// declaration itself.
fn parallel_readonly_contract_is_bridgeable(
    index: &SourceIndex,
    declaration_file: &SourceFile,
    declaration: &Declaration,
    member: &crate::workspace::Member,
    contract: &PropertyContract,
) -> bool {
    if member.is_mutable
        || contract.setter.is_some()
        || member.has_unsupported_property_shape
        || member.has_unsupported_property_annotations
    {
        return false;
    }
    let Some(local_type) = member.type_name.as_deref() else {
        return false;
    };
    if local_type.trim().ends_with('?') != contract.type_name.trim().ends_with('?')
        || !is_jvm_getter_return_type(local_type)
        || !is_jvm_getter_return_type(&contract.type_name)
    {
        return false;
    }
    let local_parameter = declaration
        .type_params
        .iter()
        .any(|parameter| parameter == local_type.trim().trim_end_matches('?').trim());
    if local_parameter {
        return contract.type_name == local_type
            && index.property_getter_return_compatible_in_files(
                &contract.type_source,
                &contract.type_name,
                &declaration_file.path,
                local_type,
            )
            && index.property_getter_return_compatible_in_files(
                &declaration_file.path,
                local_type,
                &contract.type_source,
                &contract.type_name,
            );
    }
    index.property_getter_return_compatible_in_files(
        &contract.type_source,
        &contract.type_name,
        &declaration_file.path,
        local_type,
    )
}

fn exact_type_tree_matches(
    index: &SourceIndex,
    left_file: &SourceFile,
    left: &str,
    right_file: &SourceFile,
    right: &str,
) -> bool {
    let left = left.trim();
    let right = right.trim();
    if left.ends_with('?') != right.ends_with('?') {
        return false;
    }
    let left = left.trim_end_matches('?').trim();
    let right = right.trim_end_matches('?').trim();
    let (left_variance, left) = split_variance(left);
    let (right_variance, right) = split_variance(right);
    if left_variance != right_variance {
        return false;
    }
    if left == "*" || right == "*" {
        return left == right;
    }
    let (left_base, left_args) = split_type_arguments(left);
    let (right_base, right_args) = split_type_arguments(right);
    if !type_identity_matches(index, left_file, left_base, right_file, right_base)
        || left_args.len() != right_args.len()
    {
        return false;
    }
    left_args
        .iter()
        .zip(right_args.iter())
        .all(|(left, right)| exact_type_tree_matches(index, left_file, left, right_file, right))
}

fn split_variance(type_name: &str) -> (&str, &str) {
    if let Some(rest) = type_name.strip_prefix("out ") {
        ("out", rest.trim())
    } else if let Some(rest) = type_name.strip_prefix("in ") {
        ("in", rest.trim())
    } else {
        ("", type_name)
    }
}

fn split_type_arguments(type_name: &str) -> (&str, Vec<&str>) {
    let Some(open) = type_name.find('<') else {
        return (type_name.trim(), Vec::new());
    };
    let Some(close) = type_name.rfind('>') else {
        return (type_name.trim(), Vec::new());
    };
    if close < open || !type_name[close + 1..].trim().is_empty() {
        return (type_name.trim(), Vec::new());
    }
    let mut arguments = Vec::new();
    let mut depth = 0usize;
    let mut start = open + 1;
    for (relative, character) in type_name[open + 1..close].char_indices() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let end = open + 1 + relative;
                arguments.push(type_name[start..end].trim());
                start = end + 1;
            }
            _ => {}
        }
    }
    arguments.push(type_name[start..close].trim());
    (type_name[..open].trim(), arguments)
}

fn type_identity_matches(
    index: &SourceIndex,
    left_file: &SourceFile,
    left: &str,
    right_file: &SourceFile,
    right: &str,
) -> bool {
    if left != right {
        return false;
    }
    if matches!(
        left,
        "Any"
            | "Nothing"
            | "Unit"
            | "String"
            | "Boolean"
            | "Byte"
            | "Short"
            | "Int"
            | "Long"
            | "Float"
            | "Double"
            | "Char"
            | "Array"
            | "List"
            | "MutableList"
            | "Set"
            | "MutableSet"
            | "Map"
            | "MutableMap"
            | "Collection"
            | "MutableCollection"
            | "Iterable"
            | "MutableIterable"
    ) || left.contains('.')
    {
        return true;
    }
    match (
        index.resolve_type(left_file, left),
        index.resolve_type(right_file, right),
    ) {
        (Some(left), Some(right)) => {
            left.name == right.name
                && left.language == right.language
                && index
                    .declaration_source_file(left)
                    .zip(index.declaration_source_file(right))
                    .is_some_and(|(left_file, right_file)| left_file.path == right_file.path)
        }
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => unresolved_type_identity_matches(left_file, left, right_file, right),
    }
}

fn class_supertype_properties_compatible(
    index: &SourceIndex,
    contract_owner: &Declaration,
    implementation: &Declaration,
    properties: &[PropertyContract],
    translation_roots: &[std::path::PathBuf],
) -> bool {
    let Some(implementation_file) = index.declaration_source_file(implementation) else {
        return false;
    };
    let mut pending = implementation
        .supertypes
        .iter()
        .cloned()
        .map(|supertype| {
            (
                implementation_file.path.clone(),
                supertype,
                HashMap::<String, String>::new(),
            )
        })
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while let Some((context_path, supertype, inherited_bindings)) = pending.pop() {
        let Some(context) = index.source_file(&context_path) else {
            log::debug!(
                "retained property contract for {} rejected at unresolved supertype context {}",
                implementation.name,
                context_path.display()
            );
            return false;
        };
        let resolved_supertype =
            substitute_type_parameters(&supertype, &inherited_bindings).unwrap_or(supertype);
        let (parent_name, supplied_args) = split_type_arguments(&resolved_supertype);
        let Some(parent) = index.resolve_type(context, parent_name) else {
            continue;
        };
        if !visited.insert(format!(
            "{}:{}:{}:{}",
            parent.language as u8,
            parent.name,
            index
                .declaration_source_file(parent)
                .map(|file| file.path.display().to_string())
                .unwrap_or_default(),
            resolved_supertype
        )) {
            continue;
        }
        let Some(parent_file) = index.declaration_source_file(parent) else {
            continue;
        };
        let parent_bindings = parent
            .type_params
            .iter()
            .cloned()
            .zip(
                supplied_args
                    .into_iter()
                    .map(|argument| qualify_simple_type_argument(index, context, argument)),
            )
            .collect::<HashMap<_, _>>();
        if parent.language == SourceLanguage::Kotlin {
            let is_contract_owner = parent.name == contract_owner.name
                && index
                    .declaration_source_file(contract_owner)
                    .is_some_and(|owner_file| owner_file.path == parent_file.path);
            let parent_is_on_contract_chain =
                declaration_is_subtype_of(index, parent, contract_owner)
                    || declaration_is_subtype_of(index, contract_owner, parent);
            let parallel_contract = !is_contract_owner && !parent_is_on_contract_chain;
            let has_jvm_name_annotation = parallel_contract
                && std::fs::read_to_string(&parent_file.path)
                    .is_ok_and(|source| source.contains("@JvmName"));
            for member in parent
                .members
                .iter()
                .filter(|member| !is_contract_owner && member.kind == MemberKind::Property)
            {
                let Some(contract) = properties
                    .iter()
                    .find(|contract| contract.name == member.name)
                else {
                    continue;
                };
                let inferred_member_type = member
                    .type_name
                    .is_none()
                    .then(|| {
                        let (receiver, rhs_property) =
                            simple_inferred_property_getter_rhs(index, parent, &member.name)?;
                        if rhs_property != member.name {
                            return None;
                        }
                        let inferred = inherited_property_type_from_supertypes(
                            index,
                            implementation,
                            parent,
                            &parent_bindings,
                            &member.name,
                        )?;
                        inferred_property_matches_receiver_bound(
                            index,
                            parent,
                            &parent_bindings,
                            &receiver,
                            &rhs_property,
                            &inferred,
                        )
                        .then_some(inferred)
                    })
                    .flatten();
                if let Some((inferred_type, _)) = &inferred_member_type {
                    log::debug!(
                        "inferred unannotated supertype property {}.{} as {} for {} from its inherited typed contract",
                        parent.name,
                        member.name,
                        inferred_type,
                        implementation.name
                    );
                }
                let specialized_member_type = member
                    .type_name
                    .as_deref()
                    .and_then(|ty| substitute_type_parameters(ty, &parent_bindings))
                    .or_else(|| inferred_member_type.as_ref().map(|(ty, _)| ty.clone()));
                let Some(specialized_member_type) = specialized_member_type else {
                    log::debug!(
                        "supertype property {}.{} on {} has no type after generic bindings {:?}",
                        parent.name,
                        member.name,
                        implementation.name,
                        parent_bindings
                    );
                    return false;
                };
                let property_is_unresolved_parent_parameter = parent.type_params.iter().any(|p| {
                    specialized_member_type.trim().trim_end_matches('?').trim() == p
                        && !parent_bindings
                            .get(p)
                            .is_some_and(|argument| implementation.type_params.contains(argument))
                });
                if property_is_unresolved_parent_parameter {
                    log::debug!(
                        "supertype property {}.{} on {} remains an unbound type parameter {:?} after edge {} with bindings {:?}",
                        parent.name,
                        member.name,
                        implementation.name,
                        specialized_member_type,
                        resolved_supertype,
                        parent_bindings
                    );
                    return false;
                }
                // A parallel Kotlin property changes property syntax in its
                // retained default members and descendants. Scalar read-only
                // contracts are safe when the selected interface is repaired
                // and the concrete getter satisfies both declarations.
                let local_readonly_contract = parallel_contract
                    && member.kind == MemberKind::Property
                    && !member.is_mutable
                    && is_scalar_reference_type(&specialized_member_type);
                if parallel_contract
                    && (!index.is_selected(&parent_file.path, translation_roots)
                        || !local_readonly_contract)
                {
                    log::debug!(
                        "parallel property contract {}.{} rejected for {}: selected={}, readonly_scalar={}",
                        parent.name,
                        member.name,
                        implementation.name,
                        index.is_selected(&parent_file.path, translation_roots),
                        local_readonly_contract
                    );
                    return false;
                }
                // Unsupported property shapes and JVM accessor annotations
                // remain blockers. Unrelated parallel supertypes are harmless.
                if (member.has_custom_accessor && member.has_unsupported_property_shape)
                    || member.has_unsupported_property_shape
                    || member.has_unsupported_property_annotations
                    || has_jvm_name_annotation
                    || (parallel_contract && member.has_body && !local_readonly_contract)
                    || (contract.setter.is_some() && !member.is_mutable)
                {
                    log::debug!(
                        "supertype property {}.{} on {} rejected for unsupported shape: type={:?}, custom_accessor={}, unsupported_shape={}, unsupported_annotations={}, jvm_name={}, body={}, mutable={}, contract_setter={:?}",
                        parent.name,
                        member.name,
                        implementation.name,
                        specialized_member_type,
                        member.has_custom_accessor,
                        member.has_unsupported_property_shape,
                        member.has_unsupported_property_annotations,
                        has_jvm_name_annotation,
                        member.has_body,
                        member.is_mutable,
                        contract.setter
                    );
                    return false;
                }
                if parallel_contract {
                    // A parallel interface may declare a broader read-only
                    // property than the translated root. Its own getter is
                    // preserved with that local type, while the concrete
                    // class must satisfy both contracts.
                    let local_type_uses_implementation_parameter =
                        implementation.type_params.iter().any(|parameter| {
                            specialized_member_type.trim().trim_end_matches('?').trim() == parameter
                                && parent_bindings
                                    .values()
                                    .any(|argument| argument == parameter)
                        });
                    let local_contract = PropertyContract {
                        name: contract.name.clone(),
                        type_name: specialized_member_type,
                        // A supertype edge may bind its parameter to one of
                        // the concrete implementation's own type parameters
                        // (`CreatedEvent<T> : BroadEvent<T>`). Reuse that
                        // source context so both spellings refer to the same
                        // symbol instead of treating the parent's `T` as an
                        // unresolved name.
                        type_source: if local_type_uses_implementation_parameter {
                            implementation_file.path.clone()
                        } else if let Some((_, inherited_source)) = &inferred_member_type {
                            inherited_source.clone()
                        } else {
                            parent_file.path.clone()
                        },
                        getter: contract.getter.clone(),
                        setter: None,
                    };
                    let implementation_property =
                        effective_readonly_scalar_property(index, implementation, &member.name);
                    let Some((provider, implementation_member)) = implementation_property else {
                        log::debug!(
                            "parallel property contract {}.{} rejected for {}: no unique concrete inherited scalar provider",
                            parent.name,
                            member.name,
                            implementation.name
                        );
                        return false;
                    };
                    let root_covered = property_type_matches(
                        index,
                        contract_owner,
                        provider,
                        implementation_member,
                        contract,
                    );
                    let local_covered = property_type_matches(
                        index,
                        parent,
                        provider,
                        implementation_member,
                        &local_contract,
                    );
                    if !root_covered
                        || !local_covered
                        || (provider.name != implementation.name
                            && !inherited_repairable_property_methods(
                                index,
                                contract_owner,
                                implementation,
                                std::slice::from_ref(contract),
                            ))
                    {
                        log::debug!(
                            "parallel property contract {}.{} rejected for {}: provider {}.{} has type {:?}, root contract {:?} covered={}, local contract {:?} covered={}",
                            parent.name,
                            member.name,
                            implementation.name,
                            provider.name,
                            implementation_member.name,
                            implementation_member.type_name,
                            contract.type_name,
                            root_covered,
                            local_contract.type_name,
                            local_covered
                        );
                        return false;
                    }
                } else {
                    let mut specialized_member = member.clone();
                    specialized_member.type_name = Some(specialized_member_type);
                    let exact_or_narrower_ancestor =
                        declaration_is_subtype_of(index, contract_owner, parent)
                            && !member.is_mutable
                            && contract.setter.is_none()
                            && is_scalar_reference_type(&contract.type_name)
                            && is_scalar_reference_type(
                                specialized_member.type_name.as_deref().unwrap_or_default(),
                            )
                            && index.property_getter_return_compatible_in_files(
                                &contract.type_source,
                                &contract.type_name,
                                &parent_file.path,
                                specialized_member.type_name.as_deref().unwrap_or_default(),
                            );
                    if !property_type_matches(
                        index,
                        contract_owner,
                        parent,
                        &specialized_member,
                        contract,
                    ) && !exact_or_narrower_ancestor
                    {
                        log::debug!(
                            "nonparallel supertype property {}.{} on {} is incompatible: edge={}, bindings={:?}, specialized_type={:?}, contract_type={:?} from {}",
                            parent.name,
                            member.name,
                            implementation.name,
                            resolved_supertype,
                            parent_bindings,
                            specialized_member.type_name,
                            contract.type_name,
                            contract.type_source.display()
                        );
                        return false;
                    }
                }
            }
            pending.extend(
                parent.supertypes.iter().cloned().map(|supertype| {
                    (parent_file.path.clone(), supertype, parent_bindings.clone())
                }),
            );
        }
    }
    true
}

/// Find the nearest concrete read-only scalar property available to a class.
/// Interface defaults are eligible only when they carry a getter body; an
/// abstract declaration alone cannot satisfy a concrete getter contract.
fn effective_readonly_scalar_property<'a>(
    index: &'a SourceIndex,
    implementation: &'a Declaration,
    name: &str,
) -> Option<(&'a Declaration, &'a crate::workspace::Member)> {
    if let Some(member) = implementation
        .members
        .iter()
        .find(|member| member.kind == MemberKind::Property && member.name == name)
    {
        return (!member.is_mutable
            && member
                .type_name
                .as_deref()
                .is_some_and(is_scalar_reference_type))
        .then_some((implementation, member));
    }
    let source_file = index.declaration_source_file(implementation)?;
    let mut pending = implementation
        .supertypes
        .iter()
        .cloned()
        .map(|supertype| (source_file.path.clone(), supertype, 1usize))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while !pending.is_empty() {
        let nearest_depth = pending.iter().map(|(_, _, depth)| *depth).min()?;
        let mut providers = Vec::new();
        let mut remaining = Vec::new();
        for (context_path, supertype, depth) in pending {
            if depth != nearest_depth {
                remaining.push((context_path, supertype, depth));
                continue;
            }
            let Some(context) = index.source_file(&context_path) else {
                continue;
            };
            let Some(parent) = index.resolve_type(context, &supertype) else {
                continue;
            };
            let Some(parent_file) = index.declaration_source_file(parent) else {
                continue;
            };
            let key = format!("{}:{}", parent_file.path.display(), parent.name);
            if !visited.insert(key) {
                continue;
            }
            if parent.language == SourceLanguage::Kotlin
                && let Some(member) = parent.members.iter().find(|member| {
                    member.kind == MemberKind::Property
                        && member.name == name
                        && !member.is_mutable
                        && member.has_body
                        && !member.has_unsupported_property_shape
                        && !member.has_unsupported_property_annotations
                        && member
                            .type_name
                            .as_deref()
                            .is_some_and(is_scalar_reference_type)
                })
            {
                providers.push((parent, member));
            }
            remaining.extend(
                parent
                    .supertypes
                    .iter()
                    .cloned()
                    .map(|next| (parent_file.path.clone(), next, nearest_depth + 1)),
            );
        }
        if !providers.is_empty() {
            return (providers.len() == 1).then(|| providers[0]);
        }
        pending = remaining;
    }
    None
}

/// Return the property selected by an unannotated property's getter only when
/// the getter is a single, side-effect-free member access. This keeps the
/// inherited-contract inference below from treating an arbitrary computed
/// getter as though its result had the ancestor property's exact type.
fn simple_inferred_property_getter_rhs(
    index: &SourceIndex,
    owner: &Declaration,
    wanted_property_name: &str,
) -> Option<(String, String)> {
    let file = index.declaration_source_file(owner)?;
    let source = file.source_text();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let mut stack = vec![tree.root_node()];
    let mut declarations = Vec::new();
    while let Some(node) = stack.pop() {
        if declaration_node(node)
            && declaration_name(node, source) == owner.name
            && declaration_kind_matches(owner.kind, node)
        {
            declarations.push(node);
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    let [declaration] = declarations.as_slice() else {
        // Without byte spans on Declaration, a duplicate nested simple name
        // is ambiguous. Refuse to guess which declaration owns this getter.
        return None;
    };
    let declaration = *declaration;
    let mut stack = vec![declaration];
    while let Some(node) = stack.pop() {
        if node.id() != declaration.id() && declaration_node(node) {
            continue;
        }
        if node.kind() == "property_declaration"
            && property_name(node, source) == wanted_property_name
        {
            let text = node_text(node, source);
            let (_, accessor) = text.split_once("get()")?;
            let expression = accessor.trim().strip_prefix('=')?.trim();
            let (receiver, selected) = expression.split_once('.')?;
            let is_identifier = |part: &str| {
                !part.is_empty()
                    && part.chars().enumerate().all(|(index, character)| {
                        character == '_'
                            || character.is_ascii_alphanumeric()
                                && (index > 0 || !character.is_ascii_digit())
                    })
            };
            if is_identifier(receiver) && is_identifier(selected) {
                return Some((receiver.to_string(), selected.to_string()));
            }
            return None;
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    None
}

/// Prove that a simple getter access has exactly the inferred type through
/// the receiver property's declared type parameter bound. In particular, an
/// ancestor property contract alone is insufficient because the getter may
/// return a narrower subtype.
fn inferred_property_matches_receiver_bound(
    index: &SourceIndex,
    owner: &Declaration,
    owner_bindings: &HashMap<String, String>,
    receiver: &str,
    selected_property: &str,
    inferred: &(String, std::path::PathBuf),
) -> bool {
    // The value used by a default getter can be declared on a generic
    // superinterface rather than on `owner` itself (for example, `id: T` on
    // `IBaseObject<T>`). Resolve that inherited property with the owner's own
    // type parameters intact; do not specialize it to a concrete descendant,
    // since that would only prove one implementation rather than the getter
    // contract declared by this interface.
    let receiver_type = owner
        .members
        .iter()
        .find(|member| member.kind == MemberKind::Property && member.name == receiver)
        .and_then(|member| member.type_name.clone())
        .or_else(|| {
            inherited_property_type_from_supertypes(index, owner, owner, &HashMap::new(), receiver)
                .map(|(type_name, _)| type_name)
        });
    let Some(receiver_type) = receiver_type else {
        return false;
    };
    let Some(type_parameter) = owner
        .type_params
        .iter()
        .find(|parameter| receiver_type.trim() == parameter.as_str())
    else {
        return false;
    };
    let Some(owner_file) = index.declaration_source_file(owner) else {
        return false;
    };
    let source = owner_file.source_text();
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let Some(tree) = parser.parse(source, None) else {
        return false;
    };
    let mut stack = vec![tree.root_node()];
    let mut owner_nodes = Vec::new();
    while let Some(node) = stack.pop() {
        if declaration_node(node)
            && declaration_name(node, source) == owner.name
            && declaration_kind_matches(owner.kind, node)
        {
            owner_nodes.push(node);
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    let [owner_node] = owner_nodes.as_slice() else {
        return false;
    };
    let owner_node = *owner_node;
    let owner_text = node_text(owner_node, source);
    let Some(parameters_start) = owner_text.find(&owner.name).and_then(|at| {
        owner_text[at + owner.name.len()..]
            .find('<')
            .map(|open| at + owner.name.len() + open)
    }) else {
        return false;
    };
    let mut depth = 0usize;
    let mut parameters_end = None;
    for (offset, character) in owner_text[parameters_start..].char_indices() {
        match character {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    parameters_end = Some(parameters_start + offset);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(parameters_end) = parameters_end else {
        return false;
    };
    let parameters = &owner_text[parameters_start + 1..parameters_end];
    let mut angle_depth = 0usize;
    let mut parameter_parts = Vec::new();
    let mut part_start = 0usize;
    for (offset, character) in parameters.char_indices() {
        match character {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            ',' if angle_depth == 0 => {
                parameter_parts.push(parameters[part_start..offset].trim());
                part_start = offset + 1;
            }
            _ => {}
        }
    }
    parameter_parts.push(parameters[part_start..].trim());
    let Some(bound) = parameter_parts.iter().find_map(|part| {
        let (name, bound) = part.split_once(':')?;
        (name.trim() == type_parameter.as_str()).then_some(bound.trim())
    }) else {
        return false;
    };
    let Some(bound) = substitute_type_parameters(bound, owner_bindings) else {
        return false;
    };
    let (bound_name, bound_args) = split_type_arguments(&bound);
    let Some(bound_declaration) = index.resolve_type(owner_file, bound_name) else {
        return false;
    };
    let Some(bound_file) = index.declaration_source_file(bound_declaration) else {
        return false;
    };
    let qualified_bound_name = bound_declaration
        .package
        .as_ref()
        .map(|package| format!("{package}.{}", bound_declaration.name))
        .unwrap_or_else(|| bound_declaration.name.clone());
    // `resolve_type` may use its unique-name fallback for an unresolved name.
    // That is useful for diagnostics, but it is not evidence that Kotlin can
    // name the type here. Require the written bound and resolved declaration
    // to be mutually compatible in their actual source contexts before using
    // any property declared by the bound.
    if !index.property_getter_return_compatible_in_files(
        &owner_file.path,
        bound_name,
        &bound_file.path,
        &qualified_bound_name,
    ) || !index.property_getter_return_compatible_in_files(
        &bound_file.path,
        &qualified_bound_name,
        &owner_file.path,
        bound_name,
    ) {
        return false;
    }
    let bound_bindings = bound_declaration
        .type_params
        .iter()
        .cloned()
        .zip(
            bound_args
                .into_iter()
                .map(|argument| qualify_simple_type_argument(index, owner_file, argument)),
        )
        .collect::<HashMap<_, _>>();
    let bound_member_type = bound_declaration
        .members
        .iter()
        .find(|member| member.kind == MemberKind::Property && member.name == selected_property)
        .and_then(|member| member.type_name.as_deref())
        .and_then(|ty| substitute_type_parameters(ty, &bound_bindings))
        .map(|ty| (ty, bound_file.path.clone()))
        .or_else(|| {
            inherited_property_type_from_supertypes(
                index,
                bound_declaration,
                bound_declaration,
                &bound_bindings,
                selected_property,
            )
        });
    let Some((bound_type, bound_type_source_path)) = bound_member_type else {
        return false;
    };
    let bound_type_source = if owner
        .type_params
        .iter()
        .any(|parameter| bound_type.trim().trim_end_matches('?').trim() == parameter)
    {
        owner_file
    } else {
        index
            .source_file(&bound_type_source_path)
            .unwrap_or(bound_file)
    };
    let bound_type = qualify_simple_type_argument(index, bound_type_source, &bound_type);
    if bound_type == inferred.0
        && owner
            .type_params
            .iter()
            .any(|parameter| bound_type.trim().trim_end_matches('?').trim() == parameter)
    {
        // Both occurrences have already been traced through the property's
        // receiver bound and the inherited generic contract. At this point
        // their identical spelling denotes the same type parameter declared
        // by `owner`, even when the Java ancestor used its own parameter name.
        return true;
    }
    index.property_getter_return_compatible_in_files(
        inferred.1.as_path(),
        &inferred.0,
        bound_type_source.path.as_path(),
        &bound_type,
    ) && index.property_getter_return_compatible_in_files(
        bound_type_source.path.as_path(),
        &bound_type,
        inferred.1.as_path(),
        &inferred.0,
    )
}

/// Infer an unannotated override property's return from the nearest typed
/// inherited property contract after applying the generic bindings on each
/// edge. Kotlin requires an `override val` to honor that inherited return
/// contract, so its type is concrete evidence when a getter uses inference.
fn inherited_property_type_from_supertypes(
    index: &SourceIndex,
    implementation: &Declaration,
    owner: &Declaration,
    initial_bindings: &HashMap<String, String>,
    property_name: &str,
) -> Option<(String, std::path::PathBuf)> {
    let owner_file = index.declaration_source_file(owner)?;
    let mut pending = owner
        .supertypes
        .iter()
        .cloned()
        .map(|edge| {
            (
                owner_file.path.clone(),
                edge,
                initial_bindings.clone(),
                1usize,
            )
        })
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while !pending.is_empty() {
        let depth = pending.iter().map(|(_, _, _, depth)| *depth).min()?;
        let mut providers = Vec::new();
        let mut remaining = Vec::new();
        for (context_path, edge, inherited_bindings, edge_depth) in pending {
            if edge_depth != depth {
                remaining.push((context_path, edge, inherited_bindings, edge_depth));
                continue;
            }
            let context = index.source_file(&context_path)?;
            let resolved_edge = substitute_type_parameters(&edge, &inherited_bindings)?;
            let (parent_name, supplied_args) = split_type_arguments(&resolved_edge);
            let Some(parent) = index.resolve_type(context, parent_name) else {
                continue;
            };
            let Some(parent_file) = index.declaration_source_file(parent) else {
                continue;
            };
            let key = format!(
                "{}:{}:{}",
                parent_file.path.display(),
                parent.name,
                resolved_edge
            );
            if !visited.insert(key) {
                continue;
            }
            let bindings = parent
                .type_params
                .iter()
                .cloned()
                .zip(
                    supplied_args
                        .into_iter()
                        .map(|argument| qualify_simple_type_argument(index, context, argument)),
                )
                .collect::<HashMap<_, _>>();
            if let Some(member) = parent
                .members
                .iter()
                .find(|member| member.kind == MemberKind::Property && member.name == property_name)
                && let Some(member_type) = member.type_name.as_deref()
            {
                let specialized = substitute_type_parameters(member_type, &bindings)?;
                let Some(simple) = scalar_type_identity(&specialized) else {
                    continue;
                };
                let qualified = qualify_simple_type_argument(index, parent_file, simple);
                let remains_unbound = parent.type_params.iter().any(|parameter| {
                    qualified.trim().trim_end_matches('?').trim() == parameter
                        && !bindings
                            .get(parameter)
                            .is_some_and(|argument| implementation.type_params.contains(argument))
                });
                if !remains_unbound {
                    let source =
                        if implementation.type_params.iter().any(|parameter| {
                            qualified.trim().trim_end_matches('?').trim() == parameter
                        }) {
                            index.declaration_source_file(implementation)?.path.clone()
                        } else {
                            parent_file.path.clone()
                        };
                    providers.push((qualified, source));
                }
            }
            remaining.extend(
                parent
                    .supertypes
                    .iter()
                    .cloned()
                    .map(|next| (parent_file.path.clone(), next, bindings.clone(), depth + 1)),
            );
        }
        if !providers.is_empty() {
            let first = providers.first()?.clone();
            return providers
                .iter()
                .all(|provider| provider.0 == first.0)
                .then_some(first);
        }
        pending = remaining;
    }
    None
}

fn scalar_type_identity(type_name: &str) -> Option<&str> {
    let ty = type_name.trim().trim_end_matches('?').trim();
    // Getter inference also needs the JVM-safe scalar types that are not
    // reference classifiers in the ABI helper, especially String.
    is_jvm_getter_return_type(ty).then_some(ty)
}

/// JavaBean call-site contracts introduced while repairing retained Kotlin
/// interface properties.  The owner is the Kotlin declaration whose property
/// syntax disappears; callers typed as that interface (or a descendant) must
/// invoke the explicit getter/setter after the repair.
pub fn repaired_callsite_contracts(
    index: &SourceIndex,
    generated_java: &HashSet<std::path::PathBuf>,
) -> Vec<PropertyAccessorContract> {
    repaired_callsite_contracts_with_persisted(index, generated_java, &[])
}

pub(crate) fn repaired_callsite_contracts_with_persisted(
    index: &SourceIndex,
    generated_java: &HashSet<std::path::PathBuf>,
    persisted_contracts: &[PersistedPropertyContract],
) -> Vec<PropertyAccessorContract> {
    let persisted_index = PersistedContractIndex::new(index, persisted_contracts);
    let mut cache = HashMap::new();
    let mut contracts = Vec::new();
    // The translated Java root is itself a call-site owner. This also covers
    // retained Kotlin descendants that inherit the getter without redeclaring
    // the property, a shape declaration-repair discovery cannot report.
    for source_file in &index.files {
        if source_file.language != SourceLanguage::Java
            || !is_generated_notlin_java(&source_file.path, generated_java)
        {
            continue;
        }
        for declaration in &source_file.declarations {
            // A generated Java class/enum can be a typed receiver too. Its
            // explicit JavaBean methods are callable from Kotlin synthetic
            // property syntax even though they must not participate in the
            // interface ABI-repair planner below.
            let Some(mut java_contracts) =
                java_callsite_property_contracts(declaration, source_file)
            else {
                continue;
            };
            // A Java interface getter can be hidden from Kotlin source calls
            // when its parent is still a Kotlin property. In that case the
            // caller must keep property syntax (`value.variant`) even though
            // the generated Java declaration has a `getVariant()` method.
            // Once the parent property was converted to an explicit method,
            // the persisted contract below makes the getter call valid again.
            if declaration.kind == DeclarationKind::Interface {
                java_contracts.retain(|contract| {
                    !java_contract_is_shadowed_by_unrepaired_kotlin_property(
                        index,
                        source_file,
                        declaration,
                        contract,
                        &persisted_index,
                    )
                });
            }
            if matches!(
                declaration.kind,
                DeclarationKind::Class
                    | DeclarationKind::Enum
                    | DeclarationKind::Object
                    | DeclarationKind::Record
            ) {
                java_contracts.retain(|contract| {
                    java_owner_inherits_repaired_contract(
                        index,
                        source_file,
                        declaration,
                        contract,
                        generated_java,
                        &persisted_index,
                        &mut cache,
                    )
                });
                if java_contracts.is_empty() {
                    continue;
                }
            }
            let owner_type = source_file
                .package
                .as_ref()
                .map(|package| format!("{package}.{}", declaration.name))
                .unwrap_or_else(|| declaration.name.clone());
            contracts.extend(
                java_contracts
                    .into_iter()
                    .map(|contract| PropertyAccessorContract {
                        owner_type: owner_type.clone(),
                        property: contract.name,
                        getter: contract.getter,
                        setter: contract.setter,
                    }),
            );
        }
    }
    for source_file in index.kotlin_files() {
        for declaration in &source_file.declarations {
            if declaration.kind != DeclarationKind::Interface {
                continue;
            }
            let repaired =
                contracts_for_repair(index, source_file, declaration, generated_java, &mut cache);
            for property in declaration
                .members
                .iter()
                .filter(|member| member.kind == MemberKind::Property)
            {
                let Some(contract) = repaired.iter().find(|item| item.name == property.name) else {
                    continue;
                };
                let owner_type = source_file
                    .package
                    .as_ref()
                    .map(|package| format!("{package}.{}", declaration.name))
                    .unwrap_or_else(|| declaration.name.clone());
                contracts.push(PropertyAccessorContract {
                    owner_type,
                    property: contract.name.clone(),
                    getter: contract.getter.clone(),
                    setter: contract.setter.clone(),
                });
            }

            // A previous migration round may already have replaced the
            // interface's Kotlin property with an explicit JavaBean method.
            // Recover that call-site contract only when a retained Kotlin
            // subtype still has both the corresponding property and its
            // explicit getter bridge; this avoids reclassifying arbitrary
            // methods named getX as former properties.
            let Some(interface_file) = index.declaration_source_file(declaration) else {
                continue;
            };
            let owner_type = declaration
                .package
                .as_ref()
                .map(|package| format!("{package}.{}", declaration.name))
                .unwrap_or_else(|| declaration.name.clone());
            for method in declaration.members.iter().filter(|member| {
                member.kind == MemberKind::Method && member.parameter_types.is_empty()
            }) {
                let Some((property, getter)) = getter_property(&method.name) else {
                    continue;
                };
                let Some(type_name) = method.type_name.as_deref() else {
                    continue;
                };
                let setter = declaration
                    .members
                    .iter()
                    .find(|candidate| {
                        candidate.kind == MemberKind::Method
                            && candidate.parameter_types.len() == 1
                            && candidate.name == setter_name(&property)
                    })
                    .map(|candidate| candidate.name.clone());
                let persisted = PropertyContract {
                    name: property.clone(),
                    type_name: type_name.to_string(),
                    type_source: interface_file.path.clone(),
                    getter: getter.clone(),
                    setter: setter.clone(),
                };
                if !persisted_interface_accessor_has_property_bridge(index, declaration, &persisted)
                {
                    continue;
                }
                contracts.push(PropertyAccessorContract {
                    owner_type: owner_type.clone(),
                    property,
                    getter,
                    setter,
                });
            }
        }
    }
    contracts.sort_by(|left, right| {
        (&left.owner_type, &left.property, &left.getter).cmp(&(
            &right.owner_type,
            &right.property,
            &right.getter,
        ))
    });
    contracts.dedup();
    contracts
}

fn java_owner_inherits_repaired_contract(
    index: &SourceIndex,
    source_file: &SourceFile,
    declaration: &Declaration,
    contract: &PropertyContract,
    generated_java: &HashSet<std::path::PathBuf>,
    persisted_contracts: &PersistedContractIndex<'_>,
    cache: &mut HashMap<String, Vec<PropertyContract>>,
) -> bool {
    let mut pending = declaration
        .supertypes
        .iter()
        .cloned()
        .map(|supertype| (source_file.path.clone(), supertype))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while let Some((context_path, supertype)) = pending.pop() {
        let Some(context) = index.source_file(&context_path) else {
            continue;
        };
        let Some(parent) = index.resolve_type(context, &supertype) else {
            continue;
        };
        let Some(parent_file) = index.declaration_source_file(parent) else {
            continue;
        };
        if !visited.insert((parent_file.path.clone(), parent.name.clone())) {
            continue;
        }
        if persisted_contracts
            .for_owner(parent_file, parent)
            .iter()
            .any(|persisted| {
                persisted.contract.name == contract.name
                    && persisted.contract.getter == contract.getter
            })
        {
            return true;
        }
        if parent.language == SourceLanguage::Kotlin
            && parent.kind == DeclarationKind::Interface
            && contracts_for_repair_indexed(
                index,
                parent_file,
                parent,
                generated_java,
                persisted_contracts,
                cache,
            )
            .iter()
            .any(|inherited| inherited.name == contract.name && inherited.getter == contract.getter)
        {
            return true;
        }
        pending.extend(
            parent
                .supertypes
                .iter()
                .cloned()
                .map(|next| (parent_file.path.clone(), next)),
        );
    }
    false
}

fn persisted_interface_accessor_has_property_bridge(
    index: &SourceIndex,
    owner: &Declaration,
    contract: &PropertyContract,
) -> bool {
    let mut pending = index.direct_subtypes(owner);
    let mut visited = HashSet::new();
    while let Some(descendant) = pending.pop() {
        let Some(file) = index.declaration_source_file(descendant) else {
            continue;
        };
        let key = format!("{}:{}", file.path.display(), descendant.name);
        if !visited.insert(key) {
            continue;
        }
        if descendant.language == SourceLanguage::Kotlin
            && descendant.members.iter().any(|property| {
                property.kind == MemberKind::Property
                    && property.name == contract.name
                    && !property.has_unsupported_property_shape
                    && (property.type_name.as_deref() == Some(contract.type_name.as_str())
                        || property_type_matches(index, owner, descendant, property, contract))
            })
            && descendant.members.iter().any(|method| {
                method.kind == MemberKind::Method
                    && method.name == contract.getter
                    && method.parameter_types.is_empty()
            })
        {
            return true;
        }
        pending.extend(index.direct_subtypes(descendant));
    }
    false
}

fn declaration_is_subtype_of(
    index: &SourceIndex,
    declaration: &Declaration,
    target: &Declaration,
) -> bool {
    if declaration.name == target.name
        && index
            .declaration_source_file(declaration)
            .zip(index.declaration_source_file(target))
            .is_some_and(|(left, right)| left.path == right.path)
    {
        return true;
    }
    let Some(source_file) = index.declaration_source_file(declaration) else {
        return false;
    };
    let target_path = index.declaration_source_file(target).map(|file| &file.path);
    let mut pending = vec![(source_file.path.clone(), declaration.supertypes.clone())];
    let mut visited = HashSet::new();
    while let Some((context_path, supertypes)) = pending.pop() {
        let Some(context) = index.source_file(&context_path) else {
            return false;
        };
        for supertype in supertypes {
            let Some(parent) = index.resolve_type(context, &supertype) else {
                continue;
            };
            let Some(parent_file) = index.declaration_source_file(parent) else {
                continue;
            };
            if parent.name == target.name
                && target_path.is_some_and(|path| path == &parent_file.path)
            {
                return true;
            }
            let key = format!("{}:{}", parent.language as u8, parent.name);
            if visited.insert(key) && parent.language == SourceLanguage::Kotlin {
                pending.push((parent_file.path.clone(), parent.supertypes.clone()));
            }
        }
    }
    false
}

fn imported_type(file: &SourceFile, simple_name: &str) -> Option<String> {
    file.imports
        .iter()
        .find(|name| name.rsplit('.').next() == Some(simple_name))
        .cloned()
}

fn unresolved_type_identity_matches(
    left_file: &SourceFile,
    left: &str,
    right_file: &SourceFile,
    right: &str,
) -> bool {
    match (
        imported_type(left_file, left),
        imported_type(right_file, right),
    ) {
        (Some(left), Some(right)) => left == right,
        (None, None) => left == right && left_file.package == right_file.package,
        _ => false,
    }
}

/// Repair one retained Kotlin source after Java outputs have been written.
/// Only contracts from generated Notlin Java interfaces are considered.
pub(crate) fn repair_file(index: &SourceIndex, path: &Path, source: &str) -> (String, usize) {
    repair_source(index, path, source, &HashSet::new(), &mut HashMap::new())
}

/// Repair a speculative Kotlin source against generated Java overlays that
/// have not been written to disk yet.
pub fn repair_virtual_file(
    index: &SourceIndex,
    path: &Path,
    source: &str,
    generated_java: &HashSet<std::path::PathBuf>,
) -> (String, usize) {
    repair_source(index, path, source, generated_java, &mut HashMap::new())
}

/// Repair all retained speculative sources while sharing component contract
/// discovery. Large inheritance graphs otherwise repeat the same descendant
/// walk once per Kotlin leaf.
pub fn repair_virtual_sources(
    index: &SourceIndex,
    sources: &mut [(std::path::PathBuf, String)],
    generated_java: &HashSet<std::path::PathBuf>,
) -> usize {
    repair_virtual_sources_planned(index, sources, generated_java).count
}

/// Repair speculative Kotlin inputs and return provenance recorded at each
/// concrete accessor-generation operation.
pub(crate) fn repair_virtual_sources_planned(
    index: &SourceIndex,
    sources: &mut [(std::path::PathBuf, String)],
    generated_java: &HashSet<std::path::PathBuf>,
) -> PlannedPropertyRepairs {
    repair_virtual_sources_planned_with_contracts(index, sources, generated_java, &[])
}

pub(crate) fn repair_virtual_sources_planned_with_contracts(
    index: &SourceIndex,
    sources: &mut [(std::path::PathBuf, String)],
    generated_java: &HashSet<std::path::PathBuf>,
    persisted_contracts: &[PersistedPropertyContract],
) -> PlannedPropertyRepairs {
    let persisted_index = PersistedContractIndex::new(index, persisted_contracts);
    let mut contracts = HashMap::new();
    // Discover inheritance contracts once, serially, because declarations in
    // the same component populate a shared memoization cache. Once that graph
    // work is complete each source parse and edit is independent and can use
    // Rayon without repeating expensive descendant walks per worker.
    for (path, _) in sources.iter() {
        let Some(source_file) = index.source_file(path) else {
            continue;
        };
        for declaration in &source_file.declarations {
            contracts_for_repair_indexed(
                index,
                source_file,
                declaration,
                generated_java,
                &persisted_index,
                &mut contracts,
            );
        }
    }
    crate::transpiler::fixpoint::install_parallel(|| {
        sources
            .par_iter_mut()
            .map(|(path, source)| {
                let (repaired, report) = repair_source_cached(index, path, source, &contracts);
                *source = repaired;
                report
            })
            .collect::<Vec<_>>()
    })
    .into_iter()
    .fold(PlannedPropertyRepairs::default(), |mut all, report| {
        all.append(report);
        all
    })
}

fn repair_source(
    index: &SourceIndex,
    path: &Path,
    source: &str,
    generated_java: &HashSet<std::path::PathBuf>,
    contract_cache: &mut HashMap<String, Vec<PropertyContract>>,
) -> (String, usize) {
    let Some(source_file) = index.source_file(path) else {
        return (source.to_string(), 0);
    };
    for declaration in &source_file.declarations {
        contracts_for_repair(
            index,
            source_file,
            declaration,
            generated_java,
            contract_cache,
        );
    }
    let (repaired, report) = repair_source_cached(index, path, source, contract_cache);
    (repaired, report.count)
}

fn repair_source_cached(
    index: &SourceIndex,
    path: &Path,
    source: &str,
    contract_cache: &HashMap<String, Vec<PropertyContract>>,
) -> (String, PlannedPropertyRepairs) {
    let Some(source_file) = index.source_file(path) else {
        return (source.to_string(), PlannedPropertyRepairs::default());
    };
    if source_file.language != SourceLanguage::Kotlin {
        return (source.to_string(), PlannedPropertyRepairs::default());
    }
    let tree = crate::transpiler::parse_tree(source);
    let mut edits = Vec::new();
    let mut report = PlannedPropertyRepairs::default();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if declaration_node(node) {
            let name = declaration_name(node, source);
            let indexed = source_file.declarations.iter().find(|d| d.name == name);
            if let Some(declaration) = indexed {
                let component_key = format!("{}:{}", source_file.path.display(), declaration.name);
                let contracts = contract_cache
                    .get(&component_key)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                if !contracts.is_empty() {
                    repair_declaration(
                        index,
                        node,
                        source,
                        path,
                        declaration,
                        contracts,
                        &mut edits,
                        &mut report,
                    );
                }
                // `repair_declaration` skips nested declarations while it
                // rewrites this declaration's direct properties. Keep walking
                // so a nested type can be repaired from its own supertypes.
            }
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    if edits.is_empty() {
        return (source.to_string(), PlannedPropertyRepairs::default());
    }
    report.count = edits.len();
    (crate::smart_cast::apply_edits(source, edits), report)
}

fn inherited_generated_contracts(
    index: &SourceIndex,
    source_file: &SourceFile,
    declaration: &Declaration,
    generated_java: &HashSet<std::path::PathBuf>,
    persisted_contracts: &PersistedContractIndex<'_>,
) -> Vec<PropertyContract> {
    let mut pending = declaration
        .supertypes
        .iter()
        .cloned()
        .map(|supertype| (source_file.path.clone(), supertype, HashMap::new()))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    let mut by_name = HashMap::<String, PropertyContract>::new();
    while let Some((context_path, supertype, inherited_bindings)) = pending.pop() {
        let Some(context) = index.source_file(&context_path) else {
            continue;
        };
        let Some(resolved_edge) = substitute_type_parameters(&supertype, &inherited_bindings)
        else {
            continue;
        };
        let (resolved_name, supplied_arguments) = split_type_arguments(&resolved_edge);
        let Some(resolved) = index.resolve_type(context, resolved_name) else {
            continue;
        };
        let key = format!(
            "{}:{}:{}",
            resolved.language as u8, resolved.name, resolved_edge
        );
        if !visited.insert(key) {
            continue;
        }
        let Some(file) = index.declaration_source_file(resolved) else {
            continue;
        };
        let resolved_bindings = resolved
            .type_params
            .iter()
            .cloned()
            .zip(
                supplied_arguments
                    .into_iter()
                    .map(|argument| qualify_simple_type_argument(index, context, argument)),
            )
            .collect::<HashMap<_, _>>();
        // A Kotlin declaration repaired in an earlier speculative round no
        // longer has property syntax for ordinary contract discovery. Carry
        // its typed contract through both Kotlin and generated-Java edges,
        // specializing it at each edge just like a source declaration.
        for persisted in persisted_contracts.for_owner(file, resolved) {
            let mut contract = persisted.contract.clone();
            if type_mentions_any_parameter(&contract.type_name, &resolved.type_params) {
                let Some(specialized) =
                    substitute_type_parameters(&contract.type_name, &resolved_bindings)
                else {
                    continue;
                };
                contract.type_name = specialized;
                contract.type_source = context.path.clone();
            }
            by_name.insert(contract.name.clone(), contract);
        }
        if resolved.language == SourceLanguage::Java
            && is_generated_notlin_java(file.path.as_path(), generated_java)
            && let Some(contracts) = java_property_contracts(resolved, &file.path)
        {
            for mut contract in contracts {
                let mentions_parent_parameter =
                    type_mentions_any_parameter(&contract.type_name, &resolved.type_params);
                let Some(specialized) =
                    substitute_type_parameters(&contract.type_name, &resolved_bindings)
                else {
                    continue;
                };
                if mentions_parent_parameter {
                    contract.type_name = specialized;
                    // Edge arguments are expressed in this declaration's
                    // type-variable scope; qualify named arguments above so
                    // the local file remains a safe context for the result.
                    contract.type_source = source_file.path.clone();
                }
                by_name.insert(contract.name.clone(), contract);
            }
        }
        // Generated Java interfaces can themselves extend an earlier
        // generated property contract. Retained Kotlin descendants need the
        // complete inherited getter set, not only the nearest Java interface.
        pending.extend(
            resolved
                .supertypes
                .iter()
                .cloned()
                .map(|supertype| (file.path.clone(), supertype, resolved_bindings.clone())),
        );
    }
    let mut contracts = by_name.into_values().collect::<Vec<_>>();
    contracts.sort_by(|a, b| a.name.cmp(&b.name));
    contracts
}

/// A parallel Kotlin interface has no generated Java ancestor of its own, so
/// ordinary inherited-contract discovery cannot see the Java property that
/// caused the bridge. If one of its retained descendants also inherits a
/// generated Java contract, carry matching properties back to this interface
/// so it and its redeclaring descendants are repaired as one ABI component.
fn contracts_for_repair(
    index: &SourceIndex,
    source_file: &SourceFile,
    declaration: &Declaration,
    generated_java: &HashSet<std::path::PathBuf>,
    cache: &mut HashMap<String, Vec<PropertyContract>>,
) -> Vec<PropertyContract> {
    let persisted_index = PersistedContractIndex::empty();
    contracts_for_repair_indexed(
        index,
        source_file,
        declaration,
        generated_java,
        &persisted_index,
        cache,
    )
}

fn contracts_for_repair_indexed(
    index: &SourceIndex,
    source_file: &SourceFile,
    declaration: &Declaration,
    generated_java: &HashSet<std::path::PathBuf>,
    persisted_contracts: &PersistedContractIndex<'_>,
    cache: &mut HashMap<String, Vec<PropertyContract>>,
) -> Vec<PropertyContract> {
    contracts_for_repair_inner(
        index,
        source_file,
        declaration,
        generated_java,
        persisted_contracts,
        &mut HashSet::new(),
        cache,
    )
}

fn contracts_for_repair_inner(
    index: &SourceIndex,
    source_file: &SourceFile,
    declaration: &Declaration,
    generated_java: &HashSet<std::path::PathBuf>,
    persisted_contracts: &PersistedContractIndex<'_>,
    component: &mut HashSet<String>,
    cache: &mut HashMap<String, Vec<PropertyContract>>,
) -> Vec<PropertyContract> {
    let component_key = format!("{}:{}", source_file.path.display(), declaration.name);
    if let Some(contracts) = cache.get(&component_key) {
        return contracts.clone();
    }
    if !component.insert(component_key.clone()) {
        return Vec::new();
    }
    let mut by_name = inherited_generated_contracts(
        index,
        source_file,
        declaration,
        generated_java,
        persisted_contracts,
    )
    .into_iter()
    .map(|contract| (contract.name.clone(), contract))
    .collect::<HashMap<_, _>>();
    for persisted in persisted_contracts.for_owner(source_file, declaration) {
        by_name
            .entry(persisted.contract.name.clone())
            .or_insert_with(|| persisted.contract.clone());
    }
    let direct_generated_names = by_name.keys().cloned().collect::<HashSet<_>>();

    // A parallel interface can be repaired even though it does not extend the
    // generated Java root. Its descendants still inherit the explicit getter
    // we introduce there, so carry the effective contract down every retained
    // Kotlin supertype edge as well as discovering generated Java ancestors.
    for supertype in &declaration.supertypes {
        let Some(parent) = index.resolve_type(source_file, supertype) else {
            continue;
        };
        if parent.language != SourceLanguage::Kotlin {
            continue;
        }
        let Some(parent_file) = index.declaration_source_file(parent) else {
            continue;
        };
        for contract in contracts_for_repair_inner(
            index,
            parent_file,
            parent,
            generated_java,
            persisted_contracts,
            component,
            cache,
        ) {
            if let Some(specialized) = specialize_contract_properties(
                index,
                parent,
                declaration,
                std::slice::from_ref(&contract),
            )
            .and_then(|mut contracts| contracts.pop())
            {
                by_name
                    .entry(specialized.name.clone())
                    .or_insert(specialized);
            }
        }
    }
    if declaration.kind != DeclarationKind::Interface {
        let mut contracts = by_name.into_values().collect::<Vec<_>>();
        contracts.sort_by(|a, b| a.name.cmp(&b.name));
        cache.insert(component_key, contracts.clone());
        return contracts;
    }

    let mut pending = index.direct_subtypes(declaration);
    let mut visited = HashSet::new();
    while let Some(descendant) = pending.pop() {
        let Some(file) = index.declaration_source_file(descendant) else {
            continue;
        };
        if !visited.insert(format!("{}:{}", file.path.display(), descendant.name)) {
            continue;
        }
        pending.extend(index.direct_subtypes(descendant));
        if descendant.language != SourceLanguage::Kotlin {
            continue;
        }
        let Some(descendant_file) = index.declaration_source_file(descendant) else {
            continue;
        };
        for contract in inherited_generated_contracts(
            index,
            descendant_file,
            descendant,
            generated_java,
            persisted_contracts,
        ) {
            // A direct Java ancestor already supplied the declaration's
            // contract. Readonly covariance checks are for contracts imported
            // from a parallel branch; rechecking a direct declaration here
            // would discard generic, mutable, and other supported direct ABI
            // bridges merely because the descendant has a richer shape.
            if direct_generated_names.contains(&contract.name) {
                continue;
            }
            // Mutable JavaBean contracts carry a setter and use the ordinary
            // bridge path. This covariance closure applies only to readonly
            // scalar properties; applying its fail-closed rules to a direct
            // getter/setter contract would erase a valid setter bridge.
            if contract.setter.is_some() && by_name.contains_key(&contract.name) {
                continue;
            }
            let Some(member) = declaration
                .members
                .iter()
                .find(|member| member.kind == MemberKind::Property && member.name == contract.name)
            else {
                continue;
            };
            if !parallel_readonly_contract_is_bridgeable(
                index,
                source_file,
                declaration,
                member,
                &contract,
            ) {
                // Keep the generated contract available to repair and
                // call-site discovery. The bridge planner independently
                // rejects unsafe parallel shapes; here a failed covariance
                // proof only means we must not widen/adapt the local type.
                by_name.entry(contract.name.clone()).or_insert(contract);
                continue;
            }
            if !member.has_unsupported_property_shape
                && !member.has_unsupported_property_annotations
            {
                by_name.entry(contract.name.clone()).or_insert(contract);
            }
        }
    }
    let mut contracts =
        adapt_parallel_readonly_contracts(index, declaration, by_name.into_values().collect());
    contracts.sort_by(|a, b| a.name.cmp(&b.name));
    cache.insert(component_key, contracts.clone());
    contracts
}

fn has_inherited_setter(index: &SourceIndex, declaration: &Declaration, property: &str) -> bool {
    let Some(source_file) = index.declaration_source_file(declaration) else {
        return false;
    };
    let setter = setter_name(property);
    let mut pending = declaration
        .supertypes
        .iter()
        .cloned()
        .map(|supertype| (source_file.path.clone(), supertype))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while let Some((context_path, supertype)) = pending.pop() {
        let Some(context) = index.source_file(&context_path) else {
            continue;
        };
        let Some(parent) = index.resolve_type(context, &supertype) else {
            continue;
        };
        let Some(parent_file) = index.declaration_source_file(parent) else {
            continue;
        };
        let key = format!(
            "{}:{}:{}",
            parent.language as u8,
            parent_file.path.display(),
            parent.name
        );
        if !visited.insert(key) {
            continue;
        }
        if parent.members.iter().any(|member| {
            (member.kind == MemberKind::Property && member.name == property && member.is_mutable)
                || (member.kind == MemberKind::Method
                    && member.name == setter
                    && member.parameter_types.len() == 1)
        }) {
            return true;
        }
        pending.extend(
            parent
                .supertypes
                .iter()
                .cloned()
                .map(|supertype| (parent_file.path.clone(), supertype)),
        );
    }
    false
}

fn is_generated_notlin_java(path: &Path, generated_java: &HashSet<std::path::PathBuf>) -> bool {
    generated_java.contains(path)
        || std::fs::read_to_string(path)
            .ok()
            .is_some_and(|source| source.starts_with("// NOTLIN: generated from "))
}

fn java_property_contracts(
    declaration: &Declaration,
    type_source: &Path,
) -> Option<Vec<PropertyContract>> {
    if declaration.kind != DeclarationKind::Interface {
        return None;
    }
    java_property_contracts_for_kind(declaration, type_source)
}

fn java_callsite_property_contracts(
    declaration: &Declaration,
    source_file: &SourceFile,
) -> Option<Vec<PropertyContract>> {
    if !matches!(
        declaration.kind,
        DeclarationKind::Interface
            | DeclarationKind::Class
            | DeclarationKind::Enum
            | DeclarationKind::Object
            | DeclarationKind::Record
    ) {
        return None;
    }
    let mut contracts = java_property_contracts_for_kind(declaration, &source_file.path)?;
    contracts.retain(|contract| {
        declaration.members.iter().any(|member| {
            member.kind == MemberKind::Method
                && member.name == contract.getter
                && !member.is_static
                && member.visibility.as_deref() != Some("private")
                && (declaration.kind == DeclarationKind::Interface
                    || member.visibility.as_deref() == Some("public"))
        })
    });
    (!contracts.is_empty()).then_some(contracts)
}

fn java_contract_is_shadowed_by_unrepaired_kotlin_property(
    index: &SourceIndex,
    java_file: &SourceFile,
    java_declaration: &Declaration,
    java_contract: &PropertyContract,
    persisted_contracts: &PersistedContractIndex<'_>,
) -> bool {
    let mut pending = java_declaration
        .supertypes
        .iter()
        .cloned()
        .map(|supertype| (java_file.path.clone(), supertype))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while let Some((context_path, supertype)) = pending.pop() {
        let Some(context) = index.source_file(&context_path) else {
            continue;
        };
        let Some(parent) = index.resolve_type(context, &supertype) else {
            continue;
        };
        let Some(parent_file) = index.declaration_source_file(parent) else {
            continue;
        };
        let key = format!("{}:{}", parent_file.path.display(), parent.name);
        if !visited.insert(key) {
            continue;
        }
        if parent.language == SourceLanguage::Kotlin
            && let Some(property) = parent.members.iter().find(|member| {
                member.kind == MemberKind::Property && member.name == java_contract.name
            })
            && let Some(property_type) = property.type_name.as_deref()
        {
            let was_repaired = persisted_contracts
                .for_owner(parent_file, parent)
                .iter()
                .any(|persisted| {
                    persisted.contract.name == property.name
                        && persisted.contract.getter == java_contract.getter
                });
            if !was_repaired
                && index.property_getter_return_compatible_in_files(
                    &java_contract.type_source,
                    &java_contract.type_name,
                    &parent_file.path,
                    property_type,
                )
            {
                return true;
            }
        }
        pending.extend(
            parent
                .supertypes
                .iter()
                .cloned()
                .map(|parent_supertype| (parent_file.path.clone(), parent_supertype)),
        );
    }
    false
}

fn java_property_contracts_for_kind(
    declaration: &Declaration,
    type_source: &Path,
) -> Option<Vec<PropertyContract>> {
    let methods = declaration
        .members
        .iter()
        .filter(|member| member.kind == MemberKind::Method && !member.is_static)
        .collect::<Vec<_>>();
    if methods.is_empty() {
        return None;
    }
    let mut properties = HashMap::<String, PropertyContract>::new();
    for method in &methods {
        if method.parameter_types.is_empty()
            && let Some((property, getter)) = getter_property(&method.name)
            && let Some(type_name) = method.type_name.as_deref()
            && type_name != "void"
        {
            if properties.contains_key(&property) {
                return None;
            }
            properties.insert(
                property.clone(),
                PropertyContract {
                    name: property,
                    type_name: java_type_to_kotlin(type_name).to_string(),
                    type_source: type_source.to_path_buf(),
                    getter,
                    setter: None,
                },
            );
        }
    }
    for method in methods {
        if method.has_body
            || method.parameter_types.len() != 1
            || method.type_name.as_deref() != Some("void")
        {
            continue;
        }
        if let Some(property) = setter_property(&method.name, properties.keys()) {
            let contract = properties.get_mut(&property)?;
            if contract.type_name != java_type_to_kotlin(&method.parameter_types[0])
                || contract.setter.is_some()
            {
                return None;
            }
            contract.setter = Some(method.name.clone());
        }
    }
    if properties.is_empty() {
        return None;
    }
    let mut properties = properties.into_values().collect::<Vec<_>>();
    properties.sort_by(|a, b| a.name.cmp(&b.name));
    Some(properties)
}

fn java_type_to_kotlin(type_name: &str) -> &str {
    match type_name {
        "boolean" => "Boolean",
        "byte" => "Byte",
        "short" => "Short",
        "int" => "Int",
        "long" => "Long",
        "float" => "Float",
        "double" => "Double",
        "char" => "Char",
        _ => type_name,
    }
}

fn repair_property_origin(
    index: &SourceIndex,
    declaration: &Declaration,
    property_node: Node<'_>,
    source: &str,
    path: &Path,
) -> crate::semantics::SymbolId {
    if property_node.kind() == "class_parameter" {
        // A primary-constructor `val`/`var` has no standalone declaration
        // node in the index; its identity is anchored to the containing class
        // and the parameter name at this exact repair operation.
        let mut origin = crate::semantics::workspace_symbol(index, declaration);
        origin.kind = "property".into();
        origin.name = property_name(property_node, source);
        origin.owner_path.push(declaration.name.clone());
        origin.receiver = None;
        origin.parameters.clear();
        origin
    } else {
        crate::semantics::symbol_id_for_node(source, property_node, path)
    }
}

fn record_repair_accessors(
    report: &mut PlannedPropertyRepairs,
    origin: &crate::semantics::SymbolId,
    contract: &PropertyContract,
    getter: bool,
    setter: bool,
) {
    let mut record = |name: String, parameters: Vec<String>, kind: &str| {
        let generated = crate::semantics::SymbolId {
            module: origin.module.clone(),
            package: origin.package.clone(),
            file: origin.file.clone(),
            owner_path: origin.owner_path.clone(),
            kind: "function".into(),
            name,
            receiver: None,
            parameters,
        };
        let bridge_kind = format!("property-accessor:{kind}");
        let bridge_id = crate::semantics::SymbolId::generated(origin, &bridge_kind);
        report.bridges.push(crate::translation_plan::PlannedBridge {
            id: bridge_id.clone(),
            origin: origin.clone(),
            kind: bridge_kind.clone(),
        });
        report.provenance.push(crate::semantics::OriginMap {
            generated: bridge_id,
            origin: origin.clone(),
            reason: format!("stable identity of generated property {kind}"),
        });
        report.provenance.push(crate::semantics::OriginMap {
            generated,
            origin: origin.clone(),
            reason: format!("generated by retained-property ABI repair ({kind})"),
        });
    };
    if getter {
        record(contract.getter.clone(), Vec::new(), "getter");
    }
    if setter {
        record(
            contract
                .setter
                .clone()
                .unwrap_or_else(|| setter_name(&contract.name)),
            vec![
                contract
                    .type_name
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect(),
            ],
            "setter",
        );
    }
}

// Source context and the two transactional outputs stay explicit at this boundary.
#[allow(clippy::too_many_arguments)]
fn repair_declaration(
    index: &SourceIndex,
    node: Node<'_>,
    source: &str,
    path: &Path,
    declaration: &Declaration,
    contracts: &[PropertyContract],
    edits: &mut Vec<crate::smart_cast::Edit>,
    report: &mut PlannedPropertyRepairs,
) {
    let is_interface = declaration.kind == DeclarationKind::Interface;
    let mut stack = vec![node];
    let mut added_methods = Vec::new();
    while let Some(child) = stack.pop() {
        if matches!(child.kind(), "property_declaration" | "class_parameter") {
            let name = property_name(child, source);
            if let Some(contract) = contracts.iter().find(|contract| contract.name == name) {
                let local_property = declaration
                    .members
                    .iter()
                    .find(|member| member.kind == MemberKind::Property && member.name == name);
                if !is_interface
                    && inherits_retained_kotlin_property(index, declaration, &name, None)
                {
                    // In particular, preserve `override val id` primary-
                    // constructor syntax when a retained Kotlin base class
                    // still owns `id`; turning it into a field/getter bridge
                    // makes Kotlin report that the constructor property hides
                    // the base member.
                    continue;
                }
                if inherits_kotlin_property_on_java_getter_path(
                    index,
                    declaration,
                    &name,
                    &contract.getter,
                ) {
                    // The original Kotlin property already satisfies both
                    // the retained Kotlin override and the JavaBean getter.
                    // Replacing it with a same-named function would satisfy
                    // only the Java branch and break Kotlin inheritance.
                    continue;
                }
                let inferred_contract = if is_interface
                    && local_property.is_some_and(|member| member.type_name.is_none())
                {
                    local_property.and_then(|member| {
                        inferred_interface_property_contract(index, declaration, member, contract)
                    })
                } else {
                    None
                };
                if is_interface
                    && local_property.is_some_and(|member| member.type_name.is_none())
                    && inferred_contract.is_none()
                {
                    // The bridge planner accepts an unannotated default
                    // getter only after proving its exact type through a
                    // typed receiver bound. Keep repair equally conservative.
                    continue;
                }
                let contract = inferred_contract.as_ref().unwrap_or(contract);
                let property_text = node_text(child, source);
                if is_interface {
                    let setter_override = has_inherited_setter(index, declaration, &contract.name);
                    let replacement = interface_property_methods(
                        index,
                        declaration,
                        property_text,
                        contract,
                        setter_override,
                    );
                    if let Some(replacement) = replacement {
                        let origin =
                            repair_property_origin(index, declaration, child, source, path);
                        let (_, mutable) = property_prefix(property_text).unwrap_or(("", false));
                        record_repair_accessors(report, &origin, contract, true, mutable);
                        let owner_type = declaration
                            .package
                            .as_ref()
                            .map(|package| format!("{package}.{}", declaration.name))
                            .unwrap_or_else(|| declaration.name.clone());
                        report.callsite_contracts.push((
                            path.to_path_buf(),
                            PropertyAccessorContract {
                                owner_type,
                                property: contract.name.clone(),
                                getter: contract.getter.clone(),
                                setter: contract.setter.clone(),
                            },
                        ));
                        report.abi_contracts.push(PersistedPropertyContract {
                            owner_file: path.to_path_buf(),
                            owner_name: declaration.name.clone(),
                            owner_package: declaration.package.clone(),
                            owner_kind: declaration.kind,
                            contract: contract.clone(),
                        });
                        report.repairs.push(crate::translation_plan::PlannedRepair {
                            target: origin,
                            kind: "property-abi-repair".into(),
                            detail: format!(
                                "rewrote property {} to Java-compatible accessor declarations",
                                contract.name
                            ),
                        });
                        edits.push(crate::smart_cast::Edit {
                            start: child.start_byte(),
                            end: child.end_byte(),
                            text: replacement,
                        });
                    }
                } else if let Some((replacement, getter, setter)) = class_property_bridge(
                    property_text,
                    contract,
                    contract.setter.is_some()
                        || has_inherited_setter(index, declaration, &contract.name),
                ) {
                    let origin = repair_property_origin(index, declaration, child, source, path);
                    record_repair_accessors(
                        report,
                        &origin,
                        contract,
                        getter.is_some(),
                        setter.is_some(),
                    );
                    report.abi_contracts.push(PersistedPropertyContract {
                        owner_file: path.to_path_buf(),
                        owner_name: declaration.name.clone(),
                        owner_package: declaration.package.clone(),
                        owner_kind: declaration.kind,
                        contract: contract.clone(),
                    });
                    report.repairs.push(crate::translation_plan::PlannedRepair {
                        target: origin,
                        kind: "property-abi-repair".into(),
                        detail: format!(
                            "rewrote property {} and generated explicit accessor bridge(s)",
                            contract.name
                        ),
                    });
                    edits.push(crate::smart_cast::Edit {
                        start: child.start_byte(),
                        end: child.end_byte(),
                        text: replacement,
                    });
                    if let Some(getter) = getter {
                        added_methods.push(getter);
                    }
                    if let Some(setter) = setter {
                        added_methods.push(setter);
                    }
                }
                continue;
            }
        }
        // Only direct class/interface properties and constructor parameters
        // participate. Skip function bodies and nested declarations.
        if child != node && declaration_node(child) {
            continue;
        }
        stack.extend(child.named_children(&mut child.walk()));
    }
    if !is_interface && !added_methods.is_empty() {
        if let Some(body) = node
            .named_children(&mut node.walk())
            .find(|child| matches!(child.kind(), "class_body" | "enum_class_body"))
        {
            let close = body.end_byte().saturating_sub(1);
            let last_enum_entry = last_descendant_of_kind(body, "enum_entry");
            let enum_missing_separator = body.kind() == "enum_class_body"
                && !contains_token(body, ";")
                && last_enum_entry
                    .is_some_and(|entry| source[entry.end_byte()..close].trim().is_empty());
            let first = body.named_children(&mut body.walk()).next();
            let indent = if let Some(first) = first {
                let start = source[..first.start_byte()]
                    .rfind('\n')
                    .map(|index| index + 1)
                    .unwrap_or(0);
                let line = &source[start..first.start_byte()];
                &line[..line.len() - line.trim_start().len()]
            } else {
                "    "
            };
            let newline = if source.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let indent = if indent.is_empty() { "    " } else { indent };
            let separator = format!("{newline}{indent}");
            let methods = added_methods.join(&separator);
            let (start, text) = if enum_missing_separator {
                let last_entry = last_enum_entry.unwrap();
                (
                    last_entry.end_byte(),
                    format!(";{newline}{indent}{methods}{newline}"),
                )
            } else {
                let prefix = if body.kind() == "enum_class_body" && !contains_token(body, ";") {
                    ";"
                } else {
                    ""
                };
                (
                    close,
                    format!("{prefix}{newline}{indent}{methods}{newline}"),
                )
            };
            edits.push(crate::smart_cast::Edit {
                start,
                end: close,
                text,
            });
        } else {
            let newline = if source.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let indent = "    ";
            let separator = format!("{newline}{indent}");
            let text = format!(
                " {{{newline}{indent}{}{newline}}}",
                added_methods.join(&separator),
            );
            edits.push(crate::smart_cast::Edit {
                start: node.end_byte(),
                end: node.end_byte(),
                text,
            });
        }
    }
}

fn inferred_interface_property_contract(
    index: &SourceIndex,
    declaration: &Declaration,
    member: &crate::workspace::Member,
    contract: &PropertyContract,
) -> Option<PropertyContract> {
    let (receiver, selected_property) =
        simple_inferred_property_getter_rhs(index, declaration, &member.name)?;
    if selected_property != member.name {
        return None;
    }
    let empty_bindings = HashMap::new();
    let inferred = inherited_property_type_from_supertypes(
        index,
        declaration,
        declaration,
        &empty_bindings,
        &member.name,
    )?;
    if !inferred_property_matches_receiver_bound(
        index,
        declaration,
        &empty_bindings,
        &receiver,
        &selected_property,
        &inferred,
    ) {
        return None;
    }
    let mut local = contract.clone();
    local.type_name = inferred.0;
    local.type_source = inferred.1;
    Some(local)
}

fn contains_token(node: Node<'_>, token: &str) -> bool {
    if node.kind() == token {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| contains_token(child, token))
}

fn last_descendant_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut last = None;
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == kind
            && last.is_none_or(|previous: Node<'tree>| current.start_byte() > previous.start_byte())
        {
            last = Some(current);
        }
        stack.extend(current.children(&mut current.walk()));
    }
    last
}

fn interface_property_methods(
    index: &SourceIndex,
    declaration: &Declaration,
    text: &str,
    contract: &PropertyContract,
    setter_override: bool,
) -> Option<String> {
    let (prefix, mutable) = property_prefix(text)?;
    if text.contains(" set(") {
        return None;
    }
    let overrides_property = prefix.split_whitespace().any(|word| word == "override");
    let ty = property_type(text).unwrap_or(&contract.type_name);
    let mut output = String::new();
    for annotation in targeted_annotations(prefix, "get") {
        output.push_str(&annotation);
        output.push('\n');
    }
    output.push_str(&format!(
        "{}fun {}(): {ty}",
        if overrides_property { "override " } else { "" },
        contract.getter
    ));
    if let Some(getter_at) = text.find("get()") {
        let body = text[getter_at + "get()".len()..].trim();
        if body.is_empty() {
            return None;
        }
        output.push(' ');
        output.push_str(&rewrite_super_property_access(
            index,
            declaration,
            contract,
            body,
        ));
        return (!mutable).then_some(output);
    }
    if text.contains('=') {
        return None;
    }
    if contract.setter.is_some() && !mutable {
        return None;
    }
    if mutable {
        let setter = contract
            .setter
            .clone()
            .unwrap_or_else(|| setter_name(&contract.name));
        output.push('\n');
        for annotation in targeted_annotations(prefix, "set") {
            output.push_str(&annotation);
            output.push('\n');
        }
        output.push_str(&format!(
            "{}fun {setter}(value: {ty})",
            if setter_override { "override " } else { "" }
        ));
    }
    Some(output)
}

/// Once a Kotlin property contract becomes a Java getter, a retained Kotlin
/// interface can no longer use `super.property` to delegate its implementation.
/// Keep the original default-provider semantics by invoking the retained
/// Kotlin superinterface's repaired getter explicitly.
fn rewrite_super_property_access(
    index: &SourceIndex,
    declaration: &Declaration,
    contract: &PropertyContract,
    body: &str,
) -> String {
    let Some(source_file) = index.declaration_source_file(declaration) else {
        return body.to_string();
    };

    let mut providers = Vec::new();
    for supertype in &declaration.supertypes {
        let Some(parent) = index.resolve_type(source_file, supertype) else {
            continue;
        };
        if parent.language != SourceLanguage::Kotlin || parent.kind != DeclarationKind::Interface {
            continue;
        }
        let supplies_default = parent.members.iter().any(|member| {
            member.has_body
                && ((member.kind == MemberKind::Property && member.name == contract.name)
                    || (member.kind == MemberKind::Method && member.name == contract.getter))
        });
        if supplies_default {
            providers.push(parent.name.clone());
        }
    }

    let property = &contract.name;
    let getter = &contract.getter;
    let mut rewritten = body.to_string();
    for parent in &providers {
        rewritten = rewritten.replace(
            &format!("super<{parent}>.{property}"),
            &format!("super<{parent}>.{getter}()"),
        );
    }
    if providers.len() == 1 {
        rewritten = rewritten.replace(
            &format!("super.{property}"),
            &format!("super<{}>.{getter}()", providers[0]),
        );
    }
    rewritten
}

fn class_property_bridge(
    text: &str,
    contract: &PropertyContract,
    setter_override: bool,
) -> Option<(String, Option<String>, Option<String>)> {
    let (prefix, mutable) = property_prefix(text)?;
    let ty = property_type(text)?;
    if !prefix.split_whitespace().any(|word| word == "override")
        || text.contains(" by ")
        || prefix
            .split_whitespace()
            .any(|modifier| matches!(modifier, "open" | "lateinit" | "abstract"))
        || (contract.setter.is_some() && !mutable)
    {
        return None;
    }
    let mut replacement = text.to_string();
    let val_at = replacement
        .find("val ")
        .or_else(|| replacement.find("var "))?;
    let before = &replacement[..val_at];
    let mut cleaned = before
        .split_whitespace()
        .filter(|word| {
            *word != "override" && !word.starts_with("@get:") && !word.starts_with("@set:")
        })
        .collect::<Vec<_>>()
        .join(" ");
    if !cleaned.is_empty() {
        cleaned.push(' ');
    }
    let custom_accessors = text.contains("get()") || text.contains("set(");
    if custom_accessors {
        cleaned.push_str(&format!(
            "@get:kotlin.jvm.JvmName(\"notlinProperty{}\") ",
            contract.getter
        ));
        if let Some(setter) = &contract.setter {
            cleaned.push_str(&format!(
                "@set:kotlin.jvm.JvmName(\"notlinProperty{}\") ",
                setter
            ));
        }
    } else {
        // JPA all-open can make entity properties open during compilation even
        // when the source omits `open`; Kotlin rejects @JvmField on that ABI.
        // An explicit final modifier keeps the bridge field-backed under that
        // plugin while the generated accessor method supplies the Java ABI.
        cleaned.push_str("final @JvmField ");
    }
    replacement = format!("{}{}", cleaned, &replacement[val_at..]);
    let getter_annotations = targeted_annotations(prefix, "get")
        .into_iter()
        .chain(custom_accessor_annotations(text, "get()"))
        .map(|annotation| format!("{annotation}\n"))
        .collect::<String>();
    let getter = format!(
        "{getter_annotations}override fun {}(): {ty} = {}",
        contract.getter, contract.name
    );
    let setter = mutable.then(|| {
        let setter = contract
            .setter
            .clone()
            .unwrap_or_else(|| setter_name(&contract.name));
        let annotations = targeted_annotations(prefix, "set")
            .into_iter()
            .chain(custom_accessor_annotations(text, "set("))
            .map(|annotation| format!("{annotation}\n"))
            .collect::<String>();
        format!(
            "{annotations}{}{setter}(value: {ty}) {{ {} = value }}",
            if setter_override {
                "override fun "
            } else {
                "fun "
            },
            contract.name
        )
    });
    Some((replacement, Some(getter), setter))
}

fn property_prefix(text: &str) -> Option<(&str, bool)> {
    let val = text.find("val ");
    let var = text.find("var ");
    let (at, mutable) = match (val, var) {
        (Some(a), Some(b)) if b < a => (b, true),
        (Some(a), Some(_)) => (a, false),
        (Some(a), None) => (a, false),
        (None, Some(b)) => (b, true),
        (None, None) => return None,
    };
    Some((&text[..at], mutable))
}

fn property_type(text: &str) -> Option<&str> {
    let marker = text.find("val ").or_else(|| text.find("var "))?;
    let declaration = &text[marker..];
    let (_, after) = declaration.split_once(':')?;
    let after = after.trim_start();
    let mut angle_depth = 0usize;
    let mut round_depth = 0usize;
    let mut square_depth = 0usize;
    let mut end = after.len();
    for (at, ch) in after.char_indices() {
        match ch {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '(' => round_depth += 1,
            ')' => round_depth = round_depth.saturating_sub(1),
            '[' => square_depth += 1,
            ']' => square_depth = square_depth.saturating_sub(1),
            '=' | ',' | '\n' | '\r' | '{'
                if angle_depth == 0 && round_depth == 0 && square_depth == 0 =>
            {
                end = at;
                break;
            }
            _ => {}
        }
    }
    let mut ty = after[..end].trim();
    if let Some(accessor) = [" get()", " set(", " get"]
        .iter()
        .filter_map(|marker| ty.find(marker))
        .min()
    {
        ty = ty[..accessor].trim();
    }
    (!ty.is_empty()).then_some(ty)
}

fn property_name(node: Node<'_>, source: &str) -> String {
    let val = node_text(node, source)
        .find("val ")
        .map(|offset| offset + 4);
    let var = node_text(node, source)
        .find("var ")
        .map(|offset| offset + 4);
    let start = match (val, var) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => return String::new(),
    };
    let name_end = node_text(node, source)[start..]
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '`'))
        .map(|offset| start + offset)
        .unwrap_or(node.end_byte() - node.start_byte());
    node_text(node, source)[start..name_end]
        .trim_matches('`')
        .to_string()
}

fn targeted_annotations(prefix: &str, target: &str) -> Vec<String> {
    prefix
        .split_whitespace()
        .filter_map(|token| {
            token
                .strip_prefix(&format!("@{target}:"))
                .map(|annotation| format!("@{annotation}"))
        })
        .collect()
}

fn custom_accessor_annotations(text: &str, accessor: &str) -> Vec<String> {
    let Some(property_at) = text.find("val ").or_else(|| text.find("var ")) else {
        return Vec::new();
    };
    let Some(accessor_at) = text.find(accessor) else {
        return Vec::new();
    };
    if accessor_at <= property_at {
        return Vec::new();
    }
    text[property_at..accessor_at]
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with('@') && !line.starts_with("@get:") && !line.starts_with("@set:")
        })
        .map(ToOwned::to_owned)
        .collect()
}

fn declaration_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_declaration"
            | "interface_declaration"
            | "object_declaration"
            | "enum_class_declaration"
    )
}

fn declaration_kind_matches(kind: DeclarationKind, node: Node<'_>) -> bool {
    match node.kind() {
        "object_declaration" => kind == DeclarationKind::Object,
        "interface_declaration" => kind == DeclarationKind::Interface,
        "enum_class_declaration" | "enum_declaration" => kind == DeclarationKind::Enum,
        "class_declaration" => {
            let direct_children = node.children(&mut node.walk()).collect::<Vec<_>>();
            let is_interface = direct_children
                .iter()
                .any(|child| child.kind() == "interface");
            let modifiers = direct_children
                .iter()
                .find(|child| child.kind() == "modifiers");
            let has_class_modifier = |modifier: &str| {
                modifiers.is_some_and(|modifiers| {
                    modifiers
                        .children(&mut modifiers.walk())
                        .filter(|child| child.kind() == "class_modifier")
                        .any(|class_modifier| {
                            class_modifier
                                .children(&mut class_modifier.walk())
                                .any(|child| child.kind() == modifier)
                        })
                })
            };
            if has_class_modifier("annotation") {
                kind == DeclarationKind::Annotation
            } else if is_interface {
                kind == DeclarationKind::Interface
            } else if has_class_modifier("enum") {
                kind == DeclarationKind::Enum
            } else {
                kind == DeclarationKind::Class
            }
        }
        _ => false,
    }
}

fn declaration_name(node: Node<'_>, source: &str) -> String {
    node.named_children(&mut node.walk())
        .find(|child| child.kind() == "type_identifier" || child.kind() == "identifier")
        .map(|name| node_text(name, source).to_string())
        .unwrap_or_default()
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

fn getter_property(method: &str) -> Option<(String, String)> {
    for prefix in ["get", "is"] {
        let Some(rest) = method.strip_prefix(prefix) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        // Kotlin boolean properties whose source name starts with `is` keep
        // that name in the JVM ABI (`val isEnabled` -> `isEnabled()`). Java's
        // synthetic Kotlin property name follows the same convention, so
        // `isEnabled()` maps to `isEnabled`, while `getEnabled()` maps to
        // `enabled`.
        if prefix == "is" {
            return Some((method.to_string(), method.to_string()));
        }
        let mut chars = rest.chars();
        let first = chars.next()?;
        let property =
            if first.is_uppercase() && chars.clone().next().is_some_and(char::is_uppercase) {
                rest.to_string()
            } else {
                format!("{}{}", first.to_lowercase(), chars.as_str())
            };
        return Some((property, method.to_string()));
    }
    None
}

fn setter_property<'a>(
    method: &str,
    known: impl Iterator<Item = &'a String> + Clone,
) -> Option<String> {
    let rest = method.strip_prefix("set")?;
    let mut chars = rest.chars();
    let first = chars.next()?;
    let plain = if first.is_uppercase() && chars.clone().next().is_some_and(char::is_uppercase) {
        rest.to_string()
    } else {
        format!("{}{}", first.to_lowercase(), chars.as_str())
    };
    if known.clone().any(|name| name == &plain) {
        Some(plain)
    } else {
        let is_property = format!("is{}", rest);
        known
            .into_iter()
            .any(|name| name == &is_property)
            .then_some(is_property)
    }
}

fn setter_name(property: &str) -> String {
    let property = property
        .strip_prefix("is")
        .filter(|rest| rest.chars().next().is_some_and(char::is_uppercase))
        .unwrap_or(property);
    let mut chars = property.chars();
    match chars.next() {
        Some(first) => format!("set{}{}", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

fn bridged_setter_name(contract: &PropertyContract, is_mutable: bool) -> Option<String> {
    contract
        .setter
        .clone()
        .or_else(|| is_mutable.then(|| setter_name(&contract.name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new() -> Self {
            let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("notlin-property-abi-{}-{id}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, relative: &str, source: &str) -> std::path::PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, source).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn api_java() -> &'static str {
        "// NOTLIN: generated from Api.kt\npackage sample;\npublic interface Api { String getName(); boolean getEnabled(); void setEnabled(boolean value); }\n"
    }

    #[test]
    fn java_is_getter_preserves_is_property_name_and_matches_setter() {
        let fixture = Fixture::new();
        let java = fixture.write(
            "java/Api.java",
            "// NOTLIN: generated from Api.kt\npackage sample;\npublic interface Api { boolean isEnabled(); void setEnabled(boolean value); }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let declaration = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();

        let contracts = java_property_contracts(declaration, &java).unwrap();
        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].name, "isEnabled");
        assert_eq!(contracts[0].getter, "isEnabled");
        assert_eq!(contracts[0].setter.as_deref(), Some("setEnabled"));
    }

    #[test]
    fn repairs_property_inherited_from_generated_java_default_getter() {
        let fixture = Fixture::new();
        fixture.write(
            "java/IResourceVariant.java",
            "// NOTLIN: generated from IResourceVariant.kt\npackage sample;\npublic interface IResourceVariant { default Class<?> getResourceClass() { return Object.class; } }\n",
        );
        let kotlin = fixture.write(
            "kotlin/ResourceVariant.kt",
            "package sample\ninterface ResourceVariant : IResourceVariant { override val resourceClass: Class<*> }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&kotlin).unwrap();
        let (repaired, count) = repair_file(&index, &kotlin, &source);

        assert_eq!(
            count, 1,
            "default getter contract was not discovered: {repaired}"
        );
        assert!(
            repaired.contains("override fun getResourceClass(): Class<*>"),
            "retained Kotlin subtype must implement the inherited default Java getter explicitly:\n{repaired}"
        );
        assert!(
            !repaired.contains("override val resourceClass"),
            "{repaired}"
        );
    }

    #[test]
    fn repairs_inferred_generic_default_getter_with_its_local_type_parameter() {
        let fixture = Fixture::new();
        fixture.write(
            "java/IObjectEvent.java",
            "// NOTLIN: generated from IObjectEvent.kt\npackage sample;\npublic interface IObjectEvent<T, I> { I getId(); }\n",
        );
        let kotlin = fixture.write(
            "kotlin/ObjectCreated.kt",
            "package sample\n\
             interface HasObjectId<I> {\n    val id: I\n}\n\
             interface IdAware<I> {\n    val id: I\n}\n\
             interface ObjectCreated<T : HasObjectId<I>, I> : IObjectEvent<T, I>, IdAware<I> {\n\
                 val payload: T\n\
                 override val id\n\
                     get() = payload.id\n\
             }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&kotlin).unwrap();
        let object_created = index
            .declarations()
            .find(|declaration| declaration.name == "ObjectCreated")
            .unwrap();
        let id_member = object_created
            .members
            .iter()
            .find(|member| member.kind == MemberKind::Property && member.name == "id")
            .unwrap();
        let object_file = index.declaration_source_file(object_created).unwrap();
        let mut cache = HashMap::new();
        let inherited_contracts = contracts_for_repair(
            &index,
            object_file,
            object_created,
            &HashSet::new(),
            &mut cache,
        );
        let inherited_id = inherited_contracts
            .iter()
            .find(|contract| contract.name == "id")
            .unwrap();
        let inferred_contract =
            inferred_interface_property_contract(&index, object_created, id_member, inherited_id)
                .expect("the typed receiver bound should prove the local generic id return");
        let rendered = interface_property_methods(
            &index,
            object_created,
            "override val id\n        get() = payload.id",
            &inferred_contract,
            false,
        );
        assert_eq!(
            rendered.as_deref(),
            Some("override fun getId(): I = payload.id"),
            "the inferred getter contract should render the local generic type"
        );

        let (repaired, count) = repair_file(&index, &kotlin, &source);

        assert_eq!(
            count, 2,
            "the inferred getter and parallel id contract must both be repaired: {repaired}"
        );
        assert!(
            repaired.contains("override fun getId(): I = payload.id"),
            "the repaired getter must preserve the interface's own type parameter:\n{repaired}"
        );
        assert!(
            !repaired.contains("getId(): HasObjectId")
                && !repaired.contains("getId(): ObjectCreated"),
            "do not widen the getter to its receiver bound or a concrete child type:\n{repaired}"
        );
    }

    #[test]
    fn inferred_getter_resolves_inherited_generic_receiver_and_bound_member() {
        let fixture = Fixture::new();
        let kotlin = fixture.write(
            "kotlin/LookupObject.kt",
            "package sample\n\
             interface LookupIdAware {\n    val lookupId: String\n}\n\
             interface LookupObjectId : LookupIdAware\n\
             interface BaseObject<T : LookupObjectId> {\n    val id: T\n}\n\
             interface LookupObject<T : LookupObjectId> : BaseObject<T>, LookupObjectId {\n\
                 override val lookupId\n\
                     get() = id.lookupId\n\
             }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let declaration = index
            .declarations()
            .find(|declaration| declaration.name == "LookupObject")
            .unwrap();
        let member = declaration
            .members
            .iter()
            .find(|member| member.kind == MemberKind::Property && member.name == "lookupId")
            .unwrap();
        let empty_bindings = HashMap::new();
        assert_eq!(
            simple_inferred_property_getter_rhs(&index, declaration, "lookupId"),
            Some(("id".to_string(), "lookupId".to_string()))
        );
        let inferred_parent_type = inherited_property_type_from_supertypes(
            &index,
            declaration,
            declaration,
            &empty_bindings,
            "lookupId",
        )
        .expect("LookupIdAware provides the inherited lookupId type");
        assert_eq!(inferred_parent_type.0, "String");
        assert!(
            inferred_property_matches_receiver_bound(
                &index,
                declaration,
                &empty_bindings,
                "id",
                "lookupId",
                &inferred_parent_type,
            ),
            "id:T must be found through BaseObject<T>, and T's bound supplies lookupId:String"
        );
        let contract = PropertyContract {
            name: "lookupId".to_string(),
            type_name: "String".to_string(),
            type_source: index
                .declaration_source_file(declaration)
                .unwrap()
                .path
                .clone(),
            getter: "getLookupId".to_string(),
            setter: None,
        };

        let inferred = inferred_interface_property_contract(&index, declaration, member, &contract)
            .expect("the inherited id:T property and T's bound prove lookupId:String");

        assert_eq!(inferred.type_name, "String");
        assert!(matches!(
            interface_property_methods(
                &index,
                declaration,
                "override val lookupId\n        get() = id.lookupId",
                &inferred,
                false,
            )
            .as_deref(),
            Some("override fun getLookupId(): String = id.lookupId")
        ));
        let hidden_fixture = Fixture::new();
        hidden_fixture.write(
            "foreign/LookupObjectId.kt",
            "package foreign\ninterface LookupObjectId {\n    val lookupId: String\n}\n",
        );
        let hidden_bound = hidden_fixture.write(
            "hidden/LookupObject.kt",
            "package hidden\n\
             interface LookupIdAware {\n    val lookupId: String\n}\n\
             interface BaseObject<T> {\n    val id: T\n}\n\
             interface LookupObject<T : LookupObjectId> : BaseObject<T>, LookupIdAware {\n\
                 override val lookupId\n\
                     get() = id.lookupId\n\
             }\n",
        );
        let hidden_index = SourceIndex::discover(&hidden_fixture.0).unwrap();
        let hidden_decl = hidden_index
            .declarations()
            .find(|declaration| {
                declaration.name == "LookupObject"
                    && declaration.package.as_deref() == Some("hidden")
            })
            .unwrap();
        let hidden_member = hidden_decl
            .members
            .iter()
            .find(|member| member.kind == MemberKind::Property && member.name == "lookupId")
            .unwrap();
        let hidden_file = hidden_index.declaration_source_file(hidden_decl).unwrap();
        assert!(
            hidden_index
                .resolve_type(hidden_file, "LookupObjectId")
                .is_some_and(|declaration| { declaration.package.as_deref() == Some("foreign") }),
            "fixture must exercise the unique-name fallback for an unimported foreign bound"
        );
        let hidden_contract = PropertyContract {
            name: "lookupId".to_string(),
            type_name: "String".to_string(),
            type_source: hidden_file.path.clone(),
            getter: "getLookupId".to_string(),
            setter: None,
        };
        assert!(
            inferred_interface_property_contract(
                &hidden_index,
                hidden_decl,
                hidden_member,
                &hidden_contract,
            )
            .is_none(),
            "a globally unique but unimported foreign bound cannot justify an inferred getter: {}",
            fs::read_to_string(hidden_bound).unwrap()
        );

        let duplicate_fixture = Fixture::new();
        duplicate_fixture.write(
            "kotlin/DuplicateOwners.kt",
            "package duplicate\n\
             interface LookupIdAware {\n    val lookupId: String\n}\n\
             interface LookupObjectId : LookupIdAware\n\
             interface BaseObject<T : LookupObjectId> {\n    val id: T\n}\n\
             interface LookupObject<T : LookupObjectId> : BaseObject<T>, LookupObjectId {\n\
                 override val lookupId\n\
                     get() = id.lookupId\n\
             }\n\
             class Holder {\n\
                 interface LookupObject<T : LookupObjectId> : BaseObject<T>, LookupObjectId {\n\
                     override val lookupId\n\
                         get() = id.lookupId\n\
                 }\n\
             }\n",
        );
        let duplicate_index = SourceIndex::discover(&duplicate_fixture.0).unwrap();
        let duplicate_owner = duplicate_index
            .declarations()
            .find(|declaration| declaration.name == "LookupObject")
            .unwrap();
        let duplicate_member = duplicate_owner
            .members
            .iter()
            .find(|member| member.kind == MemberKind::Property && member.name == "lookupId")
            .unwrap();
        assert!(
            simple_inferred_property_getter_rhs(
                &duplicate_index,
                duplicate_owner,
                &duplicate_member.name,
            )
            .is_none(),
            "same-named nested declarations must not borrow one another's getter evidence"
        );
        let _ = kotlin;
    }

    #[test]
    fn bare_generic_default_getter_is_rewritten_as_abstract_java_method() {
        let fixture = Fixture::new();
        fixture.write(
            "java/PayloadApi.java",
            "// NOTLIN: generated from PayloadApi.kt\npackage sample;\npublic interface PayloadApi<T> { T getPayload(); }\n",
        );
        let kotlin = fixture.write(
            "kotlin/DefaultPayload.kt",
            "package sample\ninterface DefaultPayload<T> : PayloadApi<T> { override val payload: T get }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let declaration = index
            .declarations()
            .find(|declaration| declaration.name == "DefaultPayload")
            .unwrap();
        let payload = declaration
            .members
            .iter()
            .find(|member| member.name == "payload")
            .expect("property should remain indexed");
        assert!(!payload.has_unsupported_property_shape);

        let source = fs::read_to_string(&kotlin).unwrap();
        let (rewritten, count) = repair_file(&index, &kotlin, &source);
        assert_eq!(count, 1, "default getter was not bridged: {rewritten}");
        assert!(
            rewritten.contains("override fun getPayload(): T"),
            "the redundant source accessor should become an abstract Java getter contract:\n{rewritten}"
        );
    }

    #[test]
    fn specializes_inherited_generated_getter_contracts_to_local_type_parameters() {
        let fixture = Fixture::new();
        fixture.write(
            "java/GenericApi.java",
            "// NOTLIN: generated from GenericApi.kt\npackage sample;\npublic interface GenericApi<T> { T getPayload(); }\n",
        );
        let kotlin = fixture.write(
            "kotlin/GenericChild.kt",
            "package sample\ninterface GenericChild<X> : GenericApi<X> { override val payload: X }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&kotlin).unwrap();

        let (repaired, count) = repair_file(&index, &kotlin, &source);

        assert_eq!(
            count, 1,
            "generic getter contract was not found: {repaired}"
        );
        assert!(
            repaired.contains("override fun getPayload(): X"),
            "the inherited Java getter must use the child interface's type parameter:\n{repaired}"
        );
    }

    #[test]
    fn getter_only_method_interfaces_are_jvm_compatible_candidates() {
        let fixture = Fixture::new();
        fixture.write(
            "GetterApi.kt",
            "package sample\ninterface GetterApi<T> { fun getPayload(): T; fun isReady(): Boolean; fun getLookupId(): String }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "GetterApi")
            .unwrap();
        assert!(is_getter_method_interface_candidate(target));
        fixture.write(
            "Unsafe.kt",
            "package sample\ninterface Unsafe { fun getPayload(): List<String> }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let unsupported_target = index
            .declarations()
            .find(|declaration| declaration.name == "Unsafe")
            .unwrap();
        assert!(!is_getter_method_interface_candidate(unsupported_target));
    }

    #[test]
    fn repairs_retained_interfaces_and_constructor_only_classes() {
        let fixture = Fixture::new();
        let java = fixture.write("java/Api.java", api_java());
        let kotlin = fixture.write(
            "kotlin/Impl.kt",
            "package sample\ninterface Child : Api { override val name: String; override var enabled: Boolean }\ndata class Impl(override val name: String, override var enabled: Boolean) : Child\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&kotlin).unwrap();
        let (repaired, count) = repair_file(&index, &kotlin, &source);
        assert_eq!(
            count, 5,
            "two interface properties and two class properties"
        );
        assert!(repaired.contains("override fun getName(): String"));
        assert!(repaired.contains("override fun getEnabled(): Boolean"));
        assert!(repaired.contains("override fun setEnabled(value: Boolean)"));
        assert!(repaired.contains("final @JvmField val name: String"));
        assert!(repaired.contains("final @JvmField var enabled: Boolean"));
        assert!(repaired.contains("override fun getName(): String = name"));
        assert!(repaired.contains("override fun setEnabled(value: Boolean) { enabled = value }"));
        assert!(repaired.contains(" : Child {\n    override fun "));
        assert!(java.exists());
    }

    #[test]
    fn enum_property_repair_inserts_required_entry_semicolon() {
        let fixture = Fixture::new();
        fixture.write("java/Api.java", api_java());
        let kotlin = fixture.write(
            "kotlin/Impl.kt",
            "package sample\nenum class Impl(override val name: String) : Api { ONE(\"one\") }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&kotlin).unwrap();
        let (repaired, count) = repair_file(&index, &kotlin, &source);

        assert_eq!(
            count, 2,
            "property conversion plus generated getter: {repaired}"
        );
        assert!(
            repaired.contains("ONE(\"one\");\n    override fun getName(): String = name"),
            "generated enum methods must follow a semicolon-terminated constants section:\n{repaired}"
        );
        assert!(
            !crate::transpiler::parse_tree(&repaired)
                .root_node()
                .has_error(),
            "repaired enum must remain valid Kotlin:\n{repaired}"
        );
    }

    #[test]
    fn repairs_nested_class_implementations_independently_of_the_outer_class() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface IReferenceNoAware { val referenceNo: String }\n",
        );
        let implementation = fixture.write(
            "Outer.kt",
            "package sample\nclass Outer(override val referenceNo: String) : IReferenceNoAware {\n    class Nested(override val referenceNo: String) : IReferenceNoAware\n}\n",
        );
        let planning_index = SourceIndex::discover(&fixture.0).unwrap();
        let target = planning_index
            .declarations()
            .find(|declaration| declaration.name == "IReferenceNoAware")
            .unwrap();
        let retained = retained_names(&planning_index, &["Outer", "Nested"]);
        let result = retained_subtypes_bridge(
            &planning_index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        );
        assert!(result.is_ok(), "{result:?}");

        fs::remove_file(fixture.0.join("Api.kt")).unwrap();
        fixture.write(
            "translated/IReferenceNoAware.java",
            "// NOTLIN: generated from IReferenceNoAware.kt\npackage sample;\npublic interface IReferenceNoAware { String getReferenceNo(); }\n",
        );
        let repair_index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&implementation).unwrap();
        let (rewritten, count) = repair_file(&repair_index, &implementation, &source);
        assert_eq!(count, 4, "{rewritten}");
        assert_eq!(
            rewritten.matches("final @JvmField val referenceNo").count(),
            2
        );
        assert_eq!(
            rewritten
                .matches("override fun getReferenceNo(): String = referenceNo")
                .count(),
            2
        );
    }

    fn planner_fixture(
        descendant: &str,
    ) -> (Fixture, SourceIndex, HashSet<crate::semantics::SymbolId>) {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface Api { val name: String; var enabled: Boolean }\n",
        );
        fixture.write("Desc.kt", descendant);
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let retained = index
            .declarations()
            .filter(|declaration| declaration.name != "Api")
            .map(|declaration| crate::semantics::workspace_symbol(&index, declaration))
            .collect();
        (fixture, index, retained)
    }

    fn retained_names(index: &SourceIndex, names: &[&str]) -> HashSet<crate::semantics::SymbolId> {
        index
            .declarations()
            .filter(|d| names.contains(&d.name.as_str()))
            .map(|d| crate::semantics::workspace_symbol(index, d))
            .collect()
    }

    #[test]
    fn planner_bridges_properties_on_interfaces_that_also_have_methods() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface Api { val alias: String; fun key(): String = alias }\n",
        );
        fixture.write(
            "Impl.kt",
            "package sample\nenum class Impl(override val alias: String) : Api { ONE(\"one\") }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let retained = index
            .declarations()
            .filter(|d| d.name == "Impl")
            .map(|d| crate::semantics::workspace_symbol(&index, d))
            .collect();

        assert!(is_property_interface_candidate(target));
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");

        let implementation = fixture.0.join("Impl.kt");
        let source = fs::read_to_string(&implementation).unwrap();
        let (_repaired, count) = repair_file(&index, &implementation, &source);
        assert_eq!(count, 0, "repair requires the translated Java overlay");

        fs::remove_file(fixture.0.join("Api.kt")).unwrap();
        fixture.write(
            "translated/Api.java",
            "// NOTLIN: generated from Api.kt\npackage sample;\npublic interface Api { String getAlias(); default String key() { return getAlias(); } }\n",
        );
        let repair_index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&implementation).unwrap();
        let (repaired, count) = repair_file(&repair_index, &implementation, &source);
        assert_eq!(count, 2, "{repaired}");
        assert!(repaired.contains("final @JvmField val alias: String"));
        assert!(repaired.contains("override fun getAlias(): String = alias"));
        assert!(!repaired.contains("} {"), "{repaired}");
    }

    #[test]
    fn planner_rejects_constructor_field_bridge_that_hides_retained_base_property() {
        let fixture = Fixture::new();
        fixture.write(
            "Types.kt",
            "package sample\ninterface Identified {\n    val id: String\n}\nabstract class ParentRecord {\n    abstract val id: String\n}\nclass ChildRecord(override val id: String) : ParentRecord(), Identified\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Identified")
            .unwrap();
        let retained = retained_names(&index, &["ParentRecord", "ChildRecord"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.reason.contains("retained Kotlin property")),
            "field-backed getter repair must be rejected while ParentRecord owns id: {result:?}"
        );
    }

    #[test]
    fn planner_accepts_scalar_covariance_across_parallel_readonly_contracts() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface BaseId {}\ninterface ChildId : BaseId {}\ninterface ResourceEvent<T> {\n    val id: T\n}\n",
        );
        fixture.write(
            "Parallel.kt",
            "package contracts\nimport sample.BaseId\ninterface SpecializedEvent<T : BaseId> {\n    val id: T\n        get() = defaultId()\n    fun defaultId(): T\n}\n",
        );
        fixture.write(
            "Impl.kt",
            "package sample\nimport contracts.SpecializedEvent\nclass Impl(override val id: ChildId) : ResourceEvent<ChildId>, SpecializedEvent<BaseId> {\n    override fun defaultId(): BaseId = id\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "ResourceEvent")
            .unwrap();
        let retained = retained_names(&index, &["Impl"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");

        let parallel = index
            .declarations()
            .find(|declaration| declaration.name == "SpecializedEvent")
            .unwrap();
        let adapted = adapt_parallel_readonly_contracts(
            &index,
            parallel,
            vec![PropertyContract {
                name: "id".into(),
                type_name: "ChildId".into(),
                type_source: fixture.0.join("Api.kt"),
                getter: "getId".into(),
                setter: None,
            }],
        );
        assert_eq!(adapted[0].type_name, "T");
        assert_eq!(
            adapted[0].type_source,
            fs::canonicalize(fixture.0.join("Parallel.kt")).unwrap()
        );
    }

    #[test]
    fn planner_infers_unannotated_parallel_getter_from_instantiated_parent_contract() {
        let fixture = Fixture::new();
        fixture.write(
            "Events.kt",
            "package sample\ninterface BaseId {}\nclass ChildId : BaseId {}\ninterface RootEvent<T> {\n    val id: T\n}\ninterface HasId<I> {\n    val id: I\n}\nclass Info(override val id: BaseId) : HasId<BaseId>\ninterface CreatedEvent<T : HasId<I>, I : BaseId> : HasId<I> {\n    val payload: T\n    override val id\n        get() = payload.id\n}\nclass Concrete(override val id: ChildId, override val payload: Info) : RootEvent<ChildId>, CreatedEvent<Info, BaseId>\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "RootEvent")
            .unwrap();
        let retained = retained_names(&index, &["Concrete"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn planner_rejects_unannotated_getter_narrower_than_ancestor_contract() {
        let fixture = Fixture::new();
        fixture.write(
            "Events.kt",
            "package sample\ninterface BaseId {}\nclass ChildId : BaseId {}\ninterface HasId<I> {\n    val id: I\n}\nclass NarrowInfo(override val id: ChildId) : HasId<BaseId>\ninterface RootEvent {\n    val id: BaseId\n}\ninterface CreatedEvent : HasId<BaseId> {\n    val payload: NarrowInfo\n    override val id\n        get() = payload.id\n}\nclass Concrete(override val payload: NarrowInfo) : RootEvent, CreatedEvent\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "RootEvent")
            .unwrap();
        let retained = retained_names(&index, &["Concrete"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(
            result.is_err(),
            "narrow inferred getter was accepted: {result:?}"
        );
    }

    #[test]
    fn planner_accepts_inherited_narrow_default_for_parallel_broad_contract() {
        let fixture = Fixture::new();
        fixture.write(
            "Events.kt",
            "package sample\nopen class BaseId\nclass ChildId : BaseId()\ninterface EventRoot<T> {\n    val id: T\n}\ninterface JobEvent : EventRoot<ChildId> {\n    override val id: ChildId\n        get() = ChildId()\n}\ninterface BroadEvent {\n    val id: BaseId\n}\nclass Concrete(override val payload: String) : JobEvent, BroadEvent\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "EventRoot")
            .unwrap();
        let retained = retained_names(&index, &["JobEvent", "Concrete"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn planner_accepts_broader_readonly_ancestor_contract_for_narrow_root() {
        let fixture = Fixture::new();
        fixture.write(
            "Events.kt",
            "package sample\ninterface BaseId {}\nclass ChildId : BaseId {}\ninterface RootEvent {\n    val id: ChildId\n}\ninterface ObjectIdAware {\n    val id: BaseId\n}\ninterface ChildEvent : RootEvent, ObjectIdAware\nclass Concrete(override val id: ChildId) : ChildEvent\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "RootEvent")
            .unwrap();
        let retained = retained_names(&index, &["ChildEvent", "Concrete"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn planner_matches_jdk_uuid_parallel_contracts_across_packages() {
        let fixture = Fixture::new();
        fixture.write(
            "Root.kt",
            "package base\nimport java.util.*\ninterface RootEvent { val id: UUID }\n",
        );
        fixture.write(
            "Parallel.kt",
            "package activity\nimport java.util.*\ninterface ParallelEvent<T> { val payload: T; val id: UUID }\nclass CreatedEvent(override val payload: String, override val id: UUID) : base.RootEvent, ParallelEvent<String>\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "RootEvent")
            .unwrap();
        let retained = retained_names(&index, &["CreatedEvent"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn planner_rejects_jdk_uuid_wildcard_when_domain_uuid_is_indexed() {
        let fixture = Fixture::new();
        fixture.write(
            "Root.kt",
            "package base\nimport java.util.*\ninterface RootEvent { val id: UUID }\n",
        );
        fixture.write(
            "Parallel.kt",
            "package activity\nimport java.util.*\ninterface ParallelEvent<T> { val payload: T; val id: UUID }\nclass CreatedEvent(override val payload: String, override val id: UUID) : base.RootEvent, ParallelEvent<String>\n",
        );
        fixture.write("Domain.kt", "package domain\nclass UUID\n");
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "RootEvent")
            .unwrap();
        let retained = retained_names(&index, &["CreatedEvent"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(
            result.is_err(),
            "domain UUID collision was accepted: {result:?}"
        );
    }

    #[test]
    fn planner_accepts_nonnull_readonly_property_for_nullable_contract() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface BaseId {}\ninterface Api {\n    val name: BaseId?\n}\n",
        );
        fixture.write(
            "Desc.kt",
            "package sample\ninterface Child : Api {\n    override val name: BaseId\n}\ndata class Impl(override val name: BaseId) : Child\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let retained = retained_names(&index, &["Child", "Impl"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn planner_accepts_already_repaired_class_field_and_getter_bridge() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface NameRef {}\ninterface Api {\n    val name: NameRef\n}\n",
        );
        fixture.write(
            "Impl.kt",
            "package sample\nclass Impl(@JvmField val name: NameRef) : Api {\n    override fun getName(): NameRef = name\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let retained = retained_names(&index, &["Impl"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn repair_follows_generated_java_interface_ancestry() {
        let fixture = Fixture::new();
        fixture.write(
            "Parent.java",
            "// NOTLIN: generated from Parent.kt\npackage sample;\npublic interface Parent { String getSiteId(); }\n",
        );
        fixture.write(
            "Child.java",
            "// NOTLIN: generated from Child.kt\npackage sample;\npublic interface Child extends Parent { String getOrderId(); }\n",
        );
        let implementation = fixture.write(
            "Impl.kt",
            "package sample\ninterface Impl : Child { override val siteId: String get() = \"site\"; override val orderId: String get() = \"order\" }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&implementation).unwrap();
        let (repaired, count) = repair_file(&index, &implementation, &source);
        assert_eq!(count, 2, "{repaired}");
        assert!(repaired.contains("override fun getSiteId(): String = \"site\""));
        assert!(repaired.contains("override fun getOrderId(): String = \"order\""));
    }

    #[test]
    fn planner_requires_exact_types_and_only_inherited_nonleaf_bridges() {
        let (fixture, index, retained) = planner_fixture(
            "package sample\ninterface Child : Api\ndata class Impl(override val name: String, override var enabled: Boolean) : Child\n",
        );
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");

        let (fixture, index, retained) = planner_fixture(
            "package sample\ninterface Child : Api\ndata class Impl(override val name: Any, override var enabled: Boolean) : Child\n",
        );
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(!retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));

        let (fixture, index, retained) = planner_fixture(
            "package sample\ninterface Child : Api\nopen class Impl(override val name: String, override var enabled: Boolean) : Child\nclass Derived(name: String) : Impl(name, false) { fun getName(): String = name }\n",
        );
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let bridge_owner = index
            .declarations()
            .find(|declaration| declaration.name == "Impl")
            .unwrap();
        let contracts = source_contract_properties(&index, target).unwrap();
        assert!(index.has_any_subtype("Impl"));
        assert!(!class_descendants_inherit_contracts(
            &index,
            bridge_owner,
            &contracts,
            std::slice::from_ref(&fixture.0),
            &mut HashSet::new(),
        ));
        assert!(!retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));

        let (fixture, index, retained) = planner_fixture(
            "package sample\ninterface Child : Api\nopen class Impl(override val name: String, override var enabled: Boolean) : Child\nclass Derived(name: String) : Impl(name, false)\n",
        );
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));
    }

    #[test]
    fn abstract_intermediate_class_can_defer_contract_to_descendants() {
        let (fixture, index, retained) = planner_fixture(
            "package sample\nabstract class Base : Api\nclass First(override val name: String, override var enabled: Boolean) : Base()\nclass Second(override val name: String, override var enabled: Boolean) : Base()\n",
        );
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn planner_rejects_unsupported_property_shapes() {
        for descendant in [
            "package sample\ninterface Child : Api\nopen class Impl : Child { override open val name: String get() = \"name\"; override var enabled: Boolean = false }\n",
            "package sample\ninterface Child : Api\nabstract class Impl : Child { abstract override val name: String; abstract override var enabled: Boolean }\n",
            "package sample\ninterface Child : Api\nclass Impl : Child { override lateinit var name: String; override var enabled: Boolean = false }\n",
        ] {
            let (fixture, index, retained) = planner_fixture(descendant);
            let target = index
                .declarations()
                .find(|declaration| declaration.name == "Api")
                .unwrap();
            assert!(
                !retained_subtypes_repairable(
                    &index,
                    target,
                    &retained,
                    std::slice::from_ref(&fixture.0),
                ),
                "unsupported property shape was accepted: {descendant}"
            );
        }
    }

    #[test]
    fn planner_rejects_all_open_computed_properties_but_accepts_final_ones() {
        let (fixture, index, retained) = planner_fixture(
            "package sample\ninterface Child : Api\n@Entity class Impl : Child { override val name: String get() = \"name\"; override var enabled: Boolean = false }\n",
        );
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(!retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));

        let (fixture, index, retained) = planner_fixture(
            "package sample\ninterface Child : Api\nclass Impl : Child { override val name: String get() = \"name\"; override var enabled: Boolean = false }\n",
        );
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));

        let (fixture, index, retained) = planner_fixture(
            "package sample\ninterface Child : Api\n@Entity class Impl : Child { override val name: String = \"name\"; override var enabled: Boolean = false }\n",
        );
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));
    }

    #[test]
    fn planner_accepts_initialized_lifecycle_fields_on_an_entity_hierarchy() {
        let fixture = Fixture::new();
        fixture.write(
            "Lifecycle.kt",
            "package api\nimport java.time.Instant\ninterface Lifecycle { val createdDate: Instant?; val startedDate: Instant?; val completedDate: Instant? }\n",
        );
        fixture.write(
            "Entities.kt",
            "package impl\nimport api.Lifecycle\nimport java.time.Instant\n@Entity abstract class ActivityEntity : Lifecycle { override var createdDate: Instant? = Instant.now(); override var startedDate: Instant? = null; override var completedDate: Instant? = null }\n@Entity class JobActivityEntity : ActivityEntity()\n@Entity class AssetActivityEntity : ActivityEntity()\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Lifecycle")
            .unwrap();
        let retained = index
            .declarations()
            .filter(|declaration| declaration.name != "Lifecycle")
            .map(|declaration| crate::semantics::workspace_symbol(&index, declaration))
            .collect();
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn computed_accessor_annotations_move_to_the_java_getter_bridge() {
        let (fixture, index, retained) = planner_fixture(
            "package sample\ninterface Child : Api\nclass Impl : Child { override val name: String\n    @JsonIgnore\n    get() = \"name\"\n    override var enabled: Boolean = false\n}\n",
        );
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));

        let contract = source_contract_properties(&index, target)
            .unwrap()
            .into_iter()
            .find(|contract| contract.name == "name")
            .unwrap();
        let (_, getter, _) = class_property_bridge(
            "override val name: String\n    @JsonIgnore\n    get() = \"name\"",
            &contract,
            false,
        )
        .unwrap();
        assert_eq!(
            getter.as_deref(),
            Some("@JsonIgnore\noverride fun getName(): String = name")
        );
        let (_, getter, _) =
            class_property_bridge("override val name:\n        String", &contract, false).unwrap();
        assert_eq!(
            getter.as_deref(),
            Some("override fun getName(): String = name")
        );
    }

    #[test]
    fn planner_accepts_proven_covariant_getters_but_rejects_invariant_generics() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface RuntimeState\ninterface Api { val state: RuntimeState }\n",
        );
        fixture.write(
            "Desc.kt",
            "package sample\nclass AgvState : RuntimeState\ninterface Child : Api { override val state: AgvState }\ndata class Impl(override val state: AgvState) : Child\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let retained = index
            .declarations()
            .filter(|declaration| declaration.name != "Api")
            .map(|declaration| crate::semantics::workspace_symbol(&index, declaration))
            .collect();
        assert!(retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));

        let generic = Fixture::new();
        generic.write(
            "Api.kt",
            "package sample\ninterface Api { val values: List<String> }\n",
        );
        let impl_path = generic.write(
            "Impl.kt",
            "package sample\nclass Impl(override val values: List<Int>) : Api\n",
        );
        let generic_index = SourceIndex::discover(&generic.0).unwrap();
        let generic_target = generic_index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(!generic_index.property_getter_return_compatible(
            &impl_path,
            "List<Int>",
            generic_target,
            "List<String>",
        ));
    }

    #[test]
    fn planner_accepts_exact_resolved_generic_property_contracts() {
        let exact = Fixture::new();
        exact.write(
            "Api.kt",
            "package sample\ninterface Item\ninterface Api { val values: List<Item> }\n",
        );
        exact.write(
            "Impl.kt",
            "package sample\ndata class Impl(override val values: List<Item>) : Api\n",
        );
        let exact_index = SourceIndex::discover(&exact.0).unwrap();
        let exact_target = exact_index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let exact_retained = retained_names(&exact_index, &["Impl"]);
        assert!(retained_subtypes_repairable(
            &exact_index,
            exact_target,
            &exact_retained,
            std::slice::from_ref(&exact.0),
        ));

        let mismatch = Fixture::new();
        mismatch.write(
            "Api.kt",
            "package sample\ninterface Api { val values: List<String> }\n",
        );
        mismatch.write(
            "Impl.kt",
            "package sample\ndata class Impl(override val values: List<Int>) : Api\n",
        );
        let mismatch_index = SourceIndex::discover(&mismatch.0).unwrap();
        let mismatch_target = mismatch_index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let mismatch_retained = retained_names(&mismatch_index, &["Impl"]);
        assert!(!retained_subtypes_repairable(
            &mismatch_index,
            mismatch_target,
            &mismatch_retained,
            std::slice::from_ref(&mismatch.0),
        ));

        let wrapper = Fixture::new();
        wrapper.write(
            "Api.kt",
            "package sample\ninterface Entry\ninterface Api { val values: List<Entry> }\n",
        );
        wrapper.write(
            "Impl.kt",
            "package sample\nclass Concrete : Entry\nclass EntryList : ArrayList<Concrete>()\ndata class Impl(override val values: EntryList) : Api\n",
        );
        let wrapper_index = SourceIndex::discover(&wrapper.0).unwrap();
        let wrapper_target = wrapper_index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let wrapper_retained = retained_names(&wrapper_index, &["Impl"]);
        assert!(retained_subtypes_repairable(
            &wrapper_index,
            wrapper_target,
            &wrapper_retained,
            std::slice::from_ref(&wrapper.0),
        ));
    }

    #[test]
    fn ordinary_constructor_annotations_are_preserved_but_complex_targets_fail_closed() {
        let (fixture, index, retained) = planner_fixture(
            "package sample\ninterface Child : Api\ndata class Impl(override val name: String, @Convert(converter = EnabledConverter::class) override var enabled: Boolean) : Child\n",
        );
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));

        fs::remove_file(fixture.0.join("Api.kt")).unwrap();
        let java = fixture.write(
            "translated/Api.java",
            "// NOTLIN: generated from Api.kt\npackage sample;\npublic interface Api { String getName(); boolean getEnabled(); void setEnabled(boolean value); }\n",
        );
        let implementation = fixture.write(
            "retained/AnnotatedImpl.kt",
            "package sample\ndata class AnnotatedImpl(override val name: String, @Convert(converter = EnabledConverter::class) override var enabled: Boolean) : Api\n",
        );
        let annotated_index = SourceIndex::discover(&fixture.0).unwrap();
        let original = fs::read_to_string(&implementation).unwrap();
        let (rewritten, rewrites) = repair_file(&annotated_index, &implementation, &original);
        assert_eq!(rewrites, 3);
        assert!(rewritten.contains(
            "@Convert(converter = EnabledConverter::class) final @JvmField var enabled: Boolean"
        ));
        assert!(java.exists());

        let (complex_fixture, complex_index, complex_retained) = planner_fixture(
            "package sample\ninterface Child : Api\ndata class Impl(override val name: String, @get:Type(value = IEnumerationType::class) override var enabled: Boolean) : Child\n",
        );
        let complex_target = complex_index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(!retained_subtypes_repairable(
            &complex_index,
            complex_target,
            &complex_retained,
            std::slice::from_ref(&complex_fixture.0),
        ));
    }

    #[test]
    fn intermediate_interface_unrelated_properties_do_not_block_a_bridge() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface Api { var enabled: Boolean }\n",
        );
        fixture.write(
            "Desc.kt",
            "package sample\ninterface Child : Api { override var enabled: Boolean; val name: String }\ndata class Impl(override var enabled: Boolean, override val name: String) : Child\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let retained = index
            .declarations()
            .filter(|declaration| declaration.name != "Api")
            .map(|declaration| crate::semantics::workspace_symbol(&index, declaration))
            .collect();
        assert!(retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));
    }

    #[test]
    fn same_named_contract_in_another_package_does_not_add_its_subtypes() {
        let fixture = Fixture::new();
        fixture.write(
            "sample/Api.kt",
            "package sample\ninterface Api { val enabled: Boolean }\n",
        );
        fixture.write(
            "sample/Impl.kt",
            "package sample\ndata class Impl(override val enabled: Boolean) : Api\n",
        );
        fixture.write(
            "other/Api.kt",
            "package other\ninterface Api\nclass Work : Api\n",
        );
        fixture.write(
            "other/Outer.java",
            "package other; public class Outer { public interface Impl {} } class Use implements Outer.Impl {}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| {
                declaration.package.as_deref() == Some("sample") && declaration.name == "Api"
            })
            .unwrap();
        let retained = retained_names(&index, &["Impl", "Work"]);
        assert!(index.has_any_subtype("Impl"));
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn computed_interface_getter_becomes_an_explicit_java_getter_bridge() {
        let (fixture, index, retained) = planner_fixture(
            "package sample\ninterface Child : Api {\n    override val name: String\n        get() = \"child\"\n    override var enabled: Boolean\n}\ndata class Impl(override val name: String, override var enabled: Boolean) : Child\n",
        );
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");

        let contract = PropertyContract {
            name: "referenceNo".to_string(),
            type_name: "String".to_string(),
            type_source: fixture.0.join("Api.kt"),
            getter: "getReferenceNo".to_string(),
            setter: None,
        };
        let child = index
            .declarations()
            .find(|declaration| declaration.name == "Child")
            .unwrap();
        let rewritten = interface_property_methods(
            &index,
            child,
            "@get:JsonIgnore\noverride val referenceNo: String\n    get() = parent.referenceNo",
            &contract,
            false,
        )
        .unwrap();
        assert_eq!(
            rewritten,
            "@JsonIgnore\noverride fun getReferenceNo(): String = parent.referenceNo"
        );

        let (inline_fixture, inline_index, inline_retained) = planner_fixture(
            "package sample\ninterface Child : Api { override val name: String get() = \"child\"; override var enabled: Boolean }\ndata class Impl(override val name: String, override var enabled: Boolean) : Child\n",
        );
        let inline_target = inline_index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(retained_subtypes_repairable(
            &inline_index,
            inline_target,
            &inline_retained,
            std::slice::from_ref(&inline_fixture.0),
        ));
        let inline_child = inline_index
            .declarations()
            .find(|declaration| declaration.name == "Child")
            .unwrap();
        let inline_bridge = interface_property_methods(
            &inline_index,
            inline_child,
            "override val name: String get() = \"child\"",
            &PropertyContract {
                name: "name".into(),
                type_name: "String".into(),
                type_source: inline_fixture.0.join("Api.kt"),
                getter: "getName".into(),
                setter: None,
            },
            false,
        )
        .unwrap();
        assert_eq!(inline_bridge, "override fun getName(): String = \"child\"");
    }

    #[test]
    fn computed_interface_getter_qualifies_a_retained_super_provider() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface Api { val site: String }\n",
        );
        fixture.write(
            "Provider.kt",
            "package sample\ninterface Provider : Api { override val site: String get() = \"provider\" }\n",
        );
        fixture.write(
            "Child.kt",
            "package sample\ninterface Child : Api, Provider { override val site: String get() = super.site }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let child = index
            .declarations()
            .find(|declaration| declaration.name == "Child")
            .unwrap();
        let contract = PropertyContract {
            name: "site".to_string(),
            type_name: "String".to_string(),
            type_source: fixture.0.join("Api.kt"),
            getter: "getSite".to_string(),
            setter: None,
        };
        let rewritten = interface_property_methods(
            &index,
            child,
            "override val site: String\n    get() = super.site",
            &contract,
            false,
        )
        .unwrap();
        assert_eq!(
            rewritten,
            "override fun getSite(): String = super<Provider>.getSite()"
        );
    }

    #[test]
    fn concrete_subtype_inherits_a_repaired_computed_interface_getter() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface Api { val referenceNo: String }\n",
        );
        let descendants = fixture.write(
            "Desc.kt",
            "package sample\ninterface Parent : Api {\n    override val referenceNo: String\n        get() = \"parent\"\n}\ndata class Child(val value: String) : Parent\nenum class Kind : Parent { ONE }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let retained = retained_names(&index, &["Parent", "Child", "Kind"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");

        fs::remove_file(fixture.0.join("Api.kt")).unwrap();
        fixture.write(
            "translated/Api.java",
            "// NOTLIN: generated from Api.kt\npackage sample;\npublic interface Api { String getReferenceNo(); }\n",
        );
        let migrated_index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&descendants).unwrap();
        let (rewritten, count) = repair_file(&migrated_index, &descendants, &source);
        assert_eq!(count, 1);
        assert!(
            rewritten.contains("override fun getReferenceNo(): String = \"parent\""),
            "{rewritten}"
        );
        assert!(rewritten.contains("data class Child(val value: String) : Parent"));
        assert!(rewritten.contains("enum class Kind : Parent { ONE }"));

        let mismatch = Fixture::new();
        mismatch.write(
            "Api.kt",
            "package sample\ninterface Api { val referenceNo: String }\n",
        );
        mismatch.write(
            "Desc.kt",
            "package sample\ninterface Parent : Api { override val referenceNo: Int get() = 1 }\ndata class Child(val value: String) : Parent\n",
        );
        let mismatch_index = SourceIndex::discover(&mismatch.0).unwrap();
        let mismatch_target = mismatch_index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(!retained_subtypes_repairable(
            &mismatch_index,
            mismatch_target,
            &retained_names(&mismatch_index, &["Parent", "Child"]),
            std::slice::from_ref(&mismatch.0),
        ));
    }

    #[test]
    fn computed_property_on_separate_kotlin_supertype_keeps_its_default_getter() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\nclass Order\ninterface Api { val order: Order }\n",
        );
        let desc = fixture.write(
            "Desc.kt",
            "package sample\ninterface Computed { val order: Order get() = Order() }\nclass Impl(override val order: Order) : Api, Computed\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let retained = index
            .declarations()
            .filter(|declaration| declaration.name != "Api")
            .map(|declaration| crate::semantics::workspace_symbol(&index, declaration))
            .collect();
        assert!(retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));
        fs::remove_file(fixture.0.join("Api.kt")).unwrap();
        fixture.write(
            "translated/Api.java",
            "// NOTLIN: generated from Api.kt\npackage sample;\npublic interface Api { Order getOrder(); }\n",
        );
        let repair_index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&desc).unwrap();
        let computed = repair_index
            .declarations()
            .find(|declaration| declaration.name == "Computed")
            .unwrap();
        let computed_file = repair_index.declaration_source_file(computed).unwrap();
        let mut contract_cache = HashMap::new();
        let computed_contracts = contracts_for_repair(
            &repair_index,
            computed_file,
            computed,
            &HashSet::new(),
            &mut contract_cache,
        );
        assert!(
            computed_contracts
                .iter()
                .any(|contract| contract.name == "order"),
            "the descendant's generated getter contract was not propagated to Computed: {computed_contracts:?}"
        );
        let (repaired, _) = repair_file(&repair_index, &desc, &source);
        assert!(
            repaired.contains("fun getOrder(): Order = Order()"),
            "the separate interface's default getter must survive repair:\n{repaired}"
        );
    }

    #[test]
    fn non_generic_parallel_component_waits_for_complete_callsite_proof() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface IBarcode\ninterface AssetBarcode : IBarcode\ninterface Api { val barcodes: List<IBarcode> }\n",
        );
        fixture.write(
            "AssetInfo.kt",
            "package sample\ninterface AssetInfo { val barcodes: List<IBarcode> }\n",
        );
        let child = fixture.write(
            "AssetInfoChild.kt",
            "package sample\ninterface AssetInfoChild : AssetInfo { override val barcodes: List<AssetBarcode> }\n",
        );
        let sibling = fixture.write(
            "Sibling.kt",
            "package sample\nclass Sibling(override val barcodes: List<AssetBarcode>) : AssetInfoChild\n",
        );
        fixture.write(
            "JobContainerInfo.kt",
            "package sample\ndata class JobContainerInfo(override val barcodes: List<AssetBarcode>) : Api, AssetInfoChild\n",
        );
        let use_site = fixture.write(
            "Use.kt",
            "package sample\nfun use(asset: AssetInfo, child: AssetInfoChild) { println(asset.barcodes); println(child.barcodes) }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        let retained = retained_names(&index, &["JobContainerInfo"]);
        assert!(!retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));

        fs::remove_file(fixture.0.join("Api.kt")).unwrap();
        fixture.write(
            "translated/Api.java",
            "// NOTLIN: generated from Api.kt\npackage sample;\npublic interface Api { List<IBarcode> getBarcodes(); }\n",
        );
        let migrated_index = SourceIndex::discover(&fixture.0).unwrap();
        let contracts = repaired_callsite_contracts(&migrated_index, &HashSet::new());
        let use_source = fs::read_to_string(&use_site).unwrap();
        let (rewritten_use, count) = crate::property_callsite::rewrite_file(
            &migrated_index,
            &use_site,
            &use_source,
            &contracts,
        );
        assert_eq!(count, 2);
        assert!(rewritten_use.contains("asset.getBarcodes()"));
        assert!(rewritten_use.contains("child.getBarcodes()"));
        let child_source = fs::read_to_string(&child).unwrap();
        let (rewritten, count) = repair_file(&migrated_index, &child, &child_source);
        assert_eq!(count, 1);
        assert!(rewritten.contains("override fun getBarcodes(): List<AssetBarcode>"));

        let asset_info = fixture.0.join("AssetInfo.kt");
        let asset_source = fs::read_to_string(&asset_info).unwrap();
        let (rewritten, count) = repair_file(&migrated_index, &asset_info, &asset_source);
        assert_eq!(count, 1);
        assert!(rewritten.contains("fun getBarcodes(): List<IBarcode>"));

        let implementation = fixture.0.join("JobContainerInfo.kt");
        let implementation_source = fs::read_to_string(&implementation).unwrap();
        let (rewritten, count) =
            repair_file(&migrated_index, &implementation, &implementation_source);
        assert_eq!(count, 2);
        assert!(rewritten.contains("@JvmField val barcodes: List<AssetBarcode>"));
        assert!(rewritten.contains("override fun getBarcodes(): List<AssetBarcode> = barcodes"));

        let sibling_source = fs::read_to_string(&sibling).unwrap();
        let (rewritten, count) = repair_file(&migrated_index, &sibling, &sibling_source);
        assert_eq!(count, 2);
        assert!(rewritten.contains("@JvmField val barcodes: List<AssetBarcode>"));
        assert!(rewritten.contains("override fun getBarcodes(): List<AssetBarcode> = barcodes"));
    }

    #[test]
    fn method_bearing_parallel_component_stays_kotlin() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface Api { val enabled: Boolean }\n",
        );
        fixture.write(
            "Parallel.kt",
            "package sample\ninterface Parallel { val enabled: Boolean }\n",
        );
        fixture.write(
            "Model.kt",
            "package sample\ndata class Model(override val enabled: Boolean) : Api, Parallel { fun update(): Model = this }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "Api")
            .unwrap();
        assert!(!retained_subtypes_repairable(
            &index,
            target,
            &retained_names(&index, &["Model"]),
            std::slice::from_ref(&fixture.0),
        ));
    }

    #[test]
    fn compatible_event_parallel_component_is_repaired_together() {
        let fixture = Fixture::new();
        fixture.write(
            "Event.kt",
            "package sample\ninterface RootEvent<T> { val payload: T }\n",
        );
        fixture.write(
            "Parallel.kt",
            "package sample\ninterface PayloadEvent<T> { val payload: T }\ndata class CreatedEvent<T>(override val payload: T) : RootEvent<T>, PayloadEvent<T>\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "RootEvent")
            .unwrap();
        let retained = retained_names(&index, &["PayloadEvent", "CreatedEvent"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn rediscovers_persisted_kotlin_getter_contract_from_property_bridge() {
        let fixture = Fixture::new();
        let source_path = fixture.write(
            "Api.kt",
            "package sample\ninterface Bag {\n    fun getValue(): String\n}\ndata class Impl(@JvmField val value: String) : Bag {\n    override fun getValue(): String = value\n}\ninterface Child : Bag {\n    fun read(): String = value\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let contracts = repaired_callsite_contracts(&index, &HashSet::new());
        assert!(
            contracts.iter().any(|contract| {
                contract.owner_type == "sample.Bag"
                    && contract.property == "value"
                    && contract.getter == "getValue"
            }),
            "{contracts:?}"
        );
        let source = fs::read_to_string(&source_path).unwrap();
        let (rewritten, count) =
            crate::property_callsite::rewrite_file(&index, &source_path, &source, &contracts);
        assert_eq!(count, 1, "{rewritten}");
        assert!(rewritten.contains("fun read(): String = getValue()"));
        assert!(rewritten.contains("override fun getValue(): String = value"));

        let ordinary = Fixture::new();
        ordinary.write(
            "Api.kt",
            "package sample\ninterface Bag { fun getValue(): String }\nclass Impl : Bag { override fun getValue(): String = \"value\" }\n",
        );
        let ordinary_index = SourceIndex::discover(&ordinary.0).unwrap();
        assert!(repaired_callsite_contracts(&ordinary_index, &HashSet::new()).is_empty());
    }

    #[test]
    fn generated_java_enum_getters_are_callsite_contracts_only() {
        let fixture = Fixture::new();
        fixture.write(
            "LabelApi.kt",
            "package sample\ninterface LabelApi { val label: String }\n",
        );
        let kind = fixture.write(
            "Kind.java",
            "// NOTLIN: generated from Kind.kt\npackage sample;\npublic enum Kind implements LabelApi { ONE; public String getLabel() { return \"one\"; } }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let generated_java = HashSet::from([kind]);
        assert!(repaired_callsite_contracts(&index, &generated_java).is_empty());
        let enum_decl = index
            .declarations()
            .find(|declaration| declaration.name == "Kind")
            .unwrap();
        assert_eq!(enum_decl.supertypes, ["LabelApi"]);
        let enum_file = index.declaration_source_file(enum_decl).unwrap();
        let label_api = index.resolve_type(enum_file, "LabelApi").unwrap();
        let label_file = index.declaration_source_file(label_api).unwrap();
        assert!(
            enum_decl.members.iter().any(|member| {
                member.kind == MemberKind::Method
                    && member.name == "getLabel"
                    && member.visibility.as_deref() == Some("public")
            }),
            "enum methods were not indexed: {:?}",
            enum_decl.members
        );
        let persisted = [PersistedPropertyContract {
            owner_file: label_file.path.clone(),
            owner_name: "LabelApi".into(),
            owner_package: Some("sample".into()),
            owner_kind: DeclarationKind::Interface,
            contract: PropertyContract {
                name: "label".into(),
                type_name: "String".into(),
                type_source: label_file.path.clone(),
                getter: "getLabel".into(),
                setter: None,
            },
        }];
        assert!(persisted_owner_matches(
            &persisted[0],
            label_file,
            label_api
        ));
        let persisted_index = PersistedContractIndex::new(&index, &persisted);
        assert_eq!(persisted_index.for_owner(label_file, label_api).len(), 1);
        let mut cache = HashMap::new();
        let local_contracts =
            java_callsite_property_contracts(enum_decl, enum_file).expect("enum getter contracts");
        assert!(java_owner_inherits_repaired_contract(
            &index,
            enum_file,
            enum_decl,
            &local_contracts[0],
            &generated_java,
            &persisted_index,
            &mut cache,
        ));
        let contracts =
            repaired_callsite_contracts_with_persisted(&index, &generated_java, &persisted);
        assert!(
            contracts.iter().any(|contract| {
                contract.owner_type == "sample.Kind"
                    && contract.property == "label"
                    && contract.getter == "getLabel"
            }),
            "{contracts:?}"
        );
        // Concrete Java getters are useful for call-site rewriting; they do
        // not become inherited Kotlin ABI-repair contracts.
        assert!(java_property_contracts(enum_decl, &fixture.0.join("Kind.java")).is_none());
    }

    #[test]
    fn generated_java_interface_getter_keeps_unrepaired_kotlin_property_syntax() {
        let fixture = Fixture::new();
        fixture.write(
            "Types.kt",
            "package sample\ninterface BaseVariant {\n    fun baseLabel(): String\n}\ninterface NarrowVariant : BaseVariant {\n    fun narrowLabel(): String\n}\ninterface HolderContract {\n    val variant: BaseVariant\n}\n",
        );
        fixture.write(
            "DetailHolder.kt",
            "package sample\ninterface DetailHolder : HolderContract {\n    override val variant: NarrowVariant\n}\n",
        );
        let generated_holder = fixture.write(
            "GeneratedDetailHolder.java",
            "// NOTLIN: generated from GeneratedDetailHolder.kt\npackage sample; public interface GeneratedDetailHolder extends HolderContract { NarrowVariant getVariant(); }\n",
        );
        let caller = fixture.write(
            "Use.kt",
            "package sample\nfun use(info: GeneratedDetailHolder): BaseVariant = info.variant\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let parent = index
            .declarations()
            .find(|declaration| declaration.name == "HolderContract")
            .unwrap();
        let retained = HashSet::from([crate::semantics::workspace_symbol(&index, parent)]);
        let child = index
            .declarations()
            .find(|declaration| declaration.name == "DetailHolder")
            .unwrap();
        assert_eq!(
            retained_kotlin_property_ancestor(&index, child, &retained).as_deref(),
            Some("HolderContract")
        );
        let generated_java = HashSet::from([fs::canonicalize(&generated_holder).unwrap()]);
        let contracts = repaired_callsite_contracts(&index, &generated_java);
        assert!(
            !contracts.iter().any(|contract| {
                contract.owner_type == "sample.GeneratedDetailHolder"
                    && contract.property == "variant"
            }),
            "an unrepaired Kotlin property ancestor must keep synthetic property syntax: {contracts:?}"
        );
        let source = fs::read_to_string(&caller).unwrap();
        let (rewritten, count) =
            crate::property_callsite::rewrite_file(&index, &caller, &source, &contracts);
        assert_eq!(count, 0, "{rewritten}");
        assert!(rewritten.contains("info.variant"), "{rewritten}");
        assert!(!rewritten.contains("info.getVariant()"), "{rewritten}");
    }

    #[test]
    fn same_typed_override_of_retained_property_is_kept_on_kotlin_side() {
        let fixture = Fixture::new();
        fixture.write(
            "Types.kt",
            "package sample\ninterface ContextKind\ninterface ContextContract { val contextKind: ContextKind }\n",
        );
        fixture.write(
            "GeneratedRoot.java",
            "// NOTLIN: generated from GeneratedRoot.kt\npackage sample; public interface GeneratedRoot extends ContextContract { ContextKind getContextKind(); }\n",
        );
        fixture.write(
            "ContextChild.kt",
            "package sample\ninterface ContextChild : GeneratedRoot { override val contextKind: ContextKind }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let parent = index
            .declarations()
            .find(|declaration| declaration.name == "ContextContract")
            .unwrap();
        let retained = HashSet::from([crate::semantics::workspace_symbol(&index, parent)]);
        let child = index
            .declarations()
            .find(|declaration| declaration.name == "ContextChild")
            .unwrap();
        assert_eq!(
            retained_kotlin_property_ancestor(&index, child, &retained).as_deref(),
            Some("ContextContract"),
            "same-typed overrides still need Kotlin's real override declaration"
        );
    }

    #[test]
    fn memberless_java_interface_retains_when_diamond_reaches_kotlin_property() {
        let fixture = Fixture::new();
        fixture.write(
            "Types.kt",
            "package sample\nclass ContextKind\ninterface RetainedRoot { val contextKind: ContextKind }\n",
        );
        fixture.write(
            "AbstractPath.kt",
            "package sample\ninterface AbstractPath : RetainedRoot\n",
        );
        fixture.write(
            "DefaultPath.kt",
            "package sample\ninterface DefaultPath : RetainedRoot { override val contextKind: ContextKind get() = ContextKind() }\n",
        );
        fixture.write(
            "JoinedPath.kt",
            "package sample\ninterface JoinedPath : AbstractPath, DefaultPath { override val contextKind: ContextKind get() = super<DefaultPath>.contextKind }\n",
        );
        fixture.write(
            "Bridge.java",
            "// NOTLIN: generated from Bridge.kt\npackage sample; public interface Bridge extends JoinedPath {}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let retained_root = index
            .declarations()
            .find(|declaration| declaration.name == "RetainedRoot")
            .unwrap();
        let retained = HashSet::from([crate::semantics::workspace_symbol(&index, retained_root)]);
        let bridge = index
            .declarations()
            .find(|declaration| declaration.name == "Bridge")
            .unwrap();
        assert!(bridge.members.is_empty());
        assert_eq!(
            retained_kotlin_property_ancestor(&index, bridge, &retained).as_deref(),
            Some("RetainedRoot"),
            "a memberless Java interface must stay Kotlin when its ancestry can create a fake override"
        );
    }

    #[test]
    fn constructor_property_keeps_override_against_retained_kotlin_base() {
        let fixture = Fixture::new();
        fixture.write(
            "ParentRecord.kt",
            "package sample\nabstract class ParentRecord {\n    abstract val id: String\n}\n",
        );
        let generated_api = fixture.write(
            "Identified.java",
            "// NOTLIN: generated from Identified.kt\npackage sample; public interface Identified { String getId(); }\n",
        );
        let work_order = fixture.write(
            "ChildRecord.kt",
            "package sample\nclass ChildRecord(override val id: String) : ParentRecord(), Identified\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let generated_java = HashSet::from([generated_api]);
        let mut sources = vec![(work_order.clone(), fs::read_to_string(&work_order).unwrap())];
        let report = repair_virtual_sources_planned(&index, &mut sources, &generated_java);
        assert_eq!(report.count, 0, "{:?}", sources[0].1);
        assert!(sources[0].1.contains("override val id: String"));
        assert!(!sources[0].1.contains("@JvmField"));
    }

    #[test]
    fn persisted_generic_contract_crosses_generated_memberless_interface() {
        let fixture = Fixture::new();
        let root = fixture.write(
            "Root.java",
            "// NOTLIN: generated from Root.kt\npackage sample;\nimport java.util.Map;\npublic interface Root<T> { Map<String, T> getProperties(); }\n",
        );
        let bridge = fixture.write(
            "Bridge.java",
            "// NOTLIN: generated from Bridge.kt\npackage sample;\npublic interface Bridge<T> extends Base<T> {}\n",
        );
        let base = fixture.write(
            "Base.kt",
            "package sample\ninterface Base<T> : Root<T> {\n    override val properties: Map<String, T>\n        get() = emptyMap()\n}\n",
        );
        let first_index = SourceIndex::discover(&fixture.0).unwrap();
        let mut first_sources = vec![(base.clone(), fs::read_to_string(&base).unwrap())];
        let generated = HashSet::from([root.clone()]);
        let first_report =
            repair_virtual_sources_planned(&first_index, &mut first_sources, &generated);
        assert_eq!(first_report.count, 1);
        let repaired_base = first_sources[0].1.clone();
        assert!(repaired_base.contains("fun getProperties(): Map<String, T>"));
        assert_eq!(first_report.abi_contracts.len(), 1);

        let implementation = fixture.write(
            "Implementation.kt",
            "package sample\nclass Implementation(override val properties: Map<String, String>) : Bridge<String>\n",
        );
        let second_index = SourceIndex::discover(&fixture.0)
            .unwrap()
            .with_overlays(&[crate::workspace::SourceOverlay::Replace {
                path: base.clone(),
                language: SourceLanguage::Kotlin,
                source: repaired_base,
            }])
            .unwrap();
        let mut second_sources = vec![(
            implementation.clone(),
            fs::read_to_string(&implementation).unwrap(),
        )];
        let generated = HashSet::from([root, bridge]);
        let second_report = repair_virtual_sources_planned_with_contracts(
            &second_index,
            &mut second_sources,
            &generated,
            &first_report.abi_contracts,
        );
        // Constructor-property exposure and accessor generation are separate edits.
        assert_eq!(second_report.count, 2, "{:?}", second_sources[0].1);
        assert!(second_sources[0].1.contains("@JvmField val properties"));
        assert!(
            second_sources[0]
                .1
                .contains("override fun getProperties(): Map<String, String> = properties")
        );
    }

    #[test]
    fn parallel_java_getter_keeps_inherited_kotlin_property_overrides() {
        let fixture = Fixture::new();
        fixture.write(
            "Root.java",
            "// NOTLIN: generated from Root.kt\npackage sample;\npublic interface Root<T> extends Bridge<T> { T getLabel(); }\n",
        );
        fixture.write(
            "Bridge.java",
            "// NOTLIN: generated from Bridge.kt\npackage sample;\npublic interface Bridge<T> extends KotlinBase<T> {}\n",
        );
        fixture.write(
            "KotlinBase.kt",
            "package sample\ninterface KotlinBase<T> { val label: T }\n",
        );
        let child = fixture.write(
            "Child.kt",
            "package sample\ninterface Child<T> : Root<T> { override val label: T }\n",
        );
        let implementation = fixture.write(
            "Implementation.kt",
            "package sample\nclass Implementation(override val label: String) : Child<String>\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        for path in [&child, &implementation] {
            let source = fs::read_to_string(path).unwrap();
            let (repaired, count) = repair_file(&index, path, &source);
            assert_eq!(count, 0, "{path:?}: {repaired}");
            assert_eq!(repaired, source);
        }
        let child_decl = index
            .declarations()
            .find(|declaration| declaration.name == "Child")
            .unwrap();
        assert!(inherits_kotlin_property_on_java_getter_path(
            &index, child_decl, "label", "getLabel"
        ));
    }

    #[test]
    fn generic_property_contract_substitutes_through_retained_descendants() {
        let fixture = Fixture::new();
        fixture.write(
            "ObjectEvent.kt",
            "package sample\ninterface IObjectEvent<T, I> { val payload: List<T>; val id: I }\n",
        );
        fixture.write(
            "Event.kt",
            "package sample\ninterface IEvent<T, I> : IObjectEvent<List<T>, I>\n",
        );
        fixture.write(
            "CreatedEvent.kt",
            "package sample\nclass CreatedEvent(override val payload: List<List<String>>, override val id: String) : IEvent<String, String>\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "IObjectEvent")
            .unwrap();
        assert!(is_property_interface_candidate(target));
        let retained = retained_names(&index, &["IEvent", "CreatedEvent"]);
        let result =
            retained_subtypes_bridge(&index, target, &retained, std::slice::from_ref(&fixture.0));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn generic_property_contract_rejects_invariant_argument_mismatch() {
        let fixture = Fixture::new();
        fixture.write(
            "ObjectEvent.kt",
            "package sample\ninterface IObjectEvent<T> { val payload: List<T> }\n",
        );
        fixture.write(
            "CreatedEvent.kt",
            "package sample\nclass CreatedEvent(override val payload: List<Int>) : IObjectEvent<String>\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "IObjectEvent")
            .unwrap();
        assert!(!retained_subtypes_repairable(
            &index,
            target,
            &retained_names(&index, &["CreatedEvent"]),
            std::slice::from_ref(&fixture.0),
        ));
    }

    #[test]
    fn generic_property_contract_rejects_unresolved_star_projection() {
        let fixture = Fixture::new();
        fixture.write(
            "ObjectEvent.kt",
            "package sample\ninterface IObjectEvent<T> { val payload: T }\n",
        );
        fixture.write(
            "CreatedEvent.kt",
            "package sample\nclass CreatedEvent : IObjectEvent<*>\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let target = index
            .declarations()
            .find(|declaration| declaration.name == "IObjectEvent")
            .unwrap();
        assert!(!retained_subtypes_repairable(
            &index,
            target,
            &retained_names(&index, &["CreatedEvent"]),
            std::slice::from_ref(&fixture.0),
        ));
    }

    #[test]
    fn incompatible_parallel_property_contracts_still_block_the_bridge() {
        for parallel in [
            "interface AssetInfo { val barcodes: String }",
            "interface AssetInfo { var barcodes: List<IBarcode> }",
        ] {
            let fixture = Fixture::new();
            fixture.write(
                "Api.kt",
                "package sample\ninterface IBarcode\ninterface AssetBarcode : IBarcode\ninterface Api { val barcodes: List<IBarcode> }\n",
            );
            fixture.write("AssetInfo.kt", &format!("package sample\n{parallel}\n"));
            fixture.write(
                "JobContainerInfo.kt",
                "package sample\ndata class JobContainerInfo(override val barcodes: List<AssetBarcode>) : Api, AssetInfo\n",
            );
            let index = SourceIndex::discover(&fixture.0).unwrap();
            let target = index
                .declarations()
                .find(|declaration| declaration.name == "Api")
                .unwrap();
            assert!(
                !retained_subtypes_repairable(
                    &index,
                    target,
                    &retained_names(&index, &["JobContainerInfo"]),
                    std::slice::from_ref(&fixture.0),
                ),
                "parallel contract should be rejected: {parallel}"
            );
        }
    }

    #[test]
    fn mutable_descendant_of_val_contract_keeps_its_setter_abi() {
        let fixture = Fixture::new();
        fixture.write(
            "translated/Api.java",
            "// NOTLIN: generated from Api.kt\npackage sample;\npublic interface Api { boolean getEnabled(); }\n",
        );
        let implementation = fixture.write(
            "retained/Impl.kt",
            "package sample\ninterface Child : Api { override var enabled: Boolean }\nclass Impl(override var enabled: Boolean) : Child\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let original = fs::read_to_string(&implementation).unwrap();
        let (rewritten, count) = repair_file(&index, &implementation, &original);
        assert_eq!(count, 3);
        assert!(rewritten.contains("override fun getEnabled(): Boolean"));
        assert!(rewritten.contains("fun setEnabled(value: Boolean)"));
        assert!(rewritten.contains("override fun setEnabled(value: Boolean) { enabled = value }"));
    }

    #[test]
    fn repairs_against_a_generated_java_overlay_before_it_exists_on_disk() {
        let fixture = Fixture::new();
        let implementation = fixture.write(
            "Impl.kt",
            "package sample\nclass Impl(override val enabled: Boolean) : Api\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let java_path = fixture.0.join("Api.java");
        let java_source = "// NOTLIN: generated from Api.kt\npackage sample;\npublic interface Api { boolean getEnabled(); }\n";
        let virtual_index = index
            .with_overlays(&[crate::workspace::SourceOverlay::Replace {
                path: java_path.clone(),
                language: SourceLanguage::Java,
                source: java_source.to_string(),
            }])
            .unwrap();
        assert!(!java_path.exists());

        let original = fs::read_to_string(&implementation).unwrap();
        let (rewritten, count) = repair_virtual_file(
            &virtual_index,
            &implementation,
            &original,
            &HashSet::from([java_path]),
        );

        assert_eq!(count, 2);
        assert!(rewritten.contains("@JvmField val enabled: Boolean"));
        assert!(rewritten.contains("override fun getEnabled(): Boolean = enabled"));
    }

    #[test]
    fn operation_contract_rewrites_default_and_external_reads_after_property_removal() {
        let fixture = Fixture::new();
        let generated = fixture.write(
            "GeneratedApi.java",
            "// NOTLIN: generated from fixture\npackage sample;\npublic interface GeneratedApi { String getCategory(); }\n",
        );
        let api = fixture.write(
            "Api.kt",
            "package sample\ninterface Api {\n    val category: String\n    val alias: Int\n        get() = category.length\n}\ninterface ApiChild : Api, GeneratedApi\n",
        );
        let caller = fixture.write(
            "Use.kt",
            "package sample\nfun use(value: Api): Int = value.category.length\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let generated_java = HashSet::from([generated]);
        let mut sources = vec![
            (api.clone(), fs::read_to_string(&api).unwrap()),
            (caller.clone(), fs::read_to_string(&caller).unwrap()),
        ];

        let repairs = repair_virtual_sources_planned(&index, &mut sources, &generated_java);
        let api_after_repair = sources
            .iter()
            .find(|(path, _)| path == &api)
            .unwrap()
            .1
            .clone();
        assert!(api_after_repair.contains("fun getCategory(): String"));
        assert!(
            repairs.callsite_contracts.iter().any(|(path, contract)| {
                path == &api
                    && contract.owner_type == "sample.Api"
                    && contract.property == "category"
                    && contract.getter == "getCategory"
            }),
            "the exact repair operation should retain the accessor contract"
        );

        let post_repair_index = index
            .with_overlays(&[crate::workspace::SourceOverlay::Replace {
                path: api.clone(),
                language: SourceLanguage::Kotlin,
                source: api_after_repair.clone(),
            }])
            .unwrap();
        let contracts = repairs
            .callsite_contracts
            .iter()
            .map(|(_, contract)| contract.clone())
            .collect::<Vec<_>>();
        let (rewritten_api, api_count) = crate::property_callsite::rewrite_file(
            &post_repair_index,
            &api,
            &api_after_repair,
            &contracts,
        );
        assert_eq!(api_count, 1, "{rewritten_api}");
        assert!(
            rewritten_api.contains("get() = getCategory().length"),
            "{rewritten_api}"
        );

        let caller_source = fs::read_to_string(&caller).unwrap();
        let (rewritten_caller, caller_count) = crate::property_callsite::rewrite_file(
            &post_repair_index,
            &caller,
            &caller_source,
            &contracts,
        );
        assert_eq!(caller_count, 1, "{rewritten_caller}");
        assert!(
            rewritten_caller.contains("value.getCategory().length"),
            "{rewritten_caller}"
        );
    }

    #[test]
    fn persisted_owner_index_matches_exact_unique_owners() {
        let fixture = Fixture::new();
        let file_path = fixture.write(
            "Duplicate.kt",
            "package sample\n\
             interface Shared {\n\
                 val first: String\n\
             }\n\
             \n",
        );
        let unique_index = SourceIndex::discover(&fixture.0).unwrap();
        let mut index = SourceIndex::discover(&fixture.0).unwrap();
        // Exercise ambiguous provider facts directly; source indexing may
        // coalesce duplicate syntax before this contract lookup runs.
        let file = index
            .files
            .iter_mut()
            .find(|file| file.path.ends_with("Duplicate.kt"))
            .unwrap();
        let duplicate = file
            .declarations
            .iter()
            .find(|declaration| declaration.name == "Shared")
            .unwrap()
            .clone();
        file.declarations.push(duplicate);
        let source_file = index.source_file(&file_path).unwrap();
        let shared = source_file
            .declarations
            .iter()
            .filter(|declaration| {
                declaration.name == "Shared"
                    && declaration.package.as_deref() == Some("sample")
                    && declaration.kind == DeclarationKind::Interface
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shared.len(),
            2,
            "ambiguous source must contain two indexed owners with the same identity"
        );

        let persisted = [PersistedPropertyContract {
            owner_file: source_file.path.clone(),
            owner_name: "Shared".into(),
            owner_package: Some("sample".into()),
            owner_kind: DeclarationKind::Interface,
            contract: PropertyContract {
                name: "first".into(),
                type_name: "String".into(),
                type_source: source_file.path.clone(),
                getter: "getFirst".into(),
                setter: None,
            },
        }];
        let persisted_index = PersistedContractIndex::new(&index, &persisted);
        for declaration in &shared {
            assert!(!persisted_owner_matches(
                &persisted[0],
                source_file,
                declaration
            ));
            assert!(
                persisted_index
                    .for_owner(source_file, declaration)
                    .is_empty()
            );
        }
        let no_match = PersistedContractIndex::new(&index, &[]);
        assert!(no_match.for_owner(source_file, shared[0]).is_empty());
        let unique_file = unique_index.source_file(&file_path).unwrap();
        let unique_owner = unique_file
            .declarations
            .iter()
            .find(|declaration| declaration.name == "Shared")
            .unwrap();
        assert!(persisted_owner_matches(
            &persisted[0],
            unique_file,
            unique_owner
        ));
        assert_eq!(
            PersistedContractIndex::new(&unique_index, &persisted)
                .for_owner(unique_file, unique_owner)
                .len(),
            1
        );
    }
}
