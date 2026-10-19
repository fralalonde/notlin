//! Source-only header proofs collected from the indexer's existing syntax tree.
//! No syntax nodes survive the snapshot, and ambiguous owners stay unproven.

use super::{DeclarationKind, SourceLanguage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tree_sitter::Node;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct HeaderFacts {
    /// None denotes duplicate owner names, including nested declarations.
    getters: HashMap<String, Option<HashMap<String, bool>>>,
    bounds: HashMap<String, Vec<OwnerBounds>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OwnerBounds {
    kind: DeclarationKind,
    parameters: HashMap<String, Option<String>>,
}

impl HeaderFacts {
    pub(super) fn record(&mut self, node: Node<'_>, language: SourceLanguage, source: &str) {
        let getter_owner = match language {
            SourceLanguage::Java => matches!(
                node.kind(),
                "class_declaration" | "interface_declaration" | "record_declaration"
            ),
            SourceLanguage::Kotlin => {
                matches!(node.kind(), "class_declaration" | "object_declaration")
            }
        };
        if getter_owner
            && let Some(name) = node.child_by_field_name("name")
            && let Ok(name) = name.utf8_text(source.as_bytes())
        {
            let methods = method_headers(node, language, source);
            self.getters
                .entry(name.to_owned())
                .and_modify(|entry| *entry = None)
                .or_insert(Some(methods));
        }
        if language == SourceLanguage::Kotlin
            && declaration_node(node)
            && let Some(kind) = declaration_kind(node)
        {
            self.bounds
                .entry(declaration_name(node, source))
                .or_default()
                .push(OwnerBounds {
                    kind,
                    parameters: inline_bounds(node, source),
                });
        }
        if language == SourceLanguage::Java
            && java_generic_owner(node)
            && let Some(kind) = java_declaration_kind(node)
        {
            self.bounds
                .entry(declaration_name(node, source))
                .or_default()
                .push(OwnerBounds {
                    kind,
                    parameters: java_bounds(node, source),
                });
        }
    }

    pub(super) fn getter_has_no_method_type_parameters(&self, owner: &str, getter: &str) -> bool {
        self.getters
            .get(owner)
            .and_then(Option::as_ref)
            .and_then(|methods| methods.get(getter))
            .copied()
            .unwrap_or(false)
    }

    pub(super) fn inline_bound(
        &self,
        owner: &str,
        kind: DeclarationKind,
        parameter: &str,
    ) -> Option<&str> {
        let mut owners = self
            .bounds
            .get(owner)?
            .iter()
            .filter(|owner| owner.kind == kind);
        let owner = owners.next()?;
        if owners.next().is_some() {
            return None;
        }
        owner.parameters.get(parameter)?.as_deref()
    }
}

fn java_generic_owner(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_declaration" | "interface_declaration" | "record_declaration"
    )
}

fn java_declaration_kind(node: Node<'_>) -> Option<DeclarationKind> {
    Some(match node.kind() {
        "interface_declaration" => DeclarationKind::Interface,
        "class_declaration" => DeclarationKind::Class,
        "record_declaration" => DeclarationKind::Record,
        _ => return None,
    })
}

fn java_bounds(owner: Node<'_>, source: &str) -> HashMap<String, Option<String>> {
    let Some(parameters) = owner.child_by_field_name("type_parameters") else {
        return HashMap::new();
    };
    let mut bounds = HashMap::new();
    for parameter in parameters
        .named_children(&mut parameters.walk())
        .filter(|node| node.kind() == "type_parameter")
    {
        let children = parameter
            .named_children(&mut parameter.walk())
            .collect::<Vec<_>>();
        let name = children
            .iter()
            .find(|child| child.kind() == "type_identifier")
            .and_then(|child| child.utf8_text(source.as_bytes()).ok());
        let bound = if children
            .iter()
            .any(|child| matches!(child.kind(), "annotation" | "marker_annotation"))
        {
            None
        } else if let Some(bound_node) = children.iter().find(|child| child.kind() == "type_bound")
        {
            let types = bound_node
                .named_children(&mut bound_node.walk())
                .collect::<Vec<_>>();
            match types.as_slice() {
                [ty] if ordinary_java_bound(*ty) => {
                    ty.utf8_text(source.as_bytes()).ok().map(str::to_owned)
                }
                _ => None,
            }
        } else {
            Some("java.lang.Object".to_owned())
        };
        let Some(name) = name else {
            continue;
        };
        bounds
            .entry(name.to_owned())
            .and_modify(|value| *value = None)
            .or_insert(bound);
    }
    bounds
}

fn ordinary_java_bound(node: Node<'_>) -> bool {
    matches!(node.kind(), "type_identifier" | "scoped_type_identifier")
}

fn method_headers(
    owner: Node<'_>,
    language: SourceLanguage,
    source: &str,
) -> HashMap<String, bool> {
    let body = owner.child_by_field_name("body").or_else(|| {
        (language == SourceLanguage::Kotlin)
            .then(|| {
                owner
                    .named_children(&mut owner.walk())
                    .find(|node| node.kind() == "class_body")
            })
            .flatten()
    });
    let Some(body) = body else {
        return HashMap::new();
    };
    let mut methods = HashMap::new();
    for method in body.named_children(&mut body.walk()) {
        let parameters = match language {
            SourceLanguage::Java if method.kind() == "method_declaration" => {
                method.child_by_field_name("parameters")
            }
            SourceLanguage::Kotlin if method.kind() == "function_declaration" => method
                .children(&mut method.walk())
                .find(|node| node.kind() == "function_value_parameters"),
            _ => continue,
        };
        if !parameters.is_some_and(|parameters| parameters.named_child_count() == 0) {
            continue;
        }
        let Some(name) = method
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source.as_bytes()).ok())
        else {
            continue;
        };
        let ordinary = match language {
            SourceLanguage::Java => method.child_by_field_name("type_parameters").is_none(),
            SourceLanguage::Kotlin => !method
                .children(&mut method.walk())
                .any(|node| matches!(node.kind(), "type_parameters" | ".")),
        };
        methods
            .entry(name.to_owned())
            .and_modify(|value| *value = false)
            .or_insert(ordinary);
    }
    methods
}

fn inline_bounds(owner: Node<'_>, source: &str) -> HashMap<String, Option<String>> {
    if owner
        .named_children(&mut owner.walk())
        .any(|node| node.kind() == "type_constraints")
    {
        return HashMap::new();
    }
    let Some(parameters) = owner.child_by_field_name("type_parameters").or_else(|| {
        owner
            .named_children(&mut owner.walk())
            .find(|node| node.kind() == "type_parameters")
    }) else {
        return HashMap::new();
    };
    let mut bounds = HashMap::new();
    for parameter in parameters
        .named_children(&mut parameters.walk())
        .filter(|node| node.kind() == "type_parameter")
    {
        let text = parameter.utf8_text(source.as_bytes()).unwrap_or("");
        let Some(name) = type_parameter_name(text) else {
            continue;
        };
        let bound = text.split_once(':').and_then(|(prefix, bound)| {
            let bound = bound.trim();
            (prefix.trim() == name && !bound.is_empty()).then(|| bound.to_owned())
        });
        bounds
            .entry(name.to_owned())
            .and_modify(|value| *value = None)
            .or_insert(bound);
    }
    bounds
}

fn type_parameter_name(parameter: &str) -> Option<&str> {
    let declaration = parameter.split(':').next()?.trim();
    declaration
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .rfind(|token| !token.is_empty())
}

pub(crate) fn declaration_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_declaration"
            | "interface_declaration"
            | "object_declaration"
            | "enum_class_declaration"
    )
}

pub(crate) fn declaration_kind_matches(kind: DeclarationKind, node: Node<'_>) -> bool {
    declaration_kind(node) == Some(kind)
}

fn declaration_kind(node: Node<'_>) -> Option<DeclarationKind> {
    Some(match node.kind() {
        "object_declaration" => DeclarationKind::Object,
        "interface_declaration" => DeclarationKind::Interface,
        "enum_class_declaration" | "enum_declaration" => DeclarationKind::Enum,
        "class_declaration" => {
            let children = node.children(&mut node.walk()).collect::<Vec<_>>();
            let modifiers = children.iter().find(|node| node.kind() == "modifiers");
            let has_modifier = |kind| {
                modifiers.is_some_and(|modifiers| {
                    modifiers
                        .children(&mut modifiers.walk())
                        .filter(|node| node.kind() == "class_modifier")
                        .any(|modifier| {
                            modifier
                                .children(&mut modifier.walk())
                                .any(|node| node.kind() == kind)
                        })
                })
            };
            if has_modifier("annotation") {
                DeclarationKind::Annotation
            } else if children.iter().any(|node| node.kind() == "interface") {
                DeclarationKind::Interface
            } else if has_modifier("enum") {
                DeclarationKind::Enum
            } else {
                DeclarationKind::Class
            }
        }
        _ => return None,
    })
}

pub(crate) fn declaration_name(node: Node<'_>, source: &str) -> String {
    node.named_children(&mut node.walk())
        .find(|node| matches!(node.kind(), "type_identifier" | "identifier"))
        .and_then(|node| node.utf8_text(source.as_bytes()).ok())
        .unwrap_or("")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{
        SourceFile, SourceIndex, SourceOverlay, getter_has_no_method_type_parameters,
    };
    use std::path::PathBuf;

    fn index(source: &str, language: SourceLanguage) -> SourceIndex {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&match language {
                SourceLanguage::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
                SourceLanguage::Java => tree_sitter_java::LANGUAGE.into(),
            })
            .unwrap();
        assert!(
            !parser.parse(source, None).unwrap().root_node().has_error(),
            "invalid fixture:\n{source}"
        );
        SourceIndex::default()
            .with_overlays(&[SourceOverlay::Replace {
                path: PathBuf::from(if language == SourceLanguage::Kotlin {
                    "Headers.kt"
                } else {
                    "Headers.java"
                }),
                language,
                source: source.to_owned(),
            }])
            .unwrap()
    }

    fn ordinary(file: &SourceFile, owner: &str, getter: &str) -> bool {
        let owner = file
            .declarations
            .iter()
            .find(|declaration| declaration.name == owner)
            .unwrap();
        getter_has_no_method_type_parameters(file, owner, getter)
    }

    #[test]
    fn kotlin_headers_distinguish_generics_extensions_and_overloads() {
        let index = index(
            r#"
            class Header<T : Runnable> {
                fun getPlain(): String = "plain"
                fun <U> getGeneric(): U? = null
                fun String.getExtension(): String = this
                fun getOverload(): String = "plain"
                fun getOverload(index: Int): String = "indexed"
            }
            class Constrained<T> where T : Runnable
        "#,
            SourceLanguage::Kotlin,
        );
        let file = &index.files[0];
        assert!(ordinary(file, "Header", "getPlain"));
        assert!(ordinary(file, "Header", "getOverload"));
        assert!(!ordinary(file, "Header", "getGeneric"));
        assert!(!ordinary(file, "Header", "getExtension"));
        assert_eq!(
            file.header_facts
                .inline_bound("Header", DeclarationKind::Class, "T"),
            Some("Runnable")
        );
        assert_eq!(
            file.header_facts
                .inline_bound("Constrained", DeclarationKind::Class, "T"),
            None
        );
    }

    #[test]
    fn java_headers_preserve_zero_argument_and_generic_distinctions() {
        let index = index(
            r#"
            class Header<T extends Key> {
                public String getPlain() { return "plain"; }
                public <U> U getGeneric() { return null; }
                public String getOverload() { return "plain"; }
                public String getOverload(int index) { return "indexed"; }
            }
            interface Contract { String getPlain(); }
            record RecordValue(String text) { public String getPlain() { return text; } }
        "#,
            SourceLanguage::Java,
        );
        let file = &index.files[0];
        for owner in ["Header", "Contract", "RecordValue"] {
            assert!(ordinary(file, owner, "getPlain"));
        }
        assert!(ordinary(file, "Header", "getOverload"));
        assert!(!ordinary(file, "Header", "getGeneric"));
        assert_eq!(
            file.header_facts
                .inline_bound("Header", DeclarationKind::Class, "T"),
            Some("Key")
        );
    }

    #[test]
    fn java_bounds_distinguish_single_bounds_unbounded_and_unsupported_shapes() {
        let index = index(
            r#"
            class Header<T extends Key, U, V extends Owner, W extends Key & Runnable,
                         X extends @Nullable Key, Y extends java.util.List<String>> {}
            interface Contract<T extends sample.Key> {}
            record RecordValue<T extends Key>(T value) {}
            class First { class Duplicate<T extends Key> {} }
            class Second { class Duplicate<T extends OtherKey> {} }
        "#,
            SourceLanguage::Java,
        );
        let file = &index.files[0];
        assert_eq!(
            file.header_facts
                .inline_bound("Header", DeclarationKind::Class, "T"),
            Some("Key")
        );
        assert_eq!(
            file.header_facts
                .inline_bound("Header", DeclarationKind::Class, "U"),
            Some("java.lang.Object")
        );
        assert_eq!(
            file.header_facts
                .inline_bound("Header", DeclarationKind::Class, "V"),
            Some("Owner")
        );
        assert_eq!(
            file.header_facts
                .inline_bound("Header", DeclarationKind::Class, "W"),
            None,
            "intersection bounds remain unresolved"
        );
        assert_eq!(
            file.header_facts
                .inline_bound("Header", DeclarationKind::Class, "X"),
            None,
            "annotated bounds remain unresolved"
        );
        assert_eq!(
            file.header_facts
                .inline_bound("Header", DeclarationKind::Class, "Y"),
            None,
            "generic bounds remain unresolved"
        );
        assert_eq!(
            file.header_facts
                .inline_bound("Contract", DeclarationKind::Interface, "T"),
            Some("sample.Key")
        );
        assert_eq!(
            file.header_facts
                .inline_bound("RecordValue", DeclarationKind::Record, "T"),
            Some("Key")
        );
        assert_eq!(
            file.header_facts
                .inline_bound("Duplicate", DeclarationKind::Class, "T"),
            None,
            "duplicate owner names do not share erasure facts"
        );
    }

    #[test]
    fn java_bound_facts_serialize_and_follow_overlay_snapshots() {
        let source = "class Header<T extends Key> {}\n";
        let original = index(source, SourceLanguage::Java);
        let file = &original.files[0];
        let restored: SourceFile =
            serde_json::from_str(&serde_json::to_string(file).unwrap()).unwrap();
        assert_eq!(file, &restored);
        assert_eq!(
            restored
                .header_facts
                .inline_bound("Header", DeclarationKind::Class, "T"),
            Some("Key")
        );

        let replaced = original
            .with_overlays(&[SourceOverlay::Replace {
                path: file.path.clone(),
                language: SourceLanguage::Java,
                source: "class Header<T extends OtherKey> {}\n".into(),
            }])
            .unwrap();
        assert_eq!(
            replaced.files[0]
                .header_facts
                .inline_bound("Header", DeclarationKind::Class, "T"),
            Some("OtherKey")
        );
        assert_eq!(
            file.header_facts
                .inline_bound("Header", DeclarationKind::Class, "T"),
            Some("Key")
        );
    }

    #[test]
    fn ambiguous_nested_owners_and_duplicate_getters_do_not_share_proofs() {
        let index = index(
            r#"
            class First {
                class Nested<T : Runnable> {
                    fun getToken(): String = "one"
                }
            }
            class Second {
                class Nested<T : AutoCloseable> {
                    fun getToken(): String = "two"
                }
            }
            class Duplicate {
                fun getToken(): String = "one"
                fun <T> getToken(): T? = null
            }
        "#,
            SourceLanguage::Kotlin,
        );
        let file = &index.files[0];
        assert!(!ordinary(file, "Nested", "getToken"));
        assert!(!ordinary(file, "Duplicate", "getToken"));
        assert_eq!(
            file.header_facts
                .inline_bound("Nested", DeclarationKind::Class, "T"),
            None
        );
    }

    #[test]
    fn serialization_and_speculative_replacements_preserve_snapshot_bound_facts() {
        let source = "class Header<T : Runnable> {\n    fun getToken(): String = \"token\"\n}\n";
        let original = index(source, SourceLanguage::Kotlin);
        let file = &original.files[0];
        let restored: SourceFile =
            serde_json::from_str(&serde_json::to_string(file).unwrap()).unwrap();
        assert_eq!(file, &restored);
        assert!(ordinary(&restored, "Header", "getToken"));

        let replaced = original
            .with_overlays(&[SourceOverlay::Replace {
                path: file.path.clone(),
                language: SourceLanguage::Kotlin,
                source: "class Header<T : AutoCloseable> {\n    fun <U> getToken(): U? = null\n}\n"
                    .into(),
            }])
            .unwrap();
        let changed = &replaced.files[0];
        assert!(!ordinary(changed, "Header", "getToken"));
        assert_eq!(
            changed
                .header_facts
                .inline_bound("Header", DeclarationKind::Class, "T"),
            Some("AutoCloseable")
        );
        assert!(ordinary(file, "Header", "getToken"));
        assert_eq!(
            file.header_facts
                .inline_bound("Header", DeclarationKind::Class, "T"),
            Some("Runnable")
        );
    }
}
