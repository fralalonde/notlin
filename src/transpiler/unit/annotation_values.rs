//! Kotlin supplies an empty vararg for a Java annotation's required value array.
//! Java needs that array written explicitly; declared defaults must stay omitted.

use super::Unit;
use crate::workspace::{DeclarationKind, SourceLanguage};

pub(super) fn omits_value(
    arguments: Option<tree_sitter::Node<'_>>,
    source: &str,
    text: &str,
) -> bool {
    if text
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim()
        .is_empty()
    {
        return true;
    }
    let Some(arguments) = arguments else {
        return false;
    };
    let mut pending = vec![arguments];
    let mut found = false;
    while let Some(node) = pending.pop() {
        if node.kind() == "value_argument" {
            found = true;
            if !node
                .children(&mut node.walk())
                .any(|child| child.kind() == "=")
            {
                return false;
            }
            let name = node
                .named_children(&mut node.walk())
                .find(|child| !child.is_extra())
                .filter(|child| child.kind() == "identifier")
                .and_then(|child| child.utf8_text(source.as_bytes()).ok());
            if name.is_none_or(|name| name.trim_matches('`') == "value") {
                return false;
            }
        } else {
            pending.extend(node.named_children(&mut node.walk()));
        }
    }
    found
}

pub(super) fn supply_empty_value(arguments: String, missing: bool) -> String {
    if !missing {
        return arguments;
    }
    let inner = arguments
        .trim()
        .strip_prefix('(')
        .and_then(|text| text.strip_suffix(')'))
        .unwrap_or_default()
        .trim();
    if inner.is_empty() {
        "({})".into()
    } else {
        format!("(value = {{}}, {inner})")
    }
}

pub(super) fn requires_empty_value(unit: &Unit<'_, '_>, name: &str) -> bool {
    let context = unit
        .workspace
        .and_then(|index| index.source_file(unit.workspace_file.as_deref().unwrap_or(unit.file)));
    if let Some((index, context)) = unit.workspace.zip(context)
        && let Some(declaration) = index.resolve_type(context, name)
    {
        let qualified = declaration.package.as_ref().map_or_else(
            || declaration.name.clone(),
            |package| format!("{package}.{}", declaration.name),
        );
        let visible = name == qualified
            || (!name.contains('.')
                && (declaration.package == context.package
                    || context.imports.iter().any(|import| {
                        import == &qualified
                            || import.strip_suffix(".*") == declaration.package.as_deref()
                    })));
        if visible {
            return declaration.kind == DeclarationKind::Annotation
                && declaration.language == SourceLanguage::Java
                && index
                    .declaration_source_file(declaration)
                    .is_some_and(|file| {
                        source_requires_empty_value(file.source_text(), &declaration.name)
                            == Some(true)
                    });
        }
    }
    // Signatures outside the source index. The lowering above is shared with
    // source annotations; these are just established Java library declarations.
    [
        "com.fasterxml.jackson.annotation.JsonSubTypes",
        "java.lang.SuppressWarnings",
        "java.lang.annotation.Target",
    ]
    .iter()
    .any(|canonical| external_binding(unit, context, unit.source, name, canonical))
}

pub(super) fn external_binding(
    unit: &Unit<'_, '_>,
    context: Option<&crate::workspace::SourceFile>,
    source: &str,
    name: &str,
    canonical: &str,
) -> bool {
    let (package, simple) = canonical.rsplit_once('.').unwrap();
    let root = name.split('.').next().unwrap_or(name);
    if super::class::annotation_type_is_local(unit.workspace, context, root)
        || context.is_some_and(|context| {
            unit.semantic_provider.is_some_and(|provider| {
                provider.has_type_alias_in_package(context.package.as_deref().unwrap_or(""), root)
            })
        })
    {
        return false;
    }
    if name == canonical {
        return true;
    }
    if name != simple {
        return false;
    }
    let imports = super::class::annotation_imports(source).collect::<Vec<_>>();
    let explicit = imports
        .iter()
        .filter(|(path, alias)| {
            !path.ends_with(".*")
                && alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == simple
        })
        .collect::<Vec<_>>();
    if !explicit.is_empty() {
        return explicit.len() == 1 && explicit[0].0 == canonical;
    }
    if package == "java.lang" {
        return true;
    }
    imports
        .iter()
        .any(|(path, alias)| alias.is_none() && path.strip_suffix(".*") == Some(package))
        && imports.iter().all(|(path, _)| {
            let Some(star) = path.strip_suffix(".*") else {
                return true;
            };
            if matches!(star, "java.lang" | "java.util") || star == package {
                return true;
            }
            unit.semantic_provider.is_some_and(|provider| {
                provider.has_source_package(star)
                    && !provider
                        .symbols_named(simple)
                        .iter()
                        .any(|symbol| symbol.id.package == star && symbol.id.owner_path.is_empty())
            })
        })
}

fn source_requires_empty_value(source: &str, name: &str) -> Option<bool> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let mut pending = vec![tree.root_node()];
    let mut matches = Vec::new();
    while let Some(node) = pending.pop() {
        if node.kind() == "annotation_type_declaration"
            && node
                .child_by_field_name("name")?
                .utf8_text(source.as_bytes())
                .ok()?
                == name
        {
            matches.push(node);
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    let [declaration] = matches.as_slice() else {
        return None;
    };
    if declaration.has_error() {
        return None;
    }
    let body = declaration.child_by_field_name("body")?;
    let values = body
        .named_children(&mut body.walk())
        .filter(|node| {
            node.kind() == "annotation_type_element_declaration"
                && node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                    == Some("value")
        })
        .collect::<Vec<_>>();
    let [value] = values.as_slice() else {
        return Some(false);
    };
    Some(
        value.child_by_field_name("value").is_none()
            && (value.child_by_field_name("type")?.kind() == "array_type"
                || value.child_by_field_name("dimensions").is_some()),
    )
}
