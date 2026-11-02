//! JavaBean `isReady()` means `ready`; Kotlin metadata names it `isReady`.
//! Only Jackson-exposed classes get an explicit name, keeping other projects
//! independent of Jackson and respecting source annotation bindings.
use super::Unit;
use crate::workspace::{Declaration, SourceFile, SourceLanguage};
use std::collections::HashSet;
use tree_sitter::Node;

pub(super) fn is_property_annotation(unit: &Unit<'_, '_>, annotation: &str) -> bool {
    let context = unit
        .workspace
        .and_then(|index| index.source_file(unit.workspace_file.as_deref().unwrap_or(unit.file)));
    let name = annotation
        .trim()
        .trim_start_matches('@')
        .split('(')
        .next()
        .unwrap_or("")
        .trim();
    super::annotation_values::external_binding(
        unit,
        context,
        unit.source,
        name,
        "com.fasterxml.jackson.annotation.JsonProperty",
    )
}

fn marked(unit: &Unit<'_, '_>, source: &str, context: Option<&SourceFile>, node: Node<'_>) -> bool {
    let Some(modifiers) = super::kt::child(node, "modifiers") else {
        return false;
    };
    modifiers
        .named_children(&mut modifiers.walk())
        .any(|annotation| {
            let name = super::class::annotation_fq_name(annotation, source).or_else(|| {
                annotation
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                    .map(str::to_owned)
            });
            let Some(name) = name else {
                return false;
            };
            let name = super::class::annotation_import_name(name, source);
            [
                "com.fasterxml.jackson.annotation.JsonTypeInfo",
                "com.fasterxml.jackson.annotation.JsonTypeName",
                "com.fasterxml.jackson.databind.annotation.JsonSerialize",
                "com.fasterxml.jackson.databind.annotation.JsonDeserialize",
            ]
            .iter()
            .any(|canonical| {
                super::annotation_values::external_binding(unit, context, source, &name, canonical)
            })
        })
}

pub(super) fn is_exposed(unit: &Unit<'_, '_>, declaration: Node<'_>) -> bool {
    let context = unit
        .workspace
        .and_then(|index| index.source_file(unit.workspace_file.as_deref().unwrap_or(unit.file)));
    if marked(unit, unit.source, context, declaration) {
        return true;
    }
    let Some((index, file)) = unit.workspace.zip(context) else {
        return false;
    };
    let Some(name) = declaration
        .child_by_field_name("name")
        .and_then(|name| name.utf8_text(unit.source.as_bytes()).ok())
    else {
        return false;
    };
    let Some(owner) = file.declarations.iter().find(|owner| owner.name == name) else {
        return false;
    };
    let mut pending = owner
        .supertypes
        .iter()
        .filter_map(|ty| index.resolve_type(file, ty))
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    while let Some(owner) = pending.pop() {
        if !seen.insert(owner as *const Declaration as usize) {
            continue;
        }
        let Some(file) = index.declaration_source_file(owner) else {
            continue;
        };
        pending.extend(
            owner
                .supertypes
                .iter()
                .filter_map(|ty| index.resolve_type(file, ty)),
        );
        let mut parser = tree_sitter::Parser::new();
        let language = match owner.language {
            SourceLanguage::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            SourceLanguage::Java => tree_sitter_java::LANGUAGE.into(),
        };
        if parser.set_language(&language).is_err() {
            continue;
        }
        let Some(tree) = parser.parse(file.source_text(), None) else {
            continue;
        };
        for node in tree
            .root_node()
            .named_children(&mut tree.root_node().walk())
        {
            if node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(file.source_text().as_bytes()).ok())
                == Some(owner.name.as_str())
                && marked(unit, file.source_text(), Some(file), node)
            {
                return true;
            }
        }
    }
    false
}
