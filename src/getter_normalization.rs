//! Discover complete property families that need distinct source and bean views.
//! This is the planning boundary; discovering a family does not authorize edits.
use crate::semantics::{self, SymbolId};
use crate::workspace::{Declaration, DeclarationKind, MemberKind, SourceIndex, SourceLanguage};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

mod layout;
pub use layout::{LayoutPlan, plan_layout};
mod java_callers;
pub use java_callers::{JavaCallerPlan, plan_java_callers};

#[derive(Debug, Serialize)]
pub struct PropertyOwner {
    pub owner: SymbolId,
    pub property: String,
    pub getter: Option<String>,
    pub setter: Option<String>,
    pub declared_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GetterFamily {
    pub root: SymbolId,
    pub property: String,
    pub original_getter: String,
    pub proposed_getter: String,
    pub property_owners: Vec<PropertyOwner>,
    pub independent_methods: Vec<SymbolId>,
    pub java_implementations: Vec<SymbolId>,
    pub descendants: Vec<SymbolId>,
    pub unresolved_edges: Vec<String>,
}

/// Collect source-bound families rather than matching getX spellings globally.
/// Every physical getter and setter is read from the exact owner/member pair.
pub fn discover(index: &SourceIndex) -> Vec<GetterFamily> {
    let mut roots = BTreeMap::<(SymbolId, String, String), &Declaration>::new();
    for file in index.kotlin_files() {
        for owner in &file.declarations {
            if !matches!(
                owner.kind,
                DeclarationKind::Class | DeclarationKind::Object | DeclarationKind::Enum
            ) {
                continue;
            }
            for method in owner.members.iter().filter(|member| {
                member.kind == MemberKind::Method
                    && !member.is_static
                    && member.parameter_types.is_empty()
                    && member.visibility.as_deref() != Some("private")
            }) {
                let mut ancestors = vec![owner];
                ancestors.extend(ancestry(index, owner));
                let mut properties = Vec::new();
                for ancestor in &ancestors {
                    if ancestor.language != SourceLanguage::Kotlin {
                        continue;
                    }
                    for property in ancestor.members.iter().filter(|member| {
                        member.kind == MemberKind::Property
                            && !member.is_static
                            && !member.is_jvm_field
                            && member.visibility.as_deref() != Some("private")
                    }) {
                        if index
                            .property_accessor_names(ancestor, property)
                            .getter(&property.name)
                            .as_deref()
                            == Some(method.name.as_str())
                        {
                            properties.push((*ancestor, property));
                        }
                    }
                }
                for (candidate, property) in &properties {
                    // Choose the earliest property declaration. A concrete
                    // class implementation and an intermediate interface can
                    // share one root without becoming two independent groups.
                    if properties.iter().any(|(other, other_property)| {
                        other_property.name == property.name
                            && !std::ptr::eq(*candidate, *other)
                            && ancestry(index, candidate)
                                .into_iter()
                                .any(|parent| std::ptr::eq(parent, *other))
                    }) {
                        continue;
                    }
                    roots
                        .entry((
                            semantics::workspace_symbol(index, candidate),
                            property.name.clone(),
                            method.name.clone(),
                        ))
                        .or_insert(candidate);
                }
            }
        }
    }
    let mut families = Vec::new();
    for ((root, property, getter), declaration) in roots {
        let mut family = GetterFamily {
            root,
            property: property.clone(),
            original_getter: getter.clone(),
            proposed_getter: String::new(),
            property_owners: Vec::new(),
            independent_methods: Vec::new(),
            java_implementations: Vec::new(),
            descendants: Vec::new(),
            unresolved_edges: Vec::new(),
        };
        let mut nodes = vec![declaration];
        let mut seen = BTreeSet::new();
        let mut taken_names = BTreeSet::new();
        while let Some(owner) = nodes.pop() {
            let symbol = semantics::workspace_symbol(index, owner);
            if !seen.insert(symbol.clone()) {
                continue;
            }
            nodes.extend(index.direct_subtypes(owner));
            family.descendants.push(symbol.clone());
            let Some(file) = index.declaration_source_file(owner) else {
                continue;
            };
            for edge in &owner.supertypes {
                if crate::workspace::property_receiver_type_binding(index, file, edge).is_none() {
                    family
                        .unresolved_edges
                        .push(format!("{} -> {edge}", symbol.stable_key()));
                }
            }
            for member in &owner.members {
                taken_names.insert(member.name.clone());
                if member.kind == MemberKind::Property {
                    let names = index.property_accessor_names(owner, member);
                    taken_names.extend(names.getter(&member.name));
                    taken_names.extend(names.setter(&member.name));
                    if member.name == property {
                        family.property_owners.push(PropertyOwner {
                            owner: symbol.clone(),
                            property: property.clone(),
                            getter: names.getter(&property),
                            setter: member.is_mutable.then(|| names.setter(&property)).flatten(),
                            declared_type: member.type_name.clone(),
                        });
                    }
                }
                if member.kind == MemberKind::Method
                    && member.name == getter
                    && member.parameter_types.is_empty()
                    && !member.is_static
                {
                    let mut function = symbol.clone();
                    function.owner_path.push(function.name.clone());
                    function.kind = "function".into();
                    function.name = member.name.clone();
                    function.parameters.clear();
                    if owner.language == SourceLanguage::Java {
                        family.java_implementations.push(function);
                    } else {
                        family.independent_methods.push(function);
                    }
                }
            }
        }
        let stem = format!(
            "read{}",
            property
                .chars()
                .enumerate()
                .map(|(i, c)| if i == 0 {
                    c.to_uppercase().collect::<String>()
                } else {
                    c.to_string()
                })
                .collect::<String>()
        );
        let mut name = stem.clone();
        let mut suffix = 2;
        while taken_names.contains(&name) {
            name = format!("{stem}{suffix}");
            suffix += 1;
        }
        family.proposed_getter = name;
        family.descendants.sort();
        family.independent_methods.sort();
        family.independent_methods.dedup();
        family.java_implementations.sort();
        family.java_implementations.dedup();
        family.property_owners.sort_by(|a, b| a.owner.cmp(&b.owner));
        family.unresolved_edges.sort();
        family.unresolved_edges.dedup();
        families.push(family);
    }
    families
}

fn ancestry<'a>(index: &'a SourceIndex, owner: &'a Declaration) -> Vec<&'a Declaration> {
    let mut pending = vec![owner];
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    while let Some(owner) = pending.pop() {
        let Some(file) = index.declaration_source_file(owner) else {
            continue;
        };
        for edge in &owner.supertypes {
            let Some(parent) = crate::workspace::property_receiver_type_binding(index, file, edge)
                .and_then(|binding| binding.declaration)
            else {
                continue;
            };
            let id = semantics::workspace_symbol(index, parent);
            if seen.insert(id) {
                found.push(parent);
                pending.push(parent);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_closes_one_root_across_diamonds_covariance_and_java_implementers() {
        let root = std::env::temp_dir().join(format!(
            "notlin-getter-family-census-{}",
            std::process::id()
        ));
        assert!(!root.exists());
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("Family.kt"),
            r#"package sample
interface Root {val id:Any}
interface Contract:Root {override var id:Any}
open class Base(override var id:Any):Root
open class Lookup:Base("stored"),Contract {fun getId():String="lookup";fun readId():Any=id}
class Leaf:Lookup()
class Typed(override val id:String):Root
class Own(val token:Any) {fun getToken():String="token"}
enum class Choice(val code:Any) {ONE("one");fun getCode():String="code"}
class Unrelated(val token:Any)
"#,
        )
        .unwrap();
        std::fs::write(root.join("Existing.java"), "package sample;public class Existing implements Root {public Object getId(){return \"java\";}}\n").unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        let families = discover(&index);
        assert_eq!(families.len(), 3, "{families:#?}");
        let family = families
            .iter()
            .find(|family| family.root.name == "Root")
            .unwrap();
        assert_eq!(family.property_owners.len(), 4);
        assert_eq!(family.descendants.len(), 7);
        assert_eq!(family.independent_methods.len(), 1);
        assert_eq!(family.independent_methods[0].owner_path, ["Lookup"]);
        assert_eq!(family.java_implementations.len(), 1);
        assert_eq!(family.java_implementations[0].owner_path, ["Existing"]);
        assert_eq!(family.proposed_getter, "readId2");
        assert!(family.unresolved_edges.is_empty());
        assert!(
            family
                .property_owners
                .iter()
                .any(|property| property.owner.name == "Typed"
                    && property.declared_type.as_deref() == Some("String"))
        );
        assert!(
            families
                .iter()
                .any(|family| family.root.name == "Own" && family.proposed_getter == "readToken")
        );
        assert!(
            families
                .iter()
                .any(|family| family.root.name == "Choice" && family.proposed_getter == "readCode")
        );
        assert!(root.is_absolute() && root.starts_with(std::env::temp_dir()));
        std::fs::remove_dir_all(root).unwrap();
    }
}

pub(crate) mod workspace;
