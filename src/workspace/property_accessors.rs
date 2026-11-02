//! Physical accessor names are independent of Kotlin property names.

use serde::{Deserialize, Serialize};
use tree_sitter::Node;

impl super::SourceIndex {
    pub fn property_accessor_names(
        &self,
        owner: &super::Declaration,
        member: &super::Member,
    ) -> PropertyAccessors {
        self.declaration_source_file(owner)
            .map(|file| member.accessors.in_context(self, file))
            .unwrap_or_else(|| member.accessors.clone())
    }

    /// Methods with bean-like names do not replace a distinct Kotlin property.
    pub(crate) fn property_in_type<'a>(
        &'a self,
        declaring: &std::path::Path,
        receiver: &str,
        property: &str,
    ) -> Option<(&'a super::Declaration, &'a super::Member)> {
        let file = self.source_file(declaring)?;
        let owner = super::property_receiver_type_binding(self, file, receiver)?.declaration?;
        let mut pending = vec![owner];
        let mut seen = std::collections::HashSet::new();
        let mut candidates = Vec::new();
        while let Some(owner) = pending.pop() {
            if !seen.insert(owner as *const super::Declaration as usize) {
                continue;
            }
            let members = owner
                .members
                .iter()
                .filter(|member| {
                    member.kind == super::MemberKind::Property
                        && member.name == property
                        && !member.is_static
                })
                .collect::<Vec<_>>();
            if !members.is_empty() {
                candidates.extend(members.into_iter().map(|member| (owner, member)));
                continue;
            }
            let context = self.declaration_source_file(owner)?;
            for parent in &owner.supertypes {
                pending.push(
                    super::property_receiver_type_binding(self, context, parent)?.declaration?,
                );
            }
        }
        match candidates.as_slice() {
            [candidate] => Some(*candidate),
            _ => None,
        }
    }
}

pub(crate) fn emitted_contracts(
    index: &super::SourceIndex,
    owners: &[crate::semantics::SymbolId],
) -> Vec<(
    std::path::PathBuf,
    crate::property_callsite::PropertyAccessorContract,
)> {
    let accepted = owners.iter().collect::<std::collections::HashSet<_>>();
    let mut contracts = Vec::new();
    for file in index.kotlin_files() {
        for owner in &file.declarations {
            let symbol = crate::semantics::workspace_symbol(index, owner);
            if !accepted.contains(&symbol) {
                continue;
            }
            for property in &owner.members {
                if property.kind != super::MemberKind::Property
                    || property.is_static
                    || property.is_jvm_field
                    || property.accessors.is_default()
                    || property.visibility.as_deref() == Some("private")
                {
                    continue;
                }
                let names = index.property_accessor_names(owner, property);
                if names.is_default() {
                    continue;
                }
                let Some(getter) = names.getter(&property.name) else {
                    continue;
                };
                if getter.starts_with("notlinProperty") {
                    continue;
                }
                let mut path = symbol.owner_path.clone();
                path.push(symbol.name.clone());
                let owner_type = if symbol.package.is_empty() {
                    path.join(".")
                } else {
                    format!("{}.{}", symbol.package, path.join("."))
                };
                contracts.push((
                    file.path.clone(),
                    crate::property_callsite::PropertyAccessorContract {
                        owner_type,
                        property: property.name.clone(),
                        getter,
                        setter: (property.is_mutable && !property.has_custom_accessor)
                            .then(|| names.setter(&property.name))
                            .flatten(),
                    },
                ));
            }
        }
    }
    contracts
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessorName {
    #[default]
    Default,
    Named(String),
    Unknown,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyAccessors {
    pub getter: AccessorName,
    pub setter: AccessorName,
    #[serde(default)]
    getter_annotation: Option<String>,
    #[serde(default)]
    setter_annotation: Option<String>,
}

impl PropertyAccessors {
    pub(crate) fn in_context(&self, index: &super::SourceIndex, file: &super::SourceFile) -> Self {
        let mut result = self.clone();
        // Imports/aliases were bound during extraction. The remaining short
        // spelling can be shadowed by another source in the same package.
        for (name, annotation) in [
            (&mut result.getter, &self.getter_annotation),
            (&mut result.setter, &self.setter_annotation),
        ] {
            if annotation.as_deref() == Some("JvmName")
                && index
                    .find_qualified(
                        &file
                            .package
                            .as_ref()
                            .map(|package| format!("{package}.JvmName"))
                            .unwrap_or_else(|| "JvmName".into()),
                    )
                    .is_some()
            {
                *name = AccessorName::Default;
            }
        }
        result
    }

    pub fn getter(&self, property: &str) -> Option<String> {
        match &self.getter {
            AccessorName::Default => Some(super::property_accessor_name(property)),
            AccessorName::Named(name) => Some(name.clone()),
            AccessorName::Unknown => None,
        }
    }

    pub fn setter(&self, property: &str) -> Option<String> {
        match &self.setter {
            AccessorName::Default => Some(crate::property_abi::setter_name(property)),
            AccessorName::Named(name) => Some(name.clone()),
            AccessorName::Unknown => None,
        }
    }

    pub fn is_default(&self) -> bool {
        self.getter == AccessorName::Default && self.setter == AccessorName::Default
    }
}

/// Inspect only annotations attached to the property or its accessors. An
/// initializer, comment or getter body mentioning JvmName cannot rename it.
pub(crate) fn from_node(property: Node<'_>, source: &str) -> PropertyAccessors {
    let mut result = PropertyAccessors::default();
    let mut pending = property
        .named_children(&mut property.walk())
        .filter(|node| {
            matches!(
                node.kind(),
                "modifiers" | "parameter_modifiers" | "getter" | "setter"
            )
        })
        .map(|node| (node, None))
        .collect::<Vec<_>>();
    while let Some((node, accessor)) = pending.pop() {
        let accessor = match node.kind() {
            "getter" => Some("get"),
            "setter" => Some("set"),
            _ => accessor,
        };
        if node.kind() == "annotation" {
            let binding = crate::transpiler::unit::kotlin_builtin_annotation_binding(
                node,
                "kotlin.jvm.JvmName",
                source,
                None,
                None,
            );
            if binding == Some(false) {
                continue;
            }
            if annotation_is_local(node, source) {
                continue;
            }
            let target = node
                .named_children(&mut node.walk())
                .find(|child| child.kind() == "use_site_target")
                .and_then(|target| target.utf8_text(source.as_bytes()).ok())
                .map(|target| target.trim().trim_end_matches(':'))
                .or(accessor);
            let (slot, annotation_binding) = match target {
                Some("get") => (&mut result.getter, &mut result.getter_annotation),
                Some("set") => (&mut result.setter, &mut result.setter_annotation),
                _ => continue,
            };
            *annotation_binding = crate::transpiler::unit::annotation_fq_name(node, source)
                .map(|name| crate::transpiler::unit::annotation_binding_name(name, source));
            let name = (binding == Some(true))
                .then(|| literal_name(node, source))
                .flatten();
            *slot = match (slot.clone(), name) {
                (AccessorName::Default, Some(name)) => AccessorName::Named(name),
                _ => AccessorName::Unknown,
            };
        } else {
            pending.extend(
                node.named_children(&mut node.walk())
                    .filter(|child| {
                        !matches!(child.kind(), "function_body" | "block" | "string_literal")
                    })
                    .map(|child| (child, accessor)),
            );
        }
    }
    result
}

fn literal_name(annotation: Node<'_>, source: &str) -> Option<String> {
    let mut pending = vec![annotation];
    let mut names = Vec::new();
    while let Some(node) = pending.pop() {
        if node.kind() == "value_argument" {
            let values = node
                .named_children(&mut node.walk())
                .filter(|child| !child.is_extra())
                .collect::<Vec<_>>();
            let value = values.last()?;
            if value.kind() != "string_literal"
                || value
                    .named_children(&mut value.walk())
                    .any(|part| part.kind() == "interpolation")
            {
                return None;
            }
            let name = value
                .utf8_text(source.as_bytes())
                .ok()?
                .strip_prefix('"')?
                .strip_suffix('"')?;
            if name.contains('$') || !crate::semantics::is_java_identifier(name) {
                return None;
            }
            names.push(name.to_owned());
        } else {
            pending.extend(node.named_children(&mut node.walk()));
        }
    }
    match names.as_slice() {
        [name] => Some(name.clone()),
        _ => None,
    }
}

fn annotation_is_local(annotation: Node<'_>, source: &str) -> bool {
    let mut pending = vec![annotation];
    let mut spelling = None;
    while let Some(node) = pending.pop() {
        if node.kind() == "user_type" {
            spelling = node.utf8_text(source.as_bytes()).ok();
            break;
        }
        pending.extend(
            node.named_children(&mut node.walk())
                .filter(|child| child.kind() != "value_arguments"),
        );
    }
    let Some(name) = spelling.filter(|name| !name.contains('.')) else {
        return false;
    };
    let mut scope = annotation.parent();
    while let Some(node) = scope {
        if matches!(node.kind(), "source_file" | "class_body" | "block")
            && node.named_children(&mut node.walk()).any(|declaration| {
                matches!(
                    declaration.kind(),
                    "class_declaration" | "object_declaration" | "type_alias"
                ) && declaration
                    .child_by_field_name("name")
                    .and_then(|node| node.utf8_text(source.as_bytes()).ok())
                    == Some(name)
            })
        {
            return true;
        }
        scope = node.parent();
    }
    false
}
