//! Preparation consumes proven eager call facts; helper loops are structured at acceptance.

use super::Expr;
use crate::semantics::{KotlinType, SourceLocation, TypeRef};
use tree_sitter::Node;

impl<'a, 'src, 'tree> Expr<'a, 'src, 'tree> {
    pub(super) fn planned_eager_collection_call(&mut self, call: Node<'_>) -> Option<String> {
        let fact = self
            .unit
            .eager_collection_calls
            .get(&(call.start_byte(), call.end_byte()))?
            .clone();
        let helper = self
            .unit
            .eager_collection_helpers
            .iter()
            .find(|helper| helper.origin == fact.owner && helper.operation == fact.operation)
            .cloned();
        let Some(helper) = helper else {
            self.unit.preparation_failure =
                Some("planned eager collection call has no helper".into());
            return Some(String::new());
        };
        let mut root = call;
        while let Some(parent) = root.parent() {
            root = parent;
        }
        let exact_node = |location: &SourceLocation| {
            (location.snapshot_hash == self.unit.source_hash)
                .then(|| {
                    root.named_descendant_for_byte_range(location.start_byte, location.end_byte)
                })
                .flatten()
                .filter(|node| {
                    node.start_byte() == location.start_byte && node.end_byte() == location.end_byte
                })
        };
        let (Some(receiver), Some(body)) = (exact_node(&fact.receiver), exact_node(&fact.body))
        else {
            self.unit.preparation_failure =
                Some("planned eager collection call has stale syntax".into());
            return Some(String::new());
        };
        let receiver = self.transpile(receiver);
        if let Some(projection) = &fact.property_projection {
            if projection.java_parameter_name.is_empty() {
                self.unit.preparation_failure =
                    Some("planned property projection has no Java parameter".into());
                return Some(String::new());
            }
            return Some(format!(
                "{}({}, {} -> {}.{}())",
                helper.name,
                receiver,
                projection.java_parameter_name,
                projection.java_parameter_name,
                projection.getter_name
            ));
        }
        let parameter = fact.parameter_name;
        let primitive = fact
            .element_type
            .as_ref()
            .filter(|ty| !ty.nullable)
            .and_then(|ty| {
                if let KotlinType::Primitive(name) = &ty.ty {
                    Some(crate::transpiler::types::map_type_name(name).to_owned())
                } else {
                    None
                }
            });
        let value_name = if primitive.is_some() {
            format!("{}Value", helper.parameter_name)
        } else {
            helper.parameter_name.clone()
        };
        let java_type = fact
            .element_type
            .as_ref()
            .and_then(lambda_java_type)
            .unwrap_or_else(|| "__lambda_param".into());
        let previous_type = self
            .unit
            .var_types
            .insert(parameter.clone(), java_type.clone());
        let previous_value_type = self.unit.var_types.insert(value_name.clone(), java_type);
        let previous_structured_type = self.unit.local_expression_types.remove(&parameter);
        if let Some(ty) = &fact.element_type {
            self.unit
                .local_expression_types
                .insert(parameter.clone(), ty.clone());
        }
        let previous_alias = self
            .unit
            .local_expression_aliases
            .insert(parameter.clone(), value_name.clone());
        let previous_casts = self.unit.flow_smart_casts.clone();
        self.unit.flow_smart_casts.remove(&parameter);
        let body = self.transpile(body);
        self.unit.flow_smart_casts = previous_casts;
        restore(&mut self.unit.var_types, &parameter, previous_type);
        restore(&mut self.unit.var_types, &value_name, previous_value_type);
        restore(
            &mut self.unit.local_expression_types,
            &parameter,
            previous_structured_type,
        );
        restore(
            &mut self.unit.local_expression_aliases,
            &parameter,
            previous_alias,
        );
        let body = if let Some(primitive) = primitive {
            format!(
                "{{ {primitive} {value_name} = {}; return {body}; }}",
                helper.parameter_name
            )
        } else {
            body
        };
        Some(format!(
            "{}({}, {} -> {})",
            helper.name, receiver, helper.parameter_name, body
        ))
    }
}

fn restore<T>(map: &mut std::collections::HashMap<String, T>, name: &str, previous: Option<T>) {
    if let Some(previous) = previous {
        map.insert(name.to_owned(), previous);
    } else {
        map.remove(name);
    }
}

/// Scope binding for existing expression lowering. Generic arguments belong to
/// the helper signature, while receiver classification uses this exact classifier.
fn lambda_java_type(ty: &TypeRef) -> Option<String> {
    match &ty.ty {
        KotlinType::Primitive(name) => Some(
            if ty.nullable {
                match name.as_str() {
                    "Int" => "Integer",
                    "Long" => "Long",
                    "Short" => "Short",
                    "Byte" => "Byte",
                    "Double" => "Double",
                    "Float" => "Float",
                    "Boolean" => "Boolean",
                    "Char" => "Character",
                    _ => return None,
                }
            } else {
                crate::transpiler::types::map_type_name(name)
            }
            .into(),
        ),
        KotlinType::Named { name, .. } => Some(
            match name.as_str() {
                "kotlin.String" => "String",
                "kotlin.Any" => "Object",
                "kotlin.collections.List" | "kotlin.collections.MutableList" => "List",
                "kotlin.collections.Set" | "kotlin.collections.MutableSet" => "Set",
                "kotlin.collections.Collection" | "kotlin.collections.MutableCollection" => {
                    "Collection"
                }
                "kotlin.collections.Iterable" | "kotlin.collections.MutableIterable" => "Iterable",
                _ => crate::transpiler::types::map_type_name(name),
            }
            .into(),
        ),
        KotlinType::TypeParameter(name) => Some(name.clone()),
        KotlinType::Nullable(inner) => lambda_java_type(&TypeRef {
            ty: *inner.clone(),
            nullable: true,
        }),
        KotlinType::Array(inner) => lambda_java_type(&TypeRef {
            ty: *inner.clone(),
            nullable: false,
        })
        .map(|element| format!("{element}[]")),
        KotlinType::Function { .. } | KotlinType::Unknown(_) => None,
    }
}
