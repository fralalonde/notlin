//! Scoped emission of already planned JDK-container lambda inputs and accesses.

use super::Expr;
use crate::semantics::KotlinType;
use tree_sitter::Node;

impl<'a, 'src, 'tree> Expr<'a, 'src, 'tree> {
    pub(super) fn planned_container_lambda(&mut self, node: Node<'_>) -> Option<String> {
        let binding = self
            .unit
            .container_lambdas
            .get(&(node.start_byte(), node.end_byte()))?
            .clone();
        if binding.location.snapshot_hash != self.unit.source_hash
            || binding.java_parameter_name.is_empty()
        {
            self.unit.preparation_failure =
                Some("planned container lambda has stale or missing binding".into());
            return Some(String::new());
        }
        let body = node
            .named_descendant_for_byte_range(binding.body.start_byte, binding.body.end_byte)
            .filter(|body| {
                body.start_byte() == binding.body.start_byte
                    && body.end_byte() == binding.body.end_byte
            });
        let Some(body) = body else {
            self.unit.preparation_failure =
                Some("planned container lambda body does not match snapshot".into());
            return Some(String::new());
        };
        let ty = match &binding.input_type.ty {
            KotlinType::Named { name, .. } => name.clone(),
            KotlinType::Primitive(name) => crate::transpiler::types::map_type_name(name).to_owned(),
            _ => "__lambda_param".into(),
        };
        let parameter = &binding.parameter_name;
        let previous_type = self.unit.var_types.insert(parameter.clone(), ty);
        let previous_structured = self
            .unit
            .local_expression_types
            .insert(parameter.clone(), binding.input_type);
        let previous_alias = self
            .unit
            .local_expression_aliases
            .insert(parameter.clone(), binding.java_parameter_name.clone());
        let previous_cast = self.unit.flow_smart_casts.remove(parameter);
        let java = self.transpile(body);
        restore(&mut self.unit.var_types, parameter, previous_type);
        restore(
            &mut self.unit.local_expression_types,
            parameter,
            previous_structured,
        );
        restore(
            &mut self.unit.local_expression_aliases,
            parameter,
            previous_alias,
        );
        restore(&mut self.unit.flow_smart_casts, parameter, previous_cast);
        Some(format!("{} -> {java}", binding.java_parameter_name))
    }

    pub(super) fn planned_container_property_access(&mut self, node: Node<'_>) -> Option<String> {
        let access = self
            .unit
            .container_property_accesses
            .get(&(node.start_byte(), node.end_byte()))?
            .clone();
        if access.location.snapshot_hash != self.unit.source_hash
            || access.receiver.snapshot_hash != self.unit.source_hash
            || access.projection.java_parameter_name.is_empty()
            || access.getter_path.last() != Some(&access.projection.getter_name)
        {
            self.unit.preparation_failure =
                Some("planned container property has stale or missing binding".into());
            return Some(String::new());
        }
        let receiver = node.named_child(0).filter(|receiver| {
            receiver.start_byte() == access.receiver.start_byte
                && receiver.end_byte() == access.receiver.end_byte
        });
        let Some(receiver) = receiver else {
            self.unit.preparation_failure =
                Some("planned container property receiver does not match snapshot".into());
            return Some(String::new());
        };
        let receiver_java = self.transpile(receiver);
        Some(format!(
            "{}.{}()",
            receiver_java, access.projection.getter_name
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
