//! Stable, syntax-derived semantic facts for migration planning.
//!
//! This layer deliberately reports uncertainty instead of inventing compiler
//! answers. In particular, an unresolved type is never silently Object.
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

mod map_alias;

mod bottom_returns;
mod class_literals;
mod constructor_delegation;
mod constructor_metadata;
mod container_calls;
mod container_types;
mod eager_collections;
mod enum_when;
#[cfg(test)]
mod generated_getter_tests;
mod inherited_calls;
mod inherited_properties;
mod jackson_defaults;
mod java_property_reads;
mod object_calls;
mod overloads;
mod platform_constructors;
mod platform_members;
mod property_getter_equivalence;
mod reified_calls;
mod source_calls;
mod todo;
pub(crate) use class_literals::{
    contains_unlowerable_class_literal, established_default_companion_call,
};
pub use container_calls::{ContainerMemberCall, ContainerResultType};
pub(crate) use container_types::qualify_type as qualify_type_in_file;
pub use container_types::{ContainerLambdaBinding, ContainerPropertyAccess};
pub use eager_collections::{
    EagerCollectionCall, EagerCollectionOperation, EagerPropertyProjection,
};
pub use enum_when::{EnumWhenArm, EnumWhenLowering};
pub use inherited_properties::InheritedPropertyAccess;
pub use object_calls::{ReferenceStringCall, ReferenceStringCallKind, ReferenceStringReceiver};
pub use overloads::ExactOverloadCall;
pub(crate) use platform_constructors::established_platform_constructor;
pub use reified_calls::ReifiedClassCall;
pub use todo::TodoLowering;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SymbolId {
    /// Build/module identity supplied by the analysis configuration.
    pub module: String,
    pub package: String,
    pub file: PathBuf,
    pub owner_path: Vec<String>,
    pub kind: String,
    pub name: String,
    pub receiver: Option<String>,
    pub parameters: Vec<String>,
}

/// Nullability declared by an indexed source callable's reference return.
/// `Platform` means the declaration is Java and provides no recognized
/// nullness annotation; it must not be treated as a non-null contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceReturnNullability {
    NonNull,
    Nullable,
    Platform,
}

/// An exact Java synthetic-property call, including the distinction between
/// JVM primitives and boxed/platform returns. Types are qualified in the
/// declaring file's scope; dependencies retain the original method identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaPropertyRead {
    pub target: SymbolId,
    pub method_name: String,
    pub location: SourceLocation,
    pub result_type: TypeRef,
    pub nullability: ReferenceReturnNullability,
    pub jvm_primitive: bool,
    pub dependencies: Vec<SymbolId>,
}
pub fn declaration_node_kind(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "object_declaration"
            | "function_declaration"
            | "property_declaration"
            | "type_alias"
            | "enum_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "method_declaration"
            | "primary_constructor"
            | "secondary_constructor"
            | "constructor_declaration"
    )
}
fn is_property_parameter(node: tree_sitter::Node) -> bool {
    if node.kind() != "class_parameter" {
        return false;
    }
    let mut walk = node.walk();
    node.children(&mut walk)
        .any(|child| matches!(child.kind(), "val" | "var"))
}
fn property_parameter_symbol(
    source: &str,
    node: tree_sitter::Node,
    file: &Path,
) -> Option<SymbolId> {
    if !is_property_parameter(node) {
        return None;
    }
    let mut name_node = node.child_by_field_name("name");
    if name_node.is_none() {
        let mut w = node.walk();
        name_node = node
            .named_children(&mut w)
            .find(|n| matches!(n.kind(), "identifier" | "simple_identifier"));
    }
    let name = name_node
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())?
        .to_owned();
    let mut parent = node.parent();
    let mut owner = None;
    while let Some(n) = parent {
        if matches!(n.kind(), "class_declaration" | "object_declaration") {
            owner = Some(symbol_id_for_node(source, n, file));
            break;
        }
        parent = n.parent()
    }
    let mut owner_path = owner
        .as_ref()
        .map(|id| id.owner_path.clone())
        .unwrap_or_default();
    if let Some(id) = owner {
        owner_path.push(id.name)
    }
    Some(SymbolId {
        module: String::new(),
        package: package_name(source),
        file: file.to_path_buf(),
        owner_path,
        kind: "property".into(),
        name,
        receiver: None,
        parameters: vec![],
    })
}
fn package_name(source: &str) -> String {
    source
        .lines()
        .find_map(|line| {
            line.trim().strip_prefix("package ").map(|rest| {
                rest.split("//")
                    .next()
                    .unwrap_or(rest)
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect()
            })
        })
        .unwrap_or_default()
}
fn node_name(node: tree_sitter::Node, source: &str) -> Option<String> {
    if let Some(n) = node.child_by_field_name("name") {
        return n.utf8_text(source.as_bytes()).ok().map(str::to_owned);
    }
    let mut c = node.walk();
    for n in node.named_children(&mut c) {
        if matches!(n.kind(), "simple_identifier" | "identifier") {
            return n.utf8_text(source.as_bytes()).ok().map(str::to_owned);
        }
    }
    None
}

/// Recover the declared name from the parser's top-level recovery shape for
/// annotated interfaces. Some Kotlin annotation/class-literal combinations
/// are represented as an `annotated_expression` (or an `ERROR`) around the
/// declaration rather than as a normal `interface_declaration`. In that
/// shape `node_name` either sees no name or sees an identifier from an
/// annotation, so use the first identifier token after the actual `interface`
/// keyword.
pub(crate) fn recovered_interface_name(node: tree_sitter::Node, source: &str) -> Option<String> {
    if !matches!(node.kind(), "annotated_expression" | "ERROR") {
        return None;
    }
    let mut leaves = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind().contains("comment")
            || current.kind().contains("string")
            || (current.id() != node.id()
                && (declaration_node_kind(current.kind())
                    || matches!(
                        current.kind(),
                        "class_body" | "enum_class_body" | "function_body" | "lambda_literal"
                    )))
        {
            continue;
        }
        if current.child_count() == 0 {
            leaves.push(current);
            continue;
        }
        let mut walk = current.walk();
        let children: Vec<_> = current.children(&mut walk).collect();
        stack.extend(children.into_iter().rev());
    }
    leaves.sort_by_key(|leaf| leaf.start_byte());
    let mut keywords = leaves
        .iter()
        .enumerate()
        .filter(|(_, leaf)| leaf.utf8_text(source.as_bytes()).ok() == Some("interface"));
    let (position, _) = keywords.next()?;
    if keywords.next().is_some() {
        return None;
    }
    let name = leaves.get(position + 1)?;
    if name.is_missing()
        || !matches!(
            name.kind(),
            "simple_identifier" | "identifier" | "type_identifier"
        )
    {
        return None;
    }
    name.utf8_text(source.as_bytes())
        .ok()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Parser recovery counterpart for `annotation class Name` when a preceding
/// annotation with a custom simple name is parsed as an infix expression.
/// Accept only the unambiguous keyword/name sequence in the wrapper.
pub(crate) fn recovered_annotation_class_name_node<'tree>(
    node: tree_sitter::Node<'tree>,
    source: &str,
) -> Option<tree_sitter::Node<'tree>> {
    if node.kind() != "annotated_expression" {
        return None;
    }
    let mut leaves = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind().contains("comment") || current.kind().contains("string") {
            continue;
        }
        if current.child_count() == 0 {
            leaves.push(current);
        } else {
            let mut walk = current.walk();
            stack.extend(current.children(&mut walk));
        }
    }
    leaves.sort_by_key(|leaf| leaf.start_byte());
    let mut matches = leaves.windows(3).filter(|tokens| {
        tokens[0].utf8_text(source.as_bytes()).ok() == Some("annotation")
            && tokens[1].utf8_text(source.as_bytes()).ok() == Some("class")
            && matches!(
                tokens[2].kind(),
                "simple_identifier" | "identifier" | "type_identifier"
            )
    });
    let name = matches.next()?.get(2).copied()?;
    matches.next().is_none().then_some(name)
}

pub(crate) fn recovered_annotation_class_name(
    node: tree_sitter::Node<'_>,
    source: &str,
) -> Option<String> {
    recovered_annotation_class_name_node(node, source)?
        .utf8_text(source.as_bytes())
        .ok()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Recover an annotated interface only when the prefix parse establishes the
/// same top-level declaration boundaries. Both planning and emission use this
/// syntax evidence; an opaque or malformed boundary still remains Kotlin.
pub(crate) fn recover_annotated_interface_tree(
    source: &str,
    wrapper: tree_sitter::Node<'_>,
) -> Option<tree_sitter::Tree> {
    if wrapper.kind() != "annotated_expression" {
        return None;
    }
    let prefix = source.get(..wrapper.end_byte())?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(prefix, None)?;
    recovered_annotated_interface_decl(source, wrapper, tree.root_node())?;
    Some(tree)
}

pub(crate) fn recovered_annotated_interface_decl<'tree>(
    source: &str,
    wrapper: tree_sitter::Node<'_>,
    root: tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    let name = recovered_interface_name(wrapper, source)?;
    let mut cursor = root.walk();
    let mut declarations = root.named_children(&mut cursor).filter(|candidate| {
        candidate.kind() == "class_declaration"
            && !candidate.has_error()
            && !candidate.is_missing()
            && candidate.start_byte() == wrapper.start_byte()
            && candidate.end_byte() == wrapper.end_byte()
            && candidate
                .children(&mut candidate.walk())
                .any(|child| child.kind() == "interface")
            && candidate
                .child_by_field_name("name")
                .and_then(|node| node.utf8_text(source.as_bytes()).ok())
                == Some(name.as_str())
    });
    let declaration = declarations.next()?;
    declarations.next().is_none().then_some(declaration)
}

fn parameter_signature(source: &str, node: tree_sitter::Node) -> String {
    let raw = node.utf8_text(source.as_bytes()).unwrap_or("");
    let mut ty = node
        .child_by_field_name("type")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(str::to_owned);
    if ty.is_none() && node.kind() == "spread_parameter" {
        let mut walk = node.walk();
        ty = node.named_children(&mut walk).find_map(|child| {
            child
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(str::to_owned)
        });
    }
    let normalized = ty
        .or_else(|| parameter_type(raw).map(str::to_owned))
        .map(|s| normalize_signature_type(&s))
        .unwrap_or_default();
    if raw.split_whitespace().any(|w| w == "vararg") || raw.contains("...") {
        format!("vararg:{normalized}")
    } else {
        normalized
    }
}
fn owner_segment(source: &str, node: tree_sitter::Node) -> String {
    let name = if matches!(
        node.kind(),
        "primary_constructor" | "secondary_constructor" | "constructor_declaration"
    ) {
        "<init>".to_owned()
    } else {
        node.child_by_field_name("name")
            .and_then(|n| n.utf8_text(source.as_bytes()).ok())
            .map(str::to_owned)
            .or_else(|| node_name(node, source))
            .unwrap_or_default()
    };
    let callable = matches!(
        node.kind(),
        "function_declaration"
            | "method_declaration"
            | "primary_constructor"
            | "secondary_constructor"
            | "constructor_declaration"
    );
    if !callable {
        return name;
    }
    let mut walk = node.walk();
    let named: Vec<_> = node.named_children(&mut walk).collect();
    let params_node = node.child_by_field_name("parameters").or_else(|| {
        named.iter().copied().find(|n| {
            matches!(
                n.kind(),
                "function_value_parameters" | "formal_parameters" | "class_parameters"
            )
        })
    });
    let params = params_node
        .map(|p| {
            let mut c = p.walk();
            p.named_children(&mut c)
                .filter(|n| {
                    matches!(
                        n.kind(),
                        "parameter" | "class_parameter" | "formal_parameter" | "spread_parameter"
                    )
                })
                .map(|n| parameter_signature(source, n))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let receiver = callable_receiver(source, node)
        .map(|r| format!("{r}."))
        .unwrap_or_default();
    format!("{receiver}{name}({params})")
}
fn callable_receiver(source: &str, node: tree_sitter::Node) -> Option<String> {
    if node.kind() != "function_declaration" {
        return None;
    }
    let name_start = node.child_by_field_name("name")?.start_byte();
    let mut walk = node.walk();
    node.named_children(&mut walk)
        .filter(|c| {
            c.end_byte() <= name_start
                && c.kind().contains("type")
                && !c.kind().contains("parameter")
        })
        .last()
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(normalize_signature_type)
}
fn declaration_type_node(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    match node.kind() {
        "method_declaration" => node
            .child_by_field_name("type")
            .or_else(|| named_type_child(node)),
        "function_declaration" => node.child_by_field_name("return_type").or_else(|| {
            let mut walk = node.walk();
            let children: Vec<_> = node.named_children(&mut walk).collect();
            let parameters_end = children
                .iter()
                .find(|n| matches!(n.kind(), "function_value_parameters" | "formal_parameters"))
                .map(|n| n.end_byte());
            children.into_iter().find(|child| {
                child.kind().contains("type")
                    && parameters_end.is_some_and(|end| child.start_byte() > end)
            })
        }),
        "property_declaration" => {
            let mut walk = node.walk();
            let variable = node
                .named_children(&mut walk)
                .find(|n| n.kind() == "variable_declaration")?;
            variable
                .child_by_field_name("type")
                .or_else(|| named_type_child(variable))
        }
        "type_alias" => named_type_child(node),
        _ => None,
    }
}
fn named_type_child(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut walk = node.walk();
    node.named_children(&mut walk).find(|n| {
        matches!(
            n.kind(),
            "user_type"
                | "nullable_type"
                | "function_type"
                | "suspend_function_type"
                | "parenthesized_type"
                | "type_reference"
                | "generic_type"
                | "array_type"
                | "integral_type"
                | "floating_point_type"
                | "boolean_type"
                | "void_type"
                | "type_identifier"
                | "scoped_type_identifier"
        )
    })
}
fn declaration_type_parameters(source: &str, node: tree_sitter::Node) -> Vec<String> {
    let mut names = Vec::new();
    let mut scope = Some(node);
    while let Some(current) = scope {
        let params = current.child_by_field_name("type_parameters").or_else(|| {
            let mut w = current.walk();
            current
                .named_children(&mut w)
                .find(|n| n.kind() == "type_parameters")
        });
        if let Some(params) = params {
            let mut walk = params.walk();
            for param in params
                .named_children(&mut walk)
                .filter(|n| n.kind() == "type_parameter")
            {
                if let Some(name) = type_parameter_name(param, source)
                    && !names.contains(&name)
                {
                    names.push(name)
                }
            }
        }
        scope = current.parent();
    }
    names
}

fn own_type_parameters(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    node.child_by_field_name("type_parameters").or_else(|| {
        node.named_children(&mut node.walk())
            .find(|child| child.kind() == "type_parameters")
    })
}

fn type_parameter_name(node: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    node_name(node, source).or_else(|| {
        node.named_children(&mut node.walk())
            .find(|child| child.kind() == "type_identifier")
            .and_then(|name| name.utf8_text(source.as_bytes()).ok())
            .map(str::to_owned)
    })
}

fn has_type_constraints(node: tree_sitter::Node<'_>) -> bool {
    node.named_children(&mut node.walk())
        .any(|child| child.kind() == "type_constraints")
}

pub(crate) fn is_kotlin_value_class(source: &str, node: tree_sitter::Node<'_>) -> bool {
    if node.kind() != "class_declaration" {
        return false;
    }
    declaration_has_modifier(source, node, &["value", "inline"])
}

fn declaration_has_modifier(source: &str, node: tree_sitter::Node<'_>, wanted: &[&str]) -> bool {
    let mut modifiers = node
        .named_children(&mut node.walk())
        .filter(|child| child.kind() == "modifiers")
        .collect::<Vec<_>>();
    while let Some(modifier) = modifiers.pop() {
        if modifier
            .utf8_text(source.as_bytes())
            .is_ok_and(|text| wanted.contains(&text))
        {
            return true;
        }
        // Annotation arguments are not class modifiers.
        if !matches!(modifier.kind(), "annotation" | "annotation_entry") {
            modifiers.extend(modifier.children(&mut modifier.walk()));
        }
    }
    false
}

fn public_kotlin_callable(source: &str, node: tree_sitter::Node<'_>) -> bool {
    let mut stack = node
        .named_children(&mut node.walk())
        .filter(|child| child.kind() == "modifiers")
        .collect::<Vec<_>>();
    while let Some(modifier) = stack.pop() {
        let text = modifier.utf8_text(source.as_bytes()).unwrap_or("").trim();
        if matches!(
            text,
            "private"
                | "protected"
                | "internal"
                | "suspend"
                | "inline"
                | "external"
                | "expect"
                | "actual"
        ) {
            return false;
        }
        if matches!(modifier.kind(), "annotation" | "annotation_entry") {
            let annotation = text
                .trim_start_matches('@')
                .split(|character: char| character == '(' || character.is_whitespace())
                .next()
                .unwrap_or("");
            let changes_name = |name: &str| {
                matches!(
                    name.rsplit('.').next(),
                    Some("JvmName" | "JvmStatic" | "JvmSynthetic")
                )
            };
            if changes_name(annotation)
                || kotlin_import_lines(source).any(|import| {
                    import.split_once(" as ").is_some_and(|(path, alias)| {
                        alias.trim() == annotation && changes_name(path.trim())
                    })
                })
            {
                return false;
            }
            continue;
        }
        stack.extend(modifier.named_children(&mut modifier.walk()));
    }
    true
}

fn plain_kotlin_property_getter(source: &str, node: tree_sitter::Node<'_>) -> bool {
    readable_kotlin_property_getter(source, node, false) && !has_record_property_owner(source, node)
}

fn has_record_property_owner(source: &str, node: tree_sitter::Node<'_>) -> bool {
    // Kotlin records expose component-name accessors, not the ordinary getX
    // ABI. Check the existing tree; no additional parse is needed per getter.
    if !source.contains("JvmRecord") {
        return false;
    }
    let mut ancestor = node.parent();
    while let Some(owner) = ancestor {
        if matches!(
            owner.kind(),
            "class_declaration" | "object_declaration" | "interface_declaration"
        ) {
            return owner
                .named_children(&mut owner.walk())
                .filter(|child| child.kind() == "modifiers")
                .flat_map(|modifiers| {
                    modifiers
                        .named_children(&mut modifiers.walk())
                        .collect::<Vec<_>>()
                })
                .filter(|child| matches!(child.kind(), "annotation" | "annotation_entry"))
                .filter_map(|annotation| annotation.utf8_text(source.as_bytes()).ok())
                .any(|annotation| {
                    let name = annotation
                        .trim_start_matches('@')
                        .split(|ch: char| ch == '(' || ch.is_whitespace())
                        .next()
                        .unwrap_or("");
                    matches!(name, "JvmRecord" | "kotlin.jvm.JvmRecord")
                        || kotlin_import_lines(source).any(|import| {
                            import.split_once(" as ").is_some_and(|(path, alias)| {
                                path.trim() == "kotlin.jvm.JvmRecord" && alias.trim() == name
                            })
                        })
                });
        }
        ancestor = owner.parent();
    }
    false
}

fn enum_entry_names(source: &str, declaration: tree_sitter::Node<'_>) -> Option<Vec<String>> {
    if declaration.has_error() {
        return None;
    }
    let body = declaration
        .named_children(&mut declaration.walk())
        .find(|child| matches!(child.kind(), "enum_class_body" | "class_body" | "enum_body"))?;
    let mut names = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in body
        .named_children(&mut body.walk())
        .filter(|child| matches!(child.kind(), "enum_entry" | "enum_constant"))
    {
        let name = node_name(entry, source)?;
        if !seen.insert(name.clone()) {
            return None;
        }
        names.push(name);
    }
    (!names.is_empty()).then_some(names)
}

fn ordinary_java_field_lookup(declaration: tree_sitter::Node<'_>) -> bool {
    fn annotated(node: tree_sitter::Node<'_>) -> bool {
        let mut pending = vec![node];
        while let Some(part) = pending.pop() {
            if matches!(part.kind(), "annotation" | "marker_annotation") {
                return true;
            }
            pending.extend(part.named_children(&mut part.walk()));
        }
        false
    }
    if declaration.has_error() {
        return false;
    }
    for part in declaration.named_children(&mut declaration.walk()) {
        if part.kind() == "modifiers" && annotated(part) {
            return false;
        }
        if matches!(part.kind(), "class_body" | "interface_body" | "enum_body") {
            for member in part.named_children(&mut part.walk()) {
                if matches!(member.kind(), "field_declaration" | "constant_declaration")
                    && member
                        .named_children(&mut member.walk())
                        .any(|child| child.kind() == "modifiers" && annotated(child))
                {
                    return false;
                }
            }
        }
    }
    true
}

/// Record snapshot-bound lowering decisions before Java preparation. Emitters
/// consume these decisions; unsupported `when` forms keep the existing rules.
pub fn plan_enum_when_lowerings(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    assert_eq!(
        plan.source_hash,
        *blake3::hash(source.as_bytes()).as_bytes(),
        "when planning requires the current source snapshot"
    );
    let owners = DeclarationOwnerIndex::new(plan);
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "when_expression"
            && let Some(index) = owners.owner(node)
            && let Some(lowering) = enum_when::establish(source, node, file, provider)
        {
            plan.require_symbol(
                index,
                lowering.enum_owner.name.clone(),
                FactStatus::Established(lowering.enum_owner.clone()),
            );
            for label in lowering.arms.iter().flat_map(|arm| &arm.labels) {
                plan.require_symbol(
                    index,
                    label.name.clone(),
                    FactStatus::Established(label.clone()),
                );
            }
            plan.enum_when_lowerings.push(lowering);
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
}

/// Plan exact stdlib TODO throws before Java preparation.
pub fn plan_todo_lowerings(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    assert_eq!(
        plan.source_hash,
        *blake3::hash(source.as_bytes()).as_bytes()
    );
    let owners = DeclarationOwnerIndex::new(plan);
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression"
            && owners.owner(node).is_some()
            && let Some(lowering) = todo::establish(source, node, file, provider)
        {
            plan.todo_lowerings.push(lowering);
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
}

/// Plan source-checked reference-string calls and their indexed dependencies.
pub fn plan_reference_string_calls(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    assert_eq!(
        plan.source_hash,
        *blake3::hash(source.as_bytes()).as_bytes()
    );
    let owners = DeclarationOwnerIndex::new(plan);
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression"
            && let Some(index) = owners.owner(node)
            && let Some(fact) = object_calls::establish(source, file, node, provider)
        {
            let dependency = match &fact.receiver {
                ReferenceStringReceiver::Indexed(classifier) => Some(classifier),
                ReferenceStringReceiver::Returned { producer, .. } => Some(producer),
                ReferenceStringReceiver::Standard { .. } => None,
            };
            if let Some(dependency) = dependency {
                plan.require_symbol(
                    index,
                    dependency.name.clone(),
                    FactStatus::Established(dependency.clone()),
                );
            }
            plan.reference_string_calls.push(fact);
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
}

/// Record canonical container calls and source-proven, scoped payload facts.
pub fn plan_container_calls(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    assert_eq!(
        plan.source_hash,
        *blake3::hash(source.as_bytes()).as_bytes()
    );
    let owners = DeclarationOwnerIndex::new(plan);
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression"
            && let Some(index) = owners.owner(node)
            && let Some(mut fact) = container_calls::establish(source, file, node, provider)
        {
            fact.owner = Some(plan.declarations[index].symbol_id.clone());
            for dependency in &fact.dependencies {
                plan.require_symbol(
                    index,
                    dependency.name.clone(),
                    FactStatus::Established(dependency.clone()),
                );
            }
            plan.container_calls.push(fact);
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
}

/// Record exact eager Kotlin `map` and `filterNot` extension calls before
/// Java preparation. Unsupported or shadowed calls remain unknown.
pub fn plan_eager_collection_calls(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    assert_eq!(
        plan.source_hash,
        *blake3::hash(source.as_bytes()).as_bytes(),
        "eager collection planning requires the current source snapshot"
    );
    if provider.source_for_file(file) != Some(source) {
        return;
    }
    if !source.contains("map") && !source.contains("filterNot") {
        return;
    }
    let root = tree.root_node();
    let mut calls = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression"
            && call_callee_node(node)
                .and_then(|callee| callee.utf8_text(source.as_bytes()).ok())
                .is_some_and(|name| matches!(name.trim(), "map" | "filterNot"))
        {
            calls.push(node);
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    if calls.is_empty() {
        return;
    }
    let owners = DeclarationOwnerIndex::new(plan);
    let import_lines = eager_collections::imports(source, tree.root_node());
    let extension_symbols = ["map", "filterNot"]
        .into_iter()
        .flat_map(|name| provider.symbols_named(name))
        .filter(|symbol| {
            symbol.id.receiver.is_some() && matches!(symbol.id.kind.as_str(), "function" | "method")
        })
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    let snapshot_hash = plan.source_hash;
    let context = eager_collections::PlanningContext {
        source,
        file,
        root,
        imports: &import_lines,
        extension_symbols: &extension_symbols,
        snapshot_hash,
        provider,
    };
    // Inner eager calls end before their parents, so facts are available for
    // chained receiver classification without guessing from source suffixes.
    calls.sort_by_key(|node| (node.end_byte(), node.start_byte()));
    for call in calls {
        let Some(owner_index) = owners.owner(call) else {
            continue;
        };
        if let Some(fact) = eager_collections::establish(
            &context,
            call,
            &plan.eager_collection_calls,
            plan.declarations[owner_index].symbol_id.clone(),
        ) {
            if let Some(projection) = &fact.property_projection {
                plan.require_symbol(
                    owner_index,
                    projection.target.name.clone(),
                    FactStatus::Established(projection.target.clone()),
                );
                if let Some(getter_target) = &projection.getter_target {
                    plan.require_symbol(
                        owner_index,
                        getter_target.name.clone(),
                        FactStatus::Established(getter_target.clone()),
                    );
                }
                for dependency in &projection.dependencies {
                    plan.require_symbol(
                        owner_index,
                        dependency.name.clone(),
                        FactStatus::Established(dependency.clone()),
                    );
                }
            }
            plan.eager_collection_calls.push(fact);
        }
    }
}

/// Record exact class-token forwarders before Java emission.
pub fn plan_reified_class_calls(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    assert_eq!(
        plan.source_hash,
        *blake3::hash(source.as_bytes()).as_bytes()
    );
    let owners = DeclarationOwnerIndex::new(plan);
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression"
            && let Some(index) = owners.owner(node)
            && let Some(fact) = reified_calls::establish(source, file, node, provider)
        {
            for symbol in [&fact.source_target, &fact.java_target, &fact.type_argument] {
                plan.require_symbol(
                    index,
                    symbol.name.clone(),
                    FactStatus::Established(symbol.clone()),
                );
            }
            for symbol in &fact.receiver_dependencies {
                plan.require_symbol(
                    index,
                    symbol.name.clone(),
                    FactStatus::Established(symbol.clone()),
                );
            }
            plan.reified_class_calls.push(fact);
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
}

/// Bind constructor chains before emission; uncertain overloads stay Kotlin.
pub fn plan_constructor_delegations(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    assert_eq!(
        plan.source_hash,
        *blake3::hash(source.as_bytes()).as_bytes()
    );
    constructor_delegation::plan(source, tree, file, provider, plan);
}

/// Preserve Kotlin bottom-return metadata and calls that depend on it.
pub fn plan_bottom_return_losses(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
    allow_approximations: bool,
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    bottom_returns::plan(source, tree, file, provider, allow_approximations, plan);
}

// An interface-typed invocation uses the interface's ABI, independently of
// annotations on its implementation. This fact proves source visibility only.
fn readable_kotlin_property_getter(
    source: &str,
    node: tree_sitter::Node<'_>,
    allow_annotations: bool,
) -> bool {
    let mut stack = node
        .named_children(&mut node.walk())
        .filter(|child| {
            matches!(
                child.kind(),
                "modifiers"
                    | "parameter_modifiers"
                    | "getter"
                    | "setter"
                    | "annotation"
                    | "annotation_entry"
            )
        })
        .collect::<Vec<_>>();
    while let Some(part) = stack.pop() {
        // Annotation names may be imported through aliases. Until their
        // accessor effects are established, no annotated property proves an
        // ordinary JVM getter (in particular JvmName/JvmField/JvmSynthetic).
        if matches!(part.kind(), "annotation" | "annotation_entry") {
            if allow_annotations {
                continue;
            }
            return false;
        }
        if matches!(
            part.utf8_text(source.as_bytes()).unwrap_or("").trim(),
            "private" | "protected" | "internal" | "external" | "expect" | "actual" | "inline"
        ) {
            return false;
        }
        if !matches!(part.kind(), "function_body" | "block") {
            stack.extend(part.children(&mut part.walk()));
        }
    }
    true
}

/// Extract a class-scoped upper bound without confusing a method's own type
/// parameter with a same-spelled class parameter. These facts are syntax-only;
/// bound resolution waits until the complete provider has been constructed.
fn returned_parameter_declaration<'tree>(
    source: &str,
    member: tree_sitter::Node<'tree>,
    return_type: &TypeRef,
) -> Option<(tree_sitter::Node<'tree>, tree_sitter::Node<'tree>)> {
    let TypeRef {
        ty: KotlinType::TypeParameter(parameter),
        nullable: false,
    } = return_type
    else {
        return None;
    };
    if has_type_constraints(member)
        || own_type_parameters(member).is_some_and(|parameters| {
            parameters
                .named_children(&mut parameters.walk())
                .any(|node| type_parameter_name(node, source).as_ref() == Some(parameter))
        })
    {
        return None;
    }
    let mut ancestor = member.parent();
    let owner = loop {
        let node = ancestor?;
        match node.kind() {
            "class_declaration" | "interface_declaration" => break node,
            "function_declaration"
            | "method_declaration"
            | "companion_object"
            | "object_declaration"
            | "object_literal"
            | "anonymous_object"
            | "lambda_literal" => return None,
            _ => ancestor = node.parent(),
        }
    };
    if has_type_constraints(owner) {
        return None;
    }
    let parameters = own_type_parameters(owner)?;
    let matches = parameters
        .named_children(&mut parameters.walk())
        .filter(|node| node.kind() == "type_parameter")
        .filter(|node| type_parameter_name(*node, source).as_ref() == Some(parameter))
        .collect::<Vec<_>>();
    let [parameter_node] = matches.as_slice() else {
        return None;
    };
    Some((owner, *parameter_node))
}

fn returned_parameter_bound(
    source: &str,
    member: tree_sitter::Node<'_>,
    return_type: &TypeRef,
) -> Option<TypeRef> {
    let (owner, parameter_node) = returned_parameter_declaration(source, member, return_type)?;
    let bound_node = if member.kind() == "method_declaration" {
        if parameter_node
            .named_children(&mut parameter_node.walk())
            .any(|child| matches!(child.kind(), "annotation" | "marker_annotation"))
        {
            return None;
        }
        let bounds = parameter_node
            .named_children(&mut parameter_node.walk())
            .find(|child| child.kind() == "type_bound")?;
        let types = bounds
            .named_children(&mut bounds.walk())
            .filter(|child| !child.kind().contains("comment"))
            .collect::<Vec<_>>();
        let [bound] = types.as_slice() else {
            return None;
        };
        if !matches!(
            bound.kind(),
            "type_identifier" | "scoped_type_identifier" | "generic_type"
        ) {
            return None;
        }
        *bound
    } else {
        named_type_child(parameter_node)?
    };
    let bound = TypeRef::parse_with_type_parameters(
        bound_node.utf8_text(source.as_bytes()).ok()?,
        &declaration_type_parameters(source, owner),
    );
    match &bound {
        TypeRef {
            ty: KotlinType::Named { name, arguments },
            nullable: false,
        } if !name.is_empty()
            && arguments
                .iter()
                .all(|argument| argument.ty.as_ref().is_none_or(capability_bound_argument)) =>
        {
            Some(bound)
        }
        _ => None,
    }
}

fn capability_bound_argument(ty: &KotlinType) -> bool {
    match ty {
        KotlinType::Primitive(_) | KotlinType::TypeParameter(_) => true,
        KotlinType::Named { name, arguments } => {
            !name.is_empty()
                && name.chars().all(|character| {
                    character.is_alphanumeric() || matches!(character, '.' | '_' | '$')
                })
                && arguments
                    .iter()
                    .all(|argument| argument.ty.as_ref().is_none_or(capability_bound_argument))
        }
        KotlinType::Nullable(inner) | KotlinType::Array(inner) => capability_bound_argument(inner),
        KotlinType::Function { .. } | KotlinType::Unknown(_) => false,
    }
}

/// Stable symbol identity for an actual syntax declaration node.
pub fn symbol_id_for_node(source: &str, node: tree_sitter::Node, file: &Path) -> SymbolId {
    let text = node.utf8_text(source.as_bytes()).unwrap_or("");
    let name_node = node.child_by_field_name("name").or_else(|| {
        if node.kind() == "property_declaration" {
            let mut walk = node.walk();
            node.named_children(&mut walk)
                .find(|n| n.kind() == "variable_declaration")
                .and_then(|v| {
                    let mut w = v.walk();
                    v.named_children(&mut w)
                        .find(|c| matches!(c.kind(), "identifier" | "simple_identifier"))
                })
        } else {
            None
        }
    });
    let constructor = matches!(
        node.kind(),
        "primary_constructor" | "secondary_constructor" | "constructor_declaration"
    );
    let recovered_interface = recovered_interface_name(node, source);
    let is_recovered_interface = recovered_interface.is_some();
    let recovered_annotation = recovered_annotation_class_name(node, source);
    let is_recovered_annotation = recovered_annotation.is_some();
    let name = if constructor {
        "<init>".to_string()
    } else {
        recovered_interface
            .or(recovered_annotation)
            .unwrap_or_else(|| {
                name_node
                    .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                    .map(str::to_owned)
                    .or_else(|| node_name(node, source))
                    .unwrap_or_else(|| format!("<anonymous@{}>", node.start_byte()))
            })
    };
    let prefix = name_node
        .and_then(|n| source.get(node.start_byte()..n.start_byte()))
        .unwrap_or(text);
    let kind = match node.kind() {
        "function_declaration" | "method_declaration" => "function",
        "primary_constructor" | "secondary_constructor" | "constructor_declaration" => {
            "constructor"
        }
        "property_declaration" => "property",
        "object_declaration" => "object",
        "type_alias" => "typealias",
        "enum_declaration" => "enum",
        "interface_declaration" => "interface",
        "class_declaration" if prefix.split_whitespace().any(|w| w == "interface") => "interface",
        _ if is_recovered_interface => "interface",
        _ if is_recovered_annotation => "annotation",
        "class_declaration" if prefix.split_whitespace().any(|w| w == "enum") => "enum",
        "class_declaration" if prefix.split_whitespace().any(|w| w == "annotation") => "annotation",
        _ => "class",
    }
    .to_owned();
    let package = package_name(source);
    let mut owner_path = Vec::new();
    let mut parent = node.parent();
    while let Some(p) = parent {
        if declaration_node_kind(p.kind()) {
            owner_path.push(owner_segment(source, p))
        }
        parent = p.parent()
    }
    owner_path.reverse();
    let (receiver, parameters) = if kind == "function" || kind == "constructor" {
        let mut children = node.walk();
        let named: Vec<_> = node.named_children(&mut children).collect();
        let receiver = callable_receiver(source, node);
        let params_node = node.child_by_field_name("parameters").or_else(|| {
            named.iter().copied().find(|n| {
                matches!(
                    n.kind(),
                    "function_value_parameters" | "formal_parameters" | "class_parameters"
                )
            })
        });
        let params = params_node
            .map(|pnode| {
                let mut c = pnode.walk();
                pnode
                    .named_children(&mut c)
                    .filter(|n| {
                        matches!(
                            n.kind(),
                            "parameter"
                                | "class_parameter"
                                | "formal_parameter"
                                | "spread_parameter"
                        )
                    })
                    .map(|n| parameter_signature(source, n))
                    .collect()
            })
            .unwrap_or_default();
        (receiver, params)
    } else {
        (None, vec![])
    };
    SymbolId {
        module: String::new(),
        package,
        file: file.to_path_buf(),
        owner_path,
        kind,
        name,
        receiver,
        parameters,
    }
}
fn normalize_signature_type(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}
fn parameter_type(s: &str) -> Option<&str> {
    let colon = top_level_char(s, ':')?;
    let tail = &s[colon + 1..];
    let end = top_level_char(tail, '=').unwrap_or(tail.len());
    Some(tail[..end].trim())
}
fn top_level_char(s: &str, target: char) -> Option<usize> {
    let (mut depth, mut quote, mut escaped) = (0i32, false, false);
    for (i, c) in s.char_indices() {
        if quote {
            if escaped {
                escaped = false
            } else if c == '\\' {
                escaped = true
            } else if c == '"' {
                quote = false
            }
            continue;
        }
        if c == '"' {
            quote = true;
            continue;
        }
        match c {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        if c == target && depth == 0 {
            return Some(i);
        }
    }
    None
}
fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s.char_indices().filter(|(i, _)| *i >= open) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}
impl SymbolId {
    pub fn stable_key(&self) -> String {
        // Preserve field boundaries and escaping for legal identifiers/paths.
        serde_json::to_string(self).expect("SymbolId serialization is infallible")
    }
    pub fn generated(origin: &SymbolId, kind: &str) -> Self {
        Self {
            module: origin.module.clone(),
            package: origin.package.clone(),
            file: origin.file.clone(),
            owner_path: origin.owner_path.clone(),
            kind: format!("generated:{kind}"),
            name: format!("{}${kind}", origin.name),
            receiver: origin.receiver.clone(),
            parameters: origin.parameters.clone(),
        }
    }
}

/// Construct the same declaration identity from the workspace index used by
/// retention planning. The file path is taken from the owning indexed source.
pub fn workspace_symbol(
    index: &crate::workspace::SourceIndex,
    declaration: &crate::workspace::Declaration,
) -> SymbolId {
    index
        .cached_declaration_symbol(declaration)
        .unwrap_or_else(|| workspace_symbol_uncached(index, declaration))
}

pub(crate) fn workspace_symbol_uncached(
    index: &crate::workspace::SourceIndex,
    declaration: &crate::workspace::Declaration,
) -> SymbolId {
    if let Some(file) = index.declaration_source_file(declaration) {
        let mut parser = tree_sitter::Parser::new();
        let language = if file.language == crate::workspace::SourceLanguage::Kotlin {
            tree_sitter_kotlin_ng::LANGUAGE.into()
        } else {
            tree_sitter_java::LANGUAGE.into()
        };
        if parser.set_language(&language).is_ok()
            && let Some(tree) = parser.parse(file.source_text(), None)
        {
            let mut candidates = Vec::new();
            let mut stack = vec![tree.root_node()];
            while let Some(n) = stack.pop() {
                if declaration_node_kind(n.kind())
                    && node_name(n, file.source_text()).as_deref()
                        == Some(declaration.name.as_str())
                    && symbol_id_for_node(file.source_text(), n, &file.path).kind
                        == format!("{:?}", declaration.kind).to_lowercase()
                {
                    candidates.push(n)
                }
                let mut c = n.walk();
                stack.extend(n.named_children(&mut c));
            }
            candidates.sort_by_key(|n| n.start_byte());
            if candidates.len() == 1 {
                return symbol_id_for_node(file.source_text(), candidates[0], &file.path);
            }
            if candidates.len() > 1 {
                return SymbolId {
                    module: String::new(),
                    package: declaration.package.clone().unwrap_or_default(),
                    file: file.path.clone(),
                    owner_path: vec![],
                    kind: format!("ambiguous:{}", node_symbol_kind(candidates[0].kind())),
                    name: format!("<ambiguous:{}>", declaration.name),
                    receiver: None,
                    parameters: vec![],
                };
            }
        }
    }
    SymbolId {
        module: String::new(),
        package: declaration.package.clone().unwrap_or_default(),
        file: index
            .declaration_source_file(declaration)
            .map(|f| f.path.clone())
            .unwrap_or_default(),
        owner_path: vec![],
        kind: format!("{:?}", declaration.kind).to_lowercase(),
        name: declaration.name.clone(),
        receiver: None,
        parameters: vec![],
    }
}

/// Compute the same declaration identities as `workspace_symbol`, sharing one
/// parse and syntax walk across every declaration in a source file.
pub(crate) fn workspace_symbols_for_file(file: &crate::workspace::SourceFile) -> Vec<SymbolId> {
    let source = file.source_text();
    let wanted_names = file
        .declarations
        .iter()
        .map(|declaration| declaration.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut candidates = Vec::<(usize, Option<String>, &'static str, SymbolId)>::new();
    let mut parser = tree_sitter::Parser::new();
    let language = if file.language == crate::workspace::SourceLanguage::Kotlin {
        tree_sitter_kotlin_ng::LANGUAGE.into()
    } else {
        tree_sitter_java::LANGUAGE.into()
    };
    if parser.set_language(&language).is_ok()
        && let Some(tree) = parser.parse(source, None)
    {
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if declaration_node_kind(node.kind())
                && let Some(name) = node_name(node, source)
                && wanted_names.contains(name.as_str())
            {
                candidates.push((
                    node.start_byte(),
                    Some(name),
                    node_symbol_kind(node.kind()),
                    symbol_id_for_node(source, node, &file.path),
                ));
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        candidates.sort_by_key(|(start, _, _, _)| *start);
    }

    file.declarations
        .iter()
        .map(|declaration| {
            let expected_kind = format!("{:?}", declaration.kind).to_lowercase();
            let matches = candidates
                .iter()
                .filter(|(_, candidate_name, _, symbol)| {
                    candidate_name.as_deref() == Some(declaration.name.as_str())
                        && symbol.kind == expected_kind
                })
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [(_, _, _, symbol)] => symbol.clone(),
                [(_, _, node_kind, _), ..] => SymbolId {
                    module: String::new(),
                    package: declaration.package.clone().unwrap_or_default(),
                    file: file.path.clone(),
                    owner_path: vec![],
                    kind: format!("ambiguous:{node_kind}"),
                    name: format!("<ambiguous:{}>", declaration.name),
                    receiver: None,
                    parameters: vec![],
                },
                [] => SymbolId {
                    module: String::new(),
                    package: declaration.package.clone().unwrap_or_default(),
                    file: file.path.clone(),
                    owner_path: vec![],
                    kind: expected_kind,
                    name: declaration.name.clone(),
                    receiver: None,
                    parameters: vec![],
                },
            }
        })
        .collect()
}

pub fn workspace_symbol_in_module(
    index: &crate::workspace::SourceIndex,
    declaration: &crate::workspace::Declaration,
    module: &str,
) -> SymbolId {
    let mut id = workspace_symbol(index, declaration);
    id.module = module.to_owned();
    id
}
fn node_symbol_kind(kind: &str) -> &'static str {
    match kind {
        "function_declaration" | "method_declaration" => "function",
        "property_declaration" => "property",
        "object_declaration" => "object",
        "type_alias" => "typealias",
        "enum_declaration" => "enum",
        "interface_declaration" => "interface",
        "record_declaration" => "record",
        _ => "class",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLocation {
    pub file: PathBuf,
    pub snapshot_hash: [u8; 32],
    pub start_byte: usize,
    pub end_byte: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginMap {
    pub generated: SymbolId,
    pub origin: SymbolId,
    pub reason: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticDiagnostic {
    pub code: String,
    pub message: String,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Variance {
    Invariant,
    In,
    Out,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KotlinType {
    Primitive(String),
    Named {
        name: String,
        arguments: Vec<TypeArgument>,
    },
    TypeParameter(String),
    Nullable(Box<KotlinType>),
    Array(Box<KotlinType>),
    Function {
        receiver: Option<Box<KotlinType>>,
        parameters: Vec<KotlinType>,
        result: Box<KotlinType>,
        suspend: bool,
    },
    Unknown(String),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeArgument {
    pub variance: Variance,
    pub ty: Option<KotlinType>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeRef {
    pub ty: KotlinType,
    pub nullable: bool,
}
impl TypeRef {
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        let nullable = s.ends_with('?');
        let core = s.strip_suffix('?').unwrap_or(s).trim();
        Self {
            ty: parse_type(core),
            nullable,
        }
    }
    pub fn from_java_name(s: &str) -> Self {
        let value = s.trim();
        if let Some(base) = value.strip_suffix("[]") {
            return Self {
                ty: KotlinType::Array(Box::new(Self::from_java_name(base).ty)),
                nullable: false,
            };
        }
        let primitive = match value {
            "boolean" => Some("Boolean"),
            "byte" => Some("Byte"),
            "short" => Some("Short"),
            "int" => Some("Int"),
            "long" => Some("Long"),
            "char" => Some("Char"),
            "float" => Some("Float"),
            "double" => Some("Double"),
            _ => None,
        };
        if let Some(name) = primitive {
            return Self {
                ty: KotlinType::Primitive(name.into()),
                nullable: false,
            };
        }
        let canonical = match value {
            "java.lang.String" => "String",
            "java.lang.Boolean" => "Boolean",
            "java.lang.Byte" => "Byte",
            "java.lang.Short" => "Short",
            "java.lang.Integer" => "Int",
            "java.lang.Long" => "Long",
            "java.lang.Character" => "Char",
            "java.lang.Float" => "Float",
            "java.lang.Double" => "Double",
            _ => value,
        };
        Self::parse(canonical)
    }
    pub fn parse_with_type_parameters(s: &str, parameters: &[String]) -> Self {
        let mut t = Self::parse(s);
        t.ty = classify_parameters(t.ty, parameters);
        t
    }
    pub fn substitute(&self, map: &BTreeMap<String, TypeRef>) -> Self {
        if let KotlinType::TypeParameter(name) = &self.ty
            && let Some(value) = map.get(name)
        {
            let mut value = value.clone();
            value.nullable |= self.nullable;
            return value;
        }
        Self {
            ty: substitute_type(&self.ty, map),
            nullable: self.nullable,
        }
    }
    pub fn expand_aliases(&self, aliases: &BTreeMap<String, TypeRef>) -> Self {
        if let KotlinType::Named { name, arguments } = &self.ty
            && arguments.is_empty()
            && let Some(alias) = aliases.get(name)
        {
            let mut alias = alias.clone();
            alias.nullable |= self.nullable;
            return alias;
        }
        Self {
            ty: expand_alias_type(&self.ty, aliases),
            nullable: self.nullable,
        }
    }
    pub fn expand_alias_definitions(
        &self,
        aliases: &BTreeMap<String, TypeAliasDefinition>,
    ) -> Self {
        let mut result = self.clone();
        result.ty = expand_alias_definition_type(&self.ty, aliases, &mut Vec::new());
        result
    }
}
fn expand_alias_definition_type(
    ty: &KotlinType,
    aliases: &BTreeMap<String, TypeAliasDefinition>,
    stack: &mut Vec<String>,
) -> KotlinType {
    if let KotlinType::Named { name, arguments } = ty
        && let Some(alias) = aliases
            .get(name)
            .filter(|a| a.parameters.len() == arguments.len())
    {
        if stack.contains(name) {
            return KotlinType::Unknown(format!("cyclic type alias {name}"));
        }
        stack.push(name.clone());
        let substitutions: BTreeMap<_, _> = alias
            .parameters
            .iter()
            .zip(arguments)
            .filter_map(|(param, arg)| {
                arg.ty.as_ref().map(|ty| {
                    (
                        param.clone(),
                        TypeRef {
                            ty: ty.clone(),
                            nullable: false,
                        },
                    )
                })
            })
            .collect();
        let substituted = alias.target.substitute(&substitutions);
        let expanded = expand_alias_definition_type(&substituted.ty, aliases, stack);
        stack.pop();
        return if substituted.nullable {
            KotlinType::Nullable(Box::new(expanded))
        } else {
            expanded
        };
    }
    match ty {
        KotlinType::Named { name, arguments } => KotlinType::Named {
            name: name.clone(),
            arguments: arguments
                .iter()
                .map(|a| TypeArgument {
                    variance: a.variance.clone(),
                    ty: a
                        .ty
                        .as_ref()
                        .map(|t| expand_alias_definition_type(t, aliases, stack)),
                })
                .collect(),
        },
        KotlinType::Nullable(t) => {
            KotlinType::Nullable(Box::new(expand_alias_definition_type(t, aliases, stack)))
        }
        KotlinType::Array(t) => {
            KotlinType::Array(Box::new(expand_alias_definition_type(t, aliases, stack)))
        }
        KotlinType::Function {
            receiver,
            parameters,
            result,
            suspend,
        } => KotlinType::Function {
            receiver: receiver
                .as_ref()
                .map(|t| Box::new(expand_alias_definition_type(t, aliases, stack))),
            parameters: parameters
                .iter()
                .map(|t| expand_alias_definition_type(t, aliases, stack))
                .collect(),
            result: Box::new(expand_alias_definition_type(result, aliases, stack)),
            suspend: *suspend,
        },
        other => other.clone(),
    }
}
fn classify_parameters(t: KotlinType, parameters: &[String]) -> KotlinType {
    match t {
        KotlinType::Named { name, .. } if parameters.contains(&name) => {
            KotlinType::TypeParameter(name)
        }
        KotlinType::Named { name, arguments } => KotlinType::Named {
            name,
            arguments: arguments
                .into_iter()
                .map(|a| TypeArgument {
                    variance: a.variance,
                    ty: a.ty.map(|t| classify_parameters(t, parameters)),
                })
                .collect(),
        },
        KotlinType::Nullable(t) => {
            KotlinType::Nullable(Box::new(classify_parameters(*t, parameters)))
        }
        KotlinType::Array(t) => KotlinType::Array(Box::new(classify_parameters(*t, parameters))),
        KotlinType::Function {
            receiver,
            parameters: p,
            result,
            suspend,
        } => KotlinType::Function {
            receiver: receiver.map(|t| Box::new(classify_parameters(*t, parameters))),
            parameters: p
                .into_iter()
                .map(|t| classify_parameters(t, parameters))
                .collect(),
            result: Box::new(classify_parameters(*result, parameters)),
            suspend,
        },
        x => x,
    }
}
fn substitute_type(t: &KotlinType, map: &BTreeMap<String, TypeRef>) -> KotlinType {
    match t {
        KotlinType::TypeParameter(n) => map
            .get(n)
            .map(|x| {
                if x.nullable {
                    KotlinType::Nullable(Box::new(x.ty.clone()))
                } else {
                    x.ty.clone()
                }
            })
            .unwrap_or_else(|| t.clone()),
        KotlinType::Named { name, arguments } => KotlinType::Named {
            name: name.clone(),
            arguments: arguments
                .iter()
                .map(|a| TypeArgument {
                    variance: a.variance.clone(),
                    ty: a.ty.as_ref().map(|t| substitute_type(t, map)),
                })
                .collect(),
        },
        KotlinType::Nullable(t) => KotlinType::Nullable(Box::new(substitute_type(t, map))),
        KotlinType::Array(t) => KotlinType::Array(Box::new(substitute_type(t, map))),
        KotlinType::Function {
            receiver,
            parameters,
            result,
            suspend,
        } => KotlinType::Function {
            receiver: receiver.as_ref().map(|t| Box::new(substitute_type(t, map))),
            parameters: parameters.iter().map(|t| substitute_type(t, map)).collect(),
            result: Box::new(substitute_type(result, map)),
            suspend: *suspend,
        },
        x => x.clone(),
    }
}
fn expand_alias_type(t: &KotlinType, aliases: &BTreeMap<String, TypeRef>) -> KotlinType {
    match t {
        KotlinType::Named { name, arguments } if arguments.is_empty() => aliases
            .get(name)
            .map(|x| {
                if x.nullable {
                    KotlinType::Nullable(Box::new(x.ty.clone()))
                } else {
                    x.ty.clone()
                }
            })
            .unwrap_or_else(|| t.clone()),
        KotlinType::Named { name, arguments } => KotlinType::Named {
            name: name.clone(),
            arguments: arguments
                .iter()
                .map(|a| TypeArgument {
                    variance: a.variance.clone(),
                    ty: a.ty.as_ref().map(|t| expand_alias_type(t, aliases)),
                })
                .collect(),
        },
        KotlinType::Nullable(t) => KotlinType::Nullable(Box::new(expand_alias_type(t, aliases))),
        KotlinType::Array(t) => KotlinType::Array(Box::new(expand_alias_type(t, aliases))),
        KotlinType::Function {
            receiver,
            parameters,
            result,
            suspend,
        } => KotlinType::Function {
            receiver: receiver
                .as_ref()
                .map(|t| Box::new(expand_alias_type(t, aliases))),
            parameters: parameters
                .iter()
                .map(|t| expand_alias_type(t, aliases))
                .collect(),
            result: Box::new(expand_alias_type(result, aliases)),
            suspend: *suspend,
        },
        x => x.clone(),
    }
}
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let (mut depth, mut start) = (0i32, 0usize);
    let mut out = Vec::new();
    for (i, c) in s.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            _ => {}
        }
        if c == sep && depth == 0 {
            out.push(&s[start..i]);
            start = i + 1;
        }
    }
    out.push(&s[start..]);
    out
}
fn parse_type(s: &str) -> KotlinType {
    let s = s.trim();
    if s.is_empty() {
        return KotlinType::Unknown(s.into());
    }
    if let Some(inner) = s.strip_suffix('?') {
        return KotlinType::Nullable(Box::new(parse_type(inner)));
    }
    if s.starts_with('(') && matching_paren(s, 0) == Some(s.len() - 1) {
        let inner = &s[1..s.len() - 1];
        if top_level_arrow(inner).is_some() {
            return parse_type(inner);
        }
    }
    let suspend = s.starts_with("suspend ");
    let s = s.strip_prefix("suspend ").unwrap_or(s).trim();
    if let Some(i) = top_level_arrow(s) {
        let left = s[..i].trim();
        let result = Box::new(parse_type(s[i + 2..].trim()));
        let (receiver, params) = if let Some(open) = left.find('(') {
            let prefix = left[..open].trim();
            let close = matching_paren(left, open).unwrap_or(left.len());
            if let Some((r, _)) = prefix.rsplit_once('.') {
                (Some(Box::new(parse_type(r))), &left[open + 1..close])
            } else if close + 1 == left.len() {
                (None, &left[open + 1..close])
            } else {
                (None, left)
            }
        } else {
            (None, left)
        };
        let args = if params.trim().is_empty() {
            vec![]
        } else {
            split_top(params, ',').into_iter().map(parse_type).collect()
        };
        return KotlinType::Function {
            receiver,
            parameters: args,
            result,
            suspend,
        };
    }
    if let Some(inner) = s.strip_prefix("Array<").and_then(|x| x.strip_suffix('>')) {
        return KotlinType::Array(Box::new(parse_type(inner)));
    }
    for (array, element) in [
        ("IntArray", "Int"),
        ("LongArray", "Long"),
        ("ShortArray", "Short"),
        ("ByteArray", "Byte"),
        ("CharArray", "Char"),
        ("BooleanArray", "Boolean"),
        ("FloatArray", "Float"),
        ("DoubleArray", "Double"),
    ] {
        if s == array {
            return KotlinType::Array(Box::new(KotlinType::Primitive(element.into())));
        }
    }
    let (name, args) = if let Some(i) = s.find('<') {
        if s.ends_with('>') {
            (
                &s[..i],
                split_top(&s[i + 1..s.len() - 1], ',')
                    .into_iter()
                    .map(|a| {
                        let a = a.trim();
                        if let Some(x) = a.strip_prefix("out ") {
                            TypeArgument {
                                variance: Variance::Out,
                                ty: Some(parse_type(x)),
                            }
                        } else if let Some(x) = a.strip_prefix("in ") {
                            TypeArgument {
                                variance: Variance::In,
                                ty: Some(parse_type(x)),
                            }
                        } else if a == "*" {
                            TypeArgument {
                                variance: Variance::Invariant,
                                ty: None,
                            }
                        } else {
                            TypeArgument {
                                variance: Variance::Invariant,
                                ty: Some(parse_type(a)),
                            }
                        }
                    })
                    .collect(),
            )
        } else {
            return KotlinType::Unknown(s.into());
        }
    } else {
        (s, vec![])
    };
    match name.trim() {
        "Int" | "Long" | "Short" | "Byte" | "Double" | "Float" | "Boolean" | "Char" => {
            KotlinType::Primitive(name.trim().into())
        }
        _ => KotlinType::Named {
            name: name.into(),
            arguments: args,
        },
    }
}
fn top_level_arrow(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let (mut depth, mut i) = (0i32, 0usize);
    while i + 1 < b.len() {
        if depth == 0 && b[i] == b'-' && b[i + 1] == b'>' {
            return Some(i);
        }
        match b[i] {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' if i > 0 && b[i - 1] == b'-' => {}
            b'>' | b')' | b']' => depth -= 1,
            _ => {}
        }
        i += 1
    }
    None
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JvmSignature {
    pub name: String,
    pub descriptor: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FactStatus<T> {
    Established(T),
    Inferred(T),
    Ambiguous(Vec<T>),
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticSymbol {
    pub id: SymbolId,
    pub location: SourceLocation,
    pub ty: FactStatus<TypeRef>,
}

/// A source-proven upper bound whose members are available on a returned value.
/// This is deliberately separate from the value's exact or declared type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberReceiverCapability {
    pub owner: SymbolId,
    /// Producer-scoped declaration evidence, not an instantiated result type.
    /// Generic arguments are preserved but never substituted by member lookup.
    pub declared_bound: TypeRef,
}

/// A bounded factory can expose these classes to Kotlin constructor reflection.
/// Java overloads cannot preserve primary-constructor and optional-parameter metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KotlinConstructorMetadataDependency {
    pub bound: SymbolId,
    pub factory: SymbolId,
    pub reflection_owner: SymbolId,
    pub location: SourceLocation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeAliasDefinition {
    pub parameters: Vec<String>,
    pub target: TypeRef,
}

/// Java type arguments proven for a non-generic Kotlin alias of `Map` or
/// `MutableMap`. Values are boxed names suitable for a Java map type witness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapAliasTypeArguments {
    pub key: String,
    pub value: String,
}

pub trait SemanticProvider: Send + Sync {
    fn resolve(&self, name: &str) -> FactStatus<SymbolId>;
    fn symbols(&self) -> &[SemanticSymbol];
    fn kotlin_constructor_metadata_dependency(
        &self,
        _owner: &SymbolId,
    ) -> Option<&KotlinConstructorMetadataDependency> {
        None
    }
    /// A concrete Kotlin class whose defaulted primary constructor participates
    /// in a source-proven Jackson annotation contract.
    fn has_jackson_default_constructor_semantics(&self, _owner: &SymbolId) -> bool {
        false
    }
    fn symbols_in_file(&self, file: &Path) -> Vec<&SemanticSymbol> {
        self.symbols()
            .iter()
            .filter(|symbol| symbol.id.file == file)
            .collect()
    }
    fn symbols_named(&self, name: &str) -> Vec<&SemanticSymbol> {
        self.symbols()
            .iter()
            .filter(|symbol| symbol.id.name == name)
            .collect()
    }
    fn type_aliases(&self) -> Vec<((String, String), TypeAliasDefinition)> {
        Vec::new()
    }
    fn type_alias_definition(&self, package: &str, name: &str) -> Option<TypeAliasDefinition> {
        self.type_aliases()
            .into_iter()
            .find(|((alias_package, alias_name), _)| alias_package == package && alias_name == name)
            .map(|(_, definition)| definition)
    }
    fn has_type_alias_in_package(&self, package: &str, name: &str) -> bool {
        self.type_aliases()
            .iter()
            .any(|((alias_package, alias), _)| alias_package == package && alias == name)
    }
    /// Source text for an indexed declaration, when the provider retains it.
    /// This lets callers interpret source-level types in the declaration's
    /// imports rather than accidentally re-binding them in a consumer file.
    fn source_for_file(&self, _file: &Path) -> Option<&str> {
        None
    }

    /// An existing workspace type proof, valid only for this provider snapshot.
    fn builtin_jvm_type_descriptor(&self, _file: &Path, _spelling: &str) -> Option<&'static str> {
        None
    }
    /// Prove which instance members a generic returned value supports, without
    /// instantiating or widening its declared return type.
    fn returned_receiver_capability(
        &self,
        _member: &SymbolId,
    ) -> FactStatus<MemberReceiverCapability> {
        FactStatus::Unknown
    }
    fn is_public_instance_callable(&self, _member: &SymbolId) -> bool {
        false
    }
    /// An ordinary public Kotlin property getter with no unresolved accessor
    /// annotations or visibility changes. Providers without this fact retain.
    fn has_plain_instance_property_getter(&self, _property: &SymbolId) -> bool {
        false
    }
    /// Exact explicit method equivalent to an annotated property's source
    /// getter, when the provider has established the source-level proof.
    fn equivalent_property_getter(&self, _property: &SymbolId) -> Option<SymbolId> {
        None
    }
    /// Exact public Java getX contract, including snapshot, result binding,
    /// nullability, and exception compatibility. Unknown providers retain.
    fn java_property_read(&self, _owner: &SymbolId, _property: &str) -> Option<JavaPropertyRead> {
        None
    }
    /// All declared enum entries, in source order. Missing or ambiguous enum
    /// syntax does not prove exhaustiveness.
    fn enum_entries(&self, _owner: &SymbolId) -> FactStatus<Vec<SymbolId>> {
        FactStatus::Unknown
    }
    /// Source-indexed visible Java fields. An unknown result cannot prove
    /// absence: records and annotation-generated members need richer facts.
    fn java_field_named(&self, _owner: &SymbolId, _name: &str) -> FactStatus<bool> {
        FactStatus::Unknown
    }
    /// An indexed ordinary reference classifier, excluding Kotlin value classes.
    fn is_ordinary_jvm_reference(&self, _owner: &SymbolId) -> FactStatus<bool> {
        FactStatus::Unknown
    }
    /// A getter contract available through a proven interface receiver. This
    /// grants member lookup, not an exact expression type.
    fn inherited_property_access(
        &self,
        _owner: &SymbolId,
        _name: &str,
    ) -> FactStatus<InheritedPropertyAccess> {
        FactStatus::Unknown
    }
    /// A source-backed, ordinary no-argument JVM method with an explicit,
    /// supported signature. This is independent of whether its owner migrates.
    fn has_stable_noarg_jvm_signature(&self, _member: &SymbolId) -> bool {
        false
    }
    /// Return the explicitly declared return type of a unique, public,
    /// nongeneric Kotlin instance method whose source/JVM signature is stable.
    /// Callers must already have resolved the exact call target before using
    /// this declaration-local type as a generic payload.
    fn explicit_nongeneric_kotlin_return(&self, _member: &SymbolId) -> Option<TypeRef> {
        None
    }
    /// A public instance, nongeneric, no-argument reference-return contract.
    /// Providers without exact declaration snapshots, visible Java call names,
    /// exception compatibility and recognized nullability evidence return None.
    fn reference_return_nullability(
        &self,
        _member: &SymbolId,
    ) -> Option<ReferenceReturnNullability> {
        None
    }
    /// Whether Java package annotations are known not to impose an unmodelled
    /// nullness default. Providers must opt in using their package metadata.
    fn has_plain_java_package_nullability(&self, _package: &str) -> bool {
        false
    }
    fn is_nonsealed_interface(&self, _owner: &SymbolId) -> bool {
        false
    }
    /// Members inherited by a statically identified receiver declaration.
    /// Providers without indexed ancestry deliberately return no candidates.
    fn inherited_members<'a>(&'a self, _owner: &SymbolId, _name: &str) -> Vec<&'a SemanticSymbol> {
        Vec::new()
    }
    /// Resolve an inherited zero-argument instance call through required-argument
    /// overloads. Unknown arities, applicable defaults/varargs and incomplete
    /// ancestry do not establish a target.
    fn resolve_inherited_noarg_call(&self, _owner: &SymbolId, _name: &str) -> FactStatus<SymbolId> {
        FactStatus::Unknown
    }
    /// Whether the indexed ancestry of this declaration is complete enough to
    /// prove that a member lookup did not miss an external/ambiguous parent.
    fn has_complete_inheritance(&self, _owner: &SymbolId) -> bool {
        false
    }
    /// Complete indexed ancestry edges relevant to instance contracts.
    /// Proven empty platform marker interfaces may be omitted.
    fn instance_contract_parents(&self, _owner: &SymbolId) -> FactStatus<Vec<SymbolId>> {
        FactStatus::Unknown
    }
    /// Resolve a type spelling in the context of a source file. Implementors
    /// should return `Unknown` when imports or same-package lookup are
    /// ambiguous.
    fn resolve_type_in_file(
        &self,
        _file: &Path,
        _package: &str,
        _name: &str,
    ) -> FactStatus<SymbolId> {
        FactStatus::Unknown
    }
    /// Resolve a non-generic map type alias in the requesting file's import
    /// scope. Providers without source-backed, unambiguous alias facts retain.
    fn map_alias_type_arguments(
        &self,
        _file: &Path,
        _package: &str,
        _spelling: &str,
    ) -> FactStatus<MapAliasTypeArguments> {
        FactStatus::Unknown
    }
    fn companion_members<'a>(&'a self, _owner: &SymbolId, _name: &str) -> Vec<&'a SemanticSymbol> {
        Vec::new()
    }
    /// A public callable whose source name is available on the ordinary
    /// `Companion` holder, independently of whether its owner migrates.
    fn has_default_companion_jvm_abi(&self, _member: &SymbolId) -> bool {
        false
    }
    /// Physical public Java bean getters for a direct synthetic property.
    /// Providers must not infer hidden Lombok members or ambiguous spellings.
    fn java_property_getters<'a>(
        &'a self,
        _owner: &SymbolId,
        _property: &str,
    ) -> Vec<&'a SemanticSymbol> {
        Vec::new()
    }
    fn is_java_static_member(&self, _symbol: &SymbolId) -> bool {
        false
    }
    /// Prove that a resolved source member has the same callable JVM name.
    /// Providers without JVM/source interoperability evidence fail closed.
    fn is_java_callable_member(&self, _symbol: &SymbolId) -> bool {
        false
    }
}

/// A conservative index over already parsed source snapshots.
#[derive(Debug, Clone, Default)]
pub struct SyntaxSemanticProvider {
    symbols: Vec<SemanticSymbol>,
    by_name: BTreeMap<String, Vec<usize>>,
    by_file: BTreeMap<PathBuf, Vec<usize>>,
    type_symbols_by_location: BTreeMap<(PathBuf, usize), usize>,
    aliases: BTreeMap<(String, String), TypeAliasDefinition>,
    sources: BTreeMap<PathBuf, String>,
    imports_by_file: BTreeMap<PathBuf, Vec<String>>,
    parents: BTreeMap<SymbolId, Vec<SymbolId>>,
    incomplete_inheritance: BTreeSet<SymbolId>,
    inheritance_indexed: bool,
    companion_member_owners: BTreeMap<SymbolId, SymbolId>,
    default_companion_member_locations: BTreeSet<(PathBuf, usize)>,
    java_static_member_locations: BTreeSet<(PathBuf, usize)>,
    java_public_instance_getter_locations: BTreeSet<(PathBuf, usize)>,
    java_property_read_shapes: BTreeMap<(PathBuf, usize), java_property_reads::GetterShape>,
    java_uncertain_getter_owners: BTreeSet<SymbolId>,
    reference_return_nullability_locations: BTreeMap<(PathBuf, usize), ReferenceReturnNullability>,
    annotated_java_packages: BTreeSet<String>,
    java_visible_fields: BTreeSet<(PathBuf, Vec<String>, String)>,
    java_field_lookup_locations: BTreeSet<(PathBuf, usize)>,
    public_kotlin_callable_locations: BTreeSet<(PathBuf, usize)>,
    explicit_nongeneric_kotlin_return_locations: BTreeSet<(PathBuf, usize)>,
    checked_throws_kotlin_callable_locations: BTreeSet<(PathBuf, usize)>,
    plain_kotlin_getter_locations: BTreeSet<(PathBuf, usize)>,
    property_getter_equivalents: BTreeMap<(PathBuf, usize), usize>,
    readable_kotlin_override_property_locations: BTreeSet<(PathBuf, usize)>,
    concrete_kotlin_override_property_locations: BTreeSet<(PathBuf, usize)>,
    enum_entry_locations: BTreeMap<(PathBuf, usize), Option<Vec<String>>>,
    nongeneric_kotlin_callable_locations: BTreeSet<(PathBuf, usize)>,
    unbounded_kotlin_return_locations: BTreeSet<(PathBuf, usize)>,
    jvm_value_class_locations: BTreeSet<(PathBuf, usize)>,
    nonsealed_interface_locations: BTreeSet<(PathBuf, usize)>,
    return_bound_locations: BTreeMap<(PathBuf, usize), TypeRef>,
    callable_shapes: BTreeMap<(PathBuf, usize), CallableShape>,
    constructor_metadata_dependencies: BTreeMap<SymbolId, KotlinConstructorMetadataDependency>,
    jackson_default_constructor_classes: BTreeSet<SymbolId>,
    workspace_builtin_types: BTreeMap<PathBuf, BTreeMap<String, &'static str>>,
    generated_java_property_reads: BTreeMap<(SymbolId, String), Vec<JavaPropertyRead>>,
}

/// Facts from one source parse, before any workspace name resolution.
/// Collect independently, then merge in input order to preserve symbol and
/// duplicate-location ordering. No parser tree survives collection.
#[derive(Default)]
struct FileSemanticFacts {
    symbols: Vec<SemanticSymbol>,
    aliases: BTreeMap<(String, String), TypeAliasDefinition>,
    java_static_member_locations: BTreeSet<(PathBuf, usize)>,
    java_public_instance_getter_locations: BTreeSet<(PathBuf, usize)>,
    java_property_read_shapes: BTreeMap<(PathBuf, usize), java_property_reads::GetterShape>,
    java_uncertain_getter_owners: BTreeSet<SymbolId>,
    reference_return_nullability_locations: BTreeMap<(PathBuf, usize), ReferenceReturnNullability>,
    java_visible_fields: BTreeSet<(PathBuf, Vec<String>, String)>,
    java_field_lookup_locations: BTreeSet<(PathBuf, usize)>,
    public_kotlin_callable_locations: BTreeSet<(PathBuf, usize)>,
    explicit_nongeneric_kotlin_return_locations: BTreeSet<(PathBuf, usize)>,
    checked_throws_kotlin_callable_locations: BTreeSet<(PathBuf, usize)>,
    plain_kotlin_getter_locations: BTreeSet<(PathBuf, usize)>,
    property_getter_equivalents: BTreeMap<(PathBuf, usize), usize>,
    readable_kotlin_override_property_locations: BTreeSet<(PathBuf, usize)>,
    concrete_kotlin_override_property_locations: BTreeSet<(PathBuf, usize)>,
    enum_entry_locations: BTreeMap<(PathBuf, usize), Option<Vec<String>>>,
    nongeneric_kotlin_callable_locations: BTreeSet<(PathBuf, usize)>,
    unbounded_kotlin_return_locations: BTreeSet<(PathBuf, usize)>,
    jvm_value_class_locations: BTreeSet<(PathBuf, usize)>,
    nonsealed_interface_locations: BTreeSet<(PathBuf, usize)>,
    return_bound_locations: BTreeMap<(PathBuf, usize), TypeRef>,
    callable_shapes: BTreeMap<(PathBuf, usize), CallableShape>,
}

#[derive(Debug, Clone)]
struct CallableShape {
    arity: Option<crate::workspace::CallableArity>,
    declares_throws: bool,
}

impl FileSemanticFacts {
    fn collect(file: &Path, source: &str) -> (Self, FileRelations) {
        let mut facts = Self::default();
        let mut relations = FileRelations::default();
        let hash = *blake3::hash(source.as_bytes()).as_bytes();
        let Some(tree) = parse_source(file, source) else {
            return (facts, relations);
        };
        if file.extension().and_then(|extension| extension.to_str()) != Some("java") {
            relations.constructor_metadata =
                constructor_metadata::collect(source, tree.root_node());
        }
        relations.jackson_defaults = jackson_defaults::collect(source, file, tree.root_node());
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if matches!(
                node.kind(),
                "class_declaration"
                    | "interface_declaration"
                    | "object_declaration"
                    | "enum_declaration"
            ) {
                let names = declaration_supertypes(source, node, file);
                if !names.is_empty() {
                    relations.supertypes.push((node.start_byte(), names));
                }
            }
            if node.kind() == "companion_object" {
                let mut ancestor = node.parent();
                while let Some(parent) = ancestor {
                    if matches!(
                        parent.kind(),
                        "class_declaration" | "interface_declaration" | "object_declaration"
                    ) {
                        relations.companions.push(CompanionLocation {
                            owner_start: parent.start_byte(),
                            start: node.start_byte(),
                            end: node.end_byte(),
                            unnamed: !node.named_children(&mut node.walk()).any(|child| {
                                matches!(child.kind(), "identifier" | "simple_identifier")
                            }),
                        });
                        break;
                    }
                    ancestor = parent.parent();
                }
            }
            if matches!(
                node.kind(),
                "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
            ) && file
                .extension()
                .is_some_and(|extension| extension == "java")
                && java_property_reads::may_have_generated_getters(node)
            {
                facts
                    .java_uncertain_getter_owners
                    .insert(symbol_id_for_node(source, node, file));
            }
            if matches!(node.kind(), "field_declaration" | "constant_declaration")
                && !has_java_modifier(node, "private")
            {
                let mut ancestor = node.parent();
                while let Some(owner_node) = ancestor {
                    if matches!(
                        owner_node.kind(),
                        "class_declaration"
                            | "interface_declaration"
                            | "enum_declaration"
                            | "record_declaration"
                    ) {
                        let owner = symbol_id_for_node(source, owner_node, file);
                        let mut path = owner.owner_path;
                        path.push(owner.name);
                        let mut cursor = node.walk();
                        for variable in node
                            .named_children(&mut cursor)
                            .filter(|child| child.kind() == "variable_declarator")
                        {
                            if let Some(name) = node_name(variable, source) {
                                facts.java_visible_fields.insert((
                                    file.to_path_buf(),
                                    path.clone(),
                                    name,
                                ));
                            }
                        }
                        break;
                    }
                    ancestor = owner_node.parent();
                }
            }
            if declaration_node_kind(node.kind()) || is_property_parameter(node) {
                if is_kotlin_value_class(source, node) {
                    facts
                        .jvm_value_class_locations
                        .insert((file.to_path_buf(), node.start_byte()));
                }
                let property_param = is_property_parameter(node);
                let id = if property_param {
                    property_parameter_symbol(source, node, file).unwrap()
                } else {
                    symbol_id_for_node(source, node, file)
                };
                let type_parameters = declaration_type_parameters(source, node);
                if matches!(node.kind(), "function_declaration" | "method_declaration") {
                    let language = if node.kind() == "method_declaration" {
                        crate::workspace::SourceLanguage::Java
                    } else {
                        crate::workspace::SourceLanguage::Kotlin
                    };
                    let declares_throws = match language {
                        crate::workspace::SourceLanguage::Java => {
                            source_calls::java_method_declares_throws(source, node)
                        }
                        crate::workspace::SourceLanguage::Kotlin => {
                            source_calls::declares_checked_throws(source, node)
                        }
                    };
                    facts.callable_shapes.insert(
                        (file.to_path_buf(), node.start_byte()),
                        CallableShape {
                            arity: crate::workspace::member_callable_arity(node, language),
                            declares_throws,
                        },
                    );
                }
                if matches!(
                    node.kind(),
                    "class_declaration" | "interface_declaration" | "enum_declaration"
                ) && file
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
                    && ordinary_java_field_lookup(node)
                {
                    facts
                        .java_field_lookup_locations
                        .insert((file.to_path_buf(), node.start_byte()));
                }
                if id.kind == "enum" {
                    let entries = enum_entry_names(source, node);
                    facts
                        .enum_entry_locations
                        .insert((file.to_path_buf(), node.start_byte()), entries);
                }
                if id.kind == "property"
                    && id.receiver.is_none()
                    && !id.owner_path.is_empty()
                    && declaration_has_modifier(source, node, &["override"])
                    && readable_kotlin_property_getter(source, node, true)
                {
                    let location = (file.to_path_buf(), node.start_byte());
                    facts
                        .readable_kotlin_override_property_locations
                        .insert(location.clone());
                    // Constructor properties always supply storage. Other
                    // concrete implementations need an explicit getter/body.
                    if property_param
                        || (!declaration_has_modifier(source, node, &["abstract"])
                            && node
                                .named_children(&mut node.walk())
                                .any(|child| matches!(child.kind(), "getter" | "expression")))
                    {
                        facts
                            .concrete_kotlin_override_property_locations
                            .insert(location);
                    }
                }
                if id.kind == "property"
                    && id.receiver.is_none()
                    && !id.owner_path.is_empty()
                    && file
                        .extension()
                        .is_some_and(|extension| extension == "kt" || extension == "kts")
                    && plain_kotlin_property_getter(source, node)
                {
                    facts
                        .plain_kotlin_getter_locations
                        .insert((file.to_path_buf(), node.start_byte()));
                }
                if id.kind == "property"
                    && id.receiver.is_none()
                    && !id.owner_path.is_empty()
                    && file
                        .extension()
                        .is_some_and(|extension| extension == "kt" || extension == "kts")
                    && let Some(method_start) =
                        property_getter_equivalence::candidate(source, tree.root_node(), node)
                {
                    facts
                        .property_getter_equivalents
                        .insert((file.to_path_buf(), node.start_byte()), method_start);
                }
                if id.kind == "interface" && !declaration_has_modifier(source, node, &["sealed"]) {
                    facts
                        .nonsealed_interface_locations
                        .insert((file.to_path_buf(), node.start_byte()));
                }
                let explicit = if property_param {
                    named_type_child(node)
                } else {
                    declaration_type_node(node)
                }
                .and_then(|type_node| type_node.utf8_text(source.as_bytes()).ok())
                .map(|text| TypeRef::parse_with_type_parameters(text, &type_parameters));
                let ty = explicit
                    .map(FactStatus::Established)
                    .unwrap_or(FactStatus::Unknown);
                if let Some(nullability) =
                    source_calls::reference_return_nullability(source, node, &ty)
                {
                    facts
                        .reference_return_nullability_locations
                        .insert((file.to_path_buf(), node.start_byte()), nullability);
                }
                if node.kind() == "function_declaration"
                    && id.receiver.is_none()
                    && !id.owner_path.is_empty()
                    && public_kotlin_callable(source, node)
                {
                    facts
                        .public_kotlin_callable_locations
                        .insert((file.to_path_buf(), node.start_byte()));
                    if source_calls::declares_checked_throws(source, node) {
                        facts
                            .checked_throws_kotlin_callable_locations
                            .insert((file.to_path_buf(), node.start_byte()));
                    }
                    if own_type_parameters(node).is_none() && !has_type_constraints(node) {
                        facts
                            .nongeneric_kotlin_callable_locations
                            .insert((file.to_path_buf(), node.start_byte()));
                    }
                    if declaration_type_node(node).is_some()
                        && type_parameters.is_empty()
                        && !has_type_constraints(node)
                    {
                        facts
                            .explicit_nongeneric_kotlin_return_locations
                            .insert((file.to_path_buf(), node.start_byte()));
                    }
                    if let FactStatus::Established(ty) = &ty
                        && returned_parameter_declaration(source, node, ty)
                            .is_some_and(|(_, parameter)| named_type_child(parameter).is_none())
                    {
                        facts
                            .unbounded_kotlin_return_locations
                            .insert((file.to_path_buf(), node.start_byte()));
                    }
                    if id.parameters.is_empty()
                        && let FactStatus::Established(ty) = &ty
                        && let Some(bound) = returned_parameter_bound(source, node, ty)
                    {
                        facts
                            .return_bound_locations
                            .insert((file.to_path_buf(), node.start_byte()), bound);
                    }
                }
                if node.kind() == "type_alias"
                    && let FactStatus::Established(target) = &ty
                {
                    facts.aliases.insert(
                        (package_name(source), id.name.clone()),
                        TypeAliasDefinition {
                            parameters: type_parameters,
                            target: target.clone(),
                        },
                    );
                }
                if node.kind() == "method_declaration"
                    && file
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
                    && has_java_static_modifier(source, node)
                {
                    facts
                        .java_static_member_locations
                        .insert((file.to_path_buf(), node.start_byte()));
                }
                if node.kind() == "method_declaration"
                    && id.parameters.is_empty()
                    && file
                        .extension()
                        .is_some_and(|extension| extension == "java")
                {
                    if let Some(shape) = java_property_reads::collect(source, node) {
                        facts
                            .java_property_read_shapes
                            .insert((file.to_path_buf(), node.start_byte()), shape);
                    }
                    let mut cursor = node.walk();
                    let modifiers = node
                        .named_children(&mut cursor)
                        .find(|child| child.kind() == "modifiers");
                    let has_modifier = |keyword: &str| {
                        modifiers.is_some_and(|modifiers| {
                            let mut cursor = modifiers.walk();
                            modifiers
                                .children(&mut cursor)
                                .any(|child| child.kind() == keyword)
                        })
                    };
                    let nullable = modifiers.is_some_and(|modifiers| {
                        let mut cursor = modifiers.walk();
                        modifiers.named_children(&mut cursor).any(|child| {
                            matches!(child.kind(), "annotation" | "marker_annotation")
                                && child
                                    .child_by_field_name("name")
                                    .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                                    .is_some_and(|name| {
                                        matches!(
                                            name.rsplit('.').next(),
                                            Some("Nullable" | "CheckForNull" | "NullableDecl")
                                        )
                                    })
                        })
                    });
                    if !has_modifier("static")
                        && !has_modifier("private")
                        && !has_modifier("protected")
                        && !nullable
                        && (has_modifier("public")
                            || node
                                .parent()
                                .is_some_and(|parent| parent.kind() == "interface_body"))
                    {
                        facts
                            .java_public_instance_getter_locations
                            .insert((file.to_path_buf(), node.start_byte()));
                        if let FactStatus::Established(ty) = &ty
                            && let Some(bound) = returned_parameter_bound(source, node, ty)
                        {
                            facts
                                .return_bound_locations
                                .insert((file.to_path_buf(), node.start_byte()), bound);
                        }
                    }
                }
                facts.symbols.push(SemanticSymbol {
                    id,
                    location: SourceLocation {
                        file: file.to_path_buf(),
                        snapshot_hash: hash,
                        start_byte: node.start_byte(),
                        end_byte: node.end_byte(),
                    },
                    ty,
                });
            }
            let mut c = node.walk();
            stack.extend(node.named_children(&mut c));
        }
        (facts, relations)
    }
}

struct AnalyzedSource {
    file: PathBuf,
    source: String,
    facts: FileSemanticFacts,
    relations: FileRelations,
}

impl AnalyzedSource {
    fn collect((file, source): (PathBuf, String)) -> Self {
        let (facts, relations) = FileSemanticFacts::collect(&file, &source);
        Self {
            file,
            source,
            facts,
            relations,
        }
    }
}

/// Syntax-only relations from one parse. Resolve their byte locations against
/// the complete provider after module identities and all file indexes exist.
/// Keeping compact facts here avoids retaining whole syntax trees.
#[derive(Default)]
struct FileRelations {
    supertypes: Vec<(usize, Vec<String>)>,
    companions: Vec<CompanionLocation>,
    constructor_metadata: constructor_metadata::MetadataRelations,
    jackson_defaults: jackson_defaults::FileRelations,
}

struct CompanionLocation {
    owner_start: usize,
    start: usize,
    end: usize,
    unnamed: bool,
}

impl SyntaxSemanticProvider {
    fn companion_parameters_have_plain_jvm_names(&self, member: &SymbolId) -> bool {
        member.parameters.iter().all(|signature| {
            let ty = TypeRef::parse(signature.strip_prefix("vararg:").unwrap_or(signature));
            let name = match &ty.ty {
                KotlinType::Primitive(name) | KotlinType::Named { name, .. } => name.as_str(),
                KotlinType::Function { .. } | KotlinType::Array(_) => return true,
                _ => return false,
            };
            if self.has_type_alias_in_package(&member.package, name) {
                return false;
            }
            match self.resolve_type_in_file(&member.file, &member.package, name) {
                FactStatus::Established(owner) => !self.is_jvm_value_class(&owner),
                FactStatus::Unknown => {
                    if self.source_for_file(&member.file).is_some_and(|source| {
                        standard_external_type(source, self, name, false)
                            .is_some_and(|canonical| canonical.starts_with("java."))
                    }) {
                        return true;
                    }
                    let canonical = match name {
                        "Class" | "java.lang.Class" => "java.lang.Class",
                        "String" | "kotlin.String" | "java.lang.String" => "kotlin.String",
                        "Any" | "kotlin.Any" => "kotlin.Any",
                        "Boolean" => "kotlin.Boolean",
                        "Byte" => "kotlin.Byte",
                        "Short" => "kotlin.Short",
                        "Int" => "kotlin.Int",
                        "Long" => "kotlin.Long",
                        "Float" => "kotlin.Float",
                        "Double" => "kotlin.Double",
                        "Char" => "kotlin.Char",
                        _ => return false,
                    };
                    self.source_for_file(&member.file).is_some_and(|source| {
                        !kotlin_import_lines(source).any(|import| {
                            let (path, visible) = import
                                .split_once(" as ")
                                .map(|(path, alias)| (path.trim(), alias.trim()))
                                .unwrap_or_else(|| {
                                    (import, import.rsplit('.').next().unwrap_or(import))
                                });
                            visible == name
                                && path != canonical
                                && !(name == "String" && path == "java.lang.String")
                        })
                    })
                }
                _ => false,
            }
        })
    }

    fn named_return_has_plain_jvm_name(&self, member: &SymbolId, name: &str) -> bool {
        if self.has_type_alias_in_package(&member.package, name) {
            return false;
        }
        let Some(source) = self.source_for_file(&member.file) else {
            return false;
        };
        if kotlin_import_lines(source).any(|import| {
            import
                .split_once(" as ")
                .is_some_and(|(_, alias)| alias.trim() == name)
        }) {
            return false;
        }
        match self.resolve_type_in_file(&member.file, &member.package, name) {
            FactStatus::Established(owner) => {
                matches!(owner.kind.as_str(), "class" | "interface" | "enum")
                    && !self.is_jvm_value_class(&owner)
            }
            FactStatus::Unknown => {
                // A proven Java reference result cannot introduce Kotlin
                // value-class name mangling. Resolve it in the declaration's
                // scope, including its import/shadowing checks, rather than
                // guessing from the simple spelling at the call site.
                if standard_external_type(source, self, name, false)
                    .is_some_and(|canonical| canonical.starts_with("java."))
                {
                    return true;
                }
                let canonical = match name {
                    "String" => "kotlin.String",
                    "Any" => "kotlin.Any",
                    "Unit" => "kotlin.Unit",
                    "Boolean" => "kotlin.Boolean",
                    "Byte" => "kotlin.Byte",
                    "Short" => "kotlin.Short",
                    "Int" => "kotlin.Int",
                    "Long" => "kotlin.Long",
                    "Float" => "kotlin.Float",
                    "Double" => "kotlin.Double",
                    "Char" => "kotlin.Char",
                    "List" | "MutableList" => "kotlin.collections.List",
                    "Set" | "MutableSet" => "kotlin.collections.Set",
                    "Map" | "MutableMap" => "kotlin.collections.Map",
                    "Collection" | "MutableCollection" => "kotlin.collections.Collection",
                    "Iterable" | "MutableIterable" => "kotlin.collections.Iterable",
                    _ => return false,
                };
                !kotlin_import_lines(source).any(|import| {
                    import.ends_with(".*") && import != "java.util.*"
                        || import.rsplit('.').next() == Some(name)
                            && import != canonical
                            && !(name == "String" && import == "java.lang.String")
                })
            }
            _ => false,
        }
    }

    fn type_parameter_return_has_plain_jvm_name(&self, symbol: &SemanticSymbol) -> bool {
        let location = (symbol.location.file.clone(), symbol.location.start_byte);
        if self.unbounded_kotlin_return_locations.contains(&location) {
            return true;
        }
        let Some(TypeRef {
            ty: KotlinType::Named { name, .. },
            ..
        }) = self.return_bound_locations.get(&location)
        else {
            return false;
        };
        self.named_return_has_plain_jvm_name(&symbol.id, name)
    }

    fn is_jvm_value_class(&self, owner: &SymbolId) -> bool {
        self.symbols_named(&owner.name).into_iter().any(|symbol| {
            symbol.id == *owner
                && self
                    .jvm_value_class_locations
                    .contains(&(symbol.location.file.clone(), symbol.location.start_byte))
        })
    }

    fn declaring_type(&self, method: &SemanticSymbol) -> Option<SymbolId> {
        if method.id.owner_path.is_empty() {
            return None;
        }
        let name = method.id.owner_path.last()?;
        let path = &method.id.owner_path[..method.id.owner_path.len() - 1];
        let mut owners = self
            .by_name
            .get(name)
            .into_iter()
            .flatten()
            .map(|index| &self.symbols[*index])
            .filter(|symbol| {
                symbol.id.file == method.id.file
                    && symbol.id.package == method.id.package
                    && symbol.id.owner_path == path
                    && symbol.id.name == name.as_str()
                    && matches!(
                        symbol.id.kind.as_str(),
                        "class" | "interface" | "object" | "enum"
                    )
            });
        let owner = owners.next()?.id.clone();
        owners.next().is_none().then_some(owner)
    }

    fn inherits_type(&self, descendant: &SymbolId, ancestor: &SymbolId) -> bool {
        if descendant == ancestor {
            return false;
        }
        let mut queue = self.parents.get(descendant).cloned().unwrap_or_default();
        let mut seen = BTreeSet::new();
        while let Some(current) = queue.pop() {
            if current == *ancestor {
                return true;
            }
            if seen.insert(current.clone()) {
                queue.extend(self.parents.get(&current).into_iter().flatten().cloned());
            }
        }
        false
    }

    fn discard_overridden_inherited_methods<'a>(
        &self,
        candidates: Vec<&'a SemanticSymbol>,
    ) -> Vec<&'a SemanticSymbol> {
        if candidates.len() < 2 {
            return candidates;
        }
        let owners = candidates
            .iter()
            .map(|method| {
                (matches!(method.id.kind.as_str(), "function" | "method")
                    && method.id.parameters.is_empty())
                .then(|| self.declaring_type(method))
                .flatten()
            })
            .collect::<Vec<_>>();
        candidates
            .iter()
            .enumerate()
            .filter_map(|(i, candidate)| {
                let overridden = candidates.iter().enumerate().any(|(j, other)| {
                    if i == j
                        || !candidate.id.parameters.is_empty()
                        || !other.id.parameters.is_empty()
                        || !matches!(candidate.id.kind.as_str(), "function" | "method")
                        || !matches!(other.id.kind.as_str(), "function" | "method")
                        || candidate.id.name != other.id.name
                        || candidate.id.parameters != other.id.parameters
                        || candidate.id.receiver.is_some()
                        || other.id.receiver.is_some()
                        || self.is_java_static_member(&candidate.id)
                        || self.is_java_static_member(&other.id)
                        || !matches!((&candidate.ty, &other.ty),
                        (FactStatus::Established(a), FactStatus::Established(b)) if a == b)
                    {
                        return false;
                    }
                    match (&owners[j], &owners[i]) {
                        (Some(descendant), Some(ancestor)) => {
                            descendant != ancestor && self.inherits_type(descendant, ancestor)
                        }
                        _ => false,
                    }
                });
                (!overridden).then_some(*candidate)
            })
            .collect()
    }

    pub fn new(sources: impl IntoIterator<Item = (PathBuf, String)>) -> Self {
        Self::new_in_module("", sources)
    }
    pub fn new_in_module(
        module: impl Into<String>,
        sources: impl IntoIterator<Item = (PathBuf, String)>,
    ) -> Self {
        let sources = sources.into_iter().collect::<Vec<_>>();
        let file_count = sources.len();
        let started = std::time::Instant::now();
        let analyzed = crate::transpiler::fixpoint::install_parallel(|| {
            sources
                .into_par_iter()
                .map(AnalyzedSource::collect)
                .collect::<Vec<_>>()
        });
        let collection_time = started.elapsed();
        let started = std::time::Instant::now();
        let provider = Self::from_analyzed_sources(module.into(), analyzed);
        if std::env::var_os("NOTLIN_PROFILE").is_some() {
            eprintln!(
                "NOTLIN_PROFILE semantic facts ({file_count} files): collect {:.3}s, resolve {:.3}s",
                collection_time.as_secs_f64(),
                started.elapsed().as_secs_f64()
            );
        }
        provider
    }

    fn from_analyzed_sources(module: String, analyzed: Vec<AnalyzedSource>) -> Self {
        let mut p = Self::default();
        let mut relations = BTreeMap::new();
        let mut metadata_relations = BTreeMap::new();
        let mut jackson_relations = BTreeMap::new();
        for AnalyzedSource {
            file,
            source,
            mut facts,
            relations: mut file_relations,
        } in analyzed
        {
            if file
                .file_name()
                .is_some_and(|name| name == "module-info.java")
                && source.contains('@')
            {
                p.annotated_java_packages.insert("*".into());
            }
            if file
                .file_name()
                .is_some_and(|name| name == "package-info.java")
                && let Some(package) = source_calls::annotated_java_package(&source)
            {
                p.annotated_java_packages.insert(package);
            }
            for symbol in &mut facts.symbols {
                symbol.id.module.clone_from(&module);
            }
            p.append_file_facts(facts);
            metadata_relations.insert(
                file.clone(),
                std::mem::take(&mut file_relations.constructor_metadata),
            );
            jackson_relations.insert(
                file.clone(),
                std::mem::take(&mut file_relations.jackson_defaults),
            );
            relations.insert(file.clone(), file_relations);
            let imports = import_paths(&source);
            p.sources.insert(file.clone(), source);
            p.imports_by_file.insert(file, imports);
        }
        p.index_inheritance(&relations);
        p.index_companion_members(&relations);
        p.constructor_metadata_dependencies =
            constructor_metadata::resolve(&p, &metadata_relations);
        p.jackson_default_constructor_classes = jackson_defaults::resolve(&p, &jackson_relations);
        p
    }
    pub fn index_file(&mut self, file: &Path, source: &str) {
        self.workspace_builtin_types.clear();
        self.generated_java_property_reads.clear();
        self.constructor_metadata_dependencies.clear();
        self.jackson_default_constructor_classes.clear();
        self.index_file_relations(file, source);
    }

    /// Reuse the workspace's established builtin identities without relaxing
    /// external-call resolution. All contributing snapshots must match.
    pub fn enrich_workspace_type_facts(&mut self, index: &crate::workspace::SourceIndex) {
        self.workspace_builtin_types.clear();
        self.generated_java_property_reads.clear();
        if !self.inheritance_indexed
            || self.sources.len() != index.files.len()
            || index
                .files
                .iter()
                .any(|file| self.source_for_file(&file.path) != Some(file.source_text()))
        {
            return;
        }
        if index.files.iter().any(|file| {
            let hash = *blake3::hash(file.source_text().as_bytes()).as_bytes();
            self.by_file
                .get(&file.path)
                .into_iter()
                .flatten()
                .any(|&position| self.symbols[position].location.snapshot_hash != hash)
        }) {
            return;
        }
        self.enrich_generated_property_reads(index);
        for file in &index.files {
            if file.language != crate::workspace::SourceLanguage::Kotlin
                || !file
                    .declarations
                    .iter()
                    .any(|decl| !decl.secondary_ctors.is_empty())
            {
                continue;
            }
            let mut facts = BTreeMap::new();
            for spelling in [
                "String", "Boolean", "Byte", "Short", "Int", "Long", "Float", "Double", "Char",
            ] {
                if let Some(descriptor) = index
                    .builtin_getter_descriptor_in_file(&file.path, spelling)
                    .and_then(|getter| getter.strip_prefix("()"))
                {
                    facts.insert(spelling.to_owned(), descriptor);
                }
            }
            self.workspace_builtin_types
                .insert(file.path.clone(), facts);
        }
    }
    fn index_file_relations(&mut self, file: &Path, source: &str) -> FileRelations {
        let (facts, relations) = FileSemanticFacts::collect(file, source);
        self.append_file_facts(facts);
        self.inheritance_indexed = false;
        relations
    }

    fn append_file_facts(&mut self, facts: FileSemanticFacts) {
        let FileSemanticFacts {
            symbols,
            aliases,
            java_static_member_locations,
            java_public_instance_getter_locations,
            java_property_read_shapes,
            java_uncertain_getter_owners,
            reference_return_nullability_locations,
            java_visible_fields,
            java_field_lookup_locations,
            public_kotlin_callable_locations,
            explicit_nongeneric_kotlin_return_locations,
            checked_throws_kotlin_callable_locations,
            plain_kotlin_getter_locations,
            property_getter_equivalents,
            readable_kotlin_override_property_locations,
            concrete_kotlin_override_property_locations,
            enum_entry_locations,
            nongeneric_kotlin_callable_locations,
            unbounded_kotlin_return_locations,
            jvm_value_class_locations,
            nonsealed_interface_locations,
            return_bound_locations,
            callable_shapes,
        } = facts;
        for symbol in symbols {
            let index = self.symbols.len();
            self.by_name
                .entry(symbol.id.name.clone())
                .or_default()
                .push(index);
            self.by_file
                .entry(symbol.id.file.clone())
                .or_default()
                .push(index);
            if is_type_symbol(&symbol.id) {
                self.type_symbols_by_location
                    .entry((symbol.id.file.clone(), symbol.location.start_byte))
                    .or_insert(index);
            }
            self.symbols.push(symbol);
        }
        self.aliases.extend(aliases);
        self.java_static_member_locations
            .extend(java_static_member_locations);
        self.java_public_instance_getter_locations
            .extend(java_public_instance_getter_locations);
        self.java_property_read_shapes
            .extend(java_property_read_shapes);
        self.java_uncertain_getter_owners
            .extend(java_uncertain_getter_owners);
        self.reference_return_nullability_locations
            .extend(reference_return_nullability_locations);
        self.java_visible_fields.extend(java_visible_fields);
        self.java_field_lookup_locations
            .extend(java_field_lookup_locations);
        self.public_kotlin_callable_locations
            .extend(public_kotlin_callable_locations);
        self.explicit_nongeneric_kotlin_return_locations
            .extend(explicit_nongeneric_kotlin_return_locations);
        self.checked_throws_kotlin_callable_locations
            .extend(checked_throws_kotlin_callable_locations);
        self.plain_kotlin_getter_locations
            .extend(plain_kotlin_getter_locations);
        self.property_getter_equivalents
            .extend(property_getter_equivalents);
        self.readable_kotlin_override_property_locations
            .extend(readable_kotlin_override_property_locations);
        self.concrete_kotlin_override_property_locations
            .extend(concrete_kotlin_override_property_locations);
        self.enum_entry_locations.extend(enum_entry_locations);
        self.nongeneric_kotlin_callable_locations
            .extend(nongeneric_kotlin_callable_locations);
        self.unbounded_kotlin_return_locations
            .extend(unbounded_kotlin_return_locations);
        self.jvm_value_class_locations
            .extend(jvm_value_class_locations);
        self.nonsealed_interface_locations
            .extend(nonsealed_interface_locations);
        self.return_bound_locations.extend(return_bound_locations);
        self.callable_shapes.extend(callable_shapes);
    }
    pub fn resolve_in_file(&self, file: &Path, name: &str) -> FactStatus<SymbolId> {
        let found: Vec<_> = self
            .symbols_named(name)
            .into_iter()
            .filter(|s| s.id.file == file)
            .map(|s| s.id.clone())
            .collect();
        match found.as_slice() {
            [id] => FactStatus::Established(id.clone()),
            many if !many.is_empty() => FactStatus::Ambiguous(many.to_vec()),
            _ => FactStatus::Unknown,
        }
    }
    pub fn resolve_signature(&self, name: &str, parameters: &[String]) -> FactStatus<SymbolId> {
        let found: Vec<_> = self
            .symbols_named(name)
            .into_iter()
            .filter(|s| s.id.parameters == parameters)
            .map(|s| s.id.clone())
            .collect();
        match found.as_slice() {
            [id] => FactStatus::Established(id.clone()),
            many if !many.is_empty() => FactStatus::Ambiguous(many.to_vec()),
            _ => FactStatus::Unknown,
        }
    }
    pub fn aliases(&self) -> &BTreeMap<(String, String), TypeAliasDefinition> {
        &self.aliases
    }
    pub fn aliases_in_package(&self, package: &str) -> BTreeMap<String, TypeAliasDefinition> {
        self.aliases
            .iter()
            .filter(|((pkg, _), _)| pkg == package)
            .map(|((_, name), definition)| (name.clone(), definition.clone()))
            .collect()
    }

    fn index_inheritance(&mut self, relations: &BTreeMap<PathBuf, FileRelations>) {
        let mut edges = Vec::new();
        for (file, facts) in relations {
            for (start, names) in &facts.supertypes {
                if let Some(index) = self.type_symbols_by_location.get(&(file.clone(), *start)) {
                    edges.push((self.symbols[*index].id.clone(), names));
                }
            }
        }
        for (owner, names) in edges {
            for name in names {
                let aliases_supertype = self
                    .sources
                    .get(&owner.file)
                    .is_some_and(|source| supertype_uses_import_alias(source, name));
                if aliases_supertype {
                    self.incomplete_inheritance.insert(owner.clone());
                } else {
                    match self.resolve_type_in_file(&owner.file, &owner.package, name) {
                        FactStatus::Established(parent) => {
                            self.parents.entry(owner.clone()).or_default().push(parent);
                        }
                        FactStatus::Unknown
                            if self.sources.get(&owner.file).is_some_and(|source| {
                                known_empty_platform_interface(source, name, self)
                            }) =>
                        {
                            // These exact marker interfaces add no callable members.
                        }
                        _ => {
                            self.incomplete_inheritance.insert(owner.clone());
                        }
                    }
                }
            }
        }
        for parents in self.parents.values_mut() {
            parents.sort();
            parents.dedup();
        }
        self.inheritance_indexed = true;
    }

    fn index_companion_members(&mut self, relations: &BTreeMap<PathBuf, FileRelations>) {
        let mut indexed = Vec::new();
        let mut default_locations = Vec::new();
        for (file, facts) in relations {
            if facts.companions.is_empty() {
                continue;
            }
            let file_symbols = self
                .by_file
                .get(file)
                .into_iter()
                .flatten()
                .map(|index| &self.symbols[*index])
                .collect::<Vec<_>>();
            for location in &facts.companions {
                let owner = self
                    .type_symbols_by_location
                    .get(&(file.clone(), location.owner_start))
                    .map(|index| self.symbols[*index].id.clone());
                if let Some(owner) = owner {
                    let mut expected_path = owner.owner_path.clone();
                    expected_path.push(owner.name.clone());
                    for symbol in &file_symbols {
                        let direct_path = symbol.id.owner_path == expected_path;
                        let companion_path = symbol.id.owner_path.len() == expected_path.len() + 1
                            && symbol.id.owner_path.starts_with(&expected_path)
                            && symbol
                                .id
                                .owner_path
                                .last()
                                .is_some_and(|part| part == "Companion");
                        if symbol.id.file == *file
                            && symbol.location.start_byte >= location.start
                            && symbol.location.end_byte <= location.end
                            && matches!(symbol.id.kind.as_str(), "function" | "method")
                            && (direct_path || companion_path)
                        {
                            indexed.push((symbol.id.clone(), owner.clone()));
                            if location.unnamed {
                                default_locations.push((file.clone(), symbol.location.start_byte));
                            }
                        }
                    }
                }
            }
        }
        self.companion_member_owners.extend(indexed);
        self.default_companion_member_locations
            .extend(default_locations);
    }
}

fn known_empty_platform_interface(
    source: &str,
    spelling: &str,
    provider: &SyntaxSemanticProvider,
) -> bool {
    const MARKERS: [&str; 2] = ["java.io.Serializable", "java.lang.Cloneable"];
    if provider
        .symbols_named("java")
        .iter()
        .any(|symbol| is_type_symbol(&symbol.id))
        || kotlin_import_lines(source).any(|import| import.rsplit('.').next() == Some("java"))
    {
        return false;
    }
    if MARKERS.contains(&spelling) {
        return true;
    }
    if provider
        .symbols_named(spelling)
        .iter()
        .any(|symbol| is_type_symbol(&symbol.id) && symbol.id.package == package_name(source))
    {
        return false;
    }
    // An explicit import wins over wildcard imports. Do not infer markers from
    // their simple names, wildcard imports, aliases, or competing imports.
    let matching = kotlin_import_lines(source)
        .filter(|path| !path.contains(" as "))
        .filter(|path| path.rsplit('.').next() == Some(spelling))
        .collect::<Vec<_>>();
    matches!(matching.as_slice(), [path] if MARKERS.contains(path))
}

fn supertype_uses_import_alias(source: &str, supertype: &str) -> bool {
    let raw = supertype
        .split_once('(')
        .map_or(supertype, |(name, _)| name)
        .split('<')
        .next()
        .unwrap_or(supertype)
        .trim();
    let classifiers = raw.split('.').collect::<Vec<_>>();
    let Some(simple) = classifiers.last() else {
        return false;
    };
    source.lines().any(|line| {
        let Some(import) = line.trim().strip_prefix("import ") else {
            return false;
        };
        import
            .split_once(" as ")
            .is_some_and(|(_, alias)| alias.trim().trim_end_matches(';') == *simple)
    })
}
impl SemanticProvider for SyntaxSemanticProvider {
    fn has_plain_java_package_nullability(&self, package: &str) -> bool {
        !self.annotated_java_packages.contains(package)
            && !self.annotated_java_packages.contains("*")
    }
    fn kotlin_constructor_metadata_dependency(
        &self,
        owner: &SymbolId,
    ) -> Option<&KotlinConstructorMetadataDependency> {
        self.constructor_metadata_dependencies.get(owner)
    }
    fn has_jackson_default_constructor_semantics(&self, owner: &SymbolId) -> bool {
        self.jackson_default_constructor_classes.contains(owner)
    }
    fn source_for_file(&self, file: &Path) -> Option<&str> {
        self.sources.get(file).map(String::as_str)
    }
    fn builtin_jvm_type_descriptor(&self, file: &Path, spelling: &str) -> Option<&'static str> {
        self.workspace_builtin_types
            .get(file)?
            .get(spelling)
            .copied()
    }
    fn is_public_instance_callable(&self, member: &SymbolId) -> bool {
        let matches = self
            .symbols_named(&member.name)
            .into_iter()
            .filter(|symbol| symbol.id == *member)
            .collect::<Vec<_>>();
        let [symbol] = matches.as_slice() else {
            return false;
        };
        let Some(owner) = self.declaring_type(symbol) else {
            return false;
        };
        if self.is_jvm_value_class(&owner) {
            return false;
        }
        // Value-class results can mangle a method even through a type
        // parameter bound. Inferred/unknown results cannot prove a JVM name.
        if member
            .file
            .extension()
            .is_some_and(|extension| extension == "kt")
            && !match &symbol.ty {
                FactStatus::Established(TypeRef { ty, .. }) => match ty {
                    KotlinType::Primitive(name) => {
                        self.named_return_has_plain_jvm_name(member, name)
                    }
                    KotlinType::TypeParameter(_) => {
                        self.type_parameter_return_has_plain_jvm_name(symbol)
                    }
                    KotlinType::Named { name, .. } => {
                        self.named_return_has_plain_jvm_name(member, name)
                    }
                    _ => false,
                },
                _ => false,
            }
        {
            return false;
        }
        symbol.id.receiver.is_none()
            && !symbol.id.owner_path.is_empty()
            && (self
                .public_kotlin_callable_locations
                .contains(&(symbol.location.file.clone(), symbol.location.start_byte))
                || self
                    .java_public_instance_getter_locations
                    .contains(&(symbol.location.file.clone(), symbol.location.start_byte)))
    }
    fn has_plain_instance_property_getter(&self, property: &SymbolId) -> bool {
        let candidates = self.symbols_named(&property.name);
        let matches = candidates
            .iter()
            .filter(|symbol| symbol.id == *property)
            .collect::<Vec<_>>();
        let [symbol] = matches.as_slice() else {
            return false;
        };
        self.declaring_type(symbol)
            .is_some_and(|owner| !self.is_jvm_value_class(&owner))
            && self
                .plain_kotlin_getter_locations
                .contains(&(symbol.location.file.clone(), symbol.location.start_byte))
    }
    fn equivalent_property_getter(&self, property: &SymbolId) -> Option<SymbolId> {
        if property.kind != "property"
            || property.receiver.is_some()
            || property.owner_path.len() != 1
        {
            return None;
        }
        let properties = self
            .symbols_named(&property.name)
            .into_iter()
            .filter(|symbol| symbol.id == *property)
            .collect::<Vec<_>>();
        let [property_symbol] = properties.as_slice() else {
            return None;
        };
        let source = self.source_for_file(&property.file)?;
        let snapshot = *blake3::hash(source.as_bytes()).as_bytes();
        if property_symbol.location.snapshot_hash != snapshot {
            return None;
        }
        let method_start = *self
            .property_getter_equivalents
            .get(&(property.file.clone(), property_symbol.location.start_byte))?;
        let methods = self
            .symbols_in_file(&property.file)
            .into_iter()
            .filter(|symbol| {
                symbol.location.start_byte == method_start
                    && symbol.location.snapshot_hash == snapshot
                    && symbol.id.module == property.module
                    && symbol.id.package == property.package
                    && symbol.id.owner_path == property.owner_path
                    && symbol.id.kind == "function"
                    && symbol.id.receiver.is_none()
                    && symbol.id.parameters.is_empty()
            })
            .collect::<Vec<_>>();
        let [method] = methods.as_slice() else {
            return None;
        };
        let property_type = match &property_symbol.ty {
            FactStatus::Established(ty) => ty,
            FactStatus::Unknown | FactStatus::Ambiguous(_) | FactStatus::Inferred(_) => {
                return None;
            }
        };
        for annotation in ["JvmField", "JvmName"] {
            if self.has_type_alias_in_package(&property.package, annotation)
                || self.symbols_named(annotation).iter().any(|symbol| {
                    symbol.id.module == property.module && symbol.id.package == property.package
                })
            {
                return None;
            }
        }
        (self.is_public_instance_callable(&method.id)
            && self.explicit_nongeneric_kotlin_return(&method.id).as_ref() == Some(property_type))
        .then(|| method.id.clone())
    }
    fn inherited_property_access(
        &self,
        owner: &SymbolId,
        name: &str,
    ) -> FactStatus<InheritedPropertyAccess> {
        inherited_properties::establish(self, owner, name)
    }
    fn enum_entries(&self, owner: &SymbolId) -> FactStatus<Vec<SymbolId>> {
        let candidates = self.symbols_named(&owner.name);
        let matches = candidates
            .iter()
            .filter(|symbol| symbol.id == *owner && owner.kind == "enum")
            .collect::<Vec<_>>();
        let [symbol] = matches.as_slice() else {
            return FactStatus::Unknown;
        };
        let Some(Some(names)) = self
            .enum_entry_locations
            .get(&(owner.file.clone(), symbol.location.start_byte))
        else {
            return FactStatus::Unknown;
        };
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        FactStatus::Established(
            names
                .iter()
                .map(|name| SymbolId {
                    module: owner.module.clone(),
                    package: owner.package.clone(),
                    file: owner.file.clone(),
                    owner_path: path.clone(),
                    kind: "enum_entry".into(),
                    name: name.clone(),
                    receiver: None,
                    parameters: Vec::new(),
                })
                .collect(),
        )
    }
    fn java_field_named(&self, owner: &SymbolId, name: &str) -> FactStatus<bool> {
        let candidates = self.symbols_named(&owner.name);
        let matches = candidates
            .iter()
            .filter(|symbol| symbol.id == *owner)
            .collect::<Vec<_>>();
        let [symbol] = matches.as_slice() else {
            return FactStatus::Unknown;
        };
        if !self
            .java_field_lookup_locations
            .contains(&(owner.file.clone(), symbol.location.start_byte))
            || self.source_for_file(&owner.file).is_none_or(|source| {
                *blake3::hash(source.as_bytes()).as_bytes() != symbol.location.snapshot_hash
            })
        {
            return FactStatus::Unknown;
        }
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        let field = self
            .java_visible_fields
            .contains(&(owner.file.clone(), path, name.to_owned()));
        let constant = matches!(self.enum_entries(owner), FactStatus::Established(entries) if entries.iter().any(|entry| entry.name == name));
        FactStatus::Established(field || constant)
    }
    fn is_ordinary_jvm_reference(&self, owner: &SymbolId) -> FactStatus<bool> {
        let candidates = self.symbols_named(&owner.name);
        let matches = candidates
            .iter()
            .filter(|symbol| symbol.id == *owner)
            .collect::<Vec<_>>();
        let [symbol] = matches.as_slice() else {
            return FactStatus::Unknown;
        };
        if self.source_for_file(&owner.file).is_none_or(|source| {
            *blake3::hash(source.as_bytes()).as_bytes() != symbol.location.snapshot_hash
        }) {
            return FactStatus::Unknown;
        }
        FactStatus::Established(
            matches!(
                owner.kind.as_str(),
                "class" | "interface" | "enum" | "object" | "record"
            ) && !self.is_jvm_value_class(owner),
        )
    }
    fn is_nonsealed_interface(&self, owner: &SymbolId) -> bool {
        let matches = self
            .symbols_named(&owner.name)
            .into_iter()
            .filter(|symbol| symbol.id == *owner && owner.kind == "interface")
            .collect::<Vec<_>>();
        matches!(matches.as_slice(), [symbol] if self.nonsealed_interface_locations
            .contains(&(symbol.location.file.clone(), symbol.location.start_byte)))
    }
    fn has_stable_noarg_jvm_signature(&self, member: &SymbolId) -> bool {
        if !member.parameters.is_empty() || !self.is_public_instance_callable(member) {
            return false;
        }
        let matches = self
            .symbols_named(&member.name)
            .into_iter()
            .filter(|symbol| symbol.id == *member)
            .collect::<Vec<_>>();
        let [symbol] = matches.as_slice() else {
            return false;
        };
        let is_java = member
            .file
            .extension()
            .is_some_and(|extension| extension == "java");
        if !is_java
            && !self
                .nongeneric_kotlin_callable_locations
                .contains(&(symbol.location.file.clone(), symbol.location.start_byte))
        {
            return false;
        }
        match &symbol.ty {
            FactStatus::Established(TypeRef {
                ty,
                nullable: false,
            }) => match ty {
                KotlinType::Primitive(_) | KotlinType::TypeParameter(_) => true,
                KotlinType::Named { name, arguments } if arguments.is_empty() => {
                    if self.has_type_alias_in_package(&member.package, name) {
                        return false;
                    }
                    let Some(source) = self.source_for_file(&member.file) else {
                        return false;
                    };
                    if kotlin_import_lines(source).any(|import| {
                        import
                            .split_once(" as ")
                            .is_some_and(|(_, alias)| alias.trim() == name)
                    }) {
                        return false;
                    }
                    match self.resolve_type_in_file(&member.file, &member.package, name) {
                        FactStatus::Established(owner) => {
                            matches!(owner.kind.as_str(), "class" | "interface" | "enum")
                                && !self.is_jvm_value_class(&owner)
                        }
                        FactStatus::Unknown => {
                            // Java source methods keep their names regardless
                            // of return classes. Kotlin default reference types
                            // are known only when imports do not rebind them.
                            is_java
                                || (matches!(
                                    name.as_str(),
                                    "String"
                                        | "Any"
                                        | "Unit"
                                        | "kotlin.String"
                                        | "kotlin.Any"
                                        | "kotlin.Unit"
                                ) && !kotlin_import_lines(source).any(|import| {
                                    import.ends_with(".*") && import != "java.util.*"
                                        || import.rsplit('.').next() == Some(name.as_str())
                                            && !matches!(
                                                import,
                                                "kotlin.String"
                                                    | "kotlin.Any"
                                                    | "kotlin.Unit"
                                                    | "java.lang.String"
                                            )
                                }))
                        }
                        _ => false,
                    }
                }
                _ => false,
            },
            _ => false,
        }
    }
    fn reference_return_nullability(
        &self,
        member: &SymbolId,
    ) -> Option<ReferenceReturnNullability> {
        if !member.parameters.is_empty()
            || member.receiver.is_some()
            || !self.is_java_callable_member(member)
            || (member
                .file
                .extension()
                .is_none_or(|extension| extension != "java")
                && !self.is_public_instance_callable(member))
        {
            return None;
        }
        let matches = self
            .symbols_named(&member.name)
            .into_iter()
            .filter(|symbol| symbol.id == *member)
            .collect::<Vec<_>>();
        let [symbol] = matches.as_slice() else {
            return None;
        };
        let source = self.source_for_file(&member.file)?;
        if *blake3::hash(source.as_bytes()).as_bytes() != symbol.location.snapshot_hash {
            return None;
        }
        let owner = self.declaring_type(symbol)?;
        let owners = self
            .symbols_named(&owner.name)
            .into_iter()
            .filter(|candidate| candidate.id == owner)
            .collect::<Vec<_>>();
        let [owner_symbol] = owners.as_slice() else {
            return None;
        };
        let owner_source = self.source_for_file(&owner.file)?;
        if *blake3::hash(owner_source.as_bytes()).as_bytes() != owner_symbol.location.snapshot_hash
            || !self.has_complete_inheritance(&owner)
            || !self.inherited_members(&owner, &member.name).is_empty()
        {
            return None;
        }
        if symbol
            .id
            .file
            .extension()
            .is_some_and(|extension| extension == "java")
            && (self.annotated_java_packages.contains(&symbol.id.package)
                || self.annotated_java_packages.contains("*"))
        {
            return None;
        }
        self.reference_return_nullability_locations
            .get(&(symbol.location.file.clone(), symbol.location.start_byte))
            .copied()
    }
    fn returned_receiver_capability(
        &self,
        member: &SymbolId,
    ) -> FactStatus<MemberReceiverCapability> {
        let matches = self
            .symbols_named(&member.name)
            .into_iter()
            .filter(|symbol| symbol.id == *member)
            .collect::<Vec<_>>();
        let [symbol] = matches.as_slice() else {
            return FactStatus::Unknown;
        };
        let Some(bound) = self
            .return_bound_locations
            .get(&(symbol.location.file.clone(), symbol.location.start_byte))
        else {
            return FactStatus::Unknown;
        };
        let KotlinType::Named { name, .. } = &bound.ty else {
            return FactStatus::Unknown;
        };
        let Some(source) = self.source_for_file(&member.file) else {
            return FactStatus::Unknown;
        };
        if kotlin_import_lines(source).any(|import| {
            import
                .split_once(" as ")
                .is_some_and(|(_, alias)| name.split('.').next() == Some(alias.trim()))
        }) || self.has_type_alias_in_package(&member.package, name)
        {
            return FactStatus::Unknown;
        }
        match self.resolve_type_in_file(&member.file, &member.package, name) {
            FactStatus::Established(owner)
                if matches!(owner.kind.as_str(), "class" | "interface")
                    && !self.is_jvm_value_class(&owner)
                    && self
                        .symbols_named(&owner.name)
                        .iter()
                        .filter(|symbol| symbol.id == owner)
                        .count()
                        == 1
                    && self.has_complete_inheritance(&owner) =>
            {
                FactStatus::Established(MemberReceiverCapability {
                    owner,
                    declared_bound: bound.clone(),
                })
            }
            _ => FactStatus::Unknown,
        }
    }
    fn java_property_getters<'a>(
        &'a self,
        owner: &SymbolId,
        property: &str,
    ) -> Vec<&'a SemanticSymbol> {
        let Some(name) = self.direct_java_property_getter_name(owner, property) else {
            return Vec::new();
        };
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        self.symbols_named(&name)
            .into_iter()
            .filter(|symbol| {
                symbol.id.file == owner.file
                    && symbol.id.owner_path == path
                    && symbol.id.receiver.is_none()
                    && self
                        .java_public_instance_getter_locations
                        .contains(&(symbol.id.file.clone(), symbol.location.start_byte))
            })
            .collect()
    }
    fn java_property_read(&self, owner: &SymbolId, property: &str) -> Option<JavaPropertyRead> {
        self.java_property_read_contract(owner, property)
    }
    fn is_java_callable_member(&self, id: &SymbolId) -> bool {
        if id.receiver.is_some() || !matches!(id.kind.as_str(), "function" | "method") {
            return false;
        }
        let symbols = self
            .symbols_named(&id.name)
            .into_iter()
            .filter(|symbol| &symbol.id == id)
            .collect::<Vec<_>>();
        let [symbol] = symbols.as_slice() else {
            return false;
        };
        if id
            .file
            .extension()
            .is_some_and(|extension| extension == "java")
        {
            return !id.owner_path.is_empty();
        }
        // Ordinary Kotlin source functions also expose their source name to
        // Java. Reuse the exact no-argument ABI proof rather than treating a
        // resolved project call as an unmapped stdlib operation. The proof
        // excludes visibility/name changes, method generics, and value-class
        // owners/results, independently of whether the declaration migrates.
        if enum_when::is_java_identifier(&id.name)
            && !self
                .checked_throws_kotlin_callable_locations
                .contains(&(symbol.location.file.clone(), symbol.location.start_byte))
            && self.source_for_file(&id.file).is_some_and(|source| {
                *blake3::hash(source.as_bytes()).as_bytes() == symbol.location.snapshot_hash
            })
            && self.has_stable_noarg_jvm_signature(id)
        {
            return true;
        }
        // A Kotlin override of an indexed Java getter must expose the Java
        // signature, including any compiler-generated forwarding bridge.
        // Without the independent proof above, require the inherited Java
        // contract; internal names and value-class signatures may be mangled.
        if !id.parameters.is_empty() {
            return false;
        }
        let Some(owner) = self.declaring_type(symbol) else {
            return false;
        };
        let mut pending = self.parents.get(&owner).cloned().unwrap_or_default();
        let mut seen = BTreeSet::new();
        while let Some(parent) = pending.pop() {
            if !seen.insert(parent.clone()) {
                continue;
            }
            let mut path = parent.owner_path.clone();
            path.push(parent.name.clone());
            if parent
                .file
                .extension()
                .is_some_and(|extension| extension == "java")
                && self.symbols_named(&id.name).into_iter().any(|member| {
                    member.id.file == parent.file
                        && member.id.owner_path == path
                        && member.id.receiver.is_none()
                        && member.id.parameters == id.parameters
                        && matches!(member.id.kind.as_str(), "function" | "method")
                        && !self.is_java_static_member(&member.id)
                        && matches!((&member.ty, &symbol.ty),
                            (FactStatus::Established(a), FactStatus::Established(b)) if a == b)
                })
            {
                return true;
            }
            pending.extend(self.parents.get(&parent).into_iter().flatten().cloned());
        }
        false
    }
    fn explicit_nongeneric_kotlin_return(&self, member: &SymbolId) -> Option<TypeRef> {
        if member.kind != "function"
            || member.receiver.is_some()
            || member.owner_path.is_empty()
            || !member.parameters.is_empty()
            || !member
                .file
                .extension()
                .is_some_and(|extension| extension == "kt" || extension == "kts")
        {
            return None;
        }
        let matches = self
            .symbols_named(&member.name)
            .into_iter()
            .filter(|symbol| symbol.id == *member)
            .collect::<Vec<_>>();
        let [symbol] = matches.as_slice() else {
            return None;
        };
        let location = (symbol.location.file.clone(), symbol.location.start_byte);
        let source = self.source_for_file(&member.file)?;
        if *blake3::hash(source.as_bytes()).as_bytes() != symbol.location.snapshot_hash
            || !self
                .explicit_nongeneric_kotlin_return_locations
                .contains(&location)
            || !self.public_kotlin_callable_locations.contains(&location)
            || !self
                .nongeneric_kotlin_callable_locations
                .contains(&location)
            || !self.is_public_instance_callable(member)
        {
            return None;
        }
        match &symbol.ty {
            FactStatus::Established(return_type) => Some(return_type.clone()),
            FactStatus::Inferred(_) | FactStatus::Ambiguous(_) | FactStatus::Unknown => None,
        }
    }
    fn resolve(&self, name: &str) -> FactStatus<SymbolId> {
        match self.by_name.get(name).map(|v| v.as_slice()) {
            Some([i]) => FactStatus::Established(self.symbols[*i].id.clone()),
            Some(v) if !v.is_empty() => {
                FactStatus::Ambiguous(v.iter().map(|i| self.symbols[*i].id.clone()).collect())
            }
            _ => FactStatus::Unknown,
        }
    }
    fn symbols(&self) -> &[SemanticSymbol] {
        &self.symbols
    }
    fn symbols_in_file(&self, file: &Path) -> Vec<&SemanticSymbol> {
        self.by_file
            .get(file)
            .into_iter()
            .flatten()
            .map(|index| &self.symbols[*index])
            .collect()
    }
    fn symbols_named(&self, name: &str) -> Vec<&SemanticSymbol> {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .map(|index| &self.symbols[*index])
            .collect()
    }
    fn type_aliases(&self) -> Vec<((String, String), TypeAliasDefinition)> {
        self.aliases
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }
    fn type_alias_definition(&self, package: &str, name: &str) -> Option<TypeAliasDefinition> {
        self.aliases
            .get(&(package.to_owned(), name.to_owned()))
            .cloned()
    }
    fn has_type_alias_in_package(&self, package: &str, name: &str) -> bool {
        self.aliases
            .contains_key(&(package.to_owned(), name.to_owned()))
    }
    fn inherited_members<'a>(&'a self, owner: &SymbolId, name: &str) -> Vec<&'a SemanticSymbol> {
        let mut level = self.parents.get(owner).cloned().unwrap_or_default();
        let mut visited = std::collections::BTreeSet::new();
        while !level.is_empty() {
            level.sort();
            level.dedup();
            level.retain(|id| visited.insert(id.clone()));
            if level.is_empty() {
                break;
            }
            let mut found = Vec::new();
            for parent in &level {
                let mut path = parent.owner_path.clone();
                path.push(parent.name.clone());
                found.extend(self.symbols_named(name).into_iter().filter(|symbol| {
                    symbol.id.file == parent.file
                        && symbol.id.owner_path == path
                        && symbol.id.name == name
                        && matches!(symbol.id.kind.as_str(), "function" | "method")
                }));
            }
            if !found.is_empty() {
                return self.discard_overridden_inherited_methods(found);
            }
            level = level
                .iter()
                .flat_map(|id| self.parents.get(id).cloned().unwrap_or_default())
                .collect();
        }
        Vec::new()
    }
    fn resolve_inherited_noarg_call(&self, owner: &SymbolId, name: &str) -> FactStatus<SymbolId> {
        inherited_calls::resolve(self, owner, name)
    }
    fn has_complete_inheritance(&self, owner: &SymbolId) -> bool {
        if !self.inheritance_indexed
            || !self
                .symbols_named(&owner.name)
                .iter()
                .any(|symbol| symbol.id == *owner)
        {
            return false;
        }
        let mut pending = vec![owner.clone()];
        let mut seen = BTreeSet::new();
        while let Some(current) = pending.pop() {
            if !seen.insert(current.clone()) {
                continue;
            }
            if self.incomplete_inheritance.contains(&current) {
                return false;
            }
            pending.extend(self.parents.get(&current).into_iter().flatten().cloned());
        }
        true
    }
    fn instance_contract_parents(&self, owner: &SymbolId) -> FactStatus<Vec<SymbolId>> {
        if !self.has_complete_inheritance(owner) {
            return FactStatus::Unknown;
        }
        FactStatus::Established(self.parents.get(owner).cloned().unwrap_or_default())
    }
    fn resolve_type_in_file(&self, file: &Path, package: &str, name: &str) -> FactStatus<SymbolId> {
        let spelling = name.trim().trim_end_matches('?');
        let found = if spelling.contains('.') {
            let (pkg, simple) = spelling.rsplit_once('.').unwrap();
            self.symbols_named(simple)
                .into_iter()
                .filter(|symbol| is_type_symbol(&symbol.id) && symbol.id.package == pkg)
                .map(|symbol| symbol.id.clone())
                .collect::<Vec<_>>()
        } else {
            // A public `index_file` call does not register source text. Keep
            // its old no-import behavior even though another cache may know
            // about the same path.
            let imports = if self.sources.contains_key(file) {
                self.imports_by_file
                    .get(file)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
            } else {
                &[]
            };
            let explicit = imports
                .iter()
                .filter_map(|path| {
                    let (pkg, imported) = path.rsplit_once('.')?;
                    (imported == spelling).then_some((pkg, imported))
                })
                .flat_map(|(pkg, simple)| {
                    self.symbols_named(simple)
                        .into_iter()
                        .filter(move |symbol| {
                            is_type_symbol(&symbol.id) && symbol.id.package == pkg
                        })
                        .map(|symbol| symbol.id.clone())
                })
                .collect::<Vec<_>>();
            if !explicit.is_empty() {
                explicit
            } else {
                let same_package = self
                    .symbols_named(spelling)
                    .into_iter()
                    .filter(|symbol| is_type_symbol(&symbol.id) && symbol.id.package == package)
                    .map(|symbol| symbol.id.clone())
                    .collect::<Vec<_>>();
                if !same_package.is_empty() {
                    same_package
                } else {
                    imports
                        .iter()
                        .filter(|path| path.ends_with(".*"))
                        .flat_map(|path| {
                            let pkg = path.trim_end_matches(".*");
                            self.symbols_named(spelling)
                                .into_iter()
                                .filter(move |symbol| {
                                    is_type_symbol(&symbol.id) && symbol.id.package == pkg
                                })
                                .map(|symbol| symbol.id.clone())
                        })
                        .collect()
                }
            }
        };
        fact_from_candidates(found)
    }
    fn map_alias_type_arguments(
        &self,
        file: &Path,
        package: &str,
        spelling: &str,
    ) -> FactStatus<MapAliasTypeArguments> {
        map_alias::resolve(self, file, package, spelling)
    }
    fn companion_members<'a>(&'a self, owner: &SymbolId, name: &str) -> Vec<&'a SemanticSymbol> {
        self.symbols_named(name)
            .into_iter()
            .filter(|symbol| self.companion_member_owners.get(&symbol.id) == Some(owner))
            .collect()
    }
    fn has_default_companion_jvm_abi(&self, member: &SymbolId) -> bool {
        self.symbols_named(&member.name).iter().any(|symbol| {
            symbol.id == *member
                && self
                    .default_companion_member_locations
                    .contains(&(symbol.location.file.clone(), symbol.location.start_byte))
        }) && self.is_public_instance_callable(member)
            && self.symbols_named(&member.name).iter().any(|symbol| {
                symbol.id == *member
                    && self
                        .nongeneric_kotlin_callable_locations
                        .contains(&(symbol.location.file.clone(), symbol.location.start_byte))
            })
            && self.companion_parameters_have_plain_jvm_names(member)
    }
    fn is_java_static_member(&self, symbol: &SymbolId) -> bool {
        self.symbols_named(&symbol.name).into_iter().any(|indexed| {
            indexed.id == *symbol
                && self
                    .java_static_member_locations
                    .contains(&(indexed.location.file.clone(), indexed.location.start_byte))
        })
    }
}

/// Propagate known semantic losses through type aliases. Call this alongside
/// `TranslationPlan::plan_type_losses` when planning with a workspace provider.
/// The function is deliberately conservative for wildcard imports and alias
/// collisions: any possibly imported lossy alias blocks the containing owner.
pub fn plan_alias_type_losses<P: SemanticProvider + ?Sized>(
    source: &str,
    tree: &tree_sitter::Tree,
    provider: &P,
    plan: &mut crate::translation_plan::TranslationPlan,
    allow_approximations: bool,
) {
    let aliases: BTreeMap<_, _> = provider.type_aliases().into_iter().collect();
    let package = package_name(source);
    let imported_aliases: Vec<_> = source
        .lines()
        .filter_map(|line| {
            let line = line.trim().strip_prefix("import ")?;
            let (path, alias) = line
                .split_once(" as ")
                .map(|(p, a)| (p.trim(), Some(a.trim())))
                .unwrap_or((line, None));
            let (pkg, name) = path.rsplit_once('.')?;
            Some((
                pkg.to_owned(),
                if name == "*" {
                    None
                } else {
                    Some(name.to_owned())
                },
                alias.map(str::to_owned),
            ))
        })
        .collect();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "user_type" | "type_identifier") {
            let spelling = node.utf8_text(source.as_bytes()).unwrap_or("");
            let last = spelling
                .rsplit('.')
                .next()
                .unwrap_or(spelling)
                .split('<')
                .next()
                .unwrap_or(spelling);
            let exact = imported_aliases.iter().find(|(_, target, alias)| {
                target.is_some()
                    && alias
                        .as_deref()
                        .unwrap_or_else(|| target.as_deref().unwrap_or(""))
                        == last
            });
            let candidates: Vec<_> = aliases
                .iter()
                .filter(|((pkg, name), _)| {
                    if let Some((wanted_pkg, target, _)) = exact {
                        pkg == wanted_pkg && target.as_deref() == Some(name.as_str())
                    } else if imported_aliases
                        .iter()
                        .any(|(_, target, _)| target.is_none())
                    {
                        imported_aliases.iter().any(|(wildcard_pkg, target, _)| {
                            target.is_none() && wildcard_pkg == pkg
                        }) && name == last
                    } else {
                        pkg == &package && name == last
                    }
                })
                .map(|(_, definition)| definition)
                .collect();
            let lossy = candidates
                .iter()
                .any(|alias| alias_type_is_lossy(&alias.target.ty, &aliases, &mut Vec::new()));
            if lossy
                && let Some(decision) = plan
                    .declarations
                    .iter_mut()
                    .filter(|d| {
                        node.start_byte() >= d.id.start_byte && node.end_byte() <= d.id.end_byte
                    })
                    .min_by_key(|d| d.id.end_byte - d.id.start_byte)
            {
                if allow_approximations {
                    plan.diagnostics.push(SemanticDiagnostic {
                            code: "A001".into(),
                            message: format!("semantic loss allowed: type alias `{last}` expands to a Kotlin type without equivalent Java semantics"),
                            location: SourceLocation { file: decision.id.file.clone(), snapshot_hash: plan.source_hash, start_byte: node.start_byte(), end_byte: node.end_byte() },
                        });
                } else {
                    let reason = crate::translation_plan::RetentionReason::PreparationBlocker {
                        kind: crate::translation_plan::PreparationBlockerKind::SemanticLoss,
                        message: format!(
                            "semantic loss: type alias `{last}` expands to a Kotlin type without equivalent Java semantics"
                        ),
                    };
                    if !decision.retention_reasons.contains(&reason) {
                        decision.retention_reasons.push(reason);
                        decision.candidate_owner = crate::translation_plan::BackendOwner::Kotlin;
                    }
                }
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
}

/// Retain a declaration when its bare `Unit` type may bind to a source type
/// instead of Kotlin's builtin singleton. The Java type mapper receives only
/// the spelling, so this planning-time check uses the workspace snapshot and
/// the already-parsed source tree to avoid emitting `kotlin.Unit` for an
/// ambiguous source name.
pub fn plan_builtin_unit_type_bindings<P: SemanticProvider + ?Sized>(
    source: &str,
    tree: &tree_sitter::Tree,
    provider: &P,
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    if !source.contains("Unit") {
        return;
    }
    let package = package_name(source);
    let same_package_type = provider.symbols_named("Unit").into_iter().any(|symbol| {
        symbol.id.package == package
            && symbol.id.name == "Unit"
            && symbol.id.owner_path.is_empty()
            && is_type_symbol(&symbol.id)
    }) || provider.has_type_alias_in_package(&package, "Unit");
    let conflicting_import = source.lines().any(|line| {
        let Some(import) = line.trim().strip_prefix("import ") else {
            return false;
        };
        let (path, alias) = import
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((import.trim(), None));
        alias.is_none() && path.ends_with(".*") && path != "kotlin.*"
    });
    let conflicting_kotlin_prefix = source.lines().any(|line| {
        let Some(import) = line.trim().strip_prefix("import ") else {
            return false;
        };
        let (path, alias) = import
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((import.trim(), None));
        let imported_name = alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path));
        imported_name == "kotlin" && path != "kotlin"
    });
    if !same_package_type && !conflicting_import && !conflicting_kotlin_prefix {
        return;
    }

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "user_type" | "type_identifier" | "navigation_expression"
        ) {
            let spelling = node.utf8_text(source.as_bytes()).unwrap_or("").trim();
            let base = spelling.split(['<', '?']).next().unwrap_or(spelling).trim();
            let ambiguous_unit_type = (base == "Unit"
                && (conflicting_import
                    || same_package_type
                        && crate::transpiler::kt::is_canonical_unit_type(node, source)))
                || (base == "kotlin.Unit" && conflicting_kotlin_prefix);
            if ambiguous_unit_type
                && let Some(decision) = plan
                    .declarations
                    .iter_mut()
                    .filter(|decision| {
                        node.start_byte() >= decision.id.start_byte
                            && node.end_byte() <= decision.id.end_byte
                    })
                    .min_by_key(|decision| decision.id.end_byte - decision.id.start_byte)
            {
                let reason = crate::translation_plan::RetentionReason::PreparationBlocker {
                    kind: crate::translation_plan::PreparationBlockerKind::SemanticLoss,
                    message: "`Unit` type may bind through a workspace declaration or conflicting import, so the builtin Kotlin Unit mapping is not proven".into(),
                };
                if !decision.retention_reasons.contains(&reason) {
                    decision.retention_reasons.push(reason);
                }
                decision.candidate_owner = crate::translation_plan::BackendOwner::Kotlin;
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
}
fn alias_type_is_lossy(
    ty: &KotlinType,
    aliases: &BTreeMap<(String, String), TypeAliasDefinition>,
    stack: &mut Vec<String>,
) -> bool {
    match ty {
        KotlinType::Named { name, arguments } => {
            if matches!(
                name.as_str(),
                "UInt" | "ULong" | "UShort" | "UByte" | "KClass"
            ) {
                return true;
            }
            if !stack.contains(name)
                && let Some((_, alias)) = aliases
                    .iter()
                    .find(|((_, alias_name), _)| alias_name == name)
            {
                stack.push(name.clone());
                let result = alias_type_is_lossy(&alias.target.ty, aliases, stack);
                stack.pop();
                if result {
                    return true;
                }
            }
            arguments
                .iter()
                .filter_map(|a| a.ty.as_ref())
                .any(|t| alias_type_is_lossy(t, aliases, stack))
        }
        KotlinType::Nullable(t) | KotlinType::Array(t) => alias_type_is_lossy(t, aliases, stack),
        KotlinType::Function {
            receiver,
            parameters,
            result,
            ..
        } => {
            receiver
                .as_deref()
                .is_some_and(|t| alias_type_is_lossy(t, aliases, stack))
                || parameters
                    .iter()
                    .any(|t| alias_type_is_lossy(t, aliases, stack))
                || alias_type_is_lossy(result, aliases, stack)
        }
        _ => false,
    }
}

/// Index declaration ranges once rather than scanning every declaration for
/// every call and identifier in a source snapshot.
struct DeclarationOwnerIndex {
    spans: Vec<(usize, usize, usize)>,
    prefix_end: Vec<usize>,
}

impl DeclarationOwnerIndex {
    fn new(plan: &crate::translation_plan::TranslationPlan) -> Self {
        let mut spans = plan
            .declarations
            .iter()
            .enumerate()
            .map(|(index, declaration)| (declaration.id.start_byte, declaration.id.end_byte, index))
            .collect::<Vec<_>>();
        spans.sort_unstable();
        let mut furthest_end = 0;
        let prefix_end = spans
            .iter()
            .map(|(_, end, _)| {
                furthest_end = furthest_end.max(*end);
                furthest_end
            })
            .collect();
        Self { spans, prefix_end }
    }

    fn owner(&self, node: tree_sitter::Node<'_>) -> Option<usize> {
        let start = node.start_byte();
        let end = node.end_byte();
        let mut position = self
            .spans
            .partition_point(|(declaration_start, _, _)| *declaration_start <= start);
        let mut nearest = None;
        while position > 0 {
            position -= 1;
            if self.prefix_end[position] < end {
                break;
            }
            let (declaration_start, declaration_end, index) = self.spans[position];
            if declaration_end >= end {
                let candidate = (declaration_end - declaration_start, index);
                if nearest.is_none_or(|current| candidate < current) {
                    nearest = Some(candidate);
                }
            }
        }
        nearest.map(|(_, index)| index)
    }
}

/// Resolve required calls against declaration identities and established
/// receiver facts, retaining declarations when required facts are unknown.
pub fn plan_required_calls(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
    allow_approximations: bool,
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    let hash = *blake3::hash(source.as_bytes()).as_bytes();
    let owners = DeclarationOwnerIndex::new(plan);
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "call_expression" | "method_invocation") {
            let callee = call_callee_node(node);
            if let Some(callee) = callee {
                let name = callee.utf8_text(source.as_bytes()).unwrap_or("").to_owned();
                if !name.is_empty()
                    && !inside_annotation(node)
                    && let Some(index) = owners.owner(node)
                {
                    let locally_shadowed = is_proven_local_shadow(callee, &name, source);
                    let fact = resolve_required_call(source, file, node, callee, &name, provider);
                    if let FactStatus::Established(target) = &fact
                        && let Some(overload) =
                            overloads::establish(source, file, node, target, provider)
                    {
                        plan.exact_overload_calls.push(overload);
                    }
                    if matches!(fact, FactStatus::Established(_))
                        && let Some(receiver) = explicit_receiver_node(node, callee)
                        && let Some(access) =
                            inherited_property_receiver_access(source, receiver, file, provider)
                    {
                        plan.require_symbol(
                            index,
                            access.source_property.name.clone(),
                            FactStatus::Established(access.source_property),
                        );
                        plan.require_symbol(
                            index,
                            access.getter_contract.name.clone(),
                            FactStatus::Established(access.getter_contract),
                        );
                    }
                    let generated_data_copy = matches!(fact, FactStatus::Unknown)
                        && !locally_shadowed
                        && known_data_class_copy(source, file, node, callee, &name, provider);
                    let location = SourceLocation {
                        file: file.to_path_buf(),
                        snapshot_hash: hash,
                        start_byte: node.start_byte(),
                        end_byte: node.end_byte(),
                    };
                    let planned_todo = name == "TODO"
                        && plan
                            .todo_lowerings
                            .iter()
                            .any(|lowering| lowering.location == location);
                    let planned_eager_collection = plan.eager_collection_calls.iter().any(|fact| {
                        fact.location.file == location.file
                            && fact.location.snapshot_hash == location.snapshot_hash
                            && fact.location.start_byte == location.start_byte
                            && fact.location.end_byte == location.end_byte
                    });
                    let builtin_call_is_proven = planned_todo
                        || planned_eager_collection
                        || known_builtin(&name)
                            && matches!(fact, FactStatus::Unknown)
                            && !locally_shadowed
                            && !has_wildcard_import(source)
                            && (!is_lowered_collection_factory(&name)
                                || known_collection_factory_shape(
                                    source, node, callee, &name, provider,
                                ));
                    let external_call_is_proven = matches!(fact, FactStatus::Unknown)
                        && !locally_shadowed
                        && (established_platform_constructor(source, file, node, provider)
                            .is_some()
                            || known_collection_factory_from_java_util_wildcard(
                                source, node, callee, &name, provider,
                            ))
                        || matches!(fact, FactStatus::Unknown)
                            && !locally_shadowed
                            && known_external_member_call(
                                source, file, node, callee, &name, provider,
                            );
                    if !(generated_data_copy || builtin_call_is_proven || external_call_is_proven) {
                        match &fact {
                            FactStatus::Unknown | FactStatus::Inferred(_)
                                if allow_approximations =>
                            {
                                let from = plan.declarations[index].symbol_id.clone();
                                plan.dependencies
                                    .push(crate::translation_plan::SymbolDependency {
                                        from,
                                        spelling: name.clone(),
                                        resolution: fact.clone(),
                                    });
                                plan.diagnostics.push(SemanticDiagnostic {
                                    code: "A001".into(),
                                    message: format!(
                                        "call `{name}` is unresolved; approximation was allowed"
                                    ),
                                    location,
                                });
                            }
                            FactStatus::Unknown | FactStatus::Inferred(_) => {
                                plan.require_symbol(index, name.clone(), fact.clone());
                                plan.diagnostics.push(SemanticDiagnostic {
                                    code: "S001".into(),
                                    message: format!("call `{name}` is unresolved"),
                                    location,
                                });
                            }
                            FactStatus::Ambiguous(candidates) => {
                                plan.require_symbol(index, name.clone(), fact.clone());
                                plan.diagnostics.push(SemanticDiagnostic {
                                    code: "S002".into(),
                                    message: format!(
                                        "call `{name}` matches {} declarations",
                                        candidates.len()
                                    ),
                                    location,
                                });
                            }
                            FactStatus::Established(_) => {
                                plan.require_symbol(index, name.clone(), fact.clone());
                                if known_builtin(&name) {
                                    let decision = &mut plan.declarations[index];
                                    let reason = crate::translation_plan::RetentionReason::PreparationBlocker {
                                        kind: crate::translation_plan::PreparationBlockerKind::UnresolvedAssumption,
                                        message: format!("user declaration `{name}` shadows a builtin call lowered specially by the Java backend"),
                                    };
                                    if !decision.retention_reasons.contains(&reason) {
                                        decision.retention_reasons.push(reason);
                                        decision.candidate_owner =
                                            crate::translation_plan::BackendOwner::Kotlin;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut c = node.walk();
        stack.extend(node.named_children(&mut c));
    }
}
fn inside_annotation(mut node: tree_sitter::Node) -> bool {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "annotation" | "annotation_entry" | "annotation_use_site_target"
        ) {
            return true;
        }
        node = parent
    }
    false
}
pub(crate) fn resolve_required_call<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node,
    callee: tree_sitter::Node,
    name: &str,
    provider: &P,
) -> FactStatus<SymbolId> {
    if is_proven_local_shadow(callee, name, source) {
        return FactStatus::Unknown;
    }
    if let Some(fact) = reified_calls::establish(source, file, call, provider) {
        return FactStatus::Established(fact.source_target);
    }
    if let Some(receiver) = explicit_receiver_node(call, callee)
        && receiver.kind() == "super_expression"
    {
        return resolve_super_call(source, file, call, receiver, name, provider);
    }
    let symbols = provider.symbols_named(name);
    if (call.kind() == "method_invocation" && call.child_by_field_name("object").is_some())
        || (call.kind() == "call_expression"
            && callee
                .parent()
                .is_some_and(|parent| parent.kind() == "navigation_expression"))
    {
        if let Some(receiver) = explicit_receiver_node(call, callee)
            && let Some(access) =
                inherited_property_receiver_access(source, receiver, file, provider)
        {
            return resolve_public_instance_call(&access.value_owner, call, name, provider);
        }
        if let Some(receiver_type) = explicit_receiver_type(source, call, callee, file, provider) {
            let package = package_name(source);
            let owner_name = receiver_type.rsplit('.').next().unwrap_or(&receiver_type);
            let type_qualifier =
                explicit_receiver_is_type_qualifier(source, call, callee, file, provider);
            let java_static = symbols
                .iter()
                .filter(|symbol| {
                    (type_qualifier || call.kind() == "method_invocation")
                        && symbol.id.package == package
                        && symbol.id.owner_path.len() == 1
                        && symbol.id.owner_path[0] == owner_name
                        && symbol.id.name == name
                        && symbol
                            .id
                            .file
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
                        && matches!(symbol.id.kind.as_str(), "function" | "method")
                        && provider.is_java_static_member(&symbol.id)
                })
                .map(|symbol| symbol.id.clone())
                .collect::<Vec<_>>();
            if !java_static.is_empty() {
                let mut candidates = java_static;
                retain_matching_arity(&mut candidates, call);
                return resolved_call_candidates(source, file, call, candidates, provider);
            }
            if let FactStatus::Established(receiver) =
                provider.resolve_type_in_file(file, &package, &receiver_type)
            {
                let mut path = receiver.owner_path.clone();
                path.push(receiver.name.clone());
                let direct = if type_qualifier {
                    Vec::new()
                } else {
                    symbols
                        .iter()
                        .filter(|symbol| {
                            symbol.id.file == receiver.file
                                && symbol.id.owner_path == path
                                && matches!(symbol.id.kind.as_str(), "function" | "method")
                        })
                        .map(|symbol| symbol.id.clone())
                        .collect::<Vec<_>>()
                };
                let mut candidates = if !direct.is_empty() {
                    direct
                } else {
                    let companion_members = if type_qualifier {
                        provider
                            .companion_members(&receiver, name)
                            .into_iter()
                            .map(|symbol| symbol.id.clone())
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };
                    if !companion_members.is_empty() || type_qualifier {
                        companion_members
                    } else {
                        provider
                            .inherited_members(&receiver, name)
                            .into_iter()
                            .map(|symbol| symbol.id.clone())
                            .collect()
                    }
                };
                retain_matching_arity(&mut candidates, call);
                return resolved_call_candidates(source, file, call, candidates, provider);
            }
        }
        // Generic parameter spellings are not exact receiver types. Resolve
        // their classifier only for this source member lookup, keeping them
        // out of exact-type/standard-library lowering facts.
        if let Some(receiver) = explicit_receiver_node(call, callee)
            && matches!(receiver.kind(), "identifier" | "simple_identifier")
            && let Ok(spelling) = receiver.utf8_text(source.as_bytes())
            && let Some(owner) =
                lexical_parameter_classifier(source, call, spelling, file, provider)
        {
            return resolve_public_instance_call(&owner, call, name, provider);
        }
        return resolve_capability_receiver_call(source, file, call, callee, name, provider);
    }
    // Resolve local declarations in the nearest enclosing callable first.
    let mut scope = call.parent();
    while let Some(owner) = scope {
        if owner.kind() == "function_declaration" {
            let id = symbol_id_for_node(source, owner, file);
            let mut expected = id.owner_path;
            expected.push(owner_segment(source, owner));
            let local: Vec<_> = symbols
                .iter()
                .filter(|s| {
                    s.id.file == file
                        && s.id.name == name
                        && s.id.owner_path == expected
                        && matches!(
                            s.id.kind.as_str(),
                            "function" | "method" | "class" | "object" | "enum" | "annotation"
                        )
                })
                .map(|s| s.id.clone())
                .collect();
            if !local.is_empty() {
                return resolved_call_candidates(source, file, call, local, provider);
            }
        }
        scope = owner.parent();
    }
    let mut ancestor = call.parent();
    while let Some(owner) = ancestor {
        if matches!(owner.kind(), "class_declaration" | "object_declaration") {
            let owner_id = symbol_id_for_node(source, owner, file);
            let mut expected = owner_id.owner_path.clone();
            expected.push(owner_id.name.clone());
            let members: Vec<_> = symbols
                .iter()
                .filter(|s| {
                    s.id.file == file
                        && s.id.name == name
                        && s.id.owner_path == expected
                        && matches!(
                            s.id.kind.as_str(),
                            "function" | "class" | "object" | "enum" | "annotation"
                        )
                })
                .map(|s| s.id.clone())
                .collect();
            let shadowing_properties = symbols.iter().any(|s| {
                s.id.file == file
                    && s.id.name == name
                    && s.id.kind == "property"
                    && s.id.owner_path == expected
            });
            if shadowing_properties {
                return FactStatus::Unknown;
            }
            if !members.is_empty() {
                return resolved_call_candidates(source, file, call, members, provider);
            }
            let owner_candidates = provider
                .symbols_in_file(file)
                .into_iter()
                .filter(|symbol| {
                    symbol.id.name == owner_id.name
                        && symbol.id.owner_path == owner_id.owner_path
                        && is_type_symbol(&symbol.id)
                })
                .map(|symbol| symbol.id.clone())
                .collect::<Vec<_>>();
            if let FactStatus::Established(owner_symbol) = fact_from_candidates(owner_candidates) {
                let mut inherited = provider
                    .inherited_members(&owner_symbol, name)
                    .into_iter()
                    .map(|symbol| symbol.id.clone())
                    .collect::<Vec<_>>();
                if !inherited.is_empty() {
                    retain_matching_arity(&mut inherited, call);
                    if inherited.is_empty() && call_argument_count(call) == 0 {
                        if !inherited_calls::outside_constructor_header(call)
                            || enclosing_dispatch_owner(source, call, file, provider).as_ref()
                                != Some(&owner_symbol)
                        {
                            return FactStatus::Unknown;
                        }
                        return provider.resolve_inherited_noarg_call(&owner_symbol, name);
                    }
                    return resolved_call_candidates(source, file, call, inherited, provider);
                }
            }
        }
        ancestor = owner.parent();
    }
    let package = package_name(source);
    let imports = source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("import "))
        .filter_map(|line| {
            let (path, alias) = line
                .split_once(" as ")
                .map(|(p, a)| (p.trim(), Some(a.trim())))
                .unwrap_or((line, None));
            let (pkg, imported_name) = path.rsplit_once('.')?;
            Some((
                pkg.to_owned(),
                imported_name.to_owned(),
                alias.map(str::to_owned),
            ))
        })
        .collect::<Vec<_>>();
    let imported = imports.iter().find(|(_, imported_name, alias)| {
        imported_name != "*" && alias.as_deref().unwrap_or(imported_name) == name
    });
    let collect = |wanted_package: &str, wanted_name: &str| {
        provider
            .symbols_named(wanted_name)
            .into_iter()
            .filter(|s| {
                s.id.package == wanted_package
                    && s.id.name == wanted_name
                    && s.id.owner_path.is_empty()
                    && matches!(
                        s.id.kind.as_str(),
                        "function" | "class" | "object" | "enum" | "annotation"
                    )
            })
            .map(|s| s.id.clone())
            .collect::<Vec<_>>()
    };
    let candidates = if let Some((wanted_package, imported_name, _)) = imported {
        collect(wanted_package, imported_name)
    } else {
        let local = collect(&package, name);
        if !local.is_empty() {
            local
        } else {
            imports
                .iter()
                .filter(|(_, imported_name, _)| imported_name == "*")
                .flat_map(|(wildcard_package, _, _)| collect(wildcard_package, name))
                .collect::<Vec<_>>()
        }
    };
    if !candidates.is_empty() {
        resolved_call_candidates(source, file, call, candidates, provider)
    } else {
        FactStatus::Unknown
    }
}

fn resolved_call_candidates<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    mut candidates: Vec<SymbolId>,
    provider: &P,
) -> FactStatus<SymbolId> {
    candidates.sort();
    candidates.dedup();
    if let Some(selected) =
        overloads::filter_exact_applicable(source, file, call, &candidates, provider)
    {
        return FactStatus::Established(selected);
    }
    fact_from_candidates(candidates)
}

/// Resolve a no-argument super call against the indexed direct parents of the
/// nearest lexical class, respecting an explicit supertype qualifier. The emitter still decides
/// whether a resolved Kotlin default method has a Java-visible bridge.
fn resolve_super_call<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    receiver: tree_sitter::Node<'_>,
    name: &str,
    provider: &P,
) -> FactStatus<SymbolId> {
    let receiver_text = receiver.utf8_text(source.as_bytes()).unwrap_or_default();
    if call_argument_count(call) != 0 || receiver_text.contains('@') {
        return FactStatus::Unknown;
    }
    let mut cursor = receiver.parent();
    let mut lexical_class = None;
    while let Some(node) = cursor {
        if matches!(node.kind(), "object_literal" | "anonymous_object") {
            return FactStatus::Unknown;
        }
        if matches!(node.kind(), "object_declaration" | "class_declaration") {
            lexical_class = Some(node);
            break;
        }
        cursor = node.parent();
    }
    let Some(class_node) = lexical_class else {
        return FactStatus::Unknown;
    };
    let class_id = symbol_id_for_node(source, class_node, file);
    let owners = provider
        .symbols_in_file(file)
        .into_iter()
        .filter(|symbol| symbol.id == class_id && is_type_symbol(&symbol.id))
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    let FactStatus::Established(owner) = fact_from_candidates(owners) else {
        return FactStatus::Unknown;
    };
    if !provider.has_complete_inheritance(&owner) {
        return FactStatus::Unknown;
    }
    let FactStatus::Established(mut direct_parents) = provider.instance_contract_parents(&owner)
    else {
        return FactStatus::Unknown;
    };
    if direct_parents.is_empty() {
        return FactStatus::Unknown;
    }
    if receiver_text != "super" {
        let qualifier = receiver
            .named_children(&mut receiver.walk())
            .find(|child| child.kind() == "user_type");
        let Some(qualifier) = qualifier.and_then(|node| node.utf8_text(source.as_bytes()).ok())
        else {
            return FactStatus::Unknown;
        };
        let FactStatus::Established(parent) =
            provider.resolve_type_in_file(file, &package_name(source), qualifier)
        else {
            return FactStatus::Unknown;
        };
        if !direct_parents.contains(&parent) {
            return FactStatus::Unknown;
        }
        direct_parents = vec![parent];
    }

    // Search every branch independently, including forks farther up the tree.
    // A direct declaration hides its own ancestors only. Memoization avoids
    // repeated work through diamonds; an inheritance cycle proves nothing.
    fn lookup<P: SemanticProvider + ?Sized>(
        owner: &SymbolId,
        name: &str,
        provider: &P,
        active: &mut BTreeSet<SymbolId>,
        memo: &mut BTreeMap<SymbolId, Option<Vec<SymbolId>>>,
    ) -> Option<Vec<SymbolId>> {
        if let Some(result) = memo.get(owner) {
            return result.clone();
        }
        if !active.insert(owner.clone()) {
            return None;
        }
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        let declarations = provider
            .symbols_named(name)
            .into_iter()
            .filter(|symbol| {
                symbol.id.file == owner.file
                    && symbol.id.owner_path == path
                    && matches!(symbol.id.kind.as_str(), "function" | "method")
                    && symbol.id.parameters.is_empty()
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        let result = if !declarations.is_empty() {
            declarations
                .iter()
                .all(|member| {
                    provider.is_public_instance_callable(member)
                        && provider.has_stable_noarg_jvm_signature(member)
                })
                .then_some(declarations)
        } else if let FactStatus::Established(parents) = provider.instance_contract_parents(owner) {
            let mut inherited = Vec::new();
            let mut complete = true;
            for parent in parents {
                match lookup(&parent, name, provider, active, memo) {
                    Some(candidates) => inherited.extend(candidates),
                    None => {
                        complete = false;
                        break;
                    }
                }
            }
            complete.then_some(inherited)
        } else {
            None
        };
        active.remove(owner);
        memo.insert(owner.clone(), result.clone());
        result
    }
    let mut branch_candidates = Vec::new();
    let mut active = BTreeSet::new();
    let mut memo = BTreeMap::new();
    for parent in direct_parents {
        let Some(candidates) = lookup(&parent, name, provider, &mut active, &mut memo) else {
            return FactStatus::Unknown;
        };
        branch_candidates.extend(candidates);
    }
    fact_from_candidates(branch_candidates)
}

fn is_type_symbol(id: &SymbolId) -> bool {
    matches!(
        id.kind.as_str(),
        "class" | "interface" | "object" | "enum" | "annotation" | "record"
    )
}

fn parse_source(file: &Path, source: &str) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    let language = if file
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
    {
        tree_sitter_java::LANGUAGE.into()
    } else {
        tree_sitter_kotlin_ng::LANGUAGE.into()
    };
    parser.set_language(&language).ok()?;
    parser.parse(source, None)
}

fn import_paths(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let line = line.trim().strip_prefix("import ")?;
            let path = line
                .split(" as ")
                .next()
                .unwrap_or(line)
                .trim()
                .trim_end_matches(';')
                .trim();
            Some(path.to_owned())
        })
        .collect()
}

fn has_java_modifier(node: tree_sitter::Node<'_>, keyword: &str) -> bool {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == "modifiers")
        .is_some_and(|modifiers| {
            let mut cursor = modifiers.walk();
            modifiers
                .children(&mut cursor)
                .any(|child| child.kind() == keyword)
        })
}

fn has_java_static_modifier(source: &str, node: tree_sitter::Node<'_>) -> bool {
    let Some(name) = node.child_by_field_name("name") else {
        return false;
    };
    source
        .get(node.start_byte()..name.start_byte())
        .is_some_and(|prefix| {
            prefix
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .any(|word| word == "static")
        })
}

fn declaration_supertypes(source: &str, node: tree_sitter::Node<'_>, file: &Path) -> Vec<String> {
    let java = file
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("java"));
    let fields: &[&str] = if java {
        &[
            "superclass",
            "interfaces",
            "super_interfaces",
            "extends_interfaces",
        ]
    } else {
        &["delegation_specifiers"]
    };
    let mut names = Vec::new();
    for field in fields {
        let container = node.child_by_field_name(field).or_else(|| {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .find(|child| child.kind() == *field)
        });
        let Some(container) = container else { continue };
        let mut stack = vec![container];
        while let Some(item) = stack.pop() {
            if matches!(
                item.kind(),
                "user_type" | "type_identifier" | "scoped_type_identifier" | "generic_type"
            ) && let Ok(text) = item.utf8_text(source.as_bytes())
            {
                let mut value = text.trim().to_owned();
                if let Some(index) = value.find('(') {
                    value.truncate(index);
                }
                value = value.split('<').next().unwrap_or(&value).trim().to_owned();
                if !value.is_empty() {
                    names.push(value);
                }
                continue;
            }
            let mut cursor = item.walk();
            stack.extend(item.named_children(&mut cursor));
        }
        if java && *field == "interfaces" && names.last().is_some() {
            // The Java grammar may expose a comma-separated list as a single
            // type node; each named child above is still handled separately.
        }
    }
    names.sort();
    names.dedup();
    names
}

fn call_callee_node(call: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    if let Some(name) = call.child_by_field_name("name") {
        return Some(name);
    }
    let mut cursor = call.walk();
    let first = call
        .named_children(&mut cursor)
        .find(|child| !matches!(child.kind(), "value_arguments" | "arguments"))?;
    let mut stack = vec![first];
    let mut last = None;
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "simple_identifier" | "identifier")
            && last.is_none_or(|previous: tree_sitter::Node<'_>| {
                node.start_byte() > previous.start_byte()
            })
        {
            last = Some(node);
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    last
}

fn known_data_class_copy<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    callee: tree_sitter::Node<'_>,
    name: &str,
    provider: &P,
) -> bool {
    if name != "copy" {
        return false;
    }
    let nav = callee
        .parent()
        .filter(|parent| parent.kind() == "navigation_expression");
    let mut explicit_owner = None;
    if let Some(nav) = nav {
        let nav_text = nav.utf8_text(source.as_bytes()).unwrap_or("").trim();
        if nav_text.contains("?.") {
            return false;
        }
        let Some((receiver, member)) = nav_text.rsplit_once('.') else {
            return false;
        };
        if member.trim() != "copy" {
            return false;
        }
        if receiver.trim().starts_with("this@") {
            return false;
        }
        if receiver.trim() != "this" {
            let Some(receiver_type) = explicit_receiver_type(source, call, callee, file, provider)
            else {
                return false;
            };
            let simple = receiver_type
                .split('<')
                .next()
                .unwrap_or(&receiver_type)
                .trim()
                .rsplit('.')
                .next()
                .unwrap_or("");
            let FactStatus::Established(id) =
                provider.resolve_type_in_file(file, &package_name(source), simple)
            else {
                return false;
            };
            if id.file != file || id.kind != "class" {
                return false;
            }
            explicit_owner = Some(id.name);
        }
    }
    if let Some(owner) = explicit_owner {
        let mut root = call;
        while let Some(parent) = root.parent() {
            root = parent;
        }
        let mut stack = vec![root];
        let mut matches = Vec::new();
        while let Some(node) = stack.pop() {
            if node.kind() == "class_declaration"
                && node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                    == Some(owner.as_str())
            {
                let is_data = node
                    .named_children(&mut node.walk())
                    .find(|child| child.kind() == "modifiers")
                    .and_then(|modifiers| modifiers.utf8_text(source.as_bytes()).ok())
                    .is_some_and(|modifiers| {
                        modifiers.split_whitespace().any(|word| word == "data")
                    });
                matches.push(is_data);
            }
            stack.extend(node.children(&mut node.walk()));
        }
        return matches.as_slice() == [true];
    }
    let mut parent = call.parent();
    while let Some(node) = parent {
        if node.kind() == "class_declaration" {
            let is_data = node
                .named_children(&mut node.walk())
                .find(|child| child.kind() == "modifiers")
                .and_then(|modifiers| modifiers.utf8_text(source.as_bytes()).ok())
                .is_some_and(|modifiers| modifiers.split_whitespace().any(|word| word == "data"));
            return is_data;
        }
        if matches!(
            node.kind(),
            "function_declaration" | "getter" | "setter" | "lambda_literal"
        ) {
            // Continue through callable scopes; they do not change the implicit
            // dispatch receiver. An intervening local callable named `copy`
            // is handled by `is_proven_local_shadow` at the call site.
        }
        parent = node.parent();
    }
    false
}

fn explicit_receiver_type<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    callee: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<String> {
    if call.kind() == "call_expression" && explicit_receiver_node(call, callee).is_none() {
        return indexed_call_return_type(source, call, callee, file, provider);
    }
    let receiver = explicit_receiver_node(call, callee)?;
    if matches!(receiver.kind(), "identifier" | "simple_identifier")
        && let Ok(name) = receiver.utf8_text(source.as_bytes())
        && let Some(ty) = container_types::parameter_type(source, file, call, name, provider)
    {
        return match ty.ty {
            KotlinType::Named { name, .. } if !ty.nullable => Some(name),
            KotlinType::Primitive(name) if !ty.nullable => Some(format!("kotlin.{name}")),
            _ => None,
        };
    }
    if matches!(receiver.kind(), "identifier" | "simple_identifier")
        && let Ok(name) = receiver.utf8_text(source.as_bytes())
        && lexical_parameter_is_declared(source, call, name)
        && lexical_parameter_type(source, call, name).is_none_or(|ty| ty.nullable)
    {
        return None;
    }
    if let Some(property_type) =
        established_direct_property_receiver_type(source, call, receiver, file, provider)
    {
        return Some(property_type);
    }
    if matches!(receiver.kind(), "string_literal" | "string_template") {
        return Some("java.lang.String".into());
    }
    // A constructor expression carries a stronger receiver type than its
    // spelling. Resolve only a uniquely indexed type; a same-named function
    // or unknown call result is deliberately left unresolved.
    if receiver.kind() == "call_expression" {
        let constructor = call_callee_node(receiver)?;
        let name = constructor.utf8_text(source.as_bytes()).ok()?.trim();
        if let Some(return_type) =
            indexed_call_return_type(source, receiver, constructor, file, provider)
        {
            return Some(return_type);
        }
        if let Some(result) = platform_members::establish(source, file, receiver, provider) {
            return Some(result.to_owned());
        }
        if let Some(inner_receiver_type) =
            explicit_receiver_type(source, receiver, constructor, file, provider)
            && let Some(inner_type) = standard_external_type(
                source,
                provider,
                &inner_receiver_type,
                receiver.kind() == "method_invocation",
            )
        {
            let arity = call_argument_count(receiver);
            let result = match (inner_type.as_str(), name, arity) {
                ("java.lang.String", "split", 1)
                    if known_string_split_argument(source, file, receiver, provider) =>
                {
                    Some("java.util.List")
                }
                ("java.util.stream.Stream", "filter", 1) => Some("java.util.stream.Stream"),
                (
                    "java.util.List"
                    | "kotlin.collections.List"
                    | "kotlin.collections.MutableList"
                    | "java.util.Set"
                    | "kotlin.collections.Set"
                    | "kotlin.collections.MutableSet"
                    | "java.util.Map"
                    | "kotlin.collections.Map"
                    | "kotlin.collections.MutableMap",
                    "stream",
                    0,
                ) => Some("java.util.stream.Stream"),
                _ => None,
            };
            if let Some(result) = result {
                return Some(result.to_owned());
            }
        }
        let package = package_name(source);
        let competing_function = provider.symbols_named(name).into_iter().any(|symbol| {
            !is_type_symbol(&symbol.id)
                && matches!(symbol.id.kind.as_str(), "function" | "method")
                && ((symbol.id.file == file && symbol.location.start_byte <= call.start_byte())
                    || (symbol.id.package == package && symbol.id.owner_path.is_empty()))
        });
        if !name.is_empty()
            && name.chars().all(|c| c.is_alphanumeric() || c == '_')
            && !competing_function
            && let FactStatus::Established(symbol) =
                provider.resolve_type_in_file(file, &package, name)
        {
            let mut qualified = symbol.owner_path;
            qualified.push(symbol.name);
            return Some(qualified.join("."));
        }
        if !competing_function
            && let Some(standard) = standard_external_type(
                source,
                provider,
                name,
                receiver.kind() == "method_invocation",
            )
        {
            return Some(standard);
        }
        return None;
    }
    let spelling = explicit_receiver_spelling(source, call, callee)?;
    if spelling.is_empty()
        || spelling.contains('.')
        || !spelling.chars().all(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    if spelling.chars().next().is_some_and(char::is_uppercase)
        && explicit_receiver_node(call, callee)
            .is_some_and(|receiver| is_proven_local_shadow(receiver, spelling, source))
    {
        return None;
    }
    let locally_shadowed = is_proven_local_shadow(receiver, spelling, source);
    let indexed = provider
        .symbols_named(spelling)
        .into_iter()
        // Indexed class properties cannot supply a type for a same-spelled
        // local or formal parameter. Resolve those from their lexical scope.
        .filter(|_| !locally_shadowed)
        .filter(|symbol| symbol.id.file == file && symbol.location.start_byte <= call.start_byte())
        .filter_map(|symbol| match &symbol.ty {
            FactStatus::Established(TypeRef {
                ty: KotlinType::Named { name, .. },
                nullable,
            }) => Some((
                symbol.location.start_byte,
                (!nullable).then(|| name.clone()),
            )),
            _ => None,
        })
        .max_by_key(|(start, _)| *start)
        .map(|(_, name)| name);
    if let Some(indexed) = indexed {
        // A known nullable binding must not fall through to a class qualifier
        // or a spelling-only library fact. Kotlin permits null-safe extensions
        // such as Any?.toString(), whereas a Java dereference would throw.
        return indexed;
    }
    let has_untyped_indexed_binding = provider.symbols_named(spelling).into_iter().any(|symbol| {
        symbol.id.file == file
            && symbol.location.start_byte <= call.start_byte()
            && !is_type_symbol(&symbol.id)
    });
    let mut enclosing = call.parent();
    while let Some(node) = enclosing {
        if matches!(
            node.kind(),
            "method_declaration"
                | "function_declaration"
                | "constructor_declaration"
                | "secondary_constructor"
        ) {
            break;
        }
        enclosing = node.parent();
    }
    let scope = enclosing.unwrap_or_else(|| {
        let mut root = call;
        while let Some(parent) = root.parent() {
            root = parent;
        }
        root
    });
    let mut stack = vec![scope];
    let mut candidate: Option<(usize, String)> = None;
    while let Some(node) = stack.pop() {
        if node.start_byte() < call.start_byte()
            && matches!(
                node.kind(),
                "local_variable_declaration"
                    | "formal_parameter"
                    | "field_declaration"
                    | "parameter"
                    | "property_declaration"
                    | "class_parameter"
            )
        {
            let ty = node
                .child_by_field_name("type")
                .or_else(|| named_type_child(node))
                .or_else(|| {
                    if node.kind() == "property_declaration" {
                        let mut cursor = node.walk();
                        node.named_children(&mut cursor)
                            .find(|child| child.kind() == "variable_declaration")
                            .and_then(|variable| {
                                variable
                                    .child_by_field_name("type")
                                    .or_else(|| named_type_child(variable))
                            })
                    } else {
                        None
                    }
                })
                .and_then(|type_node| type_node.utf8_text(source.as_bytes()).ok());
            // Match the binding's declared LHS name only. Searching its whole
            // initializer would mistake `val id = UUID.randomUUID()` for a
            // local binding named UUID and recursively infer the same call.
            let binding = match node.kind() {
                "property_declaration" => {
                    let mut cursor = node.walk();
                    node.named_children(&mut cursor)
                        .find(|child| child.kind() == "variable_declaration")
                        .and_then(|child| node_name(child, source))
                }
                "local_variable_declaration" | "field_declaration" => {
                    let mut cursor = node.walk();
                    node.named_children(&mut cursor)
                        .filter(|child| child.kind() == "variable_declarator")
                        .filter_map(|child| node_name(child, source))
                        .find(|name| name == spelling)
                }
                _ => node_name(node, source),
            };
            let has_name = binding.as_deref() == Some(spelling);
            let inferred_platform_type = if has_name && ty.is_none() {
                inferred_platform_initializer_type(source, file, node, provider)
            } else {
                None
            };
            if has_name && let Some(ty) = ty.or(inferred_platform_type.as_deref()) {
                let depth = node.start_byte();
                if candidate
                    .as_ref()
                    .is_none_or(|(current, _)| depth > *current)
                {
                    candidate = Some((depth, ty.to_owned()));
                }
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    if let Some((_, ty)) = candidate {
        return (!TypeRef::parse(&ty).nullable).then(|| ty.trim().to_owned());
    }
    // A binding whose initializer has no proven type must still shadow
    // an imported class with the same uppercase name.
    (!has_untyped_indexed_binding && spelling.chars().next().is_some_and(char::is_uppercase))
        .then(|| spelling.to_owned())
}

/// Prove a positional call to this class's own ordinary companion function.
/// Such functions become static methods and are legal in `this(...)` arguments.
pub(crate) fn established_own_companion_delegation_call<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> bool {
    let Some(callee) = call_callee_node(call) else {
        return false;
    };
    if call.kind() != "call_expression"
        || !matches!(callee.kind(), "identifier" | "simple_identifier")
        || explicit_receiver_node(call, callee).is_some()
        || call.named_children(&mut call.walk()).any(|child| {
            !child.is_extra()
                && !matches!(
                    child.kind(),
                    "identifier" | "simple_identifier" | "value_arguments"
                )
        })
    {
        return false;
    }
    let Some(owner) = enclosing_dispatch_owner(source, call, file, provider) else {
        return false;
    };
    let name = callee.utf8_text(source.as_bytes()).unwrap_or("");
    let FactStatus::Established(target) =
        resolve_required_call(source, file, call, callee, name, provider)
    else {
        return false;
    };
    let mut path = owner.owner_path.clone();
    path.push(owner.name.clone());
    if target.file != owner.file
        || target.owner_path != path
        || target.parameters.len() != call_argument_count(call)
        || target.parameters.iter().any(|ty| ty.starts_with("vararg:"))
        || !provider.has_default_companion_jvm_abi(&target)
    {
        return false;
    }
    let Some(arguments) = call
        .named_children(&mut call.walk())
        .find(|child| child.kind() == "value_arguments")
    else {
        return false;
    };
    for argument in arguments
        .named_children(&mut arguments.walk())
        .filter(|node| !node.is_extra())
    {
        let values = argument
            .named_children(&mut argument.walk())
            .filter(|node| !node.is_extra())
            .collect::<Vec<_>>();
        let [value] = values.as_slice() else {
            return false;
        };
        if !plain_companion_delegation_argument(source, *value, file, provider) {
            return false;
        }
    }
    let mut stack = vec![call];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "value_argument" | "value_arguments")
            && node
                .children(&mut node.walk())
                .any(|child| matches!(child.kind(), "=" | "spread_expression" | "*"))
        {
            return false;
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    let candidates = provider.symbols_named(name);
    let matches = candidates
        .iter()
        .filter(|symbol| symbol.id == target)
        .collect::<Vec<_>>();
    let [symbol] = matches.as_slice() else {
        return false;
    };
    let mut root = call;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let Some(declaration) =
        root.named_descendant_for_byte_range(symbol.location.start_byte, symbol.location.end_byte)
    else {
        return false;
    };
    if declaration.kind() != "function_declaration" {
        return false;
    }
    let Some(parameters) = declaration
        .named_children(&mut declaration.walk())
        .find(|child| child.kind() == "function_value_parameters")
    else {
        return false;
    };
    let mut stack = vec![parameters];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "=" | "vararg")
            || (node.kind() == "parameter_modifier"
                && node.utf8_text(source.as_bytes()).ok() == Some("vararg"))
        {
            return false;
        }
        stack.extend(node.children(&mut node.walk()));
    }
    true
}

fn plain_companion_delegation_argument<P: SemanticProvider + ?Sized>(
    source: &str,
    value: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> bool {
    match value.kind() {
        "identifier" | "simple_identifier" => lexical_parameter_type(
            source,
            value,
            value.utf8_text(source.as_bytes()).unwrap_or(""),
        )
        .is_some(),
        "number_literal" | "boolean_literal" | "hex_literal" | "long_literal" | "real_literal"
        | "float_literal" | "null_literal" | "string_literal" => true,
        "call_expression" => {
            established_projected_receiver_instance_call(source, value, file, provider)
                || established_own_companion_delegation_call(source, value, file, provider)
        }
        // Bare property arguments need the same accessor proof as method
        // receiver projections; a source type alone does not establish it.
        _ => false,
    }
}

/// Prove a no-argument instance call through a typed property or direct
/// getter call, including each method's visibility and unmangled JVM name.
/// The getter form also covers property repairs in speculative snapshots.
pub(crate) fn established_projected_receiver_instance_call<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> bool {
    let Some(callee) = call_callee_node(call) else {
        return false;
    };
    let Some(receiver) = explicit_receiver_node(call, callee) else {
        return false;
    };
    if call_argument_count(call) != 0 {
        return false;
    }
    if !plain_parameter_property_projection(source, call, receiver, file, provider) {
        if receiver.kind() != "call_expression" || call_argument_count(receiver) != 0 {
            return false;
        }
        let Some(inner_callee) = call_callee_node(receiver) else {
            return false;
        };
        let Some(base) = explicit_receiver_node(receiver, inner_callee)
            .filter(|base| matches!(base.kind(), "identifier" | "simple_identifier"))
        else {
            return false;
        };
        let Ok(spelling) = base.utf8_text(source.as_bytes()) else {
            return false;
        };
        // Re-establish the parameter binding after a property repair. The
        // older textual receiver inference alone does not distinguish a
        // vararg's array from its element classifier.
        if lexical_parameter_classifier(source, call, spelling, file, provider).is_none()
            || indexed_call_return_type(source, receiver, inner_callee, file, provider).is_none()
        {
            return false;
        }
        let Ok(name) = inner_callee.utf8_text(source.as_bytes()) else {
            return false;
        };
        if !matches!(resolve_required_call(source, file, receiver, inner_callee, name, provider),
            FactStatus::Established(member)
                if member.parameters.is_empty() && provider.is_public_instance_callable(&member))
        {
            return false;
        }
    }
    let Ok(name) = callee.utf8_text(source.as_bytes()) else {
        return false;
    };
    matches!(resolve_required_call(source, file, call, callee, name, provider),
        FactStatus::Established(member)
            if member.parameters.is_empty() && provider.is_public_instance_callable(&member))
}

fn plain_parameter_property_projection<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    receiver: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> bool {
    if receiver.kind() != "navigation_expression"
        || established_direct_property_receiver_type(source, call, receiver, file, provider)
            .is_none()
    {
        return false;
    }
    let parts = receiver
        .named_children(&mut receiver.walk())
        .filter(|node| !node.is_extra())
        .collect::<Vec<_>>();
    let [base, member] = parts.as_slice() else {
        return false;
    };
    if !matches!(base.kind(), "identifier" | "simple_identifier") {
        return false;
    }
    let Some(owner) = lexical_parameter_classifier(
        source,
        call,
        base.utf8_text(source.as_bytes()).unwrap_or(""),
        file,
        provider,
    ) else {
        return false;
    };
    let property = member.utf8_text(source.as_bytes()).unwrap_or("");
    if !property.is_ascii() {
        return false;
    }
    // The transitional expression renderer uses getX for this projection.
    // Kotlin's isX getter convention requires a separate lowering proof.
    if property
        .strip_prefix("is")
        .is_some_and(|rest| rest.chars().next().is_some_and(char::is_uppercase))
    {
        return false;
    }
    let mut path = owner.owner_path.clone();
    path.push(owner.name.clone());
    let candidates = provider.symbols_named(property);
    let properties = candidates
        .iter()
        .filter(|symbol| {
            symbol.id.file == owner.file
                && symbol.id.owner_path == path
                && symbol.id.kind == "property"
        })
        .collect::<Vec<_>>();
    match properties.as_slice() {
        [property] => provider.has_plain_instance_property_getter(&property.id),
        [] if owner
            .file
            .extension()
            .is_some_and(|extension| extension == "java") =>
        {
            let getters = provider.java_property_getters(&owner, property);
            matches!(getters.as_slice(), [getter] if provider.is_public_instance_callable(&getter.id))
        }
        _ => false,
    }
}

/// Establish the classifier of a direct, explicitly typed Kotlin property
/// access on a proven receiver, including a resolved method return or another
/// property projection. Each hop must identify one direct property; inherited
/// properties and Java-bean guesses require additional override and ABI facts.
fn established_direct_property_receiver_type<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    expression: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<String> {
    if expression.kind() != "navigation_expression" {
        return None;
    }
    if let Some(access) = container_types::property_access(source, file, expression, provider) {
        let ty = access.projection.result_type;
        if ty.nullable
            && !access
                .projection
                .java_property_read
                .as_ref()
                .is_some_and(|read| read.nullability == ReferenceReturnNullability::Platform)
        {
            return None;
        }
        return match ty.ty {
            KotlinType::Named { name, .. } => Some(name),
            KotlinType::Primitive(name) => Some(format!("kotlin.{name}")),
            _ => None,
        };
    }
    let text = expression.utf8_text(source.as_bytes()).ok()?.trim();
    if text.contains("?.") || text.contains("!!") {
        return None;
    }
    let mut context = Some(expression);
    while let Some(node) = context {
        if matches!(
            node.kind(),
            "lambda_literal" | "function_literal" | "anonymous_function"
        ) {
            return None;
        }
        context = node.parent();
    }
    let mut cursor = expression.walk();
    let children = expression
        .named_children(&mut cursor)
        .filter(|child| !child.kind().contains("comment"))
        .collect::<Vec<_>>();
    let [base_node, property_node] = children.as_slice() else {
        return None;
    };
    if !matches!(property_node.kind(), "identifier" | "simple_identifier") {
        return None;
    }
    let base = base_node.utf8_text(source.as_bytes()).ok()?.trim();
    let property = property_node.utf8_text(source.as_bytes()).ok()?.trim();

    let owner = if base == "this" {
        enclosing_dispatch_owner(source, expression, file, provider)?
    } else if matches!(base_node.kind(), "identifier" | "simple_identifier") {
        lexical_parameter_classifier(source, call, base, file, provider).or_else(|| {
            let classifier =
                unqualified_instance_property_classifier(source, *base_node, file, base, provider)?;
            match provider.resolve_type_in_file(file, &package_name(source), &classifier) {
                FactStatus::Established(owner) if owner.kind != "typealias" => Some(owner),
                _ => None,
            }
        })?
    } else {
        let classifier = match base_node.kind() {
            "call_expression" => indexed_call_return_type(
                source,
                *base_node,
                call_callee_node(*base_node)?,
                file,
                provider,
            )?,
            "navigation_expression" => {
                established_direct_property_receiver_type(source, call, *base_node, file, provider)?
            }
            _ => return None,
        };
        let FactStatus::Established(owner) =
            provider.resolve_type_in_file(file, &package_name(source), &classifier)
        else {
            return None;
        };
        if !is_type_symbol(&owner) || owner.kind == "typealias" {
            return None;
        }
        owner
    };

    direct_property_classifier(&owner, property, provider)
}

/// Resolve an inherited property only in an ordinary direct member body. A
/// same-named non-property constructor parameter is out of scope there; real
/// locals and method parameters still shadow the inherited property.
pub(crate) fn inherited_property_receiver_access<P: SemanticProvider + ?Sized>(
    source: &str,
    expression: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<InheritedPropertyAccess> {
    if !matches!(expression.kind(), "identifier" | "simple_identifier") {
        return None;
    }
    let navigation = expression.parent()?;
    if navigation.kind() != "navigation_expression"
        || navigation.named_child(0)?.id() != expression.id()
        || navigation.parent()?.kind() != "call_expression"
    {
        return None;
    }
    let name = expression.utf8_text(source.as_bytes()).ok()?;
    if is_proven_local_shadow(expression, name, source) {
        return None;
    }
    let mut scope = expression.parent();
    let mut functions = 0;
    while let Some(node) = scope {
        match node.kind() {
            "function_declaration" => {
                functions += 1;
                if functions > 1 {
                    return None;
                }
            }
            "for_statement" | "catch_block" | "secondary_constructor" | "getter" | "setter" => {
                return None;
            }
            "class_declaration" | "interface_declaration" | "object_declaration" => break,
            _ => {}
        }
        scope = node.parent();
    }
    if functions != 1 {
        return None;
    }
    let owner = enclosing_dispatch_owner(source, expression, file, provider)?;
    match provider.inherited_property_access(&owner, name) {
        FactStatus::Established(access) => Some(access),
        _ => None,
    }
}

fn enclosing_dispatch_owner<P: SemanticProvider + ?Sized>(
    source: &str,
    expression: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<SymbolId> {
    let mut ancestor = expression.parent();
    while let Some(node) = ancestor {
        if matches!(
            node.kind(),
            "companion_object"
                | "object_literal"
                | "anonymous_object"
                | "lambda_literal"
                | "function_literal"
                | "anonymous_function"
        ) || (node.kind() == "function_declaration" && callable_receiver(source, node).is_some())
        {
            return None;
        }
        if matches!(
            node.kind(),
            "class_declaration" | "interface_declaration" | "object_declaration"
        ) {
            let owner = symbol_id_for_node(source, node, file);
            let FactStatus::Established(resolved) =
                provider.resolve_type_in_file(file, &owner.package, &owner.name)
            else {
                return None;
            };
            return (resolved == owner).then_some(resolved);
        }
        ancestor = node.parent();
    }
    None
}

fn direct_property_classifier<P: SemanticProvider + ?Sized>(
    owner: &SymbolId,
    property: &str,
    provider: &P,
) -> Option<String> {
    let mut owner_path = owner.owner_path.clone();
    owner_path.push(owner.name.clone());
    let mut properties = provider
        .symbols_named(property)
        .into_iter()
        .filter(|symbol| {
            symbol.id.file == owner.file
                && symbol.id.package == owner.package
                && symbol.id.owner_path == owner_path
                && symbol.id.kind == "property"
                && symbol.id.receiver.is_none()
                && symbol
                    .id
                    .file
                    .extension()
                    .is_some_and(|extension| extension == "kt" || extension == "kts")
        })
        .collect::<Vec<_>>();
    if properties.is_empty()
        && owner
            .file
            .extension()
            .is_some_and(|extension| extension == "java")
    {
        properties = provider.java_property_getters(owner, property);
    }
    let [property] = properties.as_slice() else {
        return None;
    };
    let FactStatus::Established(property_type) = &property.ty else {
        return None;
    };
    if property_type.nullable {
        return None;
    }
    let KotlinType::Named { name, arguments } = &property_type.ty else {
        return None;
    };
    if name.is_empty() || provider.has_type_alias_in_package(&property.id.package, name) {
        return None;
    }
    let declaring_source = provider.source_for_file(&property.id.file)?;
    if kotlin_import_lines(declaring_source).any(|import| {
        import
            .split_once(" as ")
            .is_some_and(|(_, alias)| alias.trim() == name)
    }) {
        return None;
    }
    if !arguments.is_empty() {
        // Receiver facts for standard containers only need the outer
        // classifier: e.g. `List<T>.stream()` is known without substituting
        // the list element type. Keep arbitrary generic source properties
        // outside this path because their member and overload facts may
        // depend on those arguments.
        let java_source = property
            .id
            .file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("java"));
        let canonical = standard_external_type(declaring_source, provider, name, java_source)?;
        if !matches!(
            canonical.as_str(),
            "java.util.List"
                | "java.util.Set"
                | "java.util.Collection"
                | "java.util.Map"
                | "java.util.Optional"
                | "java.util.stream.Stream"
                | "kotlin.collections.List"
                | "kotlin.collections.MutableList"
                | "kotlin.collections.Set"
                | "kotlin.collections.MutableSet"
                | "kotlin.collections.Collection"
                | "kotlin.collections.MutableCollection"
                | "kotlin.collections.Map"
                | "kotlin.collections.MutableMap"
        ) {
            return None;
        }
        return Some(canonical);
    }
    match provider.resolve_type_in_file(&property.id.file, &property.id.package, name) {
        FactStatus::Established(id) if id.kind != "typealias" => {
            Some(qualified_source_type_name(&id))
        }
        FactStatus::Unknown => standard_external_type(
            declaring_source,
            provider,
            name,
            property
                .id
                .file
                .extension()
                .is_some_and(|extension| extension == "java"),
        ),
        FactStatus::Inferred(_) | FactStatus::Ambiguous(_) | FactStatus::Established(_) => None,
    }
}

fn unqualified_instance_property_classifier<P: SemanticProvider + ?Sized>(
    source: &str,
    expression: tree_sitter::Node<'_>,
    file: &Path,
    name: &str,
    provider: &P,
) -> Option<String> {
    if is_proven_local_shadow(expression, name, source) {
        return None;
    }
    // Loop/catch bindings and additional implicit receivers need their own
    // scope proof. An explicit `this.property` can still be resolved there.
    let mut scope = expression.parent();
    while let Some(node) = scope {
        if matches!(node.kind(), "for_statement" | "catch_block") {
            return None;
        }
        if matches!(
            node.kind(),
            "class_declaration" | "interface_declaration" | "object_declaration"
        ) {
            break;
        }
        scope = node.parent();
    }
    let owner = enclosing_dispatch_owner(source, expression, file, provider)?;
    direct_property_classifier(&owner, name, provider)
}

fn lexical_parameter_is_declared(source: &str, call: tree_sitter::Node<'_>, name: &str) -> bool {
    let mut ancestor = call.parent();
    while let Some(node) = ancestor {
        if matches!(
            node.kind(),
            "function_declaration" | "secondary_constructor"
        ) {
            return node.named_children(&mut node.walk()).any(|parameters| {
                matches!(
                    parameters.kind(),
                    "function_value_parameters" | "parameters"
                ) && parameters
                    .named_children(&mut parameters.walk())
                    .any(|parameter| {
                        parameter.kind() == "parameter"
                            && node_name(parameter, source).as_deref() == Some(name)
                    })
            });
        }
        if matches!(
            node.kind(),
            "class_declaration" | "interface_declaration" | "object_declaration"
        ) {
            return false;
        }
        ancestor = node.parent();
    }
    false
}

pub(crate) fn lexical_parameter_type(
    source: &str,
    call: tree_sitter::Node<'_>,
    name: &str,
) -> Option<TypeRef> {
    let mut ancestor = call.parent();
    let callable = loop {
        let node = ancestor?;
        if matches!(
            node.kind(),
            "lambda_literal" | "function_literal" | "anonymous_function"
        ) {
            return None;
        }
        if node.kind() == "catch_block" {
            let mut cursor = node.walk();
            if node.named_children(&mut cursor).any(|child| {
                matches!(child.kind(), "identifier" | "simple_identifier")
                    && child.utf8_text(source.as_bytes()).ok() == Some(name)
            }) {
                return None;
            }
        }
        if matches!(
            node.kind(),
            "function_declaration" | "secondary_constructor"
        ) {
            break node;
        }
        if matches!(
            node.kind(),
            "class_declaration" | "interface_declaration" | "object_declaration" | "object_literal"
        ) {
            return None;
        }
        ancestor = node.parent();
    };
    let parameters = callable.child_by_field_name("parameters").or_else(|| {
        let mut cursor = callable.walk();
        callable
            .named_children(&mut cursor)
            .find(|node| node.kind() == "function_value_parameters")
    })?;
    let mut cursor = parameters.walk();
    let matches = parameters
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "parameter")
        .filter(|node| node_name(*node, source).as_deref() == Some(name))
        .collect::<Vec<_>>();
    let [parameter] = matches.as_slice() else {
        return None;
    };
    if parameter_signature(source, *parameter).starts_with("vararg:")
        || parameter.prev_named_sibling().is_some_and(|modifiers| {
            modifiers.kind() == "parameter_modifiers"
                && modifiers
                    .utf8_text(source.as_bytes())
                    .is_ok_and(|text| text.split_whitespace().any(|word| word == "vararg"))
        })
    {
        return None;
    }
    // Any earlier local with this spelling may shadow the formal parameter.
    // This is intentionally conservative across nested blocks; it never
    // chooses a parameter when a local binding could be the receiver.
    let mut stack = vec![callable];
    while let Some(node) = stack.pop() {
        if node.start_byte() >= call.start_byte() {
            continue;
        }
        if node.start_byte() < call.start_byte()
            && matches!(
                node.kind(),
                "local_variable_declaration" | "property_declaration" | "variable_declaration"
            )
        {
            let binding = if node.kind() == "variable_declaration" {
                node_name(node, source)
            } else if node.kind() == "property_declaration" {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find(|child| child.kind() == "variable_declaration")
                    .and_then(|variable| node_name(variable, source))
            } else {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .filter(|child| child.kind() == "variable_declarator")
                    .filter_map(|child| node_name(child, source))
                    .find(|binding| binding == name)
            };
            if binding.as_deref() == Some(name) {
                return None;
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    let type_node = parameter
        .child_by_field_name("type")
        .or_else(|| named_type_child(*parameter))?;
    let spelling = type_node.utf8_text(source.as_bytes()).ok()?.trim();
    Some(TypeRef::parse_with_type_parameters(
        spelling,
        &declaration_type_parameters(source, callable),
    ))
}

fn lexical_parameter_classifier<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    name: &str,
    file: &Path,
    provider: &P,
) -> Option<SymbolId> {
    let ty = lexical_parameter_type(source, call, name)
        .or_else(|| container_types::parameter_type(source, file, call, name, provider))?;
    if ty.nullable {
        return None;
    }
    let KotlinType::Named { name, .. } = ty.ty else {
        return None;
    };
    // The owner's classifier is enough for a direct property whose own type
    // is established independently of the owner's type arguments. We never
    // substitute a generic property type in this path.
    if name.is_empty() {
        return None;
    }
    if kotlin_import_lines(source).any(|import| {
        import
            .split_once(" as ")
            .is_some_and(|(_, alias)| alias.trim() == name)
    }) {
        return None;
    }
    match provider.resolve_type_in_file(file, &package_name(source), &name) {
        FactStatus::Established(id) if id.kind != "typealias" => Some(id),
        FactStatus::Inferred(_) | FactStatus::Ambiguous(_) => None,
        FactStatus::Established(_) | FactStatus::Unknown => None,
    }
}

/// Member lookup from a returned type parameter's upper bound. Keep this out
/// of exact return-type inference and standard-library lowering proofs.
fn resolve_capability_receiver_call<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    callee: tree_sitter::Node<'_>,
    name: &str,
    provider: &P,
) -> FactStatus<SymbolId> {
    let Some(receiver) = explicit_receiver_node(call, callee)
        .filter(|receiver| receiver.kind() == "call_expression")
    else {
        return FactStatus::Unknown;
    };
    let Some(inner_callee) = call_callee_node(receiver) else {
        return FactStatus::Unknown;
    };
    let Ok(inner_name) = inner_callee.utf8_text(source.as_bytes()) else {
        return FactStatus::Unknown;
    };
    // Most chains have no type-parameter return at all. Avoid resolving their
    // prefixes again just to learn that this capability cannot apply.
    if !provider
        .symbols_named(inner_name.trim())
        .iter()
        .any(|symbol| {
            matches!(
                &symbol.ty,
                FactStatus::Established(TypeRef {
                    ty: KotlinType::TypeParameter(_),
                    nullable: false,
                })
            )
        })
    {
        return FactStatus::Unknown;
    }
    let FactStatus::Established(inner) = resolve_required_call(
        source,
        file,
        receiver,
        inner_callee,
        inner_name.trim(),
        provider,
    ) else {
        return FactStatus::Unknown;
    };
    let FactStatus::Established(capability) = provider.returned_receiver_capability(&inner) else {
        return FactStatus::Unknown;
    };
    resolve_public_instance_call(&capability.owner, call, name, provider)
}

fn resolve_public_instance_call<P: SemanticProvider + ?Sized>(
    owner: &SymbolId,
    call: tree_sitter::Node<'_>,
    name: &str,
    provider: &P,
) -> FactStatus<SymbolId> {
    if call_argument_count(call) != 0 || !provider.has_complete_inheritance(owner) {
        return FactStatus::Unknown;
    }
    let mut path = owner.owner_path.clone();
    path.push(owner.name.clone());
    let direct = provider
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| {
            symbol.id.file == owner.file
                && symbol.id.owner_path == path
                && matches!(symbol.id.kind.as_str(), "function" | "method")
        })
        .collect::<Vec<_>>();
    let members = if direct.is_empty() {
        provider.inherited_members(owner, name)
    } else {
        direct
    };
    let mut candidates = members
        .into_iter()
        .filter(|member| provider.is_public_instance_callable(&member.id))
        .map(|member| member.id.clone())
        .collect::<Vec<_>>();
    retain_matching_arity(&mut candidates, call);
    fact_from_candidates(candidates)
}

fn indexed_call_return_type<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    callee: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<String> {
    let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
    if let Some(fact) = container_calls::establish(source, file, call, provider) {
        return fact.return_type;
    }
    if let Some(return_type) = established_platform_call_return_type(source, file, call, provider) {
        return Some(return_type.to_string());
    }
    let FactStatus::Established(id) =
        resolve_required_call(source, file, call, callee, name, provider)
    else {
        return None;
    };
    let symbol = provider
        .symbols_named(name)
        .into_iter()
        .find(|symbol| symbol.id == id)?;
    let FactStatus::Established(return_type) = &symbol.ty else {
        return None;
    };
    if return_type.nullable {
        return None;
    }
    let KotlinType::Named { name, .. } = &return_type.ty else {
        return None;
    };
    // Member lookup needs the receiver's classifier, including for Box<T>.
    // This does not substitute T: a subsequent type-parameter return still
    // cannot establish another receiver classifier.
    if name.is_empty() || name.starts_with("T:") {
        return None;
    }
    if provider.has_type_alias_in_package(&symbol.id.package, name) {
        return None;
    }
    let producer_source = provider.source_for_file(&symbol.id.file);
    if producer_source.is_some_and(|source| {
        kotlin_import_lines(source).any(|import| {
            import
                .split_once(" as ")
                .is_some_and(|(_, alias)| alias.trim() == name)
        })
    }) {
        // Alias expansion is deliberately unsupported here. An alias must
        // not fall through to a different same-package classifier.
        return None;
    }
    match provider.resolve_type_in_file(&symbol.id.file, &symbol.id.package, name) {
        FactStatus::Established(type_id) => Some(qualified_source_type_name(&type_id)),
        FactStatus::Ambiguous(_) | FactStatus::Inferred(_) => None,
        FactStatus::Unknown => {
            let producer_source = producer_source?;
            let java_source = symbol
                .id
                .file
                .extension()
                .is_some_and(|extension| extension == "java");
            standard_external_type(producer_source, provider, name, java_source)
        }
    }
}

fn qualified_source_type_name(id: &SymbolId) -> String {
    let mut parts = Vec::with_capacity(id.owner_path.len() + 1);
    if !id.package.is_empty() {
        parts.push(id.package.as_str());
    }
    parts.extend(id.owner_path.iter().map(String::as_str));
    parts.push(id.name.as_str());
    parts.join(".")
}

fn known_string_split_argument<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> bool {
    let Some(arguments) = call.child_by_field_name("arguments").or_else(|| {
        call.child_by_field_name("value_arguments").or_else(|| {
            let mut cursor = call.walk();
            call.named_children(&mut cursor).find(|child| {
                matches!(
                    child.kind(),
                    "arguments" | "argument_list" | "value_arguments"
                )
            })
        })
    }) else {
        return false;
    };
    let mut cursor = arguments.walk();
    let values = arguments
        .named_children(&mut cursor)
        .filter(|child| matches!(child.kind(), "value_argument" | "argument"))
        .collect::<Vec<_>>();
    if values.len() != 1 {
        return false;
    }
    if values[0]
        .utf8_text(source.as_bytes())
        .is_ok_and(|text| text.contains('='))
    {
        // The current Java call builder does not reassemble named arguments.
        return false;
    }
    let mut cursor = values[0].walk();
    let Some(expression) = values[0].named_children(&mut cursor).last() else {
        return false;
    };
    if matches!(expression.kind(), "string_literal" | "string_template") {
        return true;
    }
    if !matches!(expression.kind(), "identifier" | "simple_identifier") {
        return false;
    }
    let Ok(name) = expression.utf8_text(source.as_bytes()) else {
        return false;
    };
    if same_file_string_const_literal(source, file, name.trim(), expression, provider).is_some() {
        return true;
    }
    let Some(scope) = enclosing_callable(call) else {
        return false;
    };
    let parameters = scope.child_by_field_name("parameters").or_else(|| {
        let mut cursor = scope.walk();
        scope.named_children(&mut cursor).find(|child| {
            matches!(
                child.kind(),
                "function_value_parameters" | "formal_parameters" | "class_parameters"
            )
        })
    });
    let Some(parameters) = parameters else {
        return false;
    };
    let mut cursor = parameters.walk();
    parameters
        .named_children(&mut cursor)
        .filter(|parameter| {
            matches!(
                parameter.kind(),
                "parameter" | "class_parameter" | "formal_parameter" | "spread_parameter"
            )
        })
        .any(|parameter| {
            node_name(parameter, source).as_deref() == Some(name.trim())
                && parameter
                    .child_by_field_name("type")
                    .or_else(|| named_type_child(parameter))
                    .and_then(|ty| ty.utf8_text(source.as_bytes()).ok())
                    .is_some_and(|ty| {
                        matches!(
                            ty.trim().trim_end_matches('?'),
                            "String" | "java.lang.String" | "kotlin.String"
                        )
                    })
        })
}

/// Return a Java-safe literal for an indexed top-level `const val NAME:
/// String = "literal"` in this exact source snapshot. The caller supplies the
/// reference node so local/parameter shadowing can keep the reference
/// unresolved. This deliberately excludes interpolated, raw, and escaped-
/// dollar strings whose source spelling differs between Kotlin and Java.
pub(crate) fn same_file_string_const_literal<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    name: &str,
    reference: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<String> {
    if name.is_empty() || is_proven_local_shadow(reference, name, source) {
        return None;
    }
    // Any other declaration with this spelling can change what an unqualified
    // reference means (including an inherited property in another file).
    let matching = provider.symbols_named(name);
    let [symbol] = matching.as_slice() else {
        return None;
    };
    if symbol.id.file != file
        || symbol.id.kind != "property"
        || !symbol.id.owner_path.is_empty()
        || symbol.location.snapshot_hash != *blake3::hash(source.as_bytes()).as_bytes()
        || !matches!(
            &symbol.ty,
            FactStatus::Established(TypeRef {
                ty: KotlinType::Named { name, .. },
                nullable: false,
            }) if name == "String" || name == "java.lang.String" || name == "kotlin.String"
        )
    {
        return None;
    }
    let canonical_string_type = match &symbol.ty {
        FactStatus::Established(TypeRef {
            ty: KotlinType::Named {
                name: type_name, ..
            },
            nullable: false,
        }) if type_name == "kotlin.String" || type_name == "java.lang.String" => true,
        FactStatus::Established(TypeRef {
            ty: KotlinType::Named {
                name: type_name, ..
            },
            nullable: false,
        }) if type_name == "String" => {
            // A simple name is only safe when the source cannot import or
            // declare a different String type. java.util.* is common and does
            // not contain String; other wildcard imports remain conservative.
            let unsafe_import = source
                .lines()
                .filter_map(|line| line.trim().strip_prefix("import "))
                .any(|line| {
                    let import = line.split(';').next().unwrap_or(line).trim();
                    let (path, alias) = import
                        .split_once(" as ")
                        .map(|(path, alias)| (path.trim(), Some(alias.trim())))
                        .unwrap_or((import, None));
                    if alias == Some("String") || path.rsplit('.').next() == Some("String") {
                        return true;
                    }
                    path.ends_with(".*") && !matches!(path, "java.util.*" | "kotlin.*")
                });
            let declared_string_type = provider.symbols_named("String").iter().any(|candidate| {
                matches!(
                    candidate.id.kind.as_str(),
                    "class" | "interface" | "enum" | "object"
                )
            }) || provider
                .type_aliases()
                .iter()
                .any(|((_, alias), _)| alias == "String");
            !unsafe_import && !declared_string_type
        }
        _ => false,
    };
    if !canonical_string_type {
        return None;
    }
    if source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("import "))
        .any(|line| {
            let path = line.split(';').next().unwrap_or(line).trim();
            let (path, alias) = path
                .split_once(" as ")
                .map(|(path, alias)| (path.trim(), Some(alias.trim())))
                .unwrap_or((path, None));
            alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == name
        })
    {
        return None;
    }
    let mut root = reference;
    while let Some(parent) = root.parent() {
        root = parent;
    }
    let mut cursor = root.walk();
    let declarations = root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "property_declaration")
        .filter(|node| {
            declaration_identifier(*node, source)
                .and_then(|identifier| identifier.utf8_text(source.as_bytes()).ok())
                == Some(name)
        })
        .collect::<Vec<_>>();
    let [declaration] = declarations.as_slice() else {
        return None;
    };
    if declaration.start_byte() != symbol.location.start_byte
        || declaration.end_byte() != symbol.location.end_byte
        || !declaration_type_node(*declaration)
            .and_then(|ty| ty.utf8_text(source.as_bytes()).ok())
            .is_some_and(|ty| matches!(ty.trim(), "String" | "java.lang.String" | "kotlin.String"))
    {
        return None;
    }
    let mut cursor = declaration.walk();
    let has_const = declaration
        .named_children(&mut cursor)
        .find(|child| child.kind() == "modifiers")
        .and_then(|modifiers| modifiers.utf8_text(source.as_bytes()).ok())
        .is_some_and(|text| {
            text.split(|character: char| !character.is_alphanumeric() && character != '_')
                .any(|modifier| modifier == "const")
        });
    if !has_const {
        return None;
    }
    let mut cursor = declaration.walk();
    let children = declaration.children(&mut cursor).collect::<Vec<_>>();
    let equals = children.iter().position(|child| child.kind() == "=")?;
    let initializer = children[equals + 1..]
        .iter()
        .find(|child| child.is_named())
        .copied()?;
    let literal = unwrap_single_string_literal(initializer)?;
    let raw = literal.utf8_text(source.as_bytes()).ok()?;
    safe_java_string_literal(raw).then(|| raw.to_owned())
}

fn unwrap_single_string_literal<'tree>(
    mut node: tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    loop {
        if node.kind() == "string_literal" {
            return Some(node);
        }
        let mut cursor = node.walk();
        let children = node.named_children(&mut cursor).collect::<Vec<_>>();
        if children.len() != 1 || !matches!(node.kind(), "expression" | "parenthesized_expression")
        {
            return None;
        }
        node = children[0];
    }
}

fn safe_java_string_literal(raw: &str) -> bool {
    if !raw.starts_with('"')
        || !raw.ends_with('"')
        || raw.starts_with("\"\"\"")
        || raw.contains('$')
    {
        return false;
    }
    let mut escaped = false;
    for character in raw[1..raw.len() - 1].chars() {
        if escaped {
            if !matches!(character, '\\' | '"' | '\'' | 't' | 'b' | 'n' | 'r') {
                return false;
            }
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '\n' || character == '\r' {
            return false;
        }
    }
    !escaped
}

fn enclosing_callable<'tree>(
    mut node: tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "function_declaration"
                | "method_declaration"
                | "constructor_declaration"
                | "secondary_constructor"
        ) {
            return Some(parent);
        }
        node = parent;
    }
    None
}

/// Exact platform return facts shared with Java emission. Never infer a
/// platform factory from its simple name without a proven explicit receiver.
pub(crate) fn established_platform_static_call_return_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<&'static str> {
    let callee = call_callee_node(call)?;
    let spelling = explicit_receiver_spelling(source, call, callee)?;
    let package = package_name(source);
    if provider.symbols_named(spelling).iter().any(|symbol| {
        !is_type_symbol(&symbol.id)
            && (symbol.id.file == file
                || (symbol.id.package == package && symbol.id.owner_path.is_empty()))
    }) {
        // File/package values remain visible when declared after this call.
        // Their spelling is not evidence of a static type qualifier.
        return None;
    }
    if !explicit_receiver_is_type_qualifier(source, call, callee, file, provider) {
        return None;
    }
    established_platform_call_return_type(source, file, call, provider)
}

pub(crate) fn established_platform_call_return_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<&'static str> {
    let callee = call_callee_node(call)?;
    let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
    if let Some(result) =
        object_calls::established_object_to_string(source, file, call, callee, name, provider)
    {
        return Some(result);
    }
    if let Some(constructor) = established_platform_constructor(source, file, call, provider) {
        return Some(constructor);
    }
    platform_members::establish(source, file, call, provider)
}

/// Shared proof for Kotlin String case conversion calls. The Locale overload
/// is accepted only when its sole argument has a proven `java.util.Locale`
/// type, and an indexed workspace extension takes precedence over this fact.
pub(crate) fn established_standard_string_case_call<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> bool {
    let Some(callee) = call_callee_node(call) else {
        return false;
    };
    let name = callee.utf8_text(source.as_bytes()).unwrap_or("").trim();
    if !matches!(name, "lowercase" | "uppercase")
        || provider.symbols_named(name).iter().any(|symbol| {
            symbol.id.receiver.is_some() && matches!(symbol.id.kind.as_str(), "function" | "method")
        })
        || kotlin_import_lines(source).any(|import| {
            let (path, alias) = import
                .split_once(" as ")
                .map_or((import, None), |(path, alias)| {
                    (path.trim(), Some(alias.trim()))
                });
            alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or("")) == name
                && path != format!("kotlin.text.{name}")
        })
        || explicit_receiver_node(call, callee).is_none()
        || !matches!(
            resolve_required_call(source, file, call, callee, name, provider),
            FactStatus::Unknown
        )
    {
        return false;
    }
    let Some(receiver_type) = explicit_receiver_type(source, call, callee, file, provider) else {
        return false;
    };
    if standard_external_type(source, provider, &receiver_type, false).as_deref()
        != Some("java.lang.String")
    {
        return false;
    }
    match call_argument_count(call) {
        0 => true,
        1 => known_locale_argument(source, file, call, provider),
        _ => false,
    }
}

pub(crate) fn established_java_callable_member<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> bool {
    let Some(callee) = call_callee_node(call) else {
        return false;
    };
    if explicit_receiver_node(call, callee).is_none() {
        return false;
    }
    let name = callee.utf8_text(source.as_bytes()).unwrap_or("");
    matches!(resolve_required_call(source, file, call, callee, name, provider),
        FactStatus::Established(id) if provider.is_java_callable_member(&id))
        || established_projected_receiver_instance_call(source, call, file, provider)
}

fn known_external_member_call<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    callee: tree_sitter::Node<'_>,
    name: &str,
    provider: &P,
) -> bool {
    if container_calls::establish(source, file, call, provider).is_some() {
        return true;
    }
    if platform_members::establish(source, file, call, provider).is_some() {
        return true;
    }
    if object_calls::established_object_to_string(source, file, call, callee, name, provider)
        .is_some()
    {
        return true;
    }
    let Some(receiver_type) = explicit_receiver_type(source, call, callee, file, provider) else {
        return false;
    };
    let Some(receiver_type) = standard_external_type(
        source,
        provider,
        &receiver_type,
        call.kind() == "method_invocation",
    ) else {
        return false;
    };
    let arity = call_argument_count(call);
    if receiver_type == "java.lang.String" && name == "split" {
        if call.kind() == "method_invocation" {
            // Java String.split is the regex-based JDK method. Java source
            // does not use the Kotlin lowering, so retain its declared
            // one- and two-argument forms here.
            return matches!(arity, 1 | 2);
        }
        return arity == 1 && known_string_split_argument(source, file, call, provider);
    }
    if receiver_type == "java.lang.String" && matches!(name, "lowercase" | "uppercase") {
        return established_standard_string_case_call(source, file, call, provider);
    }
    if matches!(name, "mapKeys" | "filterValues")
        && matches!(
            receiver_type.as_str(),
            "kotlin.collections.Map" | "kotlin.collections.MutableMap"
        )
        && arity == 1
    {
        return known_kotlin_collection_extension(source, call, name, provider);
    }
    if name == "associateBy"
        && matches!(
            receiver_type.as_str(),
            "kotlin.collections.List"
                | "kotlin.collections.MutableList"
                | "kotlin.collections.Set"
                | "kotlin.collections.MutableSet"
                | "java.util.List"
                | "java.util.Set"
        )
        && matches!(arity, 1 | 2)
    {
        return known_kotlin_collection_extension(source, call, name, provider);
    }
    matches!(
        (receiver_type.as_str(), name, arity),
        ("java.util.UUID", "toString" | "hashCode", 0)
            | ("java.util.UUID", "equals", 1)
            | (
                "java.lang.String",
                "toString" | "hashCode" | "lowercase" | "uppercase" | "trim",
                0
            )
            | ("java.lang.String", "equals" | "compareTo", 1)
            | (
                "java.util.Map"
                    | "java.util.HashMap"
                    | "kotlin.collections.Map"
                    | "kotlin.collections.MutableMap",
                "containsKey",
                1
            )
            | (
                "java.util.Map"
                    | "java.util.HashMap"
                    | "kotlin.collections.Map"
                    | "kotlin.collections.MutableMap",
                "hashCode",
                0
            )
            | ("java.util.List", "add", 1)
            | ("kotlin.collections.MutableList", "add", 1)
            | ("java.util.Set", "add", 1)
            | ("kotlin.collections.MutableSet", "add", 1)
            | (
                "java.util.List" | "kotlin.collections.List" | "kotlin.collections.MutableList",
                "last",
                0
            )
            | ("java.util.List", "toList", 0)
            | ("kotlin.Array", "toList", 0)
    )
}

/// The lowering for these calls implements the Kotlin stdlib extension itself.
/// Refuse same-workspace functions or explicit imports that could select a
/// different extension with the same spelling.
fn known_kotlin_collection_extension<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    name: &str,
    provider: &P,
) -> bool {
    let expected_lambdas = if name == "associateBy" && call_argument_count(call) == 2 {
        2
    } else {
        1
    };
    if !collection_lambda_shape_supported(source, call, expected_lambdas) {
        return false;
    }
    if provider
        .symbols_named(name)
        .iter()
        .any(|symbol| !is_type_symbol(&symbol.id))
    {
        return false;
    }
    for line in source.lines() {
        let Some(import) = line.trim().strip_prefix("import ") else {
            continue;
        };
        let (path, alias) = import
            .split(';')
            .next()
            .unwrap_or(import)
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((import.trim(), None));
        if alias == Some(name) && path != format!("kotlin.collections.{name}") {
            return false;
        }
        if path.rsplit('.').next() == Some(name) && path != format!("kotlin.collections.{name}") {
            return false;
        }
    }
    true
}

/// The Java lowering supports one expression lambda per selector and a
/// single, simple parameter. Multi-statement, destructuring, or nested lambda
/// bodies are retained instead of being flattened into an unsafe expression.
fn collection_lambda_shape_supported(
    source: &str,
    call: tree_sitter::Node<'_>,
    expected: usize,
) -> bool {
    let mut lambdas = Vec::new();
    let mut stack = vec![call];
    while let Some(node) = stack.pop() {
        if node.kind() == "lambda_literal" {
            lambdas.push(node);
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    if lambdas.len() != expected {
        return false;
    }
    for lambda in lambdas {
        let mut cursor = lambda.walk();
        let named: Vec<_> = lambda.named_children(&mut cursor).collect();
        let parameters: Vec<_> = named
            .iter()
            .copied()
            .filter(|child| child.kind() == "lambda_parameters")
            .collect();
        if parameters.len() > 1 {
            return false;
        }
        if let Some(parameters) = parameters.first() {
            let mut cursor = parameters.walk();
            let entries: Vec<_> = parameters.named_children(&mut cursor).collect();
            if entries.len() != 1
                || !(matches!(entries[0].kind(), "identifier" | "simple_identifier")
                    || simple_lambda_variable(entries[0], source))
            {
                return false;
            }
        }
        let bodies: Vec<_> = named
            .into_iter()
            .filter(|child| child.kind() != "lambda_parameters")
            .collect();
        if bodies.len() != 1 {
            return false;
        }
        let body = bodies[0];
        let mut cursor = body.walk();
        let body_nodes: Vec<_> = if body.kind() == "statements" {
            body.named_children(&mut cursor).collect()
        } else {
            vec![body]
        };
        if body_nodes.len() != 1 {
            return false;
        }
        let mut stack = vec![body_nodes[0]];
        while let Some(node) = stack.pop() {
            if node.kind() == "lambda_literal" {
                return false;
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }
    true
}

fn simple_lambda_variable(node: tree_sitter::Node<'_>, source: &str) -> bool {
    if node.kind() != "variable_declaration" || node_name(node, source).is_none() {
        return false;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| matches!(child.kind(), "identifier" | "simple_identifier"))
        .count()
        == 1
}

fn standard_external_type<P: SemanticProvider + ?Sized>(
    source: &str,
    provider: &P,
    spelling: &str,
    java_source: bool,
) -> Option<String> {
    let spelling = spelling
        .trim()
        .trim_end_matches('?')
        .split('<')
        .next()
        .unwrap_or("")
        .trim();
    let (simple, canonical, intrinsic) = match spelling {
        "BigDecimal" | "java.math.BigDecimal" => ("BigDecimal", "java.math.BigDecimal", false),
        "UUID" | "java.util.UUID" => ("UUID", "java.util.UUID", false),
        "Locale" | "java.util.Locale" => ("Locale", "java.util.Locale", false),
        "Instant" | "java.time.Instant" => ("Instant", "java.time.Instant", false),
        "String" | "java.lang.String" | "kotlin.String" => ("String", "java.lang.String", true),
        "Any" | "kotlin.Any" if !java_source => ("Any", "kotlin.Any", true),
        "Object" | "java.lang.Object" => ("Object", "java.lang.Object", true),
        "Int" | "kotlin.Int" => ("Int", "kotlin.Int", true),
        "Long" | "kotlin.Long" => ("Long", "kotlin.Long", true),
        "Short" | "kotlin.Short" => ("Short", "kotlin.Short", true),
        "Byte" | "kotlin.Byte" => ("Byte", "kotlin.Byte", true),
        "Double" | "kotlin.Double" => ("Double", "kotlin.Double", true),
        "Float" | "kotlin.Float" => ("Float", "kotlin.Float", true),
        "Boolean" | "kotlin.Boolean" => ("Boolean", "kotlin.Boolean", true),
        "Char" | "kotlin.Char" => ("Char", "kotlin.Char", true),
        "StringBuilder" | "java.lang.StringBuilder" => {
            ("StringBuilder", "java.lang.StringBuilder", true)
        }
        "IllegalArgumentException" | "java.lang.IllegalArgumentException" => (
            "IllegalArgumentException",
            "java.lang.IllegalArgumentException",
            true,
        ),
        "IllegalStateException" | "java.lang.IllegalStateException" => (
            "IllegalStateException",
            "java.lang.IllegalStateException",
            true,
        ),
        "Map" if java_source => ("Map", "java.util.Map", false),
        "Map" | "kotlin.collections.Map" => ("Map", "kotlin.collections.Map", true),
        "MutableMap" if java_source => ("MutableMap", "java.util.Map", false),
        "MutableMap" | "kotlin.collections.MutableMap" => {
            ("MutableMap", "kotlin.collections.MutableMap", true)
        }
        "java.util.Map" => ("Map", "java.util.Map", false),
        "HashMap" | "java.util.HashMap" => ("HashMap", "java.util.HashMap", false),
        "List" if java_source => ("List", "java.util.List", false),
        "List" | "kotlin.collections.List" => ("List", "kotlin.collections.List", true),
        "MutableList" if java_source => ("MutableList", "java.util.List", false),
        "MutableList" | "kotlin.collections.MutableList" => {
            ("MutableList", "kotlin.collections.MutableList", true)
        }
        "java.util.List" => ("List", "java.util.List", false),
        "Set" if java_source => ("Set", "java.util.Set", false),
        "Set" | "kotlin.collections.Set" => ("Set", "kotlin.collections.Set", true),
        "MutableSet" if java_source => ("MutableSet", "java.util.Set", false),
        "MutableSet" | "kotlin.collections.MutableSet" => {
            ("MutableSet", "kotlin.collections.MutableSet", true)
        }
        "java.util.Set" => ("Set", "java.util.Set", false),
        "Collection" if java_source => ("Collection", "java.util.Collection", false),
        "Collection" | "kotlin.collections.Collection" => {
            ("Collection", "kotlin.collections.Collection", true)
        }
        "MutableCollection" | "kotlin.collections.MutableCollection" => (
            "MutableCollection",
            "kotlin.collections.MutableCollection",
            true,
        ),
        "Iterable" if java_source => ("Iterable", "java.lang.Iterable", true),
        "java.lang.Iterable" => ("Iterable", "java.lang.Iterable", true),
        "Iterable" | "kotlin.collections.Iterable" => {
            ("Iterable", "kotlin.collections.Iterable", true)
        }
        "MutableIterable" | "kotlin.collections.MutableIterable" => (
            "MutableIterable",
            "kotlin.collections.MutableIterable",
            true,
        ),
        "java.util.Collection" => ("Collection", "java.util.Collection", false),
        "Optional" | "java.util.Optional" => ("Optional", "java.util.Optional", false),
        "Stream" | "java.util.stream.Stream" => ("Stream", "java.util.stream.Stream", false),
        "Array" | "kotlin.Array" => ("Array", "kotlin.Array", true),
        _ => return None,
    };
    if provider
        .symbols_named(simple)
        .iter()
        .any(|symbol| is_type_symbol(&symbol.id))
    {
        return None;
    }
    if spelling.contains('.') {
        return (spelling == canonical).then(|| canonical.to_owned());
    }
    if canonical == "java.math.BigDecimal"
        && provider.has_type_alias_in_package(&package_name(source), simple)
    {
        return None;
    }
    let mut explicit_imports = Vec::new();
    let mut matching_wildcard = None;
    for line in source.lines() {
        let Some(import) = line.trim().strip_prefix("import ") else {
            continue;
        };
        let import = import.split(';').next().unwrap_or(import).trim();
        let (path, alias) = import
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((import, None));
        let imported_simple = alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path));
        if imported_simple == simple {
            explicit_imports.push(path);
        } else if alias.is_none() && external_wildcard_covers(path, canonical) {
            matching_wildcard = Some(path);
        }
    }
    if !explicit_imports.is_empty() {
        if let [path] = explicit_imports.as_slice()
            && matches!(
                (simple, *path),
                ("List", "java.util.List")
                    | ("Set", "java.util.Set")
                    | ("Map", "java.util.Map")
                    | ("Collection", "java.util.Collection")
            )
        {
            return Some((*path).to_owned());
        }
        return (explicit_imports.len() == 1 && explicit_imports[0] == canonical)
            .then(|| canonical.to_owned());
    }
    if source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("import "))
        .any(|import| {
            let path = import.split(';').next().unwrap_or(import).trim();
            path.ends_with(".*")
                && !is_known_jdk_wildcard(path)
                && !(canonical == "java.math.BigDecimal" && path == "java.math.*")
        })
    {
        return None;
    }
    match matching_wildcard {
        Some(path) if external_wildcard_covers(path, canonical) => Some(canonical.to_owned()),
        Some(_) => None,
        None if intrinsic => Some(canonical.to_owned()),
        None => None,
    }
}

fn external_wildcard_covers(path: &str, canonical: &str) -> bool {
    match canonical {
        "java.math.BigDecimal" => path == "java.math.*",
        "java.util.Optional"
        | "java.util.Collection"
        | "java.util.UUID"
        | "java.util.Locale"
        | "java.util.Map"
        | "java.util.HashMap"
        | "java.util.List"
        | "java.util.Set" => path == "java.util.*",
        "java.time.Instant" => path == "java.time.*",
        "java.util.stream.Stream" => path == "java.util.stream.*",
        "java.lang.String"
        | "java.lang.StringBuilder"
        | "java.lang.IllegalArgumentException"
        | "java.lang.IllegalStateException" => path == "java.lang.*",
        _ => false,
    }
}

fn is_known_jdk_wildcard(path: &str) -> bool {
    matches!(
        path,
        "java.util.*" | "java.lang.*" | "java.util.stream.*" | "java.time.*"
    )
}

fn explicit_receiver_spelling<'a>(
    source: &'a str,
    call: tree_sitter::Node<'_>,
    callee: tree_sitter::Node<'_>,
) -> Option<&'a str> {
    let receiver = explicit_receiver_node(call, callee)?;
    receiver.utf8_text(source.as_bytes()).ok().map(str::trim)
}

fn explicit_receiver_node<'tree>(
    call: tree_sitter::Node<'tree>,
    callee: tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    if call.kind() == "method_invocation" {
        call.child_by_field_name("object")
    } else {
        let mut navigation = callee.parent()?;
        while navigation.id() != call.id() && navigation.kind() != "navigation_expression" {
            navigation = navigation.parent()?;
        }
        if navigation.id() == call.id() {
            return None;
        }
        let mut cursor = navigation.walk();
        navigation
            .named_children(&mut cursor)
            .find(|child| child.id() != callee.id())
    }
}

fn explicit_receiver_is_type_qualifier<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    callee: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> bool {
    let Some(spelling) = explicit_receiver_spelling(source, call, callee) else {
        return false;
    };
    if explicit_receiver_node(call, callee).is_some_and(|node| node.kind() == "call_expression") {
        return false;
    }
    spelling.chars().next().is_some_and(char::is_uppercase)
        && !provider.symbols_named(spelling).into_iter().any(|symbol| {
            symbol.id.file == file
                && symbol.location.start_byte <= call.start_byte()
                && !is_type_symbol(&symbol.id)
        })
}
fn retain_matching_arity(candidates: &mut Vec<SymbolId>, call: tree_sitter::Node<'_>) {
    let argument_count = call_argument_count(call);
    candidates.retain(|candidate| candidate.parameters.len() == argument_count);
}
fn inferred_platform_initializer_type<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    declaration: tree_sitter::Node<'_>,
    provider: &P,
) -> Option<String> {
    let declaration_text = declaration.utf8_text(source.as_bytes()).ok()?;
    let (_, initializer) = declaration_text.split_once('=')?;
    let initializer = initializer.trim().trim_end_matches(';').trim();
    let mut pending = vec![declaration];
    while let Some(candidate) = pending.pop() {
        if matches!(candidate.kind(), "call_expression" | "method_invocation")
            && candidate.utf8_text(source.as_bytes()).ok() == Some(initializer)
            && let Some(return_type) =
                established_platform_call_return_type(source, file, candidate, provider)
        {
            return Some(return_type.to_string());
        }
        let mut cursor = candidate.walk();
        pending.extend(candidate.named_children(&mut cursor));
    }
    None
}

fn known_locale_argument<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    provider: &P,
) -> bool {
    let Some(arguments) = call.child_by_field_name("value_arguments").or_else(|| {
        call.child_by_field_name("arguments").or_else(|| {
            let mut cursor = call.walk();
            call.named_children(&mut cursor).find(|child| {
                matches!(
                    child.kind(),
                    "arguments" | "argument_list" | "value_arguments" | "value_argument_list"
                )
            })
        })
    }) else {
        return false;
    };
    let mut cursor = arguments.walk();
    let values = arguments
        .named_children(&mut cursor)
        .filter(|child| matches!(child.kind(), "value_argument" | "argument"))
        .collect::<Vec<_>>();
    let [argument] = values.as_slice() else {
        return false;
    };
    let argument_text = argument
        .utf8_text(source.as_bytes())
        .unwrap_or_default()
        .trim();
    let mut pending = vec![*argument];
    while let Some(candidate) = pending.pop() {
        if matches!(candidate.kind(), "call_expression" | "method_invocation")
            && candidate.utf8_text(source.as_bytes()).ok() == Some(argument_text)
            && established_platform_call_return_type(source, file, candidate, provider)
                == Some("java.util.Locale")
        {
            return true;
        }
        let mut cursor = candidate.walk();
        pending.extend(candidate.named_children(&mut cursor));
    }

    let argument_text = argument_text
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    let name = argument_text.trim_matches('`');
    if name.is_empty()
        || !name.chars().enumerate().all(|(position, ch)| {
            ch == '_' || ch.is_alphanumeric() && (position > 0 || ch.is_alphabetic())
        })
    {
        return false;
    }

    let mut enclosing = call.parent();
    while let Some(node) = enclosing {
        if matches!(
            node.kind(),
            "method_declaration"
                | "function_declaration"
                | "constructor_declaration"
                | "secondary_constructor"
        ) {
            break;
        }
        enclosing = node.parent();
    }
    let scope = enclosing.unwrap_or_else(|| {
        let mut root = call;
        while let Some(parent) = root.parent() {
            root = parent;
        }
        root
    });
    let mut pending = vec![scope];
    let mut binding = None::<(usize, Option<String>, tree_sitter::Node<'_>)>;
    while let Some(candidate) = pending.pop() {
        if candidate.start_byte() < call.start_byte()
            && matches!(
                candidate.kind(),
                "local_variable_declaration"
                    | "field_declaration"
                    | "property_declaration"
                    | "formal_parameter"
                    | "parameter"
                    | "class_parameter"
            )
        {
            let ty = candidate
                .child_by_field_name("type")
                .or_else(|| named_type_child(candidate))
                .and_then(|type_node| type_node.utf8_text(source.as_bytes()).ok())
                .map(str::to_owned);
            let candidate_name = match candidate.kind() {
                "property_declaration" => {
                    let mut cursor = candidate.walk();
                    candidate
                        .named_children(&mut cursor)
                        .find(|child| child.kind() == "variable_declaration")
                        .and_then(|child| node_name(child, source))
                }
                "local_variable_declaration" | "field_declaration" => {
                    let mut cursor = candidate.walk();
                    candidate
                        .named_children(&mut cursor)
                        .filter(|child| child.kind() == "variable_declarator")
                        .filter_map(|child| node_name(child, source))
                        .find(|candidate_name| candidate_name == name)
                }
                _ => node_name(candidate, source),
            };
            if candidate_name.as_deref() == Some(name)
                && binding
                    .as_ref()
                    .is_none_or(|(start, _, _)| candidate.start_byte() > *start)
            {
                binding = Some((candidate.start_byte(), ty, candidate));
            }
        }
        let mut cursor = candidate.walk();
        pending.extend(candidate.named_children(&mut cursor));
    }

    if let Some((_, Some(ty), _)) = binding.as_ref() {
        return standard_external_type(source, provider, ty, false).as_deref()
            == Some("java.util.Locale");
    }
    if let Some((_, None, declaration)) = binding {
        return inferred_platform_initializer_type(source, file, declaration, provider).as_deref()
            == Some("java.util.Locale");
    }

    provider
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| {
            symbol.id.file == file
                && symbol.location.start_byte <= call.start_byte()
                && !is_type_symbol(&symbol.id)
        })
        .max_by_key(|symbol| symbol.location.start_byte)
        .and_then(|symbol| match &symbol.ty {
            FactStatus::Established(TypeRef {
                ty: KotlinType::Named { name, .. },
                ..
            }) => standard_external_type(source, provider, name, false),
            _ => None,
        })
        .is_some_and(|ty| ty == "java.util.Locale")
}

fn call_argument_count(call: tree_sitter::Node<'_>) -> usize {
    let argument_list = call
        .child_by_field_name("arguments")
        .or_else(|| {
            call.child_by_field_name("value_arguments").or_else(|| {
                let mut cursor = call.walk();
                call.named_children(&mut cursor).find(|child| {
                    matches!(
                        child.kind(),
                        "arguments" | "argument_list" | "value_arguments" | "value_argument_list"
                    )
                })
            })
        })
        .or_else(|| {
            // The Kotlin grammar represents `consume(1) { ... }` as an outer call
            // whose callee is an inner `call_expression` (`consume(1)`). Its
            // argument list belongs to that callee node, but is still an explicit
            // argument of the outer call.
            let mut cursor = call.walk();
            call.named_children(&mut cursor)
                .find(|child| child.kind() == "call_expression")
                .and_then(|callee| {
                    callee
                        .child_by_field_name("arguments")
                        .or_else(|| callee.child_by_field_name("value_arguments"))
                        .or_else(|| {
                            let mut cursor = callee.walk();
                            callee.named_children(&mut cursor).find(|child| {
                                matches!(
                                    child.kind(),
                                    "arguments"
                                        | "argument_list"
                                        | "value_arguments"
                                        | "value_argument_list"
                                )
                            })
                        })
                })
        });
    let mut count = argument_list
        .map(|arguments| {
            let mut cursor = arguments.walk();
            arguments
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra())
                .count()
        })
        .unwrap_or(0);
    // Kotlin's grammar wraps a trailing lambda in `annotated_lambda` (even
    // without annotations). Count that wrapper as one argument, but only when
    // it is a direct child of the call and is not already inside `arguments`.
    let has_trailing_lambda = direct_trailing_lambda(call).is_some();
    if has_trailing_lambda
        && argument_list.is_none_or(|arguments| direct_trailing_lambda(arguments).is_none())
    {
        count += 1;
    }
    count
}

fn direct_trailing_lambda(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|child| {
        matches!(
            child.kind(),
            "lambda_literal" | "lambda_expression" | "annotated_lambda"
        )
    })
}
fn has_wildcard_import(source: &str) -> bool {
    source.lines().any(|line| {
        line.trim().strip_prefix("import ").is_some_and(|path| {
            path.split(" as ")
                .next()
                .unwrap_or(path)
                .trim()
                .ends_with(".*")
        })
    })
}

/// A Java `java.util.*` import cannot shadow Kotlin's collection factory
/// functions, but arbitrary wildcard imports can. Keep unresolved wildcard
/// calls conservative unless every wildcard import is exactly `java.util.*`.
/// Use the emitter's structural proof for populated calls too. Kotlin
/// read-only collections intentionally map to ordinary mutable Java
/// collections; arbitrary package wildcard imports remain unsupported.
fn known_collection_factory_from_java_util_wildcard<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node,
    callee: tree_sitter::Node,
    name: &str,
    provider: &P,
) -> bool {
    let empty_factory = matches!(name, "emptyList" | "emptySet" | "emptyMap");
    if (!is_lowered_collection_factory(name) && !empty_factory)
        || callee
            .parent()
            .is_some_and(|parent| parent.kind() == "navigation_expression")
    {
        return false;
    }

    let mut saw_java_util_wildcard = false;
    for import in kotlin_import_lines(source) {
        let path = import.split(" as ").next().unwrap_or(import).trim();
        if path.ends_with(".*") {
            if path != "java.util.*" || import.contains(" as ") {
                return false;
            }
            saw_java_util_wildcard = true;
        } else {
            let imported_name = import
                .split_once(" as ")
                .map(|(_, alias)| alias.trim())
                .or_else(|| path.rsplit('.').next())
                .unwrap_or("");
            if imported_name == name && path != format!("kotlin.collections.{name}") {
                return false;
            }
        }
    }
    saw_java_util_wildcard
        && if empty_factory {
            call_argument_count(call) == 0
                && !is_proven_local_shadow(callee, name, source)
                && !is_proven_constructor_property_shadow(callee, name, source)
                && !same_package_factory_name_is_shadowed(source, call, name, provider)
        } else {
            known_collection_factory_shape(source, call, callee, name, provider)
        }
}

fn is_lowered_collection_factory(name: &str) -> bool {
    matches!(
        name,
        "listOf"
            | "mutableListOf"
            | "arrayListOf"
            | "setOf"
            | "mutableSetOf"
            | "linkedSetOf"
            | "hashSetOf"
            | "mapOf"
            | "mutableMapOf"
            | "linkedMapOf"
            | "hashMapOf"
    )
}

/// Admit only call forms whose eager Kotlin argument evaluation maps directly
/// to the Java collection constructors/collector. Pair factories are accepted
/// only when every argument is the stdlib infix `to` form; spreads, named args,
/// explicit type arguments, custom `to` declarations, and imported shadows
/// stay Kotlin.
pub(crate) fn known_collection_factory_shape<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    callee: tree_sitter::Node<'_>,
    name: &str,
    provider: &P,
) -> bool {
    if !is_lowered_collection_factory(name) {
        return true;
    }
    if callee
        .parent()
        .is_some_and(|parent| parent.kind() == "navigation_expression")
        || is_proven_local_shadow(callee, name, source)
        || is_proven_constructor_property_shadow(callee, name, source)
        || same_package_factory_name_is_shadowed(source, call, name, provider)
    {
        return false;
    }
    let mut pending = vec![callee];
    while let Some(node) = pending.pop() {
        if node.kind() == "type_arguments" {
            return false;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    let Some(arguments) = call
        .named_children(&mut call.walk())
        .find(|child| child.kind() == "value_arguments")
    else {
        return false;
    };
    for import in kotlin_import_lines(source) {
        let (path, alias) = import
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((import.trim(), None));
        if path.ends_with(".*") && path != "java.util.*" {
            return false;
        }
        let stdlib_path = format!("kotlin.collections.{name}");
        if (alias == Some(name) || (alias.is_none() && path.rsplit('.').next() == Some(name)))
            && path != stdlib_path
        {
            return false;
        }
    }
    let value_arguments: Vec<_> = arguments
        .named_children(&mut arguments.walk())
        .filter(|child| child.kind() == "value_argument")
        .collect();
    if value_arguments.is_empty() {
        return true;
    }
    let mut expressions = Vec::with_capacity(value_arguments.len());
    for argument in value_arguments {
        if argument.child_by_field_name("name").is_some() {
            return false;
        }
        let mut cursor = argument.walk();
        let named: Vec<_> = argument.named_children(&mut cursor).collect();
        if named.len() != 1 || named[0].kind() == "spread_expression" {
            return false;
        }
        expressions.push(named[0]);
    }
    if !matches!(name, "mapOf" | "mutableMapOf" | "linkedMapOf" | "hashMapOf") {
        return true;
    }
    if same_package_factory_name_is_shadowed(source, call, "to", provider) {
        return false;
    }
    for import in kotlin_import_lines(source) {
        let (path, alias) = import
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((import.trim(), None));
        if (alias == Some("to") || (alias.is_none() && path.rsplit('.').next() == Some("to")))
            && path != "kotlin.to"
        {
            return false;
        }
    }
    expressions.into_iter().all(|expression| {
        let pair = if expression.kind() == "parenthesized_expression" {
            expression
                .named_children(&mut expression.walk())
                .find(|child| child.kind() == "infix_expression")
                .unwrap_or(expression)
        } else {
            expression
        };
        if pair.kind() != "infix_expression" {
            return false;
        }
        let named: Vec<_> = pair.named_children(&mut pair.walk()).collect();
        named.len() == 3 && named[1].utf8_text(source.as_bytes()).ok() == Some("to")
    })
}

fn kotlin_import_lines(source: &str) -> impl Iterator<Item = &str> {
    source.lines().flat_map(|line| {
        line.trim()
            .strip_prefix("import ")
            .into_iter()
            .flat_map(|imports| imports.split(';').map(str::trim))
            .filter(|import| !import.is_empty())
    })
}

fn same_package_factory_name_is_shadowed<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    name: &str,
    provider: &P,
) -> bool {
    let package = package_name(source);
    if provider.symbols_named(name).iter().any(|symbol| {
        matches!(symbol.id.kind.as_str(), "function" | "property")
            && symbol.id.package == package
            && symbol.id.owner_path.is_empty()
    }) {
        return true;
    }

    // `to` in a map literal is an infix call, so lexical bindings and member
    // extensions must be checked at that expression's location. Unrelated
    // declarations elsewhere in indexed files are not candidates in Kotlin's
    // scope and must not suppress the stdlib factory proof.
    let mut references = Vec::new();
    if name == "to" {
        let arguments = call.child_by_field_name("value_arguments").or_else(|| {
            let mut cursor = call.walk();
            call.named_children(&mut cursor)
                .find(|child| child.kind() == "value_arguments")
        });
        if let Some(arguments) = arguments {
            let mut cursor = arguments.walk();
            for argument in arguments
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "value_argument")
            {
                let mut nested = argument.walk();
                if let Some(expression) = argument
                    .named_children(&mut nested)
                    .find(|child| child.kind() != "annotation")
                {
                    let mut pair = expression;
                    while pair.kind() == "parenthesized_expression" {
                        let mut child_cursor = pair.walk();
                        let Some(inner) = pair
                            .named_children(&mut child_cursor)
                            .find(|child| child.kind() != "annotation")
                        else {
                            break;
                        };
                        pair = inner;
                    }
                    if pair.kind() != "infix_expression" {
                        continue;
                    }
                    let mut children = pair.walk();
                    let named = pair.named_children(&mut children).collect::<Vec<_>>();
                    if named.len() == 3 {
                        references.push(named[1]);
                    }
                }
            }
        }
    } else {
        references.push(call);
    }

    references.into_iter().any(|reference| {
        if is_proven_local_shadow(reference, name, source)
            || is_proven_local_function_shadow(reference, name, source)
        {
            return true;
        }
        let mut constructor_defaults = false;
        let mut ancestor = reference.parent();
        while let Some(node) = ancestor {
            if node.kind() == "primary_constructor" {
                let mut cursor = node.walk();
                if node.named_children(&mut cursor).any(|parameters| {
                    parameters.kind() == "class_parameters" && {
                        let mut cursor = parameters.walk();
                        parameters.named_children(&mut cursor).any(|parameter| {
                            parameter.kind() == "class_parameter"
                                && parameter.start_byte() < reference.start_byte()
                                && node_name(parameter, source).as_deref() == Some(name)
                        })
                    }
                }) {
                    return true;
                }
                constructor_defaults = true;
            }
            if node.kind() == "function_declaration" && callable_receiver(source, node).is_some() {
                // An extension body has an additional implicit receiver whose
                // members are not established by lexical classifier lookup.
                return true;
            }
            if matches!(node.kind(), "lambda_literal" | "function_literal") {
                // Receiver lambdas may introduce implicit extension receivers;
                // without a receiver-type fact, do not assume `to` is stdlib.
                return true;
            }
            if node.kind() == "companion_object" {
                let Some(outer) = node.parent().and_then(|parent| {
                    let mut ancestor = Some(parent);
                    while let Some(candidate) = ancestor {
                        if matches!(
                            candidate.kind(),
                            "class_declaration" | "interface_declaration" | "object_declaration"
                        ) {
                            return Some(candidate);
                        }
                        ancestor = candidate.parent();
                    }
                    None
                }) else {
                    return true;
                };
                let Some(owner) = indexed_type_at(outer, source, &package, provider) else {
                    return true;
                };
                if !declaration_supertypes(source, node, &owner.file).is_empty() {
                    // Companion inheritance is not indexed as a classifier.
                    return true;
                }
                if provider.symbols_named(name).iter().any(|symbol| {
                    symbol.id.file == owner.file
                        && symbol.id.package == owner.package
                        && symbol.location.start_byte >= node.start_byte()
                        && symbol.location.end_byte <= node.end_byte()
                        && matches!(symbol.id.kind.as_str(), "function" | "property")
                }) {
                    return true;
                }
            } else if matches!(
                node.kind(),
                "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "object_declaration"
            ) {
                let Some(owner) = indexed_type_at(node, source, &package, provider) else {
                    return true;
                };
                if constructor_defaults {
                    // A primary constructor default has no instance receiver.
                    // Its class's companion is in scope, but inherited
                    // instance members cannot shadow the collection factory.
                    constructor_defaults = false;
                    let mut cursor = node.walk();
                    let body = node
                        .named_children(&mut cursor)
                        .find(|child| child.kind() == "class_body");
                    if body.is_some_and(|body| {
                        let mut cursor = body.walk();
                        body.named_children(&mut cursor)
                            .filter(|child| child.kind() == "companion_object")
                            .any(|companion| {
                                !declaration_supertypes(source, companion, &owner.file).is_empty()
                                    || provider.symbols_named(name).iter().any(|symbol| {
                                        symbol.id.file == owner.file
                                            && symbol.location.start_byte >= companion.start_byte()
                                            && symbol.location.end_byte <= companion.end_byte()
                                    })
                            })
                    }) {
                        return true;
                    }
                    ancestor = node.parent();
                    continue;
                }
                let mut expected_path = owner.owner_path.clone();
                expected_path.push(owner.name.clone());
                let direct_shadow = provider.symbols_named(name).iter().any(|symbol| {
                    symbol.id.package == owner.package
                        && symbol.id.owner_path == expected_path
                        && symbol.id.file == owner.file
                        && matches!(symbol.id.kind.as_str(), "function" | "property")
                });
                if direct_shadow
                    || provider
                        .inherited_members(&owner, name)
                        .iter()
                        .any(|symbol| matches!(symbol.id.kind.as_str(), "function" | "property"))
                    || !provider.has_complete_inheritance(&owner)
                {
                    return true;
                }
            }
            ancestor = node.parent();
        }
        false
    })
}

fn indexed_type_at<P: SemanticProvider + ?Sized>(
    node: tree_sitter::Node<'_>,
    source: &str,
    package: &str,
    provider: &P,
) -> Option<SymbolId> {
    let name = node_name(node, source)?;
    let candidates = provider
        .symbols_named(&name)
        .into_iter()
        .filter(|symbol| {
            symbol.id.package == package
                && symbol.location.start_byte == node.start_byte()
                && is_type_symbol(&symbol.id)
        })
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    match candidates.as_slice() {
        [owner] => Some(owner.clone()),
        _ => None,
    }
}

fn is_proven_local_function_shadow(
    reference: tree_sitter::Node<'_>,
    name: &str,
    source: &str,
) -> bool {
    let reference_scope = nearest_lexical_scope(reference);
    let mut scope = reference.parent();
    while let Some(owner) = scope {
        if matches!(
            owner.kind(),
            "function_declaration" | "lambda_literal" | "function_literal"
        ) {
            let mut stack = vec![owner];
            while let Some(candidate) = stack.pop() {
                if candidate.kind() == "function_declaration"
                    && node_name(candidate, source).as_deref() == Some(name)
                    && nearest_lexical_scope(candidate) == reference_scope
                {
                    return true;
                }
                let mut cursor = candidate.walk();
                stack.extend(candidate.named_children(&mut cursor));
            }
            return false;
        }
        scope = owner.parent();
    }
    false
}

fn fact_from_candidates(mut candidates: Vec<SymbolId>) -> FactStatus<SymbolId> {
    candidates.sort();
    candidates.dedup();
    match candidates.as_slice() {
        [id] => FactStatus::Established(id.clone()),
        _ if !candidates.is_empty() => FactStatus::Ambiguous(candidates),
        _ => FactStatus::Unknown,
    }
}
fn known_builtin(name: &str) -> bool {
    matches!(
        name,
        "print"
            | "println"
            | "TODO"
            | "error"
            | "require"
            | "requireNotNull"
            | "check"
            | "checkNotNull"
            | "assert"
            | "lazy"
            | "run"
            | "with"
            | "let"
            | "also"
            | "apply"
            | "use"
            | "listOf"
            | "mutableListOf"
            | "arrayListOf"
            | "emptyList"
            | "setOf"
            | "mutableSetOf"
            | "linkedSetOf"
            | "sortedSetOf"
            | "emptySet"
            | "mapOf"
            | "mutableMapOf"
            | "linkedMapOf"
            | "hashMapOf"
            | "emptyMap"
            | "arrayOf"
            | "emptyArray"
            | "intArrayOf"
            | "longArrayOf"
            | "shortArrayOf"
            | "byteArrayOf"
            | "charArrayOf"
            | "booleanArrayOf"
            | "floatArrayOf"
            | "doubleArrayOf"
            | "sequenceOf"
            | "emptySequence"
            | "maxOf"
            | "minOf"
            | "repeat"
            | "synchronized"
            | "String"
            | "Object"
            | "ArrayList"
            | "LinkedList"
            | "HashMap"
            | "TreeMap"
            | "HashSet"
            | "Collections"
            | "Math"
            | "Integer"
            | "Long"
            | "Boolean"
            | "File"
            | "Path"
            | "Duration"
            | "BigInteger"
            | "Pair"
            | "Triple"
            | "Regex"
            | "Exception"
            | "RuntimeException"
            | "Class"
            | "Thread"
            | "AtomicBoolean"
            | "AtomicInteger"
            | "ConcurrentHashMap"
    )
}

/// Retain owners whose bare property reads cannot be emitted as Java variable
/// references. `retained_hint` contains exact property identities that the
/// current workspace round has decided must remain Kotlin.
pub fn plan_required_property_references<P: SemanticProvider + ?Sized>(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    provider: &P,
    retained_hint: &std::collections::HashSet<SymbolId>,
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    let hash = *blake3::hash(source.as_bytes()).as_bytes();
    let owners = DeclarationOwnerIndex::new(plan);
    let package = package_name(source);
    let imports: Vec<_> = source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("import "))
        .filter_map(|line| {
            let (path, alias) = line
                .split_once(" as ")
                .map(|(p, a)| (p.trim(), Some(a.trim())))
                .unwrap_or((line, None));
            let (pkg, name) = path.rsplit_once('.')?;
            if name == "*" {
                Some((pkg.to_owned(), None, None))
            } else {
                Some((
                    pkg.to_owned(),
                    Some(name.to_owned()),
                    Some(alias.unwrap_or(name).to_owned()),
                ))
            }
        })
        .collect();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "identifier" | "simple_identifier")
            && !is_non_reference_identifier(node, source)
        {
            let spelling = node.utf8_text(source.as_bytes()).unwrap_or("");
            let exact_import = imports
                .iter()
                .find(|(_, name, alias)| name.is_some() && alias.as_deref() == Some(spelling));
            let wildcard_packages: Vec<_> = imports
                .iter()
                .filter(|(_, name, _)| name.is_none())
                .map(|(p, _, _)| p.as_str())
                .collect();
            let wanted_name = exact_import
                .and_then(|(_, name, _)| name.as_deref())
                .unwrap_or(spelling);
            let properties: Vec<_> = provider
                .symbols_named(wanted_name)
                .into_iter()
                .filter(|s| {
                    s.id.kind == "property"
                        && s.id.owner_path.is_empty()
                        && if let Some((p, n, _)) = exact_import {
                            s.id.package == *p && Some(s.id.name.as_str()) == n.as_deref()
                        } else if !wildcard_packages.is_empty() {
                            wildcard_packages.contains(&s.id.package.as_str())
                        } else {
                            s.id.package == package
                        }
                })
                .map(|s| s.id.clone())
                .collect();
            let Some(index) = owners.owner(node) else {
                let mut c = node.walk();
                stack.extend(node.named_children(&mut c));
                continue;
            };
            let crosses_file = properties.iter().any(|p| p.file != file);
            let retained = properties
                .iter()
                .any(|p| crate::transpiler::retention_queries::contains(retained_hint, p));
            if !properties.is_empty()
                && (retained || crosses_file)
                && !is_proven_local_shadow(node, spelling, source)
                && !is_proven_constructor_property_shadow(node, spelling, source)
                && same_file_string_const_literal(source, file, spelling, node, provider).is_none()
            {
                let decision = &mut plan.declarations[index];
                let reason = crate::translation_plan::RetentionReason::PreparationBlocker {
                    kind: crate::translation_plan::PreparationBlockerKind::UnresolvedAssumption,
                    message: if crosses_file {
                        format!(
                            "cross-file top-level property `{spelling}` has no safe bare Java reference"
                        )
                    } else {
                        format!(
                            "bare read of retained Kotlin property `{spelling}` cannot be emitted as a Java variable"
                        )
                    },
                };
                if !decision.retention_reasons.contains(&reason) {
                    decision.retention_reasons.push(reason);
                    decision.candidate_owner = crate::translation_plan::BackendOwner::Kotlin;
                }
                let candidates = properties.clone();
                let resolution = match candidates.as_slice() {
                    [id] => FactStatus::Established(id.clone()),
                    _ => FactStatus::Ambiguous(candidates),
                };
                plan.dependencies
                    .push(crate::translation_plan::SymbolDependency {
                        from: decision.symbol_id.clone(),
                        spelling: spelling.to_owned(),
                        resolution,
                    });
                plan.diagnostics.push(SemanticDiagnostic {
                    code: "S005".into(),
                    message: format!("property reference `{spelling}` requires Kotlin ownership"),
                    location: SourceLocation {
                        file: file.to_path_buf(),
                        snapshot_hash: hash,
                        start_byte: node.start_byte(),
                        end_byte: node.end_byte(),
                    },
                });
            }
        }
        let mut c = node.walk();
        stack.extend(node.named_children(&mut c));
    }
}
fn is_non_reference_identifier(node: tree_sitter::Node, source: &str) -> bool {
    let mut parent = node.parent();
    while let Some(p) = parent {
        if matches!(
            p.kind(),
            "annotation"
                | "annotation_entry"
                | "import"
                | "package_header"
                | "user_type"
                | "type_identifier"
                | "nullable_type"
                | "function_type"
        ) {
            return true;
        }
        if matches!(
            p.kind(),
            "property_declaration"
                | "function_declaration"
                | "class_declaration"
                | "object_declaration"
                | "type_alias"
                | "class_parameter"
                | "parameter"
        ) && declaration_identifier(p, source).is_some_and(|name| {
            name.start_byte() == node.start_byte() && name.end_byte() == node.end_byte()
        }) {
            return true;
        }
        // The navigation suffix contains the selector (`obj.property`), which
        // is not a lexical top-level variable reference. The receiver is not
        // inside this suffix and must continue through normal lookup.
        if p.kind() == "navigation_suffix" {
            return true;
        }
        if p.kind() == "value_argument"
            && p.child_by_field_name("name")
                .is_some_and(|name| name.start_byte() == node.start_byte())
        {
            return true;
        }
        parent = p.parent();
    }
    false
}
fn declaration_identifier<'a>(
    node: tree_sitter::Node<'a>,
    source: &str,
) -> Option<tree_sitter::Node<'a>> {
    if let Some(name) = node.child_by_field_name("name") {
        return Some(name);
    }
    let mut c = node.walk();
    let named: Vec<_> = node.named_children(&mut c).collect();
    if node.kind() == "property_declaration" {
        return named
            .iter()
            .find(|n| n.kind() == "variable_declaration")
            .and_then(|variable| {
                let mut w = variable.walk();
                variable
                    .named_children(&mut w)
                    .find(|n| matches!(n.kind(), "identifier" | "simple_identifier"))
            });
    }
    named.into_iter().find(|n| {
        matches!(n.kind(), "identifier" | "simple_identifier")
            && n.utf8_text(source.as_bytes()).is_ok()
    })
}
fn is_proven_local_shadow(node: tree_sitter::Node, name: &str, source: &str) -> bool {
    let mut scope = node.parent();
    while let Some(owner) = scope {
        if matches!(
            owner.kind(),
            "function_declaration" | "lambda_literal" | "function_literal"
        ) {
            let mut stack = vec![owner];
            while let Some(candidate) = stack.pop() {
                if matches!(
                    candidate.kind(),
                    "parameter" | "class_parameter" | "lambda_parameters"
                ) && node_name(candidate, source).as_deref() == Some(name)
                    && nearest_lexical_scope(candidate) == owner.id()
                {
                    return true;
                }
                if candidate.kind() == "lambda_parameters" {
                    let mut c = candidate.walk();
                    if candidate.named_children(&mut c).any(|parameter| {
                        matches!(parameter.kind(), "identifier" | "simple_identifier")
                            && parameter.utf8_text(source.as_bytes()).ok() == Some(name)
                    }) && nearest_lexical_scope(candidate) == owner.id()
                    {
                        return true;
                    }
                }
                if candidate.kind() == "variable_declaration"
                    && candidate.start_byte() < node.start_byte()
                    && node_name(candidate, source).as_deref() == Some(name)
                {
                    let candidate_scope = nearest_lexical_scope(candidate);
                    let mut reference_scope = Some(node);
                    while let Some(reference) = reference_scope {
                        if candidate_scope == reference.id() {
                            return true;
                        }
                        reference_scope = reference.parent();
                    }
                }
                let mut c = candidate.walk();
                stack.extend(candidate.named_children(&mut c));
            }
        }
        scope = owner.parent();
    }
    false
}
/// A property parameter is an instance member in its owning class, so it
/// shadows same-named imported top-level properties in instance declarations.
/// Stop at the nearest receiver boundary: a companion/object has no implicit
/// instance receiver for its enclosing class.
fn is_proven_constructor_property_shadow(
    node: tree_sitter::Node,
    name: &str,
    source: &str,
) -> bool {
    let mut scope = node.parent();
    while let Some(owner) = scope {
        match owner.kind() {
            "companion_object" | "object_declaration" => return false,
            "class_declaration" => {
                let Some(parameters) = owner
                    .child_by_field_name("primary_constructor")
                    .or_else(|| {
                        let mut c = owner.walk();
                        owner
                            .named_children(&mut c)
                            .find(|child| child.kind() == "primary_constructor")
                    })
                    .and_then(|constructor| {
                        let mut c = constructor.walk();
                        constructor
                            .named_children(&mut c)
                            .find(|child| child.kind() == "class_parameters")
                    })
                else {
                    return false;
                };
                let mut c = parameters.walk();
                return parameters.named_children(&mut c).any(|parameter| {
                    is_property_parameter(parameter)
                        && node_name(parameter, source).as_deref() == Some(name)
                });
            }
            _ => {}
        }
        scope = owner.parent();
    }
    false
}
fn nearest_lexical_scope(mut node: tree_sitter::Node) -> usize {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "block" | "lambda_literal" | "function_literal" | "function_declaration"
        ) {
            return parent.id();
        }
        node = parent
    }
    node.id()
}
impl std::fmt::Display for TypeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}{}", self.ty, if self.nullable { "?" } else { "" })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisConfig {
    pub version: u32,
    pub module: String,
    #[serde(default)]
    pub classpath: Vec<PathBuf>,
    #[serde(default)]
    pub compiler_args: Vec<String>,
    #[serde(default)]
    pub language_targets: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisRequest {
    pub version: u32,
    pub config: AnalysisConfig,
    pub files: BTreeMap<PathBuf, String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisResponse {
    pub version: u32,
    pub symbols: Vec<SemanticSymbol>,
    pub resolved_references: Vec<(SourceLocation, FactStatus<SymbolId>)>,
    pub jvm_signatures: Vec<(SymbolId, JvmSignature)>,
    pub diagnostics: Vec<String>,
    pub capabilities: Vec<String>,
}
pub fn analyze_request(r: AnalysisRequest) -> AnalysisResponse {
    let version_ok = r.version == 1 && r.config.version == 1;
    let module = r.config.module.clone();
    let p = SyntaxSemanticProvider::new_in_module(module, r.files);
    let mut diagnostics = if version_ok {
        vec![]
    } else {
        vec![format!(
            "unsupported analysis request version (request {}, config {})",
            r.version, r.config.version
        )]
    };
    let mut identities = BTreeMap::<SymbolId, usize>::new();
    for symbol in &p.symbols {
        *identities.entry(symbol.id.clone()).or_default() += 1
    }
    for (id, count) in identities.into_iter().filter(|(_, count)| *count > 1) {
        diagnostics.push(format!(
            "ambiguous duplicate declaration identity `{}` ({count} declarations)",
            id.stable_key()
        ));
    }
    AnalysisResponse {
        version: 1,
        symbols: p.symbols,
        resolved_references: vec![],
        jvm_signatures: vec![],
        diagnostics,
        capabilities: vec!["syntax-symbols".into(), "structured-kotlin-types".into()],
    }
}

#[cfg(test)]
mod call_argument_count_tests {
    use super::call_argument_count;

    #[test]
    fn counts_parenthesized_and_wrapped_trailing_lambdas_once() {
        let source = r#"
            fun sample() {
                consume { it }
                consume({ it })
                consume(1) { it }
                consume()
            }
        "#;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut found = Vec::new();
        fn visit<'tree>(
            node: tree_sitter::Node<'tree>,
            source: &str,
            found: &mut Vec<(String, usize)>,
        ) {
            if node.kind() == "call_expression" {
                let call_text = &source[node.start_byte()..node.end_byte()];
                found.push((call_text.to_owned(), call_argument_count(node)));
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                visit(child, source, found);
            }
        }
        visit(tree.root_node(), source, &mut found);
        // A trailing-lambda call is represented as an outer call plus the
        // call-expression used as its callee, so the inner invocation also
        // appears independently in the syntax tree.
        assert_eq!(found.len(), 5, "calls: {found:?}");
        assert_eq!(found[0].1, 1, "wrapped trailing lambda: {found:?}");
        assert_eq!(found[1].1, 1, "parenthesized lambda: {found:?}");
        assert_eq!(found[2].1, 2, "selector plus trailing lambda: {found:?}");
        assert_eq!(found[3].1, 1, "callee invocation: {found:?}");
        assert_eq!(found[4].1, 0, "empty call: {found:?}");
    }
}

#[cfg(test)]
mod syntax_provider_index_tests {
    use super::*;

    #[test]
    fn parallel_source_collection_preserves_serial_facts_and_input_order() {
        let sources = vec![
            (
                PathBuf::from("z/Types.kt"),
                r#"package sample
typealias Label = String
interface Marker {
    val value: String
}
class Key(override val value: String) : Marker {
    companion object {
        fun create(): Key = Key("value")
    }
    @Throws(java.io.IOException::class)
    fun read(): String = value
}
enum class Choice { FIRST, SECOND }
@JvmInline value class Token(val value: Int)
"#
                .to_owned(),
            ),
            (
                PathBuf::from("a/Holder.java"),
                r#"package sample;
public class Holder<T extends Marker> {
    public String code;
    public static String build() { return "value"; }
    @org.jetbrains.annotations.Nullable public T getValue() { return null; }
}
"#
                .to_owned(),
            ),
            (
                PathBuf::from("other/Types.kt"),
                "package other\nclass Key {}\n".to_owned(),
            ),
            (
                PathBuf::from("a/package-info.java"),
                "@unknown.NullMarked package sample;".to_owned(),
            ),
        ];
        let serial = SyntaxSemanticProvider::from_analyzed_sources(
            "module".into(),
            sources
                .clone()
                .into_iter()
                .map(AnalyzedSource::collect)
                .collect(),
        );
        let expected = format!("{serial:#?}");
        for threads in [1, 2, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let analyzed = pool.install(|| {
                sources
                    .clone()
                    .into_par_iter()
                    .map(AnalyzedSource::collect)
                    .collect()
            });
            let actual = SyntaxSemanticProvider::from_analyzed_sources("module".into(), analyzed);
            assert_eq!(format!("{actual:#?}"), expected, "{threads} threads");
        }
        let actual = SyntaxSemanticProvider::new_in_module("module", sources);
        assert_eq!(format!("{actual:#?}"), expected);
        assert!(
            actual
                .symbols
                .iter()
                .all(|symbol| symbol.id.module == "module")
        );
        let keys = actual.symbols_named("Key");
        assert_eq!(
            keys.iter()
                .map(|symbol| symbol.id.package.as_str())
                .collect::<Vec<_>>(),
            ["sample", "other"]
        );
        assert!(matches!(actual.resolve("Key"), FactStatus::Ambiguous(_)));
        let FactStatus::Established(token) = actual.resolve("Token") else {
            panic!("missing Token")
        };
        assert_eq!(
            actual.is_ordinary_jvm_reference(&token),
            FactStatus::Established(false)
        );
        let FactStatus::Established(choice) = actual.resolve("Choice") else {
            panic!("missing Choice")
        };
        let FactStatus::Established(entries) = actual.enum_entries(&choice) else {
            panic!("missing enum entries")
        };
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["FIRST", "SECOND"]
        );
        assert!(!actual.has_plain_java_package_nullability("sample"));
        assert_eq!(
            actual.aliases_in_package("sample")["Label"].target,
            TypeRef::parse("String")
        );
    }

    fn legacy_resolve_type_in_file(
        provider: &SyntaxSemanticProvider,
        file: &Path,
        package: &str,
        name: &str,
    ) -> FactStatus<SymbolId> {
        let spelling = name.trim().trim_end_matches('?');
        let type_symbols = provider
            .symbols
            .iter()
            .filter(|symbol| is_type_symbol(&symbol.id));
        let candidates = if spelling.contains('.') {
            let (pkg, simple) = spelling.rsplit_once('.').unwrap();
            type_symbols
                .filter(|symbol| symbol.id.package == pkg && symbol.id.name == simple)
                .map(|symbol| symbol.id.clone())
                .collect::<Vec<_>>()
        } else {
            let imports = provider
                .sources
                .get(file)
                .map(|source| import_paths(source))
                .unwrap_or_default();
            let explicit = imports
                .iter()
                .filter_map(|path| {
                    let (pkg, imported) = path.rsplit_once('.')?;
                    (imported == spelling).then_some((pkg, imported))
                })
                .flat_map(|(pkg, simple)| {
                    provider
                        .symbols
                        .iter()
                        .filter(move |symbol| {
                            is_type_symbol(&symbol.id)
                                && symbol.id.package == pkg
                                && symbol.id.name == simple
                        })
                        .map(|symbol| symbol.id.clone())
                })
                .collect::<Vec<_>>();
            if !explicit.is_empty() {
                explicit
            } else {
                let same_package = provider
                    .symbols
                    .iter()
                    .filter(|symbol| {
                        is_type_symbol(&symbol.id)
                            && symbol.id.package == package
                            && symbol.id.name == spelling
                    })
                    .map(|symbol| symbol.id.clone())
                    .collect::<Vec<_>>();
                if !same_package.is_empty() {
                    same_package
                } else {
                    imports
                        .iter()
                        .filter(|path| path.ends_with(".*"))
                        .flat_map(|path| {
                            let pkg = path.trim_end_matches(".*");
                            provider
                                .symbols
                                .iter()
                                .filter(move |symbol| {
                                    is_type_symbol(&symbol.id)
                                        && symbol.id.package == pkg
                                        && symbol.id.name == spelling
                                })
                                .map(|symbol| symbol.id.clone())
                        })
                        .collect()
                }
            }
        };
        fact_from_candidates(candidates)
    }

    fn legacy_parents(provider: &SyntaxSemanticProvider) -> BTreeMap<SymbolId, Vec<SymbolId>> {
        let mut parents = BTreeMap::<SymbolId, Vec<SymbolId>>::new();
        for (file, source) in &provider.sources {
            let Some(tree) = parse_source(file, source) else {
                continue;
            };
            let mut stack = vec![tree.root_node()];
            while let Some(node) = stack.pop() {
                if matches!(
                    node.kind(),
                    "class_declaration"
                        | "interface_declaration"
                        | "object_declaration"
                        | "enum_declaration"
                ) {
                    let owner = provider
                        .symbols
                        .iter()
                        .find(|symbol| {
                            symbol.id.file == *file
                                && symbol.location.start_byte == node.start_byte()
                                && is_type_symbol(&symbol.id)
                        })
                        .map(|symbol| symbol.id.clone());
                    if let Some(owner) = owner {
                        for name in declaration_supertypes(source, node, file) {
                            if let FactStatus::Established(parent) = legacy_resolve_type_in_file(
                                provider,
                                &owner.file,
                                &owner.package,
                                &name,
                            ) {
                                parents.entry(owner.clone()).or_default().push(parent);
                            }
                        }
                    }
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
        }
        for parents in parents.values_mut() {
            parents.sort();
            parents.dedup();
        }
        parents
    }

    fn legacy_companion_owners(provider: &SyntaxSemanticProvider) -> BTreeMap<SymbolId, SymbolId> {
        let mut indexed = Vec::new();
        for (file, source) in &provider.sources {
            if !source.contains("companion") {
                continue;
            }
            let file_symbols = provider
                .symbols
                .iter()
                .filter(|symbol| symbol.id.file == *file)
                .collect::<Vec<_>>();
            let Some(tree) = parse_source(file, source) else {
                continue;
            };
            let mut stack = vec![tree.root_node()];
            while let Some(node) = stack.pop() {
                if node.kind() == "companion_object" {
                    let mut ancestor = node.parent();
                    let mut owner = None;
                    while let Some(parent) = ancestor {
                        if matches!(
                            parent.kind(),
                            "class_declaration" | "interface_declaration" | "object_declaration"
                        ) {
                            owner = file_symbols
                                .iter()
                                .find(|symbol| {
                                    symbol.location.start_byte == parent.start_byte()
                                        && is_type_symbol(&symbol.id)
                                })
                                .map(|symbol| symbol.id.clone());
                            break;
                        }
                        ancestor = parent.parent();
                    }
                    if let Some(owner) = owner {
                        let mut expected_path = owner.owner_path.clone();
                        expected_path.push(owner.name.clone());
                        for symbol in &file_symbols {
                            let direct_path = symbol.id.owner_path == expected_path;
                            let companion_path = symbol.id.owner_path.len()
                                == expected_path.len() + 1
                                && symbol.id.owner_path.starts_with(&expected_path)
                                && symbol
                                    .id
                                    .owner_path
                                    .last()
                                    .is_some_and(|part| part == "Companion");
                            if symbol.id.file == *file
                                && symbol.location.start_byte >= node.start_byte()
                                && symbol.location.end_byte <= node.end_byte()
                                && matches!(symbol.id.kind.as_str(), "function" | "method")
                                && (direct_path || companion_path)
                            {
                                indexed.push((symbol.id.clone(), owner.clone()));
                            }
                        }
                    }
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
        }
        indexed.into_iter().collect()
    }

    fn candidates(status: FactStatus<SymbolId>) -> Vec<SymbolId> {
        match status {
            FactStatus::Established(symbol) => vec![symbol],
            FactStatus::Ambiguous(symbols) => symbols,
            FactStatus::Inferred(symbol) => vec![symbol],
            FactStatus::Unknown => Vec::new(),
        }
    }

    #[test]
    fn indexed_resolvers_match_legacy_order_with_package_and_nested_collisions() {
        let provider = SyntaxSemanticProvider::new(vec![
            (
                PathBuf::from("a.kt"),
                "package a\nclass Same {}\ninterface Base {}\nclass Child : Base {}\nclass Dup {}\nclass Dup {}\nclass Outer {\n interface NestedBase {}\n class NestedChild : NestedBase {}\n companion object { fun create(): Same = TODO() }\n}\n".into(),
            ),
            (
                PathBuf::from("b.kt"),
                "package b\nclass Same {}\ninterface Base {}\nclass Child : Base {}\n".into(),
            ),
            (
                PathBuf::from("explicit.kt"),
                "package c\nimport b.Same\nclass Uses : Same {}\n".into(),
            ),
            (
                PathBuf::from("wildcards.kt"),
                "package d\nimport a.*\nimport b.*\nclass Uses : Same {}\n".into(),
            ),
        ]);
        for (file, package, ty) in [
            (Path::new("a.kt"), "a", "Same"),
            (Path::new("a.kt"), "a", "Dup"),
            (Path::new("b.kt"), "b", "Same"),
            (Path::new("explicit.kt"), "c", "Same"),
            (Path::new("wildcards.kt"), "d", "Same"),
            (Path::new("a.kt"), "a", "a.Same"),
            (Path::new("a.kt"), "a", "Outer"),
            (Path::new("a.kt"), "a", "NestedBase"),
        ] {
            assert_eq!(
                provider.resolve_type_in_file(file, package, ty),
                legacy_resolve_type_in_file(&provider, file, package, ty),
                "{file:?} / {package} / {ty}"
            );
        }
        assert_eq!(provider.parents, legacy_parents(&provider));

        let owner = provider
            .symbols_named("Outer")
            .into_iter()
            .find(|symbol| symbol.id.file == Path::new("a.kt"))
            .unwrap();
        let new_members = provider.companion_members(&owner.id, "create");
        assert_eq!(new_members.len(), 1);
        assert_eq!(new_members[0].id.name, "create");
        assert_eq!(
            provider.companion_member_owners,
            legacy_companion_owners(&provider)
        );
    }

    #[test]
    fn name_and_file_indexes_preserve_order_and_manual_index_file_import_scope() {
        let provider = SyntaxSemanticProvider::new(vec![
            (
                PathBuf::from("first.kt"),
                "package p\nclass Same {}\nclass Same {}\n".into(),
            ),
            (
                PathBuf::from("second.kt"),
                "package p\nclass Same {}\n".into(),
            ),
        ]);
        let legacy_in_file: Vec<_> = provider
            .symbols
            .iter()
            .filter(|symbol| symbol.id.file == Path::new("first.kt") && symbol.id.name == "Same")
            .map(|symbol| symbol.id.clone())
            .collect();
        let indexed_in_file = candidates(provider.resolve_in_file(Path::new("first.kt"), "Same"));
        assert_eq!(indexed_in_file, legacy_in_file);
        let legacy_signature: Vec<_> = provider
            .symbols
            .iter()
            .filter(|symbol| symbol.id.name == "Same" && symbol.id.parameters.is_empty())
            .map(|symbol| symbol.id.clone())
            .collect();
        let indexed_signature = candidates(provider.resolve_signature("Same", &[]));
        assert_eq!(indexed_signature, legacy_signature);

        let mut manually_indexed = SyntaxSemanticProvider::default();
        manually_indexed.index_file(Path::new("a.kt"), "package a\nclass Only {}\n");
        manually_indexed.index_file(
            Path::new("manual.kt"),
            "package c\nimport a.Only\nclass Use : Only {}\n",
        );
        assert!(manually_indexed.sources.is_empty());
        assert_eq!(
            manually_indexed.resolve_type_in_file(Path::new("manual.kt"), "c", "Only"),
            FactStatus::Unknown,
            "manual index_file calls did not make import text visible before"
        );
        assert_eq!(
            manually_indexed
                .symbols_in_file(Path::new("manual.kt"))
                .len(),
            manually_indexed
                .symbols
                .iter()
                .filter(|symbol| symbol.id.file == Path::new("manual.kt"))
                .count()
        );

        let mut duplicate_location = SyntaxSemanticProvider::default();
        let source = "package repeated\nclass Same {}\n";
        duplicate_location.index_file(Path::new("repeated.kt"), source);
        let first = duplicate_location
            .symbols
            .iter()
            .position(|symbol| is_type_symbol(&symbol.id))
            .unwrap();
        duplicate_location.index_file(Path::new("repeated.kt"), source);
        let first_type_start = duplicate_location.symbols[first].location.start_byte;
        assert_eq!(
            duplicate_location
                .type_symbols_by_location
                .get(&(PathBuf::from("repeated.kt"), first_type_start)),
            Some(&first),
            "the first type symbol at a duplicated file/byte key remains the selected one"
        );
    }
}
