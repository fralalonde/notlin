use crate::java_ir::{
    JavaCompilationUnit, JavaElement, JavaIrError, JavaSyntaxNode, JavaToken, JavaTokenClass,
    parse_java, render,
};
use crate::semantics::{EagerCollectionCall, SymbolId};
use crate::translation_plan::PlannedEagerCollectionHelper;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const TEMPLATE_HOST: &str = "NotlinEagerCollectionTemplateHost";
const TEMPLATE_METHOD: &str = "__notlin_eager_collection_helper_template";

/// Generated projection parameters are unique across the source snapshot.
/// Carry their established property identity onto the accepted invocation,
/// even when other classes expose getters with the same Java spelling.
pub(crate) fn attach_projection_targets(
    unit: &mut JavaCompilationUnit,
    owners: &[SymbolId],
    calls: &[EagerCollectionCall],
    container_calls: &[crate::semantics::ContainerMemberCall],
) -> Result<(), JavaIrError> {
    let mut targets = calls
        .iter()
        .filter(|call| owners.contains(&call.owner))
        .filter_map(|call| call.property_projection.as_ref())
        .map(|projection| ProjectionTarget {
            parameter: projection.java_parameter_name.clone(),
            getter_path: vec![projection.getter_name.clone()],
            target: projection
                .getter_target
                .as_ref()
                .unwrap_or(&projection.target)
                .clone(),
        })
        .collect::<Vec<_>>();
    targets.extend(
        container_calls
            .iter()
            .filter(|call| {
                call.owner
                    .as_ref()
                    .is_some_and(|owner| owners.contains(owner))
            })
            .filter_map(|call| call.lambda_binding.as_ref())
            .flat_map(|binding| {
                binding
                    .property_accesses
                    .iter()
                    .map(|access| ProjectionTarget {
                        parameter: binding.java_parameter_name.clone(),
                        getter_path: access.getter_path.clone(),
                        target: access
                            .projection
                            .getter_target
                            .as_ref()
                            .unwrap_or(&access.projection.target)
                            .clone(),
                    })
            }),
    );
    let targets = projection_target_index(targets)?;
    if targets.is_empty() {
        return Ok(());
    }
    let mut candidate = unit.clone();
    attach_targets(&mut candidate, &targets)?;
    *unit = candidate;
    Ok(())
}

fn attach_targets(
    unit: &mut JavaCompilationUnit,
    targets: &BTreeMap<(String, Vec<String>), SymbolId>,
) -> Result<(), JavaIrError> {
    let mut attached = BTreeSet::new();
    let mut conflict = false;
    unit.root.visit_mut(&mut |node| {
        if node.kind != "method_invocation" {
            return;
        }
        let Some((receiver_text, getter_path)) = invocation_path(node) else {
            return;
        };
        let key = (receiver_text, getter_path);
        if let Some(target) = targets.get(&key) {
            attached.insert(key);
            if node
                .origin_target
                .as_ref()
                .is_some_and(|existing| existing != target)
            {
                conflict = true;
            } else {
                node.origin_target = Some(target.clone());
            }
        }
    });
    if conflict {
        return Err(error(
            "accepted projection invocation has a conflicting target",
        ));
    }
    if targets.keys().any(|key| !attached.contains(key)) {
        return Err(error(
            "planned projection path is missing from accepted Java unit",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ProjectionTarget {
    parameter: String,
    getter_path: Vec<String>,
    target: SymbolId,
}

/// Keep only unambiguous source-path mappings. Duplicate evidence for the
/// same path and target is harmless; conflicting accepted targets abort.
fn projection_target_index(
    targets: Vec<ProjectionTarget>,
) -> Result<BTreeMap<(String, Vec<String>), SymbolId>, JavaIrError> {
    let mut indexed = BTreeMap::<(String, Vec<String>), SymbolId>::new();
    for entry in targets {
        let key = (entry.parameter, entry.getter_path);
        match indexed.entry(key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(entry.target);
            }
            std::collections::btree_map::Entry::Occupied(slot) => {
                if slot.get() != &entry.target {
                    return Err(error("planned projection path has conflicting targets"));
                }
            }
        }
    }
    Ok(indexed)
}

/// Returns the simple identifier root and ordered method names for a pure
/// getter invocation chain. Calls and other receiver forms are not guessed.
fn invocation_path(node: &JavaSyntaxNode) -> Option<(String, Vec<String>)> {
    if node.kind != "method_invocation" {
        return None;
    }
    if !has_empty_arguments(node) {
        return None;
    }
    let object = node
        .children
        .iter()
        .filter_map(element_node)
        .find(|child| child.field_name.as_deref() == Some("object"))?;
    let object = transparent_expression(object)?;
    let name = declaration_name(node)?;
    let (root, mut path) = if object.kind == "identifier" {
        (first_identifier(object)?, Vec::new())
    } else {
        invocation_path(object)?
    };
    path.push(name);
    Some((root, path))
}

fn transparent_expression(mut node: &JavaSyntaxNode) -> Option<&JavaSyntaxNode> {
    while node.kind == "parenthesized_expression" {
        let mut expressions = node.children.iter().filter_map(element_node);
        node = expressions.next()?;
        if expressions.next().is_some() {
            return None;
        }
    }
    Some(node)
}

fn has_empty_arguments(node: &JavaSyntaxNode) -> bool {
    let Some(arguments) = node
        .children
        .iter()
        .filter_map(element_node)
        .find(|child| child.kind == "argument_list")
    else {
        return false;
    };
    !arguments
        .children
        .iter()
        .any(|child| element_node(child).is_some())
}

/// Add planned eager collection helpers to the top-level type represented by
/// `path`. The operation is transactional: the unit changes only after the
/// completed Java tree has passed the structural parser.
pub(crate) fn inject(
    unit: &mut JavaCompilationUnit,
    path: &str,
    owners: &[SymbolId],
    helpers: &[PlannedEagerCollectionHelper],
) -> Result<Vec<PlannedEagerCollectionHelper>, JavaIrError> {
    if helpers.is_empty() {
        return Ok(Vec::new());
    }
    let owner_set = owners.iter().cloned().collect::<BTreeSet<_>>();
    let mut by_id = BTreeMap::<String, PlannedEagerCollectionHelper>::new();
    for helper in helpers
        .iter()
        .filter(|helper| owner_set.contains(&helper.origin))
    {
        let key = helper.id.stable_key();
        if let Some(previous) = by_id.get(&key) {
            if previous != helper {
                return Err(error("eager helper id has conflicting descriptors"));
            }
        } else {
            by_id.insert(key, helper.clone());
        }
    }
    let selected = by_id.into_values().collect::<Vec<_>>();
    if selected.is_empty() {
        return Ok(selected);
    }

    let host_name = Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| error("eager helper output path has no UTF-8 type stem"))?;
    let host_indices = unit
        .root
        .children
        .iter()
        .enumerate()
        .filter_map(|(index, element)| {
            let node = element_node(element)?;
            (is_supported_host(node) && declaration_name(node).as_deref() == Some(host_name))
                .then_some(index)
        })
        .collect::<Vec<_>>();
    if host_indices.len() != 1 {
        return Err(error(if host_indices.is_empty() {
            "eager helper host does not match the output path stem"
        } else {
            "eager helper host is ambiguous for the output path stem"
        }));
    }

    let host_index = host_indices[0];
    let host = element_node(&unit.root.children[host_index]).expect("host index was resolved");
    let existing_methods = direct_method_names(host);
    let mut names = BTreeSet::new();
    for helper in &selected {
        if !is_java_identifier(&helper.name) {
            return Err(error("eager helper name is not a safe Java identifier"));
        }
        if !names.insert(helper.name.as_str()) {
            return Err(error(
                "distinct eager helpers share a generated method name",
            ));
        }
        if existing_methods.contains(&helper.name) {
            return Err(error(
                "eager helper method name collides with an existing host method",
            ));
        }
    }

    let mut methods = selected
        .iter()
        .map(build_method)
        .collect::<Result<Vec<_>, _>>()?;
    let mut candidate = unit.clone();
    let host = element_node_mut(&mut candidate.root.children[host_index])
        .expect("host index was resolved in cloned tree");
    for helper in &selected {
        mark_matching_invocations(host, helper);
    }
    let is_enum = host.kind == "enum_declaration";
    let body = find_host_body_mut(host).ok_or_else(|| error("eager helper host has no body"))?;

    let close_index = body
        .children
        .iter()
        .rposition(|element| is_token(element, "}"))
        .ok_or_else(|| error("eager helper host body has no closing brace"))?;
    let mut additions = Vec::new();
    if is_enum && !body.children.iter().any(|element| is_token(element, ";")) {
        additions.push(JavaElement::Token(JavaToken {
            kind: ";".into(),
            field_name: None,
            text: ";".into(),
            class: JavaTokenClass::OperatorOrPunctuation,
            leading_trivia: "\n    ".into(),
        }));
    }
    additions.extend(methods.drain(..).map(JavaElement::Declaration));
    body.children.splice(close_index..close_index, additions);
    if let Some(JavaElement::Token(close)) = body.children.last_mut()
        && close.text == "}"
        && close.leading_trivia.is_empty()
    {
        close.leading_trivia = "\n".into();
    }

    let rendered = render(&candidate);
    parse_java(&rendered)?;
    *unit = candidate;
    Ok(selected)
}

fn build_method(helper: &PlannedEagerCollectionHelper) -> Result<JavaSyntaxNode, JavaIrError> {
    let body = match helper.operation {
        crate::semantics::EagerCollectionOperation::Map => format!(
            concat!(
                "private static <E, R> java.util.List<R> {TEMPLATE_METHOD}(java.lang.Iterable<? extends E> values, java.util.function.Function<? super E, ? extends R> transform) {{\n",
                "        java.util.ArrayList<R> result = new java.util.ArrayList<R>(values instanceof java.util.Collection<?> ? ((java.util.Collection<?>) values).size() : 10);\n",
                "        for (E value : values) {{\n",
                "            result.add(transform.apply(value));\n",
                "        }}\n",
                "        return result;\n",
                "    }}"
            ),
            TEMPLATE_METHOD = TEMPLATE_METHOD
        ),
        crate::semantics::EagerCollectionOperation::FilterNot => format!(
            concat!(
                "private static <E> java.util.List<E> {TEMPLATE_METHOD}(java.lang.Iterable<? extends E> values, java.util.function.Predicate<? super E> predicate) {{\n",
                "        java.util.ArrayList<E> result = new java.util.ArrayList<E>();\n",
                "        for (E value : values) {{\n",
                "            if (!predicate.test(value)) {{\n",
                "                result.add(value);\n",
                "            }}\n",
                "        }}\n",
                "        return result;\n",
                "    }}"
            ),
            TEMPLATE_METHOD = TEMPLATE_METHOD
        ),
    };
    let source = format!("class {TEMPLATE_HOST} {{\n    {body}\n}}\n");
    let mut unit = parse_java(&source)?;
    let host = unit
        .root
        .children
        .iter_mut()
        .filter_map(element_node_mut)
        .find(|node| node.kind == "class_declaration")
        .ok_or_else(|| error("trusted eager helper template has no class"))?;
    let body = find_host_body_mut(host)
        .ok_or_else(|| error("trusted eager helper template class has no body"))?;
    let mut methods = body
        .children
        .iter_mut()
        .filter_map(element_node_mut)
        .filter(|node| node.kind == "method_declaration");
    let mut method = methods
        .next()
        .cloned()
        .ok_or_else(|| error("trusted eager helper template has no method"))?;
    if methods.next().is_some() {
        return Err(error("trusted eager helper template has multiple methods"));
    }
    rename_identifier_tokens(&mut method, TEMPLATE_METHOD, &helper.name);
    method.origin_target = Some(helper.id.clone());
    Ok(method)
}

fn mark_matching_invocations(host: &mut JavaSyntaxNode, helper: &PlannedEagerCollectionHelper) {
    if host.kind == "method_invocation"
        && declaration_name(host).as_deref() == Some(helper.name.as_str())
        && host.origin_target.is_none()
    {
        host.origin_target = Some(helper.id.clone());
    }
    for child in &mut host.children {
        if let Some(child) = element_node_mut(child) {
            mark_matching_invocations(child, helper);
        }
    }
}

fn rename_identifier_tokens(node: &mut JavaSyntaxNode, from: &str, to: &str) {
    for child in &mut node.children {
        match child {
            JavaElement::Token(token)
                if token.class == JavaTokenClass::Identifier && token.text == from =>
            {
                token.text = to.to_owned();
            }
            other => {
                if let Some(node) = element_node_mut(other) {
                    rename_identifier_tokens(node, from, to);
                }
            }
        }
    }
}

fn find_host_body_mut(host: &mut JavaSyntaxNode) -> Option<&mut JavaSyntaxNode> {
    let body_kind = match host.kind.as_str() {
        "class_declaration" | "record_declaration" => "class_body",
        "interface_declaration" => "interface_body",
        "enum_declaration" => "enum_body",
        _ => return None,
    };
    find_node_mut(host, body_kind)
}

fn find_node_mut<'a>(node: &'a mut JavaSyntaxNode, kind: &str) -> Option<&'a mut JavaSyntaxNode> {
    if node.kind == kind {
        return Some(node);
    }
    for child in &mut node.children {
        if let Some(child) = element_node_mut(child)
            && let Some(found) = find_node_mut(child, kind)
        {
            return Some(found);
        }
    }
    None
}

fn direct_method_names(host: &JavaSyntaxNode) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Some(body) = find_node(
        host,
        match host.kind.as_str() {
            "class_declaration" | "record_declaration" => "class_body",
            "interface_declaration" => "interface_body",
            "enum_declaration" => "enum_body",
            _ => return names,
        },
    ) else {
        return names;
    };
    for element in &body.children {
        if let Some(member) = element_node(element)
            && member.kind == "method_declaration"
            && let Some(name) = declaration_name(member)
        {
            names.insert(name);
        }
    }
    names
}

fn find_node<'a>(node: &'a JavaSyntaxNode, kind: &str) -> Option<&'a JavaSyntaxNode> {
    if node.kind == kind {
        return Some(node);
    }
    for child in &node.children {
        if let Some(child) = element_node(child)
            && let Some(found) = find_node(child, kind)
        {
            return Some(found);
        }
    }
    None
}

fn is_supported_host(node: &JavaSyntaxNode) -> bool {
    matches!(
        node.kind.as_str(),
        "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration"
    )
}

fn declaration_name(node: &JavaSyntaxNode) -> Option<String> {
    let name_node = node.children.iter().find_map(|element| {
        let child = element_node(element)?;
        (child.field_name.as_deref() == Some("name")).then_some(child)
    });
    let name_node = name_node?;
    first_identifier(name_node)
}

fn first_identifier(node: &JavaSyntaxNode) -> Option<String> {
    for element in &node.children {
        match element {
            JavaElement::Token(token) if token.class == JavaTokenClass::Identifier => {
                return Some(token.text.clone());
            }
            other => {
                if let Some(node) = element_node(other)
                    && let Some(name) = first_identifier(node)
                {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn element_node(element: &JavaElement) -> Option<&JavaSyntaxNode> {
    match element {
        JavaElement::Declaration(node)
        | JavaElement::Type(node)
        | JavaElement::Expression(node)
        | JavaElement::Statement(node)
        | JavaElement::Annotation(node)
        | JavaElement::Import(node)
        | JavaElement::Other(node) => Some(node),
        JavaElement::Token(_) => None,
    }
}

fn element_node_mut(element: &mut JavaElement) -> Option<&mut JavaSyntaxNode> {
    match element {
        JavaElement::Declaration(node)
        | JavaElement::Type(node)
        | JavaElement::Expression(node)
        | JavaElement::Statement(node)
        | JavaElement::Annotation(node)
        | JavaElement::Import(node)
        | JavaElement::Other(node) => Some(node),
        JavaElement::Token(_) => None,
    }
}

fn is_token(element: &JavaElement, text: &str) -> bool {
    matches!(element, JavaElement::Token(token) if token.text == text)
}

fn is_java_identifier(name: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "abstract",
        "assert",
        "boolean",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extends",
        "final",
        "finally",
        "float",
        "for",
        "goto",
        "if",
        "implements",
        "import",
        "instanceof",
        "int",
        "interface",
        "long",
        "native",
        "new",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "static",
        "strictfp",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "try",
        "void",
        "volatile",
        "while",
        "true",
        "false",
        "null",
        "_",
    ];
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
        && !KEYWORDS.contains(&name)
}

fn error(message: &str) -> JavaIrError {
    JavaIrError {
        message: message.to_owned(),
        start_byte: 0,
        end_byte: 0,
    }
}

#[cfg(test)]
fn visit(node: &JavaSyntaxNode, f: &mut impl FnMut(&JavaSyntaxNode)) {
    f(node);
    for child in &node.children {
        if let Some(child) = element_node(child) {
            visit(child, f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantics::EagerCollectionOperation;

    fn symbol(name: &str, kind: &str) -> SymbolId {
        SymbolId {
            module: "test".into(),
            package: "sample".into(),
            file: "src/Foo.kt".into(),
            owner_path: Vec::new(),
            kind: kind.into(),
            name: name.into(),
            receiver: None,
            parameters: Vec::new(),
        }
    }

    fn helper(
        name: &str,
        operation: EagerCollectionOperation,
        origin: SymbolId,
    ) -> PlannedEagerCollectionHelper {
        PlannedEagerCollectionHelper {
            id: symbol(&format!("__{name}"), "generated:eager-collection"),
            origin,
            name: name.into(),
            parameter_name: "value".into(),
            operation,
        }
    }

    fn parsed(source: &str) -> JavaCompilationUnit {
        parse_java(source).expect("valid Java test host")
    }

    #[test]
    fn injects_map_and_filter_not_into_matching_host_with_provenance() {
        let origin_map = symbol("mapped", "function");
        let origin_filter = symbol("filtered", "function");
        let helpers = [
            helper(
                "__mapValues",
                EagerCollectionOperation::Map,
                origin_map.clone(),
            ),
            helper(
                "__filterNotValues",
                EagerCollectionOperation::FilterNot,
                origin_filter.clone(),
            ),
        ];
        let owners = [origin_map.clone(), origin_filter.clone()];
        let mut unit = parsed(
            "package sample; public class Foo { public static Object mapped() { __mapValues(null, null); return __mapValues(null, null); } }",
        );
        let already_resolved = symbol("anotherTarget", "function");
        let mut first_call = true;
        unit.root.visit_mut(&mut |node| {
            if first_call
                && node.kind == "method_invocation"
                && declaration_name(node).as_deref() == Some("__mapValues")
            {
                node.origin_target = Some(already_resolved.clone());
                first_call = false;
            }
        });
        let inserted = inject(&mut unit, "out/Foo.java", &owners, &helpers).unwrap();
        assert_eq!(inserted.len(), 2);
        let rendered = render(&unit);
        assert!(rendered.contains("java.util.Collection<?>"));
        assert!(rendered.contains("transform.apply(value)"));
        assert!(rendered.contains("predicate.test(value)"));
        assert!(
            rendered.find("Collection<?>) values).size()").unwrap()
                < rendered.find("result.add(transform.apply(value))").unwrap()
        );
        assert!(!rendered.contains("values.size()"));

        let mut method_targets = Vec::new();
        let mut invocation_targets = Vec::new();
        visit(&unit.root, &mut |node| {
            if node.kind == "method_declaration"
                && declaration_name(node).is_some_and(|name| name.starts_with("__"))
            {
                method_targets.push(node.origin_target.clone());
            }
            if node.kind == "method_invocation"
                && declaration_name(node).is_some_and(|name| name.starts_with("__"))
            {
                invocation_targets.push(node.origin_target.clone());
            }
        });
        assert_eq!(method_targets.len(), 2);
        assert!(method_targets.iter().all(Option::is_some));
        assert_eq!(
            invocation_targets,
            vec![Some(already_resolved), Some(helpers[0].id.clone())]
        );
    }

    #[test]
    fn filters_by_origin_and_deduplicates_stable_helper_ids() {
        let owner = symbol("mapped", "function");
        let unused = symbol("unused", "function");
        let map = helper("__mapValues", EagerCollectionOperation::Map, owner.clone());
        let ignored = helper("__ignored", EagerCollectionOperation::Map, unused);
        let mut unit = parsed("class Foo {}");
        let inserted = inject(
            &mut unit,
            "Foo.java",
            &[owner],
            &[map.clone(), map.clone(), ignored],
        )
        .unwrap();
        assert_eq!(inserted, vec![map]);
        assert_eq!(render(&unit).matches("__mapValues(").count(), 1);
    }

    #[test]
    fn rejects_name_collisions_and_ambiguous_hosts_transactionally() {
        let owner = symbol("mapped", "function");
        let helper = helper("__mapValues", EagerCollectionOperation::Map, owner.clone());
        let mut collision = parsed("class Foo { private void __mapValues() {} }");
        let before = collision.clone();
        assert!(
            inject(
                &mut collision,
                "Foo.java",
                std::slice::from_ref(&owner),
                std::slice::from_ref(&helper)
            )
            .is_err()
        );
        assert_eq!(collision, before);

        let mut ambiguous = parsed("class Foo {} interface Foo {}");
        let before = ambiguous.clone();
        assert!(
            inject(
                &mut ambiguous,
                "Foo.java",
                std::slice::from_ref(&owner),
                std::slice::from_ref(&helper)
            )
            .is_err()
        );
        assert_eq!(ambiguous, before);

        let mut missing = parsed("class Other {}");
        let before = missing.clone();
        assert!(inject(&mut missing, "Foo.java", &[owner], &[helper]).is_err());
        assert_eq!(missing, before);
    }

    #[test]
    fn injects_inside_enum_body_after_constants_and_preserves_existing_targets() {
        let origin = symbol("mapped", "function");
        let helper = helper("__mapValues", EagerCollectionOperation::Map, origin.clone());
        let mut unit = parsed("enum Foo { FIRST, SECOND }");
        let inserted = inject(
            &mut unit,
            "Foo.java",
            &[origin],
            std::slice::from_ref(&helper),
        )
        .unwrap();
        assert_eq!(inserted.len(), 1);
        let rendered = render(&unit);
        assert!(
            rendered.contains("SECOND\n    ;\n    private static"),
            "{rendered}"
        );
        parse_java(&rendered).unwrap();
    }

    #[test]
    fn chained_projection_targets_match_the_full_receiver_path() {
        let label_data = symbol("labelData", "property");
        let label_other = symbol("labelOther", "property");
        let targets = projection_target_index(vec![
            ProjectionTarget {
                parameter: "root".into(),
                getter_path: vec!["getData".into(), "getLabel".into()],
                target: label_data.clone(),
            },
            ProjectionTarget {
                parameter: "root".into(),
                getter_path: vec!["getOther".into(), "getLabel".into()],
                target: label_other.clone(),
            },
        ])
        .unwrap();
        let mut unit = parsed(
            "class Consumer { void run(Entry root) { root.getData().getLabel(); root.getOther().getLabel(); } }",
        );
        attach_targets(&mut unit, &targets).unwrap();

        let mut attached = BTreeMap::new();
        visit(&unit.root, &mut |node| {
            if node.kind == "method_invocation"
                && declaration_name(node).as_deref() == Some("getLabel")
            {
                let path = invocation_path(node).expect("pure getter chain");
                attached.insert(path, node.origin_target.clone());
            }
        });
        assert_eq!(
            attached.get(&("root".into(), vec!["getData".into(), "getLabel".into()])),
            Some(&Some(label_data))
        );
        assert_eq!(
            attached.get(&("root".into(), vec!["getOther".into(), "getLabel".into()])),
            Some(&Some(label_other))
        );
    }

    #[test]
    fn conflicting_projection_targets_for_one_path_are_rejected() {
        let first = symbol("first", "property");
        let second = symbol("second", "property");
        let conflict = projection_target_index(vec![
            ProjectionTarget {
                parameter: "root".into(),
                getter_path: vec!["getLabel".into()],
                target: first,
            },
            ProjectionTarget {
                parameter: "root".into(),
                getter_path: vec!["getLabel".into()],
                target: second,
            },
        ]);
        assert!(conflict.is_err());
    }

    #[test]
    fn path_matching_accepts_parentheses_but_rejects_calls_with_arguments() {
        let unit = parsed(
            "class Consumer { void run(Entry root) { ((root)).getLabel(); root.getLabel(1); root.getData(1).getLabel(); } }",
        );
        let mut paths = Vec::new();
        visit(&unit.root, &mut |node| {
            if node.kind == "method_invocation" {
                paths.push((declaration_name(node), invocation_path(node)));
            }
        });
        let labels = paths
            .iter()
            .filter(|(name, _)| name.as_deref() == Some("getLabel"))
            .collect::<Vec<_>>();
        assert_eq!(labels.len(), 3);
        assert_eq!(labels.iter().filter(|(_, path)| path.is_some()).count(), 1);
    }
}
