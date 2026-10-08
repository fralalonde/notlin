//! Validated, lossless, structured Java syntax used at the lowering boundary.
//!
//! This is deliberately a syntax IR rather than a second Java type checker. Every
//! non-terminal remains a typed syntax category, while terminals retain only
//! their lexeme and the whitespace/comments between terminals. No source slices
//! are stored, so rendering is a walk over the tree.

use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaIrError {
    pub message: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

impl std::fmt::Display for JavaIrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at bytes {}..{}",
            self.message, self.start_byte, self.end_byte
        )
    }
}
impl std::error::Error for JavaIrError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaCompilationUnit {
    pub root: JavaSyntaxNode,
}

/// A grammar node classified by its Java role. The original grammar kind is
/// retained for precise downstream inspection (for example `record_declaration`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaCategory {
    CompilationUnit,
    Declaration,
    Type,
    Expression,
    Statement,
    Annotation,
    Import,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaSyntaxNode {
    pub category: JavaCategory,
    pub kind: String,
    pub field_name: Option<String>,
    pub children: Vec<JavaElement>,
    pub trailing_trivia: String,
    /// A resolver may attach the identity of the source declaration that
    /// produced this Java declaration. It is intentionally opaque to syntax.
    pub origin_target: Option<crate::semantics::SymbolId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JavaElement {
    Declaration(JavaSyntaxNode),
    Type(JavaSyntaxNode),
    Expression(JavaSyntaxNode),
    Statement(JavaSyntaxNode),
    Annotation(JavaSyntaxNode),
    Import(JavaSyntaxNode),
    Other(JavaSyntaxNode),
    Token(JavaToken),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JavaToken {
    pub kind: String,
    pub field_name: Option<String>,
    pub text: String,
    pub class: JavaTokenClass,
    pub leading_trivia: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaTokenClass {
    Identifier,
    Literal,
    Keyword,
    OperatorOrPunctuation,
    Comment,
    Other,
}

impl JavaSyntaxNode {
    pub fn new(
        category: JavaCategory,
        kind: impl Into<String>,
        children: Vec<JavaElement>,
    ) -> Self {
        Self {
            category,
            kind: kind.into(),
            field_name: None,
            children,
            trailing_trivia: String::new(),
            origin_target: None,
        }
    }

    pub fn with_origin_target(mut self, target: crate::semantics::SymbolId) -> Self {
        self.origin_target = Some(target);
        self
    }

    /// Attach resolved identities after syntax parsing. The callback sees each
    /// structured node (including call and member-access expressions) and may
    /// return no identity for syntax that does not resolve to a Kotlin symbol.
    pub fn attach_symbols(
        &mut self,
        mut lookup: impl FnMut(&JavaSyntaxNode) -> Option<crate::semantics::SymbolId>,
    ) {
        self.visit_mut(&mut |node| {
            if let Some(target) = lookup(node) {
                node.origin_target = Some(target);
            }
        });
    }

    pub fn visit_mut(&mut self, f: &mut impl FnMut(&mut JavaSyntaxNode)) {
        f(self);
        for child in &mut self.children {
            if let Some(node) = child.node_mut() {
                node.visit_mut(f);
            }
        }
    }
}

impl JavaElement {
    fn node_mut(&mut self) -> Option<&mut JavaSyntaxNode> {
        match self {
            Self::Declaration(n)
            | Self::Type(n)
            | Self::Expression(n)
            | Self::Statement(n)
            | Self::Annotation(n)
            | Self::Import(n)
            | Self::Other(n) => Some(n),
            Self::Token(_) => None,
        }
    }
}

pub fn parse_java(source: &str) -> Result<JavaCompilationUnit, JavaIrError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .map_err(|e| JavaIrError {
            message: format!("could not load Java grammar: {e}"),
            start_byte: 0,
            end_byte: 0,
        })?;
    let tree = parser.parse(source, None).ok_or_else(|| JavaIrError {
        message: "Java parser did not produce a tree".into(),
        start_byte: 0,
        end_byte: source.len(),
    })?;
    let root = tree.root_node();
    if root.has_error() {
        let bad = first_error(root).unwrap_or(root);
        return Err(JavaIrError {
            message: if bad.is_missing() {
                "missing Java syntax"
            } else {
                "invalid Java syntax"
            }
            .into(),
            start_byte: bad.start_byte(),
            end_byte: bad.end_byte(),
        });
    }
    Ok(JavaCompilationUnit {
        root: build_node(root, source),
    })
}

fn first_error(node: Node<'_>) -> Option<Node<'_>> {
    if node.is_error() || node.is_missing() {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error()
            && let Some(found) = first_error(child)
        {
            return Some(found);
        }
    }
    None
}

fn build_node(node: Node<'_>, src: &str) -> JavaSyntaxNode {
    let kind = node.kind().to_owned();
    let node_category = category(&kind);
    let mut children = Vec::new();
    let mut cursor = node.walk();
    let mut byte = node.start_byte();
    for (child_index, child) in node.children(&mut cursor).enumerate() {
        let field_name = node
            .field_name_for_child(child_index as u32)
            .map(str::to_owned);
        let start = child.start_byte();
        let trivia = src.get(byte..start).unwrap_or("").to_owned();
        if child.child_count() == 0 {
            let text = src
                .get(child.start_byte()..child.end_byte())
                .unwrap_or("")
                .to_owned();
            let token = JavaToken {
                kind: child.kind().to_owned(),
                field_name: field_name.clone(),
                class: token_class(child.kind(), child.is_named()),
                text,
                leading_trivia: trivia,
            };
            let child_category = category(child.kind());
            if child_category == JavaCategory::Other {
                children.push(JavaElement::Token(token));
            } else {
                let leaf = JavaSyntaxNode {
                    category: child_category,
                    kind: child.kind().to_owned(),
                    field_name: field_name.clone(),
                    children: vec![JavaElement::Token(token)],
                    trailing_trivia: String::new(),
                    origin_target: None,
                };
                children.push(wrap(child_category, leaf));
            }
        } else {
            let mut nested = build_node(child, src);
            nested.field_name = field_name;
            // Leading trivia belongs to the element, represented by a synthetic
            // token wrapper only where needed; retain it in the node's first child.
            if !trivia.is_empty() {
                nested.children.insert(
                    0,
                    JavaElement::Token(JavaToken {
                        kind: "trivia".into(),
                        field_name: None,
                        text: String::new(),
                        class: JavaTokenClass::Other,
                        leading_trivia: trivia,
                    }),
                );
            }
            children.push(wrap(category(&kind_of(child)), nested));
        }
        byte = child.end_byte();
    }
    JavaSyntaxNode {
        category: node_category,
        kind,
        field_name: None,
        children,
        trailing_trivia: src.get(byte..node.end_byte()).unwrap_or("").to_owned(),
        origin_target: None,
    }
}

fn kind_of(node: Node<'_>) -> String {
    node.kind().to_owned()
}
fn wrap(category: JavaCategory, node: JavaSyntaxNode) -> JavaElement {
    match category {
        JavaCategory::Declaration => JavaElement::Declaration(node),
        JavaCategory::Type => JavaElement::Type(node),
        JavaCategory::Expression => JavaElement::Expression(node),
        JavaCategory::Statement => JavaElement::Statement(node),
        JavaCategory::Annotation => JavaElement::Annotation(node),
        JavaCategory::Import => JavaElement::Import(node),
        _ => JavaElement::Other(node),
    }
}

fn category(kind: &str) -> JavaCategory {
    if kind == "program" {
        return JavaCategory::CompilationUnit;
    }
    if kind == "import_declaration" {
        return JavaCategory::Import;
    }
    if matches!(
        kind,
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "method_declaration"
            | "constructor_declaration"
            | "field_declaration"
            | "constant_declaration"
            | "enum_constant"
            | "compact_constructor_declaration"
            | "module_declaration"
            | "package_declaration"
            | "local_variable_declaration"
            | "formal_parameter"
            | "spread_parameter"
    ) {
        return JavaCategory::Declaration;
    }
    if matches!(
        kind,
        "annotation" | "marker_annotation" | "annotation_argument_list"
    ) {
        return JavaCategory::Annotation;
    }
    if matches!(
        kind,
        "type"
            | "integral_type"
            | "floating_point_type"
            | "boolean_type"
            | "void_type"
            | "generic_type"
            | "array_type"
            | "scoped_type_identifier"
            | "type_identifier"
            | "wildcard"
            | "annotated_type"
            | "inferred_parameters"
    ) || kind.ends_with("_type")
    {
        return JavaCategory::Type;
    }
    if kind.ends_with("_statement")
        || matches!(
            kind,
            "block"
                | "catch_clause"
                | "finally_clause"
                | "switch_block"
                | "switch_block_statement_group"
                | "switch_rule"
        )
    {
        return JavaCategory::Statement;
    }
    if kind.ends_with("_expression")
        || kind.ends_with("_literal")
        || matches!(
            kind,
            "identifier"
                | "this"
                | "super"
                | "object_creation_expression"
                | "lambda_expression"
                | "method_reference"
                | "parenthesized_expression"
                | "array_creation_expression"
                | "array_initializer"
                | "class_literal"
                | "method_invocation"
                | "field_access"
                | "array_access"
                | "assignment_expression"
                | "binary_expression"
                | "ternary_expression"
                | "unary_expression"
                | "update_expression"
                | "cast_expression"
                | "instanceof_expression"
                | "switch_expression"
                | "explicit_constructor_invocation"
                | "class_instance_creation_expression"
                | "expression_list"
        )
    {
        return JavaCategory::Expression;
    }
    JavaCategory::Other
}

fn token_class(kind: &str, named: bool) -> JavaTokenClass {
    if matches!(kind, "line_comment" | "block_comment") {
        JavaTokenClass::Comment
    } else if matches!(
        kind,
        "identifier" | "type_identifier" | "scoped_identifier" | "package_name"
    ) {
        JavaTokenClass::Identifier
    } else if kind.contains("literal")
        || matches!(
            kind,
            "decimal_integer_literal"
                | "hex_integer_literal"
                | "octal_integer_literal"
                | "binary_integer_literal"
                | "decimal_floating_point_literal"
                | "hex_floating_point_literal"
                | "character_literal"
                | "string_literal"
                | "text_block"
        )
    {
        JavaTokenClass::Literal
    } else if !named && kind.chars().all(|c| c.is_alphanumeric() || c == '_') {
        JavaTokenClass::Keyword
    } else if !named {
        JavaTokenClass::OperatorOrPunctuation
    } else {
        JavaTokenClass::Other
    }
}

pub fn render(unit: &JavaCompilationUnit) -> String {
    let mut out = String::new();
    render_node(&unit.root, &mut out);
    out
}

/// Removes unused imports for the Lombok annotations emitted by the Kotlin
/// lowerer. This deliberately handles only a fixed set of exact, non-static
/// imports; all usage checks walk identifier tokens in the structured tree, so
/// comments and string literals cannot keep an import alive accidentally.
/// Imports with comments attached are retained to avoid deleting user trivia.
pub fn remove_unused_lombok_imports(unit: &mut JavaCompilationUnit) -> usize {
    const LOMBOK_TYPES: &[&str] = &[
        "Data",
        "Value",
        "NonNull",
        "AllArgsConstructor",
        "EqualsAndHashCode",
    ];

    let mut used = std::collections::HashSet::new();
    collect_identifiers(&unit.root, false, &mut used);
    let mut removed = 0;
    unit.root.children.retain(|element| {
        let JavaElement::Import(import) = element else {
            return true;
        };
        let Some(name) = import_name(import) else {
            return true;
        };
        let Some(simple_name) = name.strip_prefix("lombok.") else {
            return true;
        };
        if !LOMBOK_TYPES.contains(&simple_name) || used.contains(simple_name) || has_comment(import)
        {
            return true;
        }
        removed += 1;
        false
    });
    removed
}

fn collect_identifiers(
    node: &JavaSyntaxNode,
    in_import: bool,
    identifiers: &mut std::collections::HashSet<String>,
) {
    let in_import = in_import || node.category == JavaCategory::Import;
    for child in &node.children {
        match child {
            JavaElement::Token(token) => {
                if !in_import && token.class == JavaTokenClass::Identifier {
                    identifiers.insert(token.text.clone());
                }
            }
            JavaElement::Declaration(child)
            | JavaElement::Type(child)
            | JavaElement::Expression(child)
            | JavaElement::Statement(child)
            | JavaElement::Annotation(child)
            | JavaElement::Import(child)
            | JavaElement::Other(child) => collect_identifiers(child, in_import, identifiers),
        }
    }
}

fn import_name(node: &JavaSyntaxNode) -> Option<String> {
    let mut tokens = Vec::new();
    collect_import_tokens(node, &mut tokens);
    let mut name = String::new();
    let mut saw_import = false;
    let mut saw_semicolon = false;
    for token in tokens {
        match token.text.as_str() {
            "import" if !saw_import => saw_import = true,
            ";" if saw_import => {
                saw_semicolon = true;
                break;
            }
            "." if saw_import => name.push('.'),
            "static" | "*" if saw_import => return None,
            _ if saw_import && token.class == JavaTokenClass::Identifier => {
                name.push_str(&token.text)
            }
            _ => {}
        }
    }
    (saw_import && saw_semicolon && !name.is_empty()).then_some(name)
}

fn collect_import_tokens<'a>(node: &'a JavaSyntaxNode, tokens: &mut Vec<&'a JavaToken>) {
    for child in &node.children {
        match child {
            JavaElement::Token(token) => tokens.push(token),
            JavaElement::Declaration(child)
            | JavaElement::Type(child)
            | JavaElement::Expression(child)
            | JavaElement::Statement(child)
            | JavaElement::Annotation(child)
            | JavaElement::Import(child)
            | JavaElement::Other(child) => collect_import_tokens(child, tokens),
        }
    }
}

fn has_comment(node: &JavaSyntaxNode) -> bool {
    node.children.iter().any(|child| match child {
        JavaElement::Token(token) => {
            token.class == JavaTokenClass::Comment
                || token.leading_trivia.contains("//")
                || token.leading_trivia.contains("/*")
        }
        JavaElement::Declaration(child)
        | JavaElement::Type(child)
        | JavaElement::Expression(child)
        | JavaElement::Statement(child)
        | JavaElement::Annotation(child)
        | JavaElement::Import(child)
        | JavaElement::Other(child) => has_comment(child),
    })
}

fn render_node(node: &JavaSyntaxNode, out: &mut String) {
    for child in &node.children {
        match child {
            JavaElement::Token(t) => {
                out.push_str(&t.leading_trivia);
                out.push_str(&t.text);
            }
            JavaElement::Declaration(n)
            | JavaElement::Type(n)
            | JavaElement::Expression(n)
            | JavaElement::Statement(n)
            | JavaElement::Annotation(n)
            | JavaElement::Import(n)
            | JavaElement::Other(n) => render_node(n, out),
        }
    }
    out.push_str(&node.trailing_trivia);
}

/// Constructs a string literal token with Java's required escaping.
pub fn string_literal(value: &str) -> JavaElement {
    let mut text = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' => text.push_str("\\\\"),
            '"' => text.push_str("\\\""),
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            '\t' => text.push_str("\\t"),
            '\u{08}' => text.push_str("\\b"),
            '\u{0c}' => text.push_str("\\f"),
            c if c.is_control() && (c as u32) <= 0xff => {
                text.push_str(&format!("\\{:03o}", c as u32))
            }
            c if c.is_control() => text.push_str(&format!("\\u{:04x}", c as u32)),
            c => text.push(c),
        }
    }
    text.push('"');
    JavaElement::Token(JavaToken {
        kind: "string_literal".into(),
        field_name: None,
        text,
        class: JavaTokenClass::Literal,
        leading_trivia: String::new(),
    })
}

pub fn identifier(name: impl Into<String>) -> JavaElement {
    JavaElement::Token(JavaToken {
        kind: "identifier".into(),
        field_name: None,
        text: name.into(),
        class: JavaTokenClass::Identifier,
        leading_trivia: String::new(),
    })
}

pub fn operator(text: impl Into<String>) -> JavaElement {
    JavaElement::Token(JavaToken {
        kind: "operator".into(),
        field_name: None,
        text: text.into(),
        class: JavaTokenClass::OperatorOrPunctuation,
        leading_trivia: String::new(),
    })
}

pub fn keyword(text: impl Into<String>) -> JavaElement {
    JavaElement::Token(JavaToken {
        kind: "keyword".into(),
        field_name: None,
        text: text.into(),
        class: JavaTokenClass::Keyword,
        leading_trivia: String::new(),
    })
}

/// Build an expression tree with grouping inserted when a child has lower or
/// equal precedence. Parse-native expressions already retain explicit groups;
/// this helper preserves the requested tree shape for generated nodes.
pub fn binary_expression(op: &str, left: JavaElement, right: JavaElement) -> JavaElement {
    let assignment = precedence(op) == 2;
    let left = grouped(left, precedence(op), true);
    let right = with_leading(grouped(right, precedence(op), !assignment), " ");
    JavaElement::Expression(JavaSyntaxNode::new(
        JavaCategory::Expression,
        "binary_expression",
        vec![left, operator_with_trivia(op, " "), right],
    ))
}

pub fn unary_expression(op: &str, expression: JavaElement) -> JavaElement {
    let expression = grouped(expression, 14, true);
    JavaElement::Expression(JavaSyntaxNode::new(
        JavaCategory::Expression,
        "unary_expression",
        vec![operator(op), expression],
    ))
}

pub fn member_access(receiver: JavaElement, member: impl Into<String>) -> JavaElement {
    let receiver = grouped(receiver, 16, false);
    JavaElement::Expression(JavaSyntaxNode::new(
        JavaCategory::Expression,
        "field_access",
        vec![receiver, operator("."), identifier(member)],
    ))
}

pub fn call_expression(callee: JavaElement, arguments: Vec<JavaElement>) -> JavaElement {
    let callee = grouped(callee, 16, false);
    let mut args = vec![operator("(")];
    for (index, argument) in arguments.into_iter().enumerate() {
        if index > 0 {
            args.push(operator(","));
        }
        args.push(if index > 0 {
            with_leading(argument, " ")
        } else {
            argument
        });
    }
    args.push(operator(")"));
    let arguments = JavaSyntaxNode::new(JavaCategory::Other, "argument_list", args);
    JavaElement::Expression(JavaSyntaxNode::new(
        JavaCategory::Expression,
        "method_invocation",
        vec![callee, JavaElement::Other(arguments)],
    ))
}

fn grouped(
    expression: JavaElement,
    parent_precedence: u8,
    equal_needs_grouping: bool,
) -> JavaElement {
    let child_precedence = element_precedence(&expression);
    if child_precedence < parent_precedence
        || (equal_needs_grouping && child_precedence == parent_precedence)
    {
        let node = JavaSyntaxNode::new(
            JavaCategory::Expression,
            "parenthesized_expression",
            vec![operator("("), expression, operator(")")],
        );
        JavaElement::Expression(node)
    } else {
        expression
    }
}

fn with_leading(mut element: JavaElement, trivia: &str) -> JavaElement {
    match &mut element {
        JavaElement::Token(token) => token.leading_trivia.insert_str(0, trivia),
        JavaElement::Declaration(node)
        | JavaElement::Type(node)
        | JavaElement::Expression(node)
        | JavaElement::Statement(node)
        | JavaElement::Annotation(node)
        | JavaElement::Import(node)
        | JavaElement::Other(node) => node.children.insert(
            0,
            JavaElement::Token(JavaToken {
                kind: "trivia".into(),
                field_name: None,
                text: String::new(),
                class: JavaTokenClass::Other,
                leading_trivia: trivia.into(),
            }),
        ),
    }
    element
}

fn element_precedence(element: &JavaElement) -> u8 {
    match element {
        JavaElement::Expression(node) => match node.kind.as_str() {
            "lambda_expression" => 1,
            "assignment_expression" => 2,
            "ternary_expression" | "conditional_expression" => 3,
            "binary_expression" => {
                let op = node.children.iter().find_map(|child| match child {
                    JavaElement::Token(t) if t.class == JavaTokenClass::OperatorOrPunctuation => {
                        Some(t.text.as_str())
                    }
                    _ => None,
                });
                op.map(precedence).unwrap_or(15)
            }
            "unary_expression" | "cast_expression" => 14,
            "method_invocation" | "field_access" | "array_access" | "update_expression" => 16,
            _ => 18,
        },
        _ => 18,
    }
}

fn precedence(op: &str) -> u8 {
    match op {
        "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|=" | "^=" | "<<=" | ">>=" | ">>>=" => 2,
        "||" => 4,
        "&&" => 5,
        "|" => 6,
        "^" => 7,
        "&" => 8,
        "==" | "!=" => 9,
        "<" | ">" | "<=" | ">=" | "instanceof" => 10,
        "<<" | ">>" | ">>>" => 11,
        "+" | "-" => 12,
        "*" | "/" | "%" => 13,
        _ => 15,
    }
}

fn operator_with_trivia(text: &str, trivia: &str) -> JavaElement {
    JavaElement::Token(JavaToken {
        kind: "operator".into(),
        field_name: None,
        text: text.into(),
        class: JavaTokenClass::OperatorOrPunctuation,
        leading_trivia: trivia.into(),
    })
}
