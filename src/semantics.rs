//! Stable, syntax-derived semantic facts for migration planning.
//!
//! This layer deliberately reports uncertainty instead of inventing compiler
//! answers. In particular, an unresolved type is never silently Object.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
                if let Some(name) = node_name(param, source)
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
    let name = if constructor {
        "<init>".to_string()
    } else {
        recovered_interface.unwrap_or_else(|| {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeAliasDefinition {
    pub parameters: Vec<String>,
    pub target: TypeRef,
}
pub trait SemanticProvider: Send + Sync {
    fn resolve(&self, name: &str) -> FactStatus<SymbolId>;
    fn symbols(&self) -> &[SemanticSymbol];
    fn symbols_named(&self, name: &str) -> Vec<&SemanticSymbol> {
        self.symbols()
            .iter()
            .filter(|symbol| symbol.id.name == name)
            .collect()
    }
    fn type_aliases(&self) -> Vec<((String, String), TypeAliasDefinition)> {
        Vec::new()
    }
    /// Members inherited by a statically identified receiver declaration.
    /// Providers without indexed ancestry deliberately return no candidates.
    fn inherited_members<'a>(&'a self, _owner: &SymbolId, _name: &str) -> Vec<&'a SemanticSymbol> {
        Vec::new()
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
    fn companion_members<'a>(&'a self, _owner: &SymbolId, _name: &str) -> Vec<&'a SemanticSymbol> {
        Vec::new()
    }
    fn is_java_static_member(&self, _symbol: &SymbolId) -> bool {
        false
    }
}

/// A conservative index over already parsed source snapshots.
#[derive(Debug, Clone, Default)]
pub struct SyntaxSemanticProvider {
    symbols: Vec<SemanticSymbol>,
    by_name: BTreeMap<String, Vec<usize>>,
    aliases: BTreeMap<(String, String), TypeAliasDefinition>,
    sources: BTreeMap<PathBuf, String>,
    parents: BTreeMap<SymbolId, Vec<SymbolId>>,
    companion_member_owners: BTreeMap<SymbolId, SymbolId>,
    java_static_member_locations: BTreeSet<(PathBuf, usize)>,
}
impl SyntaxSemanticProvider {
    pub fn new(sources: impl IntoIterator<Item = (PathBuf, String)>) -> Self {
        Self::new_in_module("", sources)
    }
    pub fn new_in_module(
        module: impl Into<String>,
        sources: impl IntoIterator<Item = (PathBuf, String)>,
    ) -> Self {
        let mut p = Self::default();
        let module = module.into();
        for (file, source) in sources {
            let first = p.symbols.len();
            p.index_file(&file, &source);
            p.sources.insert(file, source);
            for sym in &mut p.symbols[first..] {
                sym.id.module = module.clone();
            }
        }
        p.index_inheritance();
        p.index_companion_members();
        p
    }
    pub fn index_file(&mut self, file: &Path, source: &str) {
        let hash = *blake3::hash(source.as_bytes()).as_bytes();
        let mut parser = tree_sitter::Parser::new();
        let language = if file
            .extension()
            .is_some_and(|x| x.eq_ignore_ascii_case("java"))
        {
            tree_sitter_java::LANGUAGE.into()
        } else {
            tree_sitter_kotlin_ng::LANGUAGE.into()
        };
        if parser.set_language(&language).is_err() {
            return;
        }
        let Some(tree) = parser.parse(source, None) else {
            return;
        };
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if declaration_node_kind(node.kind()) || is_property_parameter(node) {
                let property_param = is_property_parameter(node);
                let id = if property_param {
                    property_parameter_symbol(source, node, file).unwrap()
                } else {
                    symbol_id_for_node(source, node, file)
                };
                let type_parameters = declaration_type_parameters(source, node);
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
                if node.kind() == "type_alias"
                    && let FactStatus::Established(target) = &ty
                {
                    self.aliases.insert(
                        (package_name(source), id.name.clone()),
                        TypeAliasDefinition {
                            parameters: type_parameters,
                            target: target.clone(),
                        },
                    );
                }
                let idx = self.symbols.len();
                self.by_name.entry(id.name.clone()).or_default().push(idx);
                if node.kind() == "method_declaration"
                    && file
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("java"))
                    && has_java_static_modifier(source, node)
                {
                    self.java_static_member_locations
                        .insert((file.to_path_buf(), node.start_byte()));
                }
                self.symbols.push(SemanticSymbol {
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
    }
    pub fn resolve_in_file(&self, file: &Path, name: &str) -> FactStatus<SymbolId> {
        let found: Vec<_> = self
            .symbols
            .iter()
            .filter(|s| s.id.file == file && s.id.name == name)
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
            .symbols
            .iter()
            .filter(|s| s.id.name == name && s.id.parameters == parameters)
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

    fn index_inheritance(&mut self) {
        let mut edges = Vec::new();
        for (file, source) in &self.sources {
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
                    let id = self
                        .symbols
                        .iter()
                        .find(|symbol| {
                            symbol.id.file == *file
                                && symbol.location.start_byte == node.start_byte()
                                && is_type_symbol(&symbol.id)
                        })
                        .map(|symbol| symbol.id.clone());
                    if let Some(id) = id {
                        let names = declaration_supertypes(source, node, file);
                        if !names.is_empty() {
                            edges.push((id, names));
                        }
                    }
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
        }
        for (owner, names) in edges {
            for name in names {
                if let FactStatus::Established(parent) =
                    self.resolve_type_in_file(&owner.file, &owner.package, &name)
                {
                    self.parents.entry(owner.clone()).or_default().push(parent);
                }
            }
        }
        for parents in self.parents.values_mut() {
            parents.sort();
            parents.dedup();
        }
    }

    fn index_companion_members(&mut self) {
        let mut indexed = Vec::new();
        for (file, source) in &self.sources {
            if !source.contains("companion") {
                continue;
            }
            let file_symbols = self
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
        self.companion_member_owners.extend(indexed);
    }
}
impl SemanticProvider for SyntaxSemanticProvider {
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
                return found;
            }
            level = level
                .iter()
                .flat_map(|id| self.parents.get(id).cloned().unwrap_or_default())
                .collect();
        }
        Vec::new()
    }
    fn resolve_type_in_file(&self, file: &Path, package: &str, name: &str) -> FactStatus<SymbolId> {
        let spelling = name.trim().trim_end_matches('?');
        let type_symbols = self
            .symbols
            .iter()
            .filter(|symbol| is_type_symbol(&symbol.id));
        let found = if spelling.contains('.') {
            let (pkg, simple) = spelling.rsplit_once('.').unwrap();
            type_symbols
                .filter(|symbol| symbol.id.package == pkg && symbol.id.name == simple)
                .map(|symbol| symbol.id.clone())
                .collect::<Vec<_>>()
        } else {
            let imports = self
                .sources
                .get(file)
                .map(|source| {
                    source
                        .lines()
                        .filter_map(|line| {
                            let line = line.trim().strip_prefix("import ")?;
                            let path = line.split(" as ").next().unwrap_or(line).trim();
                            Some(path.to_owned())
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let explicit = imports
                .iter()
                .filter_map(|path| {
                    let (pkg, imported) = path.rsplit_once('.')?;
                    (imported == spelling).then_some((pkg, imported))
                })
                .flat_map(|(pkg, simple)| {
                    self.symbols
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
                let same_package = self
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
                            self.symbols
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
        fact_from_candidates(found)
    }
    fn companion_members<'a>(&'a self, owner: &SymbolId, name: &str) -> Vec<&'a SemanticSymbol> {
        self.symbols
            .iter()
            .filter(|symbol| {
                symbol.id.name == name
                    && self.companion_member_owners.get(&symbol.id) == Some(owner)
            })
            .collect()
    }
    fn is_java_static_member(&self, symbol: &SymbolId) -> bool {
        self.symbols.iter().any(|indexed| {
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
                    let location = SourceLocation {
                        file: file.to_path_buf(),
                        snapshot_hash: hash,
                        start_byte: node.start_byte(),
                        end_byte: node.end_byte(),
                    };
                    if known_builtin(&name)
                        && matches!(fact, FactStatus::Unknown)
                        && !locally_shadowed
                        && !has_wildcard_import(source)
                        || matches!(fact, FactStatus::Unknown)
                            && !locally_shadowed
                            && known_external_member_call(
                                source, file, node, callee, &name, provider,
                            )
                    {
                    } else {
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
fn resolve_required_call<P: SemanticProvider + ?Sized>(
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
    let symbols = provider.symbols_named(name);
    if (call.kind() == "method_invocation" && call.child_by_field_name("object").is_some())
        || (call.kind() == "call_expression"
            && callee
                .parent()
                .is_some_and(|parent| parent.kind() == "navigation_expression"))
    {
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
                return fact_from_candidates(candidates);
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
                return fact_from_candidates(candidates);
            }
        }
        return FactStatus::Unknown;
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
                return fact_from_candidates(local);
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
                return fact_from_candidates(members);
            }
            let owner_candidates = provider
                .symbols()
                .iter()
                .filter(|symbol| {
                    symbol.id.file == file
                        && symbol.id.name == owner_id.name
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
                    return fact_from_candidates(inherited);
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
        fact_from_candidates(candidates)
    } else {
        FactStatus::Unknown
    }
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
                ("java.lang.StringBuilder", "append", 1) => Some("java.lang.StringBuilder"),
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
    let indexed = provider
        .symbols_named(spelling)
        .into_iter()
        .filter(|symbol| symbol.id.file == file && symbol.location.start_byte <= call.start_byte())
        .filter_map(|symbol| match &symbol.ty {
            FactStatus::Established(TypeRef {
                ty: KotlinType::Named { name, .. },
                ..
            }) => Some((symbol.location.start_byte, name.clone())),
            _ => None,
        })
        .max_by_key(|(start, _)| *start)
        .map(|(_, name)| name);
    if indexed.is_some() {
        return indexed;
    }
    if provider.symbols_named(spelling).into_iter().any(|symbol| {
        symbol.id.file == file
            && symbol.location.start_byte <= call.start_byte()
            && !is_type_symbol(&symbol.id)
    }) {
        // An indexed binding exists but has no established type. Preserve
        // that shadow evidence instead of interpreting an uppercase variable
        // name as a type qualifier.
        return None;
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
                            .and_then(|variable| variable.child_by_field_name("type"))
                    } else {
                        None
                    }
                })
                .and_then(|type_node| type_node.utf8_text(source.as_bytes()).ok());
            let mut descendants = vec![node];
            let mut has_name = false;
            while let Some(child) = descendants.pop() {
                if child.kind() == "identifier"
                    && child.utf8_text(source.as_bytes()).ok() == Some(spelling)
                {
                    has_name = true;
                    break;
                }
                let mut cursor = child.walk();
                descendants.extend(child.named_children(&mut cursor));
            }
            if has_name && let Some(ty) = ty {
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
    candidate.map(|(_, ty)| ty.trim().to_owned()).or_else(|| {
        spelling
            .chars()
            .next()
            .is_some_and(char::is_uppercase)
            .then(|| spelling.to_owned())
    })
}

fn indexed_call_return_type<P: SemanticProvider + ?Sized>(
    source: &str,
    call: tree_sitter::Node<'_>,
    callee: tree_sitter::Node<'_>,
    file: &Path,
    provider: &P,
) -> Option<String> {
    let name = callee.utf8_text(source.as_bytes()).ok()?.trim();
    let FactStatus::Established(id) =
        resolve_required_call(source, file, call, callee, name, provider)
    else {
        return None;
    };
    provider
        .symbols_named(name)
        .into_iter()
        .find(|symbol| symbol.id == id)
        .and_then(|symbol| match &symbol.ty {
            FactStatus::Established(TypeRef {
                ty: KotlinType::Named { name, .. },
                ..
            }) => Some(name.clone()),
            _ => None,
        })
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

fn known_external_member_call<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    callee: tree_sitter::Node<'_>,
    name: &str,
    provider: &P,
) -> bool {
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
            | ("java.lang.StringBuilder", "toString", 0)
            | ("java.lang.StringBuilder", "append", 1)
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
            | ("java.util.stream.Stream", "filter", 1)
            | (
                "java.util.List" | "kotlin.collections.List" | "kotlin.collections.MutableList",
                "last",
                0
            )
            | ("java.util.List", "toList", 0)
            | ("kotlin.Array", "toList", 0)
    )
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
        "UUID" | "java.util.UUID" => ("UUID", "java.util.UUID", false),
        "String" | "java.lang.String" | "kotlin.String" => ("String", "java.lang.String", true),
        "StringBuilder" | "java.lang.StringBuilder" => {
            ("StringBuilder", "java.lang.StringBuilder", true)
        }
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
    let mut matching_import = None;
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
        if imported_simple == simple || external_wildcard_covers(path, canonical) {
            matching_import = Some(path);
        }
    }
    if source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("import "))
        .any(|import| {
            let path = import.split(';').next().unwrap_or(import).trim();
            path.ends_with(".*") && !is_known_jdk_wildcard(path)
        })
    {
        return None;
    }
    match matching_import {
        Some(path) if path == canonical => Some(canonical.to_owned()),
        Some(path) if external_wildcard_covers(path, canonical) => Some(canonical.to_owned()),
        Some(_) => None,
        None if intrinsic => Some(canonical.to_owned()),
        None => None,
    }
}

fn external_wildcard_covers(path: &str, canonical: &str) -> bool {
    match canonical {
        "java.util.UUID" | "java.util.Map" | "java.util.HashMap" | "java.util.List"
        | "java.util.Set" => path == "java.util.*",
        "java.util.stream.Stream" => path == "java.util.stream.*",
        "java.lang.String" | "java.lang.StringBuilder" => path == "java.lang.*",
        _ => false,
    }
}

fn is_known_jdk_wildcard(path: &str) -> bool {
    matches!(path, "java.util.*" | "java.lang.*" | "java.util.stream.*")
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
fn call_argument_count(call: tree_sitter::Node<'_>) -> usize {
    let argument_list = call.child_by_field_name("arguments").or_else(|| {
        call.child_by_field_name("value_arguments").or_else(|| {
            let mut cursor = call.walk();
            call.named_children(&mut cursor).find(|child| {
                matches!(
                    child.kind(),
                    "arguments" | "argument_list" | "value_arguments" | "value_argument_list"
                )
            })
        })
    });
    let mut count = argument_list
        .map(|arguments| {
            let mut cursor = arguments.walk();
            arguments.named_children(&mut cursor).count()
        })
        .unwrap_or(0);
    let mut cursor = call.walk();
    let has_trailing_lambda = call
        .named_children(&mut cursor)
        .any(|child| matches!(child.kind(), "lambda_literal" | "lambda_expression"));
    if has_trailing_lambda
        && argument_list.is_none_or(|arguments| {
            let mut cursor = arguments.walk();
            !arguments
                .named_children(&mut cursor)
                .any(|child| matches!(child.kind(), "lambda_literal" | "lambda_expression"))
        })
    {
        count += 1;
    }
    count
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
            | "StringBuilder"
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
            | "BigDecimal"
            | "BigInteger"
            | "Pair"
            | "Triple"
            | "Regex"
            | "Exception"
            | "IllegalArgumentException"
            | "IllegalStateException"
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
                    let reference_scope = nearest_lexical_scope(node);
                    if candidate_scope == reference_scope {
                        return true;
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
