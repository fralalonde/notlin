//! Exact separator-only joining for array/vararg parameters.

use super::{KotlinType, SemanticProvider};
use std::path::Path;
use tree_sitter::Node;

pub(crate) fn establish<'n, P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: Node<'n>,
    provider: &P,
) -> Option<(Node<'n>, Option<Node<'n>>, bool)> {
    let callee = super::call_callee_node(call)?;
    if callee.utf8_text(source.as_bytes()).ok()? != "joinToString"
        || provider.source_for_file(file)? != source
        || super::call_argument_count(call) > 1
        || super::direct_trailing_lambda(call).is_some()
        || provider
            .symbols_named("joinToString")
            .iter()
            .any(|symbol| symbol.id.receiver.is_some())
        || super::kotlin_import_lines(source).any(|import| {
            let (path, alias) = import
                .split_once(" as ")
                .map_or((import, None), |(path, alias)| (path, Some(alias)));
            alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == "joinToString"
                && path != "kotlin.collections.joinToString"
        })
    {
        return None;
    }
    let receiver = super::explicit_receiver_node(call, callee)?;
    if !matches!(receiver.kind(), "identifier" | "simple_identifier") {
        return None;
    }
    let name = receiver.utf8_text(source.as_bytes()).ok()?;
    let string_type = |ty: &KotlinType| match ty {
        KotlinType::Named { name, arguments } if arguments.is_empty() => {
            super::standard_external_type(source, provider, name, false).as_deref()
                == Some("java.lang.String")
        }
        _ => false,
    };
    let array_elements =
        super::lexical_parameter_type(source, call, name).and_then(|ty| match ty.ty {
            KotlinType::Array(element) if !ty.nullable => Some(string_type(&element)),
            _ => None,
        });
    let string_elements = if let Some(string_elements) = array_elements {
        string_elements
    } else {
        // Varargs are arrays at runtime, while the existing lexical classifier
        // deliberately refuses to treat one as its element type.
        let mut owner = call.parent()?;
        while !matches!(
            owner.kind(),
            "function_declaration" | "secondary_constructor"
        ) {
            if matches!(
                owner.kind(),
                "lambda_literal" | "class_declaration" | "object_declaration"
            ) {
                return None;
            }
            owner = owner.parent()?;
        }
        let mut pending = vec![owner];
        while let Some(node) = pending.pop() {
            if node.start_byte() < call.start_byte()
                && node.kind() == "variable_declaration"
                && super::node_name(node, source).as_deref() == Some(name)
            {
                return None;
            }
            pending.extend(node.named_children(&mut node.walk()));
        }
        let parameters = owner
            .named_children(&mut owner.walk())
            .find(|node| node.kind() == "function_value_parameters")?;
        let parameter = parameters
            .named_children(&mut parameters.walk())
            .find(|node| {
                node.kind() == "parameter"
                    && super::node_name(*node, source).as_deref() == Some(name)
            })?;
        let signature = super::parameter_signature(source, parameter);
        let vararg = parameter.prev_named_sibling().is_some_and(|node| {
            node.kind() == "parameter_modifiers"
                && node
                    .utf8_text(source.as_bytes())
                    .is_ok_and(|text| text.split_whitespace().any(|word| word == "vararg"))
        });
        let element = signature
            .strip_prefix("vararg:")
            .or_else(|| vararg.then_some(signature.as_str()))?;
        string_type(&super::TypeRef::parse(element).ty)
    };
    let arguments = call
        .named_children(&mut call.walk())
        .find(|node| node.kind() == "value_arguments")?;
    let separator = arguments
        .named_children(&mut arguments.walk())
        .find(|node| node.kind() == "value_argument");
    let separator = if let Some(argument) = separator {
        if !super::known_string_split_argument(source, file, call, provider) {
            return None;
        }
        Some(argument.named_child(0)?)
    } else {
        None
    };
    Some((receiver, separator, string_elements))
}
