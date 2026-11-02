//! Source demand for compatibility methods, rather than hypothetical callers.

use super::{Declaration, SourceFile, SourceIndex, SourceLanguage};
use crate::semantics::{self, KotlinType, SymbolId};
use tree_sitter::Node;

fn any_node(root: Node<'_>, mut test: impl FnMut(Node<'_>) -> bool) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if test(node) {
            return true;
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    false
}

fn parse(file: &SourceFile) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&match file.language {
            SourceLanguage::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            SourceLanguage::Java => tree_sitter_java::LANGUAGE.into(),
        })
        .ok()?;
    let tree = parser.parse(file.source_text(), None)?;
    (!tree.root_node().has_error()).then_some(tree)
}

fn navigation(node: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    if node.kind() == "field_access" {
        return Some((
            node.child_by_field_name("object")?,
            node.child_by_field_name("field")?,
        ));
    }
    if node.kind() != "navigation_expression" {
        return None;
    }
    let children = node
        .named_children(&mut node.walk())
        .filter(|node| !node.kind().contains("comment"))
        .collect::<Vec<_>>();
    let [receiver, name] = children.as_slice() else {
        return None;
    };
    (name.kind() == "identifier").then_some((*receiver, *name))
}

impl SourceIndex {
    /// A direct enum call, a call through its declared parameter type, or a
    /// consumed parent contract plus an actual enum value use establishes
    /// demand. Declaration names, comments and unrelated calls do not.
    pub(crate) fn retained_default_method_is_used(
        &self,
        owner: &Declaration,
        method: &str,
        required_contracts: &[SymbolId],
    ) -> bool {
        let Some(owner_file) = self.declaration_source_file(owner) else {
            return false;
        };
        let Some(tree) = parse(owner_file) else {
            return false;
        };
        let owner_id = semantics::workspace_symbol(self, owner);
        let Some(declaration) = super::default_getters::exact_declaration_node(
            tree.root_node(),
            owner_file.source_text(),
            &owner_file.path,
            &owner_id,
        ) else {
            return false;
        };
        let mut entries = Vec::new();
        any_node(declaration, |node| {
            if node.kind() == "enum_entry"
                && node
                    .parent()
                    .and_then(|body| body.parent())
                    .is_some_and(|parent| parent.id() == declaration.id())
                && let Some(name) = node
                    .child_by_field_name("name")
                    .or_else(|| crate::transpiler::kt::child(node, "identifier"))
            {
                entries.push(
                    name.utf8_text(owner_file.source_text().as_bytes())
                        .unwrap_or(""),
                );
            }
            false
        });
        let enum_type = |file: &SourceFile, node: Node<'_>| {
            let source = file.source_text();
            let qualifier = node.utf8_text(source.as_bytes()).ok()?;
            let head = qualifier.split('.').next()?;
            if crate::function_callsite::local_shadow(node, head, source)
                || semantics::lexical_binding_type(source, node, head).is_some()
            {
                return None;
            }
            let target = super::canonical_type_binding(self, file, qualifier)?.declaration?;
            (semantics::workspace_symbol(self, target) == owner_id).then_some(target)
        };
        let entry_owner = |file: &SourceFile, node: Node<'_>| {
            let (qualifier, entry) = navigation(node)?;
            let entry = entry.utf8_text(file.source_text().as_bytes()).ok()?;
            entries
                .contains(&entry)
                .then(|| enum_type(file, qualifier))
                .flatten()
        };
        let files = self
            .kotlin_files()
            .chain(self.java_files())
            .collect::<Vec<_>>();
        let value_is_used = files.iter().any(|file| {
            if file.ctor_calls.iter().any(|call| {
                super::canonical_type_binding(self, file, &call.callee)
                    .and_then(|binding| binding.declaration)
                    .is_some_and(|target| semantics::workspace_symbol(self, target) == owner_id)
            }) {
                return true;
            }
            let possible = entries
                .iter()
                .any(|entry| file.identifier_counts.contains_key(*entry))
                || ["values", "getEntries", "valueOf", "entries"]
                    .iter()
                    .any(|name| file.identifier_counts.contains_key(*name));
            possible
                && parse(file).is_some_and(|tree| {
                    any_node(tree.root_node(), |node| {
                        if entry_owner(file, node).is_some() {
                            return true;
                        }
                        if let Some((qualifier, name)) = navigation(node)
                            && name.utf8_text(file.source_text().as_bytes()).ok() == Some("entries")
                            && enum_type(file, qualifier).is_some()
                        {
                            return true;
                        }
                        if !matches!(node.kind(), "call_expression" | "method_invocation") {
                            return false;
                        }
                        let Some(callee) = semantics::call_callee_node(node) else {
                            return false;
                        };
                        matches!(
                            callee.utf8_text(file.source_text().as_bytes()).ok(),
                            Some("values" | "getEntries" | "valueOf")
                        ) && semantics::explicit_receiver_node(node, callee)
                            .is_some_and(|receiver| enum_type(file, receiver).is_some())
                    })
                })
        });
        let property = super::property_name_from_getter(method).or_else(|| {
            method
                .strip_prefix("is")
                .filter(|tail| tail.chars().next().is_some_and(char::is_uppercase))
                .map(|_| method.to_owned())
        });
        files.into_iter().any(|file| {
            if !file.identifier_counts.contains_key(method)
                && property
                    .as_ref()
                    .is_none_or(|name| !file.identifier_counts.contains_key(name))
            {
                return false;
            }
            let Some(tree) = parse(file) else {
                return false;
            };
            let source = file.source_text();
            any_node(tree.root_node(), |node| {
                let receiver = if matches!(node.kind(), "call_expression" | "method_invocation") {
                    let Some(callee) = semantics::call_callee_node(node) else {
                        return false;
                    };
                    if callee.utf8_text(source.as_bytes()).ok() != Some(method)
                        || node
                            .child_by_field_name("arguments")
                            .or_else(|| crate::transpiler::kt::child(node, "value_arguments"))
                            .or_else(|| {
                                crate::transpiler::kt::child(node, "call_suffix").and_then(
                                    |suffix| {
                                        crate::transpiler::kt::child(suffix, "value_arguments")
                                    },
                                )
                            })
                            .is_none_or(|args| {
                                args.named_children(&mut args.walk())
                                    .any(|arg| !arg.kind().contains("comment"))
                            })
                    {
                        return false;
                    }
                    semantics::explicit_receiver_node(node, callee)
                } else if let Some((receiver, name)) = navigation(node)
                    && property.as_deref() == name.utf8_text(source.as_bytes()).ok()
                {
                    Some(receiver)
                } else {
                    return false;
                };
                if receiver.is_none_or(|receiver| {
                    receiver.utf8_text(source.as_bytes()).ok() == Some("this")
                }) {
                    if receiver.is_none()
                        && crate::function_callsite::local_shadow(node, method, source)
                    {
                        return false;
                    }
                    let mut ancestor = node.parent();
                    while ancestor.is_some_and(|parent| parent.kind() != "class_declaration") {
                        ancestor = ancestor.and_then(|parent| parent.parent());
                    }
                    return required_contracts.is_empty()
                        && ancestor.is_some_and(|parent| {
                            semantics::symbol_id_for_node(source, parent, &file.path) == owner_id
                        });
                }
                let receiver_owner = if let Some(receiver) = receiver {
                    if let Some(target) = entry_owner(file, receiver) {
                        Some(target)
                    } else {
                        let name = receiver.utf8_text(source.as_bytes()).unwrap_or("");
                        let Some(ty) = semantics::lexical_binding_type(source, node, name) else {
                            return false;
                        };
                        let KotlinType::Named { name, .. } = ty.ty else {
                            return false;
                        };
                        super::canonical_type_binding(self, file, &name)
                            .and_then(|binding| binding.declaration)
                    }
                } else {
                    None
                };
                receiver_owner.is_some_and(|target| {
                    let consumes_required_slot = required_contracts.is_empty()
                        || self
                            .effective_interface_value_member(target, file, method)
                            .is_some_and(|(provider, _)| {
                                required_contracts
                                    .contains(&semantics::workspace_symbol(self, provider))
                            });
                    consumes_required_slot
                        && (semantics::workspace_symbol(self, target) == owner_id
                            || value_is_used
                                && super::proven_reference_subtype(
                                    self,
                                    owner_file,
                                    &super::declaration_key(owner),
                                    self.declaration_source_file(target).unwrap_or(file),
                                    &super::declaration_key(target),
                                ))
                })
            })
        })
    }
}
