use super::Unit;
use crate::transpiler::kt;
use tree_sitter::Node;

impl Unit<'_, '_> {
    pub(super) fn type_parameter_declaration(
        &mut self,
        declaration: Node<'_>,
        parameter: Node<'_>,
    ) -> Option<String> {
        let name = kt::child(parameter, "identifier")?;
        let name = self.text(name).to_owned();
        let mut bounds = crate::type_parameter_bounds::bounds(declaration, parameter, self.source)
            .into_iter()
            .map(|node| {
                (
                    node,
                    if declaration.kind() == "function_declaration" {
                        kt::java_type_ann(node, self.source, self.annots)
                    } else {
                        kt::java_type(node, self.source)
                    },
                )
            })
            .filter(|(_, ty)| {
                !matches!(
                    ty.as_str(),
                    "Object" | "Any" | "java.lang.Object" | "kotlin.Any"
                )
            })
            .collect::<Vec<_>>();
        // Java requires its class bound before interface bounds, even when a
        // Kotlin where clause lists that class last.
        if bounds.len() > 1 {
            let mut class = None;
            if let Some(index) = self.workspace
                && let Some(file) =
                    index.source_file(self.workspace_file.as_deref().unwrap_or(self.file))
            {
                for (position, (node, _)) in bounds.iter().enumerate() {
                    let spelling = self.text(*node).trim_end_matches('?');
                    if let Some(bound) = index.resolve_type(file, spelling)
                        && bound.kind != crate::workspace::DeclarationKind::Interface
                    {
                        if class.is_some() {
                            self.diag_untranslatable(
                                parameter,
                                "multiple class bounds have no Java form",
                            );
                            return None;
                        }
                        class = Some(position);
                    }
                }
            }
            if let Some(position) = class {
                let bound = bounds.remove(position);
                bounds.insert(0, bound);
            }
        }
        if bounds.is_empty() {
            Some(name)
        } else {
            Some(format!(
                "{name} extends {}",
                bounds
                    .into_iter()
                    .map(|(_, ty)| ty)
                    .collect::<Vec<_>>()
                    .join(" & ")
            ))
        }
    }
}
