//! Lexical upper bounds establish member capabilities, never an exact value type.
use crate::transpiler::kt;
use crate::workspace::{SourceFile, SourceIndex};
use tree_sitter::Node;

pub(crate) fn bounds<'t>(
    declaration: Node<'t>,
    parameter: Node<'t>,
    source: &str,
) -> Vec<Node<'t>> {
    let Some(name) = kt::child(parameter, "identifier") else {
        return Vec::new();
    };
    let mut result = parameter
        .named_children(&mut parameter.walk())
        .filter(|node| is_type(*node))
        .collect::<Vec<_>>();
    if let Some(constraints) = kt::child(declaration, "type_constraints") {
        for constraint in constraints
            .named_children(&mut constraints.walk())
            .filter(|node| node.kind() == "type_constraint")
        {
            if kt::child(constraint, "identifier")
                .is_some_and(|id| kt::text(id, source) == kt::text(name, source))
            {
                result.extend(
                    constraint
                        .named_children(&mut constraint.walk())
                        .filter(|node| is_type(*node)),
                );
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    result.retain(|node| seen.insert(kt::text(*node, source)));
    result
}

fn is_type(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "user_type"
            | "nullable_type"
            | "function_type"
            | "parenthesized_type"
            | "intersection_type"
    )
}

pub(crate) fn collection_element_capabilities(
    index: &SourceIndex,
    file: &SourceFile,
    source: &str,
    origin: Node<'_>,
    ty: &str,
) -> Option<Vec<String>> {
    use crate::semantics::{KotlinType, TypeRef, Variance};
    let parsed = TypeRef::parse(ty);
    let KotlinType::Named { name, arguments } = parsed.ty else {
        return None;
    };
    let (package, simple) = name
        .rsplit_once('.')
        .map_or((None, name.as_str()), |(package, simple)| {
            (Some(package), simple)
        });
    if !matches!(
        simple,
        "List"
            | "MutableList"
            | "ArrayList"
            | "Set"
            | "MutableSet"
            | "HashSet"
            | "LinkedHashSet"
            | "Collection"
            | "Iterable"
            | "Sequence"
    ) || !crate::receiver_binding::file_scoped_type_reference(origin, &name, source)
    {
        return None;
    }
    if let Some(package) = package {
        if !matches!(
            package,
            "kotlin.collections" | "java.util" | "kotlin.sequences"
        ) || crate::workspace::canonical_type_binding(index, file, &name).is_some()
        {
            return None;
        }
    } else if !crate::workspace::collection_base_is_unshadowed(index, file, simple)
        || index
            .type_aliases_for(&file.path)
            .iter()
            .any(|(alias, _)| alias == simple)
        || file.imports.iter().any(|import| {
            import
                .split_once(" as ")
                .is_some_and(|(_, alias)| alias.trim() == simple)
        })
    {
        return None;
    }
    let [argument] = arguments.as_slice() else {
        return None;
    };
    if argument.variance == Variance::In {
        return None;
    }
    let element = crate::transpiler::types::kotlin_type_text(argument.ty.as_ref()?, false);
    if let Some(bounds) = member_capabilities(index, file, source, origin, &element) {
        return Some(bounds);
    }
    if !crate::receiver_binding::file_scoped_type_reference(origin, &element, source) {
        return None;
    }
    Some(vec![
        crate::workspace::property_receiver_type_binding(index, file, &element)?.qualified,
    ])
}

fn lexical<'t>(origin: Node<'t>, name: &str, source: &str) -> Option<(Node<'t>, Node<'t>)> {
    let mut current = Some(origin);
    while let Some(scope) = current {
        if let Some(parameters) = kt::child(scope, "type_parameters") {
            let matches = parameters
                .named_children(&mut parameters.walk())
                .filter(|node| node.kind() == "type_parameter")
                .filter(|node| {
                    kt::child(*node, "identifier")
                        .is_some_and(|id| kt::text(id, source).trim_matches('`') == name)
                })
                .collect::<Vec<_>>();
            if let [parameter] = matches.as_slice() {
                return Some((scope, *parameter));
            }
            if !matches.is_empty() {
                return None;
            }
        }
        current = scope.parent();
    }
    None
}

/// `Some([])` means a lexical parameter owns the spelling but its bounds do
/// not prove any member receiver. It must not fall back to a source class
/// with the same short name. Every nominal bound is bound in its own scope.
pub(crate) fn member_capabilities(
    index: &SourceIndex,
    file: &SourceFile,
    source: &str,
    origin: Node<'_>,
    ty: &str,
) -> Option<Vec<String>> {
    fn collect(
        index: &SourceIndex,
        file: &SourceFile,
        source: &str,
        origin: Node<'_>,
        ty: &str,
        seen: &mut std::collections::HashSet<(usize, String)>,
    ) -> Option<Vec<String>> {
        let name = ty.trim().trim_end_matches('?').trim_matches('`');
        let (scope, parameter) = lexical(origin, name, source)?;
        if !seen.insert((scope.start_byte(), name.to_owned())) || seen.len() > 32 {
            return Some(Vec::new());
        }
        let mut result = Vec::new();
        for bound in bounds(scope, parameter, source) {
            let spelling = kt::text(bound, source).trim();
            if let Some(nested) = collect(index, file, source, scope, spelling, seen) {
                if nested.is_empty() {
                    return Some(Vec::new());
                }
                result.extend(nested);
            } else if crate::receiver_binding::file_scoped_type_reference(scope, spelling, source) {
                let Some(binding) =
                    crate::workspace::property_receiver_type_binding(index, file, spelling)
                else {
                    return Some(Vec::new());
                };
                result.push(binding.qualified);
            } else {
                return Some(Vec::new());
            }
        }
        seen.remove(&(scope.start_byte(), name.to_owned()));
        result.sort();
        result.dedup();
        Some(result)
    }
    collect(
        index,
        file,
        source,
        origin,
        ty,
        &mut std::collections::HashSet::new(),
    )
}
