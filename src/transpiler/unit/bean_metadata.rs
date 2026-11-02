//! Keep runtime bean recognition when a property uses a nonstandard JVM name.
use super::{Unit, annotation_values, kt};
use tree_sitter::Node;

pub(super) fn preserve(unit: &Unit<'_, '_>, declaration: Node<'_>, annotations: &mut Vec<String>) {
    let mut properties = kt::child(declaration, "primary_constructor")
        .and_then(|constructor| kt::child(constructor, "class_parameters"))
        .map(|parameters| {
            parameters
                .named_children(&mut parameters.walk())
                .filter(|parameter| {
                    parameter.kind() == "class_parameter"
                        && parameter
                            .children(&mut parameter.walk())
                            .any(|token| matches!(token.kind(), "val" | "var"))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(body) =
        kt::child(declaration, "class_body").or_else(|| kt::child(declaration, "enum_class_body"))
    {
        for member in body.named_children(&mut body.walk()) {
            if member.kind() == "property_declaration" {
                properties.push(member);
            }
            if member.kind() == "enum_body_declarations" {
                properties.extend(
                    member
                        .named_children(&mut member.walk())
                        .filter(|member| member.kind() == "property_declaration"),
                );
            }
        }
    }
    let renamed = properties.into_iter().any(|property| {
        let variable = kt::child(property, "variable_declaration").unwrap_or(property);
        let Some(name) = kt::child(variable, "identifier").map(|name| unit.text(name)) else {
            return false;
        };
        let context = if property.kind() == "class_parameter" {
            declaration
        } else {
            property
        };
        unit.property_accessor_names(context, name)
            .getter(name)
            .is_some_and(|getter| getter != crate::workspace::property_accessor_name(name))
    });
    if !renamed {
        return;
    }
    let file = unit
        .workspace
        .and_then(|index| index.source_file(unit.workspace_file.as_deref().unwrap_or(unit.file)));
    if annotations.iter().any(|annotation| {
        let name = annotation
            .trim_start_matches('@')
            .split(['(', ' ', '\n'])
            .next()
            .unwrap_or("");
        annotation_values::external_binding(unit, file, unit.source, name, "java.beans.JavaBean")
    }) {
        return;
    }
    // Kotlin's runtime metadata lets reflection-based binders recognize an
    // otherwise empty bean. The standard Java marker preserves that behavior
    // without adding a Jackson dependency or exposing a hidden property.
    annotations.push("@java.beans.JavaBean".into());
}
