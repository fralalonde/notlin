//! Kotlin reflection paths whose behavior depends on constructor metadata.
//!
//! This is intentionally a narrow syntax relation: a generic class reader
//! stores `Class<T>`, queries Kotlin constructors for that stored class, and
//! invokes `callBy`; a typed factory then exposes that reader for a bounded
//! `T`. The workspace planner can use the resolved bound to retain only
//! concrete Kotlin implementations that may be passed through that factory.

use super::{
    FactStatus, KotlinConstructorMetadataDependency, KotlinType, SemanticProvider, SourceLocation,
    SymbolId, SyntaxSemanticProvider, TypeRef,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tree_sitter::Node;

#[derive(Default)]
pub(super) struct MetadataRelations {
    readers: Vec<ReaderSyntax>,
    factories: Vec<FactorySyntax>,
    concrete_classes: Vec<ConcreteClassSyntax>,
}

struct ReaderSyntax {
    owner_start: usize,
    reflection_start: usize,
    reflection_end: usize,
}

struct FactorySyntax {
    start: usize,
    builder_name: String,
    bound_name: String,
}

struct ConcreteClassSyntax {
    start: usize,
}

/// Collect compact source facts from a tree that the caller already parsed.
pub(super) fn collect(source: &str, root: Node<'_>) -> MetadataRelations {
    let mut relations = MetadataRelations::default();
    collect_concrete_classes(source, root, &mut relations);
    // The literal type token is only a cheap negative prefilter. Positive
    // matches are always established from parsed type and expression nodes.
    if !source.contains("Class") {
        return relations;
    }
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "class_declaration" => {
                collect_class(source, node, &mut relations);
                // Nested classes are separate metadata owners. They are
                // visited from this worklist, not as part of the outer class.
                pending.extend(named_children(node));
            }
            "function_declaration" => {
                if let Some(factory) = collect_factory(source, node) {
                    relations.factories.push(factory);
                }
                pending.extend(named_children(node));
            }
            _ => pending.extend(named_children(node)),
        }
    }
    relations
}

fn collect_class(source: &str, owner: Node<'_>, relations: &mut MetadataRelations) {
    let type_parameters = direct_type_parameter_names(source, owner);
    if type_parameters.len() != 1 {
        return;
    }

    let properties = class_properties(source, owner, &type_parameters);
    if properties.is_empty() {
        return;
    }

    let mut saw_call_by = false;
    let mut reflection_properties = BTreeMap::<String, (usize, usize)>::new();
    let mut pending = named_children(owner);
    while let Some(node) = pending.pop() {
        if node.id() != owner.id() && is_declaration_boundary(node.kind()) {
            continue;
        }
        if is_call_by(node, source) {
            saw_call_by = true;
        }
        if let Some(property) = constructor_reflection_property(node, source, &properties) {
            reflection_properties
                .entry(property)
                .or_insert((node.start_byte(), node.end_byte()));
        }
        // A constructor-selection chain and callBy may be separated by local
        // declarations, so inspect the entire (non-nested-class) owner body.
        pending.extend(named_children(node));
    }

    if !saw_call_by {
        return;
    }
    for (property_name, _) in properties {
        let Some((reflection_start, reflection_end)) = reflection_properties.get(&property_name)
        else {
            continue;
        };
        relations.readers.push(ReaderSyntax {
            owner_start: owner.start_byte(),
            reflection_start: *reflection_start,
            reflection_end: *reflection_end,
        });
    }
}

fn collect_concrete_classes(source: &str, root: Node<'_>, relations: &mut MetadataRelations) {
    let mut cursor = root.walk();
    let mut finished = false;
    while !finished {
        let node = cursor.node();
        if node.kind() == "class_declaration"
            && !super::declaration_has_modifier(source, node, &["abstract"])
        {
            relations.concrete_classes.push(ConcreteClassSyntax {
                start: node.start_byte(),
            });
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                finished = true;
                break;
            }
        }
    }
}

fn class_properties(
    source: &str,
    owner: Node<'_>,
    type_parameters: &[String],
) -> Vec<(String, String)> {
    let mut properties = Vec::new();
    for node in named_children(owner) {
        match node.kind() {
            "primary_constructor" => {
                for parameter in named_children(node)
                    .into_iter()
                    .flat_map(named_children)
                    .filter(|child| child.kind() == "class_parameter")
                {
                    if !has_direct_child_kind(parameter, "val")
                        && !has_direct_child_kind(parameter, "var")
                    {
                        continue;
                    }
                    if let Some(property) = class_property_type(source, parameter, type_parameters)
                    {
                        properties.push(property);
                    }
                }
            }
            "class_body" => {
                for member in named_children(node)
                    .into_iter()
                    .filter(|child| child.kind() == "property_declaration")
                {
                    if let Some(property) = class_property_type(source, member, type_parameters) {
                        properties.push(property);
                    }
                }
            }
            _ => {}
        }
    }
    properties.sort();
    properties.dedup();
    properties
}

fn class_property_type(
    source: &str,
    property: Node<'_>,
    type_parameters: &[String],
) -> Option<(String, String)> {
    let name = if property.kind() == "class_parameter" {
        direct_identifier(property, source)?
    } else {
        let variable = property
            .named_children(&mut property.walk())
            .find(|child| child.kind() == "variable_declaration")?;
        direct_identifier(variable, source)?
    };
    let ty = if property.kind() == "class_parameter" {
        direct_type_node(property)?
    } else {
        let variable = property
            .named_children(&mut property.walk())
            .find(|child| child.kind() == "variable_declaration")?;
        direct_type_node(variable)?
    };
    let text = ty.utf8_text(source.as_bytes()).ok()?;
    let parsed = TypeRef::parse_with_type_parameters(text, type_parameters);
    let KotlinType::Named {
        name: class_name,
        arguments,
    } = parsed.ty
    else {
        return None;
    };
    if parsed.nullable
        || !matches!(class_name.as_str(), "Class" | "java.lang.Class")
        || arguments.len() != 1
    {
        return None;
    }
    match arguments[0].ty.as_ref()? {
        KotlinType::TypeParameter(parameter) if type_parameters.contains(parameter) => {
            Some((name, parameter.clone()))
        }
        _ => None,
    }
}

fn collect_factory(source: &str, function: Node<'_>) -> Option<FactorySyntax> {
    let type_parameters = direct_type_parameter_nodes(function);
    if type_parameters.is_empty() {
        return None;
    }
    let explicit_bounds = type_parameters
        .iter()
        .filter_map(|parameter| direct_type_parameter_bound(source, *parameter))
        .collect::<Vec<_>>();
    if explicit_bounds.len() != 1 {
        return None;
    }
    let (type_parameter, bound_name) = explicit_bounds.into_iter().next()?;

    let parameters = function
        .named_children(&mut function.walk())
        .find(|child| child.kind() == "function_value_parameters")?;
    let type_parameter_names = type_parameters
        .iter()
        .filter_map(|node| type_parameter_name(source, *node))
        .collect::<Vec<_>>();
    let matching_class_parameter = named_children(parameters)
        .into_iter()
        .filter(|child| child.kind() == "parameter")
        .filter_map(|parameter| {
            let ty = direct_type_node(parameter)?;
            let text = ty.utf8_text(source.as_bytes()).ok()?;
            class_type_parameter(text, &type_parameter_names).filter(|name| name == &type_parameter)
        })
        .count();
    if matching_class_parameter != 1 {
        return None;
    }

    let return_type = direct_return_type(function)?;
    let return_text = return_type.utf8_text(source.as_bytes()).ok()?;
    let names = type_parameters
        .iter()
        .filter_map(|node| type_parameter_name(source, *node))
        .collect::<Vec<_>>();
    let parsed = TypeRef::parse_with_type_parameters(return_text, &names);
    let KotlinType::Named {
        name: builder_name,
        arguments,
    } = parsed.ty
    else {
        return None;
    };
    if parsed.nullable
        || arguments.len() != 1
        || !matches!(arguments[0].ty.as_ref(), Some(KotlinType::TypeParameter(name)) if name == &type_parameter)
    {
        return None;
    }

    Some(FactorySyntax {
        start: function.start_byte(),
        builder_name,
        bound_name,
    })
}

fn direct_type_parameter_bound(source: &str, parameter: Node<'_>) -> Option<(String, String)> {
    let name = type_parameter_name(source, parameter)?;
    let children = named_children(parameter);
    let bound = children.iter().find(|child| is_type_node(child.kind()))?;
    let text = bound.utf8_text(source.as_bytes()).ok()?.trim();
    let parsed = TypeRef::parse(text);
    if parsed.nullable {
        return None;
    }
    match parsed.ty {
        KotlinType::Named {
            name: bound_name,
            arguments,
        } if arguments.is_empty() => Some((name, bound_name)),
        _ => None,
    }
}

fn class_type_parameter(text: &str, type_parameters: &[String]) -> Option<String> {
    let parsed = TypeRef::parse_with_type_parameters(text, type_parameters);
    let KotlinType::Named { name, arguments } = parsed.ty else {
        return None;
    };
    if parsed.nullable
        || !matches!(name.as_str(), "Class" | "java.lang.Class")
        || arguments.len() != 1
    {
        return None;
    }
    match arguments[0].ty.as_ref()? {
        KotlinType::TypeParameter(parameter) if type_parameters.contains(parameter) => {
            Some(parameter.clone())
        }
        _ => None,
    }
}

fn direct_return_type(function: Node<'_>) -> Option<Node<'_>> {
    function.child_by_field_name("return_type").or_else(|| {
        let parameters_end = function
            .named_children(&mut function.walk())
            .find(|child| child.kind() == "function_value_parameters")?
            .end_byte();
        named_children(function)
            .into_iter()
            .find(|child| child.start_byte() >= parameters_end && is_type_node(child.kind()))
    })
}

fn direct_type_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("type").or_else(|| {
        named_children(node)
            .into_iter()
            .find(|child| is_type_node(child.kind()))
    })
}

fn direct_type_parameter_nodes(function: Node<'_>) -> Vec<Node<'_>> {
    named_children(function)
        .into_iter()
        .find(|child| child.kind() == "type_parameters")
        .map(named_children)
        .unwrap_or_default()
        .into_iter()
        .filter(|child| child.kind() == "type_parameter")
        .collect()
}

fn direct_type_parameter_names(source: &str, declaration: Node<'_>) -> Vec<String> {
    named_children(declaration)
        .into_iter()
        .find(|child| child.kind() == "type_parameters")
        .map(named_children)
        .unwrap_or_default()
        .into_iter()
        .filter(|child| child.kind() == "type_parameter")
        .filter_map(|parameter| type_parameter_name(source, parameter))
        .collect()
}

fn type_parameter_name(source: &str, parameter: Node<'_>) -> Option<String> {
    named_children(parameter)
        .into_iter()
        .find(|child| matches!(child.kind(), "identifier" | "simple_identifier"))
        .and_then(|child| child.utf8_text(source.as_bytes()).ok())
        .map(str::to_owned)
}

fn direct_identifier(node: Node<'_>, source: &str) -> Option<String> {
    named_children(node)
        .into_iter()
        .find(|child| matches!(child.kind(), "identifier" | "simple_identifier"))
        .and_then(|child| child.utf8_text(source.as_bytes()).ok())
        .map(str::to_owned)
}

fn constructor_reflection_property(
    node: Node<'_>,
    source: &str,
    properties: &[(String, String)],
) -> Option<String> {
    if node.kind() != "navigation_expression" {
        return None;
    }
    let children = named_children(node);
    if children.len() != 2 {
        return None;
    }
    let receiver = children[0];
    let selector = node_text(children[1], source);
    if !matches!(selector.as_str(), "constructors" | "primaryConstructor") {
        return None;
    }
    if !navigation_receiver(receiver, source) {
        return None;
    }
    let property_name = stored_property_receiver(receiver_child(receiver), source)?;
    properties
        .iter()
        .any(|(name, _)| name == &property_name)
        .then_some(property_name)
}

fn navigation_receiver(node: Node<'_>, source: &str) -> bool {
    if node.kind() != "navigation_expression" {
        return false;
    }
    let children = named_children(node);
    if children.len() != 2 || simple_identifier(children[1], source).as_deref() != Some("kotlin") {
        return false;
    }
    true
}

fn stored_property_receiver(node: Node<'_>, source: &str) -> Option<String> {
    if node.kind() == "navigation_expression" {
        let children = named_children(node);
        if children.len() != 2 {
            return None;
        }
        if children[0].kind() != "this_expression" {
            return None;
        }
        return simple_identifier(children[1], source);
    }
    simple_identifier(node, source)
}

fn receiver_child(node: Node<'_>) -> Node<'_> {
    named_children(node).into_iter().next().unwrap_or(node)
}

fn is_call_by(node: Node<'_>, source: &str) -> bool {
    if node.kind() != "call_expression" {
        return false;
    }
    let Some(callee) = named_children(node).into_iter().next() else {
        return false;
    };
    callee.kind() == "navigation_expression"
        && named_children(callee).last().is_some_and(|selector| {
            simple_identifier(*selector, source).as_deref() == Some("callBy")
        })
}

fn has_direct_child_kind(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

fn is_declaration_boundary(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration" | "interface_declaration" | "object_declaration" | "enum_declaration"
    )
}

fn is_type_node(kind: &str) -> bool {
    matches!(
        kind,
        "user_type"
            | "type_reference"
            | "generic_type"
            | "nullable_type"
            | "type_identifier"
            | "scoped_type_identifier"
    )
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn node_text(node: Node<'_>, source: &str) -> String {
    node.utf8_text(source.as_bytes()).unwrap_or("").to_owned()
}

fn simple_identifier(node: Node<'_>, source: &str) -> Option<String> {
    if matches!(node.kind(), "identifier" | "simple_identifier") {
        node.utf8_text(source.as_bytes()).ok().map(str::to_owned)
    } else {
        None
    }
}

/// Resolve collected syntax against the completed, stable symbol graph and
/// return concrete Kotlin classes whose constructor metadata the factory can
/// observe.
pub(super) fn resolve(
    provider: &SyntaxSemanticProvider,
    files: &BTreeMap<PathBuf, MetadataRelations>,
) -> BTreeMap<SymbolId, KotlinConstructorMetadataDependency> {
    let mut result = BTreeMap::new();
    let mut readers = Vec::new();
    let mut factories = Vec::new();
    let mut concrete_classes = Vec::new();
    for (path, relations) in files {
        readers.extend(relations.readers.iter().map(|reader| (path, reader)));
        factories.extend(relations.factories.iter().map(|factory| (path, factory)));
        concrete_classes.extend(relations.concrete_classes.iter().map(|class| (path, class)));
    }
    let concrete_classes = concrete_classes
        .into_iter()
        .filter_map(|(path, class)| symbol_at(provider, path, class.start, "class"))
        .filter(|class| {
            class
                .file
                .extension()
                .and_then(|extension| extension.to_str())
                == Some("kt")
        })
        .collect::<Vec<_>>();

    for (reader_path, reader) in readers {
        let Some(reflection_owner) = symbol_at(provider, reader_path, reader.owner_start, "class")
        else {
            continue;
        };
        let builder = reflection_owner.clone();

        for (factory_path, factory_syntax) in &factories {
            let Some(factory) = symbol_at(provider, factory_path, factory_syntax.start, "function")
            else {
                continue;
            };
            let Some(factory_package) = provider
                .sources
                .get(*factory_path)
                .map(|s| super::package_name(s))
            else {
                continue;
            };
            let Some(factory_builder) = established_type(
                provider,
                factory_path,
                &factory_package,
                &factory_syntax.builder_name,
            ) else {
                continue;
            };
            if factory_builder != builder {
                continue;
            }
            let Some(bound) = established_type(
                provider,
                factory_path,
                &factory_package,
                &factory_syntax.bound_name,
            ) else {
                continue;
            };
            let Some(reflection_source) = provider.sources.get(reader_path) else {
                continue;
            };
            let location = SourceLocation {
                file: reader_path.clone(),
                snapshot_hash: *blake3::hash(reflection_source.as_bytes()).as_bytes(),
                start_byte: reader.reflection_start,
                end_byte: reader.reflection_end,
            };

            for class in &concrete_classes {
                if class != &bound && !provider.inherits_type(class, &bound) {
                    continue;
                }
                result.entry(class.clone()).or_insert_with(|| {
                    KotlinConstructorMetadataDependency {
                        bound: bound.clone(),
                        factory: factory.clone(),
                        reflection_owner: reflection_owner.clone(),
                        location: location.clone(),
                    }
                });
            }
        }
    }
    result
}

fn established_type(
    provider: &SyntaxSemanticProvider,
    file: &Path,
    package: &str,
    name: &str,
) -> Option<SymbolId> {
    match provider.resolve_type_in_file(file, package, name) {
        FactStatus::Established(symbol) => Some(symbol),
        _ => None,
    }
}

fn symbol_at(
    provider: &SyntaxSemanticProvider,
    path: &Path,
    start: usize,
    kind: &str,
) -> Option<SymbolId> {
    let matches = provider
        .by_file
        .get(path)?
        .iter()
        .map(|index| &provider.symbols[*index])
        .filter(|symbol| symbol.location.start_byte == start && symbol.id.kind == kind)
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [symbol] => Some(symbol.clone()),
        _ => None,
    }
}
