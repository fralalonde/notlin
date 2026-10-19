//! Preserve Kotlin constructor defaults when Jackson can observe a DTO.
//!
//! Java constructors cannot distinguish an omitted creator property from an
//! explicitly supplied null while applying Kotlin defaults. These compact
//! syntax facts are collected from the workspace's existing parse snapshots,
//! then joined to the stable type graph after inheritance is indexed.

use super::{SymbolId, SyntaxSemanticProvider};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tree_sitter::Node;

#[derive(Default)]
pub(super) struct FileRelations {
    classes: Vec<ClassFact>,
}

#[derive(Clone)]
struct ClassFact {
    start: usize,
    has_primary_defaults: bool,
    own_annotation: bool,
}

pub(super) fn collect(source: &str, file: &Path, root: Node<'_>) -> FileRelations {
    let is_kotlin = file.extension().and_then(|ext| ext.to_str()) != Some("java");
    // These negatives only avoid work. Annotation identities are established
    // from syntax below, never from the presence of a word in source text.
    let may_have_jackson = source.contains("jackson");
    if !is_kotlin && !may_have_jackson {
        return FileRelations::default();
    }
    let bindings = AnnotationBindings::collect(source, root);
    let mut relations = FileRelations::default();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "class_declaration"
                | "interface_declaration"
                | "object_declaration"
                | "enum_class_declaration"
                | "enum_declaration"
        ) {
            let has_primary_defaults = is_kotlin && primary_constructor_has_defaults(node);
            let own_annotation =
                may_have_jackson && has_jackson_annotation(source, node, &bindings);
            if has_primary_defaults || own_annotation {
                relations.classes.push(ClassFact {
                    start: node.start_byte(),
                    has_primary_defaults,
                    own_annotation,
                });
            }
        }
        pending.extend(named_children(node));
    }
    relations
}

pub(super) fn resolve(
    provider: &SyntaxSemanticProvider,
    files: &BTreeMap<PathBuf, FileRelations>,
) -> BTreeSet<SymbolId> {
    let mut candidates = Vec::new();
    let mut annotated_ancestors = BTreeSet::new();
    for (file, relations) in files {
        for fact in &relations.classes {
            let Some(symbol) = symbol_at(provider, file, fact.start) else {
                continue;
            };
            if fact.own_annotation {
                // Jackson annotations on inherited constructors, parameters,
                // properties, or accessors can affect the concrete subtype's
                // serialized/creator contract too.
                annotated_ancestors.insert(symbol.clone());
            }
            if fact.has_primary_defaults {
                candidates.push(symbol);
            }
        }
    }
    // Propagate the observed contract through each inheritance edge once,
    // rather than repeating ancestry searches for every candidate/annotation pair.
    let mut children = BTreeMap::<&SymbolId, Vec<&SymbolId>>::new();
    for (child, parents) in &provider.parents {
        for parent in parents {
            children.entry(parent).or_default().push(child);
        }
    }
    let mut pending: Vec<_> = annotated_ancestors.iter().cloned().collect();
    while let Some(owner) = pending.pop() {
        for child in children.get(&owner).into_iter().flatten() {
            if annotated_ancestors.insert((*child).clone()) {
                pending.push((*child).clone());
            }
        }
    }
    candidates
        .into_iter()
        .filter(|owner| annotated_ancestors.contains(owner))
        .collect()
}

fn symbol_at(provider: &SyntaxSemanticProvider, file: &Path, start: usize) -> Option<SymbolId> {
    let mut found = provider
        .by_file
        .get(file)?
        .iter()
        .map(|index| &provider.symbols[*index])
        .filter(|symbol| {
            symbol.id.file == file
                && symbol.location.start_byte == start
                && matches!(
                    symbol.id.kind.as_str(),
                    "class" | "interface" | "object" | "enum"
                )
        });
    let result = found.next()?.id.clone();
    found.next().is_none().then_some(result)
}

fn primary_constructor_has_defaults(class: Node<'_>) -> bool {
    let Some(constructor) = named_children(class)
        .into_iter()
        .find(|child| child.kind() == "primary_constructor")
    else {
        return false;
    };
    let Some(parameters) = named_children(constructor)
        .into_iter()
        .find(|child| child.kind() == "class_parameters")
    else {
        return false;
    };
    named_children(parameters)
        .into_iter()
        .filter(|child| child.kind() == "class_parameter")
        .any(|parameter| {
            let mut cursor = parameter.walk();
            parameter
                .children(&mut cursor)
                .any(|child| child.kind() == "=")
        })
}

fn has_jackson_annotation(
    source: &str,
    declaration: Node<'_>,
    bindings: &AnnotationBindings,
) -> bool {
    if annotations_in_headers(source, declaration, bindings) {
        return true;
    }
    named_children(declaration)
        .into_iter()
        .filter(|node| {
            matches!(
                node.kind(),
                "class_body" | "interface_body" | "enum_class_body"
            )
        })
        .flat_map(named_children)
        .filter(|member| {
            matches!(
                member.kind(),
                "property_declaration"
                    | "field_declaration"
                    | "constant_declaration"
                    | "function_declaration"
                    | "method_declaration"
                    | "constructor_declaration"
                    | "primary_constructor"
                    | "secondary_constructor"
            )
        })
        .any(|member| annotations_in_headers(source, member, bindings))
}

fn annotations_in_headers(source: &str, root: Node<'_>, bindings: &AnnotationBindings) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.id() != root.id()
            && matches!(
                node.kind(),
                "class_body"
                    | "interface_body"
                    | "enum_class_body"
                    | "function_body"
                    | "block"
                    | "lambda_literal"
                    | "class_declaration"
                    | "interface_declaration"
                    | "object_declaration"
                    | "enum_class_declaration"
                    | "enum_declaration"
            )
        {
            continue;
        }
        if is_annotation_node(node.kind())
            && annotation_name(source, node).is_some_and(|name| bindings.is_jackson(&name))
        {
            return true;
        }
        pending.extend(named_children(node));
    }
    false
}

struct AnnotationBindings {
    imports: Vec<(String, Option<String>)>,
    local_names: BTreeSet<String>,
}

impl AnnotationBindings {
    fn collect(source: &str, root: Node<'_>) -> Self {
        let imports = root
            .named_children(&mut root.walk())
            .filter(|node| matches!(node.kind(), "import" | "import_declaration"))
            .filter_map(|node| {
                node.utf8_text(source.as_bytes())
                    .ok()?
                    .trim()
                    .strip_prefix("import")
            })
            .map(|import| {
                let import = import.trim().trim_end_matches(';').trim();
                let import = import.strip_prefix("static ").unwrap_or(import);
                let (path, alias) = import
                    .split_once(" as ")
                    .map_or((import, None), |(path, alias)| {
                        (path.trim(), Some(alias.trim().to_owned()))
                    });
                (path.to_owned(), alias)
            })
            .collect();
        Self {
            imports,
            local_names: local_annotation_names(source, root),
        }
    }

    fn is_jackson(&self, name: &str) -> bool {
        if is_jackson_path(name) {
            return true;
        }
        if name.contains('.') {
            return false;
        }
        let explicit = self
            .imports
            .iter()
            .filter(|(path, alias)| {
                alias
                    .as_deref()
                    .unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path))
                    == name
                    && !path.ends_with(".*")
            })
            .collect::<Vec<_>>();
        if !explicit.is_empty() {
            return explicit.len() == 1 && is_jackson_path(&explicit[0].0);
        }
        if self.local_names.contains(name) {
            return false;
        }
        let wildcards = self
            .imports
            .iter()
            .filter(|(path, alias)| alias.is_none() && path.ends_with(".*"))
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>();
        let jackson_wildcard = wildcards.iter().any(|path| {
            matches!(
                *path,
                "com.fasterxml.jackson.annotation.*"
                    | "com.fasterxml.jackson.databind.annotation.*"
            )
        });
        jackson_wildcard
            && wildcards.iter().all(|path| {
                matches!(
                    *path,
                    "com.fasterxml.jackson.annotation.*"
                        | "com.fasterxml.jackson.databind.annotation.*"
                )
            })
    }
}

fn is_jackson_path(path: &str) -> bool {
    path.starts_with("com.fasterxml.jackson.annotation.")
        || path.starts_with("com.fasterxml.jackson.databind.annotation.")
}

fn annotation_name(source: &str, annotation: Node<'_>) -> Option<String> {
    if let Some(name) = annotation.child_by_field_name("name") {
        return name
            .utf8_text(source.as_bytes())
            .ok()
            .map(|name| name.chars().filter(|ch| !ch.is_whitespace()).collect());
    }
    let mut pending = vec![annotation];
    while let Some(node) = pending.pop() {
        if node.kind() == "user_type" {
            return node
                .utf8_text(source.as_bytes())
                .ok()
                .map(|name| name.chars().filter(|ch| !ch.is_whitespace()).collect());
        }
        pending.extend(named_children(node).into_iter().rev());
    }
    None
}

fn local_annotation_names(source: &str, root: Node<'_>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "class_declaration"
                | "interface_declaration"
                | "object_declaration"
                | "enum_class_declaration"
                | "enum_declaration"
                | "annotation_declaration"
        ) && let Some(name) = node.child_by_field_name("name")
        {
            let name = name.utf8_text(source.as_bytes()).unwrap_or("").to_owned();
            names.insert(name);
        }
        pending.extend(named_children(node));
    }
    names
}

fn is_annotation_node(kind: &str) -> bool {
    matches!(
        kind,
        "annotation" | "marker_annotation" | "annotation_entry"
    )
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
