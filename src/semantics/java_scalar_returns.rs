//! One bound scalar return signature for Java calls and synthetic properties.

use super::{ReferenceReturnFact, ReferenceReturnNullability, java_metadata};
use tree_sitter::Node;

pub(super) struct ScalarReturn {
    pub spelling: String,
    pub nullness: ReferenceReturnFact,
}

pub(super) fn collect(
    source: &str,
    method: Node<'_>,
    scope: &java_metadata::Scope<'_>,
) -> Option<ScalarReturn> {
    if method.kind() != "method_declaration" {
        return None;
    }
    let ty = super::declaration_type_node(method)?;
    let (spelling, type_annotations) = scalar_type(source, ty)?;
    let modifiers = method
        .named_children(&mut method.walk())
        .find(|child| child.kind() == "modifiers");
    let declaration_annotations = modifiers.into_iter().flat_map(|node| {
        node.named_children(&mut node.walk())
            .filter(|child| annotation(*child))
            .collect::<Vec<_>>()
    });
    let mut bindings = Vec::new();
    let mut nullability = None;
    for (node, on_type) in declaration_annotations
        .map(|node| (node, false))
        .chain(type_annotations.into_iter().map(|node| (node, true)))
    {
        let binding = scope.binding(node)?;
        let effect = match binding.canonical.as_str() {
            "org.jspecify.annotations.NonNull" | "org.jspecify.annotations.Nullable"
                if !on_type && spelling.contains('.') =>
            {
                // A prefix type annotation describes the enclosing type (or
                // inadmissibly a package), not the returned nested instance.
                // Only annotations on the final component enhance this value.
                return None;
            }
            "org.jetbrains.annotations.NotNull" | "org.jspecify.annotations.NonNull" => {
                Some(ReferenceReturnNullability::NonNull)
            }
            "org.jetbrains.annotations.Nullable" | "org.jspecify.annotations.Nullable" => {
                Some(ReferenceReturnNullability::Nullable)
            }
            _ if binding.neutral_for_nullness() => None,
            _ => return None,
        };
        if let Some(effect) = effect
            && nullability.replace(effect).is_some()
        {
            return None;
        }
        bindings.push(binding);
    }
    Some(ScalarReturn {
        spelling,
        nullness: ReferenceReturnFact {
            nullability: nullability.unwrap_or(ReferenceReturnNullability::Platform),
            bindings,
        },
    })
}

fn annotation(node: Node<'_>) -> bool {
    matches!(node.kind(), "annotation" | "marker_annotation")
}

fn scalar_type<'tree>(source: &str, ty: Node<'tree>) -> Option<(String, Vec<Node<'tree>>)> {
    if ty.has_error() {
        return None;
    }
    match ty.kind() {
        "type_identifier" => Some((crate::java_ir::qualified_name(ty, source)?, Vec::new())),
        "integral_type" | "floating_point_type" | "boolean_type" => {
            Some((ty.utf8_text(source.as_bytes()).ok()?.to_owned(), Vec::new()))
        }
        "scoped_type_identifier" => {
            let children = ty
                .named_children(&mut ty.walk())
                .filter(|child| !child.is_extra())
                .collect::<Vec<_>>();
            let (scope, rest) = children.split_first()?;
            let (name, annotations) = rest.split_last()?;
            if name.kind() != "type_identifier" || !annotations.iter().all(|node| annotation(*node))
            {
                return None;
            }
            // qualified_name rejects annotations in enclosing components as
            // well as generic arguments: those are distinct nullness slots.
            Some((
                format!(
                    "{}.{}",
                    crate::java_ir::qualified_name(*scope, source)?,
                    crate::java_ir::qualified_name(*name, source)?
                ),
                annotations.to_vec(),
            ))
        }
        _ => None,
    }
}
