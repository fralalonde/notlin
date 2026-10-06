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
pub(crate) struct PropertyBridgeFailure {
    pub declaration: String,
    pub reason: String,
}

pub(crate) fn is_property_interface_candidate(target: &Declaration) -> bool {
    target.kind == DeclarationKind::Interface
        && target.type_params.is_empty()
        && target
            .members
            .iter()
            .any(|member| member.kind == MemberKind::Property)
        && target.members.iter().all(|member| {
            member.kind == MemberKind::Method
                || (member.kind == MemberKind::Property && !member.is_static && !member.has_body)
        })
}

/// Whether every retained Kotlin subtype can be repaired before this simple
/// property interface is translated. A missing declaration, unselected file,
/// custom accessor, or property shape we cannot rewrite keeps the old ABI.
#[cfg(test)]
pub(crate) fn retained_subtypes_repairable(
    index: &SourceIndex,
    target: &Declaration,
    retained: &HashSet<String>,
    translation_roots: &[std::path::PathBuf],
) -> bool {
    retained_subtypes_bridge(index, target, retained, translation_roots).is_ok()
}

pub(crate) fn retained_subtypes_bridge(
    index: &SourceIndex,
    target: &Declaration,
    retained: &HashSet<String>,
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
    if properties.is_empty() || !target.type_params.is_empty() {
        return Err(bridge_rejected(
            index,
            target,
            target,
            "the root contract is empty or generic",
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
    retained: &HashSet<String>,
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
            declaration.language == SourceLanguage::Kotlin && retained.contains(&declaration.name)
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
        let covers_properties =
            subtype_covers_properties(index, contract_owner, subtype, properties);
        let covers_or_inherits_properties = properties.iter().all(|property| {
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
                properties,
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
        ) && !class_supertype_properties_compatible(index, contract_owner, subtype, properties)
        {
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
                    properties
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
                properties.iter().any(|property| {
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
            properties,
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
        && (!member.has_body || !member.has_inline_getter)
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
                let is_contract_owner = parent.name == contract_owner.name
                    && index
                        .declaration_source_file(contract_owner)
                        .is_some_and(|owner_file| owner_file.path == parent_file.path);
                let supplies_method = match parent.kind {
                    DeclarationKind::Interface => parent.members.iter().any(|member| {
                        member.kind == MemberKind::Property
                            && member.name == property.name
                            && interface_member_repairable(
                                index,
                                contract_owner,
                                parent,
                                member,
                                property,
                            )
                    }),
                    DeclarationKind::Class | DeclarationKind::Object => subtype_covers_properties(
                        index,
                        contract_owner,
                        parent,
                        std::slice::from_ref(property),
                    ),
                    _ => false,
                };
                if !is_contract_owner
                    && declaration_is_subtype_of(index, parent, contract_owner)
                    && supplies_method
                {
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
            || member.has_body
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
        matching.is_some_and(|member| {
            let setter = bridged_setter_name(contract, member.is_mutable);
            !member.is_static
                && property_type_matches(index, contract_owner, subtype, member, contract)
                && !subtype.type_params.iter().any(|parameter| {
                    contract.type_name == *parameter
                        || member.type_name.as_deref() == Some(parameter.as_str())
                })
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
        })
    })
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
    contract_owner: &Declaration,
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
            && index.property_getter_return_compatible(
                &subtype_file.path,
                member_type,
                contract_owner,
                &contract.type_name,
            );
    }
    if member_type != contract.type_name {
        let Some(subtype_file) = index.declaration_source_file(subtype) else {
            return false;
        };
        return index.property_getter_return_compatible(
            &subtype_file.path,
            member_type,
            contract_owner,
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
        (Some(left), Some(right)) => {
            left.name == right.name
                && left.language == right.language
                && index
                    .declaration_source_file(left)
                    .zip(index.declaration_source_file(right))
                    .is_some_and(|(left_file, right_file)| left_file.path == right_file.path)
        }
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => {
            unresolved_type_identity_matches(contract_file, simple, subtype_file, simple)
        }
    }
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
) -> bool {
    let Some(implementation_file) = index.declaration_source_file(implementation) else {
        return false;
    };
    let mut pending = implementation
        .supertypes
        .iter()
        .cloned()
        .map(|supertype| (implementation_file.path.clone(), supertype))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while let Some((context_path, supertype)) = pending.pop() {
        let Some(context) = index.source_file(&context_path) else {
            return false;
        };
        let Some(parent) = index.resolve_type(context, &supertype) else {
            continue;
        };
        if !visited.insert(format!("{}:{}", parent.language as u8, parent.name)) {
            continue;
        }
        let Some(parent_file) = index.declaration_source_file(parent) else {
            continue;
        };
        if parent.language == SourceLanguage::Kotlin {
            let parallel_contract = !declaration_is_subtype_of(index, parent, contract_owner);
            let has_jvm_name_annotation = parallel_contract
                && std::fs::read_to_string(&parent_file.path)
                    .is_ok_and(|source| source.contains("@JvmName"));
            for member in parent
                .members
                .iter()
                .filter(|member| member.kind == MemberKind::Property)
            {
                let Some(contract) = properties
                    .iter()
                    .find(|contract| contract.name == member.name)
                else {
                    continue;
                };
                // A parallel Kotlin property changes the meaning of property
                // syntax throughout retained callers when this root becomes a
                // Java getter. The current call-site pass covers useful exact
                // shapes but is not yet an exhaustive proof over inferred and
                // chained receivers, so keep the component Kotlin.
                if parallel_contract {
                    return false;
                }
                // Replacing a parallel interface property with an explicit
                // JavaBean method also changes bare and chained property reads
                // in its default members. Until the shared call-site pass can
                // prove all of those shapes, admit declaration-only matching
                // interfaces. Unrelated parallel supertypes remain harmless.
                if (member.has_custom_accessor
                    && (member.type_name.is_none() || member.has_unsupported_property_shape))
                    || member.has_unsupported_property_shape
                    || member.has_unsupported_property_annotations
                    || has_jvm_name_annotation
                    || (parallel_contract && member.has_body)
                    || (parallel_contract && member.is_mutable && contract.setter.is_none())
                    || (contract.setter.is_some() && !member.is_mutable)
                    || !property_type_matches(index, contract_owner, parent, member, contract)
                {
                    return false;
                }
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
    true
}

/// JavaBean call-site contracts introduced while repairing retained Kotlin
/// interface properties.  The owner is the Kotlin declaration whose property
/// syntax disappears; callers typed as that interface (or a descendant) must
/// invoke the explicit getter/setter after the repair.
pub fn repaired_callsite_contracts(
    index: &SourceIndex,
    generated_java: &HashSet<std::path::PathBuf>,
) -> Vec<PropertyAccessorContract> {
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
            let Some(java_contracts) = java_property_contracts(declaration, &source_file.path)
            else {
                continue;
            };
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
            contracts_for_repair(
                index,
                source_file,
                declaration,
                generated_java,
                &mut contracts,
            );
        }
    }
    crate::transpiler::fixpoint::install_parallel(|| {
        sources
            .par_iter_mut()
            .map(|(path, source)| {
                let (repaired, count) = repair_source_cached(index, path, source, &contracts);
                *source = repaired;
                count
            })
            .sum()
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
    repair_source_cached(index, path, source, contract_cache)
}

fn repair_source_cached(
    index: &SourceIndex,
    path: &Path,
    source: &str,
    contract_cache: &HashMap<String, Vec<PropertyContract>>,
) -> (String, usize) {
    let Some(source_file) = index.source_file(path) else {
        return (source.to_string(), 0);
    };
    if source_file.language != SourceLanguage::Kotlin {
        return (source.to_string(), 0);
    }
    let tree = crate::transpiler::parse_tree(source);
    let mut edits = Vec::new();
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
                    repair_declaration(index, node, source, declaration, contracts, &mut edits);
                }
                // `repair_declaration` skips nested declarations while it
                // rewrites this declaration's direct properties. Keep walking
                // so a nested type can be repaired from its own supertypes.
            }
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    let count = edits.len();
    if count == 0 {
        return (source.to_string(), 0);
    }
    (crate::smart_cast::apply_edits(source, edits), count)
}

fn inherited_generated_contracts(
    index: &SourceIndex,
    source_file: &SourceFile,
    declaration: &Declaration,
    generated_java: &HashSet<std::path::PathBuf>,
) -> Vec<PropertyContract> {
    let mut pending = declaration
        .supertypes
        .iter()
        .cloned()
        .map(|supertype| (source_file.path.clone(), supertype))
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    let mut by_name = HashMap::<String, PropertyContract>::new();
    while let Some((context_path, supertype)) = pending.pop() {
        let Some(context) = index.source_file(&context_path) else {
            continue;
        };
        let Some(resolved) = index.resolve_type(context, &supertype) else {
            continue;
        };
        let key = format!("{}:{}", resolved.language as u8, resolved.name);
        if !visited.insert(key) {
            continue;
        }
        let Some(file) = index.declaration_source_file(resolved) else {
            continue;
        };
        if resolved.language == SourceLanguage::Java
            && is_generated_notlin_java(file.path.as_path(), generated_java)
            && let Some(contracts) = java_property_contracts(resolved, &file.path)
        {
            by_name.extend(
                contracts
                    .into_iter()
                    .map(|contract| (contract.name.clone(), contract)),
            );
        }
        // Generated Java interfaces can themselves extend an earlier
        // generated property contract. Retained Kotlin descendants need the
        // complete inherited getter set, not only the nearest Java interface.
        pending.extend(
            resolved
                .supertypes
                .iter()
                .cloned()
                .map(|supertype| (file.path.clone(), supertype)),
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
    contracts_for_repair_inner(
        index,
        source_file,
        declaration,
        generated_java,
        &mut HashSet::new(),
        cache,
    )
}

fn contracts_for_repair_inner(
    index: &SourceIndex,
    source_file: &SourceFile,
    declaration: &Declaration,
    generated_java: &HashSet<std::path::PathBuf>,
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
    let mut by_name =
        inherited_generated_contracts(index, source_file, declaration, generated_java)
            .into_iter()
            .map(|contract| (contract.name.clone(), contract))
            .collect::<HashMap<_, _>>();

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
        for contract in
            contracts_for_repair_inner(index, parent_file, parent, generated_java, component, cache)
        {
            by_name.entry(contract.name.clone()).or_insert(contract);
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
        for contract in
            inherited_generated_contracts(index, descendant_file, descendant, generated_java)
        {
            let redeclares_compatible_shape = declaration.members.iter().any(|member| {
                member.kind == MemberKind::Property
                    && member.name == contract.name
                    && !member.has_unsupported_property_shape
                    && !member.has_unsupported_property_annotations
            });
            if redeclares_compatible_shape {
                by_name.entry(contract.name.clone()).or_insert(contract);
            }
        }
    }
    let mut contracts = by_name.into_values().collect::<Vec<_>>();
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

fn repair_declaration(
    index: &SourceIndex,
    node: Node<'_>,
    source: &str,
    declaration: &Declaration,
    contracts: &[PropertyContract],
    edits: &mut Vec<crate::smart_cast::Edit>,
) {
    let is_interface = declaration.kind == DeclarationKind::Interface;
    let mut stack = vec![node];
    let mut added_methods = Vec::new();
    while let Some(child) = stack.pop() {
        if matches!(child.kind(), "property_declaration" | "class_parameter") {
            let name = property_name(child, source);
            if let Some(contract) = contracts.iter().find(|contract| contract.name == name) {
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
    let ty = property_type(text)?;
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
    if let Some(accessor) = [" get()", " set("]
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
        let retained = HashSet::from(["Outer".to_string(), "Nested".to_string()]);
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

    fn planner_fixture(descendant: &str) -> (Fixture, SourceIndex, HashSet<String>) {
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
            .map(|declaration| declaration.name.clone())
            .collect();
        (fixture, index, retained)
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
        let retained = HashSet::from(["Impl".to_string()]);

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
            .map(|declaration| declaration.name.clone())
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
            .map(|declaration| declaration.name.clone())
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
        let exact_retained = HashSet::from(["Impl".to_string()]);
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
        let mismatch_retained = HashSet::from(["Impl".to_string()]);
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
        let wrapper_retained = HashSet::from(["Impl".to_string()]);
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
            .map(|declaration| declaration.name.clone())
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
        let retained = HashSet::from(["Impl".to_string(), "Work".to_string()]);
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
        assert!(!retained_subtypes_repairable(
            &inline_index,
            inline_target,
            &inline_retained,
            std::slice::from_ref(&inline_fixture.0),
        ));
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
        let retained = HashSet::from([
            "Parent".to_string(),
            "Child".to_string(),
            "Kind".to_string(),
        ]);
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
            &HashSet::from(["Parent".to_string(), "Child".to_string()]),
            std::slice::from_ref(&mismatch.0),
        ));
    }

    #[test]
    fn computed_property_on_separate_kotlin_supertype_blocks_the_bridge() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\nclass Order\ninterface Api { val order: Order }\n",
        );
        fixture.write(
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
            .map(|declaration| declaration.name.clone())
            .collect();
        assert!(!retained_subtypes_repairable(
            &index,
            target,
            &retained,
            std::slice::from_ref(&fixture.0),
        ));
    }

    #[test]
    fn compatible_parallel_component_waits_for_exhaustive_call_site_proof() {
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
        let retained = HashSet::from(["JobContainerInfo".to_string()]);
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
                    &HashSet::from(["JobContainerInfo".to_string()]),
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
}
