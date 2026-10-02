//! Retained-Kotlin smart-cast repair — the `N6C94` retention boundary.
//!
//! Translating the OWNER of a property turns a Kotlin property read into a Java
//! getter call, and Kotlin's flow analysis does not carry a smart cast across two
//! such calls. Retained Kotlin that reads the property in the test and again in
//! the narrowed branch therefore stops compiling:
//!
//! ```text
//! if (position.data is AssetData) { use(position.data.asset) }
//! // kotlinc sees two independent getData() calls: no smart cast
//! ```
//!
//! Binding the read once repairs it without changing behaviour — the property was
//! stable enough for Kotlin to smart cast it in the first place:
//!
//! ```text
//! val data = position.data
//! if (data is AssetData) { use(data.asset) }
//! ```
//!
//! This module owns BOTH halves of that boundary: the site shapes that can be
//! repaired (so the retention decision can ask whether translating the owner is
//! safe) and the text edit itself. Both derive from the same [`sites`] walk, so
//! the answer the planner gets and the repair the writer performs cannot disagree
//! — a planner that refuses a shape the writer would have fixed costs retention,
//! but one that accepts a shape the writer refuses breaks the build.
//!
//! Supported shapes, matched on the AST (never on line text):
//!
//! ```text
//! if (x.p is T) { ... x.p ... }        // positive narrowing
//! if (x.p !is T) return ...; x.p ...   // negative narrowing, early exit
//! when (x.p) { is T -> ... x.p ... }   // subject narrowing
//! if (x.p is T && x.p.foo) { ... }     // conjunction: the RHS is dominated
//! ```
//!
//! Refused on purpose (those owners keep their Kotlin retention until each shape
//! is covered): a chain deeper than `receiver.property`, safe calls (`x?.p`),
//! `getP()`-style call receivers, a `!is` branch that falls through, and a
//! conjunction whose test is not the first operand (hoisting it would reorder
//! evaluation relative to the operands before it).

use crate::transpiler::kt;
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SmartCastShape {
    /// `if (x.p is T) { ... }`
    IfIs,
    /// `if (x.p !is T) return ...; ... x.p ...`
    IfNotIs,
    /// `when (x.p) { is T -> ... }`
    WhenSubject,
    /// `if (x.p is T && x.p.foo) { ... }`
    Conjunction,
}

impl SmartCastShape {
    pub fn label(self) -> &'static str {
        match self {
            Self::IfIs => "if (x.p is T)",
            Self::IfNotIs => "if (x.p !is T) early exit",
            Self::WhenSubject => "when (x.p) { is T -> }",
            Self::Conjunction => "x.p is T && x.p...",
        }
    }
}

/// One smart-cast site in one Kotlin source: where the narrowed chain is, which
/// shape the narrowing takes, and whether [`rewrite`] can repair it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SmartCastSite {
    /// The narrowed chain as written, e.g. `position.data`.
    pub chain: String,
    /// The chain's receiver, e.g. `position`. Resolved against the file's
    /// [`bindings`] to find the property OWNER a translation would move to Java.
    /// Empty for a chain whose receiver cannot be named (a deeper chain).
    pub receiver: String,
    /// The narrowed property, e.g. `data`.
    pub property: String,
    pub shape: SmartCastShape,
    /// True when the shape is one [`rewrite`] supports. False means no rewrite
    /// repairs this site, so a dependent owner must stay Kotlin.
    pub repairable: bool,
    /// How many narrowed uses depend on the cast (occurrences beyond the test
    /// itself). Zero means translating the owner cannot break this site.
    pub uses: usize,
    /// Byte ranges to replace with the bound local: the test's own chain plus
    /// every use it dominates. Empty when `!repairable`.
    pub ranges: Vec<(usize, usize)>,
    /// Byte offset of the line the binding is inserted before.
    pub insert_at: usize,
    /// 1-based line of the control-flow statement, for reporting.
    pub line: usize,
}

impl SmartCastSite {
    /// True when translating the property owner would break this site: something
    /// downstream depends on the cast and only a rewrite can keep it alive.
    pub fn needs_repair(&self) -> bool {
        self.uses > 0
    }
}

/// Every smart-cast site in `source`, in source order.
pub fn sites(source: &str) -> Vec<SmartCastSite> {
    sites_in(&crate::transpiler::parse_tree(source), source)
}

/// [`sites`] against an already-parsed tree: the index parses once and must not
/// pay for a second parse just to fill its smart-cast evidence.
pub fn sites_in(tree: &tree_sitter::Tree, source: &str) -> Vec<SmartCastSite> {
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "if_expression" => {
                if let Some(site) = analyse_if(node, source) {
                    found.push(site);
                }
            }
            "when_expression" => {
                if let Some(site) = analyse_when(node, source) {
                    found.push(site);
                }
            }
            _ => {}
        }
        for child in node.named_children(&mut node.walk()) {
            stack.push(child);
        }
    }
    found.sort_by_key(|site| (site.insert_at, site.property.clone()));
    found
}

/// The type names a file's own declarations give its names: parameters, local
/// `val`/`var`, class-body properties and constructor parameters. Only simple
/// names are kept (a generic or qualified type names no owner we can resolve),
/// and a name declared with two different types is dropped rather than guessed.
pub fn bindings(source: &str) -> HashMap<String, String> {
    bindings_in(&crate::transpiler::parse_tree(source), source)
}

/// [`bindings`] against an already-parsed tree.
pub fn bindings_in(tree: &tree_sitter::Tree, source: &str) -> HashMap<String, String> {
    let mut table: HashMap<String, String> = HashMap::new();
    let mut ambiguous: HashSet<String> = HashSet::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        // `parameter`/`class_parameter` carry `name: Type`; a property wraps the
        // name in a `variable_declaration`, and a local val/var with an explicit
        // type arrives as `variable_declaration` directly.
        let declared = match node.kind() {
            "parameter" | "class_parameter" | "variable_declaration" => declared_pair(node, source),
            "property_declaration" => kt::child(node, "variable_declaration")
                .and_then(|variable| declared_pair(variable, source)),
            _ => None,
        };
        if let Some((name, ty)) = declared {
            match table.get(&name) {
                Some(existing) if existing != &ty => {
                    ambiguous.insert(name);
                }
                Some(_) => {}
                None => {
                    table.insert(name, ty);
                }
            }
        }
        for child in node.named_children(&mut node.walk()) {
            stack.push(child);
        }
    }
    for name in ambiguous {
        table.remove(&name);
    }
    table
}

/// `(name, simple type)` for a node that declares one, when the type is a plain
/// simple name. A declaration without an explicit type (`val x = ...`) names no
/// type we can resolve, so it is dropped.
fn declared_pair(node: Node, source: &str) -> Option<(String, String)> {
    let mut name = None;
    let mut ty = None;
    for child in node.named_children(&mut node.walk()) {
        match child.kind() {
            "identifier" if name.is_none() => {
                name = Some(kt::text(child, source).to_string());
            }
            "user_type" | "nullable_type" if ty.is_none() => {
                ty = Some(simple_type(kt::text(child, source)));
            }
            _ => {}
        }
    }
    let name = name?;
    let ty = ty?;
    if ty.is_empty() {
        return None;
    }
    Some((name, ty))
}

/// `pkg.Holder<T>?` -> `Holder`.
pub fn simple_type(text: &str) -> String {
    text.trim()
        .trim_end_matches('?')
        .split(['<', '('])
        .next()
        .unwrap_or("")
        .trim()
        .rsplit('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Rewrite every site that needs repair and whose property owner `owner_ok`
/// accepts (the receiver's declared type and the property name). Returns the new
/// text and how many sites were repaired. A site whose receiver has no resolvable
/// type is left alone: an unproven owner is the retention decision's problem, not
/// a guess.
pub fn rewrite(source: &str, owner_ok: &dyn Fn(&str, &str) -> bool) -> (String, usize) {
    let tree = crate::transpiler::parse_tree(source);
    let found = sites_in(&tree, source);
    if found.is_empty() {
        return (source.to_string(), 0);
    }
    let table = bindings_in(&tree, source);
    let mut taken: HashSet<String> = identifiers_in(&tree, source);
    let mut edits: Vec<Edit> = Vec::new();
    let mut rewritten_ranges: Vec<(usize, usize)> = Vec::new();
    let mut repaired = 0;
    for site in &found {
        if !site.repairable || !site.needs_repair() {
            continue;
        }
        let Some(ty) = table.get(&site.receiver) else {
            continue;
        };
        if !owner_ok(ty, &site.property) {
            continue;
        }
        // A site whose reads an enclosing site already rewrote needs no binding
        // of its own: those reads now name the outer local, and a local `val`
        // smart-casts freely. Checked per range, not per envelope — an enclosing
        // site's FIRST and LAST read can straddle a whole nested site without
        // touching any of its reads (`h.p` before and after a nested `g.q`).
        if !site.ranges.is_empty()
            && site.ranges.iter().all(|(start, end)| {
                rewritten_ranges.iter().any(|(covered_start, covered_end)| {
                    covered_start <= start && end <= covered_end
                })
            })
        {
            continue;
        }
        let local = local_name(&site.property, &site.receiver, &mut taken);
        let line_start = line_start(source, site.insert_at);
        let indent: String = source[line_start..]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        edits.push(Edit {
            start: line_start,
            end: line_start,
            text: format!(
                "{indent}val {local} = {chain}{ending}",
                chain = site.chain,
                ending = line_ending(source)
            ),
        });
        for (start, end) in &site.ranges {
            edits.push(Edit {
                start: *start,
                end: *end,
                text: local.clone(),
            });
        }
        rewritten_ranges.extend(site.ranges.iter().copied());
        repaired += 1;
    }
    if repaired == 0 {
        return (source.to_string(), 0);
    }
    (apply_edits(source, edits), repaired)
}

/// One text replacement in a source file. Shared by the retained-Kotlin repair
/// passes (`crate::smart_cast`, `crate::ctor_defaults`), which both collect
/// edits against byte ranges of the same parse and apply them once.
pub(crate) struct Edit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

pub(crate) fn apply_edits(source: &str, mut edits: Vec<Edit>) -> String {
    // Overlapping edits are refused rather than guessed at: the first (outermost,
    // lowest offset) wins and the later one is dropped.
    edits.sort_by_key(|edit| (edit.start, edit.end));
    let mut out = String::with_capacity(source.len() + edits.len() * 24);
    let mut cursor = 0usize;
    for edit in edits {
        if edit.start < cursor || edit.end < edit.start || edit.end > source.len() {
            continue;
        }
        out.push_str(&source[cursor..edit.start]);
        out.push_str(&edit.text);
        cursor = edit.end;
    }
    out.push_str(&source[cursor..]);
    out
}

/// `property`, else `propertyNotlin1`, else `propertyNotlin2`... — never an
/// existing identifier and never the receiver itself, because a local that
/// shadowed the receiver would silently re-point later uses of that name.
fn local_name(property: &str, receiver: &str, taken: &mut HashSet<String>) -> String {
    if property != receiver && !taken.contains(property) {
        taken.insert(property.to_string());
        return property.to_string();
    }
    for n in 1..64 {
        let candidate = format!("{property}Notlin{n}");
        if !taken.contains(&candidate) {
            taken.insert(candidate.clone());
            return candidate;
        }
    }
    format!("{property}Notlin")
}

fn identifiers_in(tree: &tree_sitter::Tree, source: &str) -> HashSet<String> {
    // Names a new local could collide with or shadow. The LEAF of a navigation
    // is a property read, not a name in scope: `h.payload` putting `payload` in
    // this set would make every binding pick the collision suffix.
    let mut names = HashSet::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "navigation_expression" {
            let children: Vec<Node> = node.named_children(&mut node.walk()).collect();
            let keep = children.len().saturating_sub(1);
            for child in children.iter().take(keep) {
                stack.push(*child);
            }
            continue;
        }
        if node.kind() == "identifier" {
            names.insert(kt::text(node, source).to_string());
        }
        for child in node.named_children(&mut node.walk()) {
            stack.push(child);
        }
    }
    names
}

fn line_start(source: &str, at: usize) -> usize {
    source[..at].rfind('\n').map(|index| index + 1).unwrap_or(0)
}

fn line_ending(source: &str) -> &'static str {
    if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn byte_line(source: &str, at: usize) -> usize {
    source[..at].bytes().filter(|byte| *byte == b'\n').count() + 1
}

// --- shape analysis -------------------------------------------------------

fn analyse_if(node: Node, source: &str) -> Option<SmartCastSite> {
    let condition = kt::field(node, "condition")?;
    let (then_branch, else_branch) = if_branches(node, condition);
    if condition.kind() == "is_expression" {
        let left = kt::field(condition, "left")?;
        let negated = kt::child(condition, "!is").is_some();
        let shape = if negated {
            SmartCastShape::IfNotIs
        } else {
            SmartCastShape::IfIs
        };
        let Some(chain) = chain_of_opt(left, source) else {
            return Some(unrepairable(node, source, left, shape));
        };
        let supported = match (negated, else_branch, then_branch) {
            // Positive test: the then-branch is the narrowed region.
            (false, _, Some(_)) => true,
            // Negative test: the `else` branch, or the statements after an `if`
            // that cannot fall through, are the narrowed region.
            (true, Some(_), _) => true,
            (true, None, Some(then_branch)) => always_exits(then_branch, source),
            _ => false,
        };
        if !supported {
            return Some(unrepairable(node, source, left, shape));
        }
        let mut ranges = vec![range_of(left)];
        if let Some(block) = (negated && else_branch.is_none())
            .then(|| following_block(node))
            .flatten()
        {
            collect_after(block, node, source, &chain, &mut ranges);
        }
        if let Some(branch) = negated.then_some(else_branch).flatten() {
            collect_occurrences(branch, source, &chain, &mut ranges);
        }
        if let Some(branch) = then_branch {
            collect_occurrences(branch, source, &chain, &mut ranges);
        }
        return Some(finish(node, source, chain, shape, ranges, true));
    }
    // Conjunction: `x.p is T && x.p...` — the operands after the test, and the
    // then-branch, are dominated by it.
    if condition.kind() == "binary_expression" {
        let operands = conjunction_operands(condition);
        let test = operands.iter().find(|operand| {
            operand.kind() == "is_expression"
                && kt::child(**operand, "is").is_some()
                && kt::field(**operand, "left")
                    .is_some_and(|left| chain_of_opt(left, source).is_some())
        })?;
        let index = operands
            .iter()
            .position(|operand| operand.id() == test.id())
            .unwrap_or(0);
        let left = kt::field(*test, "left")?;
        let chain = chain_of_opt(left, source)?;
        // Hoisting the binding is only order-preserving when the test comes
        // first: the chain was already evaluated before every other operand.
        let supported = index == 0 && then_branch.is_some();
        let mut ranges = Vec::new();
        if supported {
            ranges.push(range_of(left));
            for operand in &operands[index..] {
                collect_occurrences(*operand, source, &chain, &mut ranges);
            }
            if let Some(branch) = then_branch {
                collect_occurrences(branch, source, &chain, &mut ranges);
            }
        }
        return Some(finish(
            node,
            source,
            chain,
            SmartCastShape::Conjunction,
            ranges,
            supported,
        ));
    }
    None
}

fn analyse_when(node: Node, source: &str) -> Option<SmartCastSite> {
    let subject = kt::child(node, "when_subject")?;
    let subject_expr = subject.named_children(&mut subject.walk()).next()?;
    let narrowed: Vec<Node> = node
        .named_children(&mut node.walk())
        .filter(|child| child.kind() == "when_entry")
        .filter(|entry| {
            kt::field(*entry, "condition").is_some_and(|condition| {
                condition.kind() == "type_test" && kt::child(condition, "is").is_some()
            })
        })
        .collect();
    if narrowed.is_empty() {
        // No `is` entry: nothing is narrowed, so this is not a smart-cast site
        // at all — the owner can translate without touching the `when`.
        return None;
    }
    let Some(chain) = chain_of_opt(subject_expr, source) else {
        return Some(unrepairable(
            node,
            source,
            subject_expr,
            SmartCastShape::WhenSubject,
        ));
    };
    let mut ranges = vec![range_of(subject_expr)];
    for entry in narrowed {
        for child in entry.named_children(&mut entry.walk()) {
            if child.kind() != "type_test" {
                collect_occurrences(child, source, &chain, &mut ranges);
            }
        }
    }
    Some(finish(
        node,
        source,
        chain,
        SmartCastShape::WhenSubject,
        ranges,
        true,
    ))
}

/// A site the analyser recognised but cannot repair: the retention decision must
/// see it (its property still cannot move to Java) while the writer leaves it be.
fn unrepairable(
    node: Node,
    source: &str,
    chain_node: Node,
    shape: SmartCastShape,
) -> SmartCastSite {
    let property = chain_node
        .named_children(&mut chain_node.walk())
        .filter(|child| child.kind() == "identifier")
        .last()
        .map(|leaf| kt::text(leaf, source).to_string())
        .unwrap_or_default();
    SmartCastSite {
        chain: kt::text(chain_node, source).to_string(),
        receiver: String::new(),
        property,
        shape,
        repairable: false,
        uses: 1,
        ranges: Vec::new(),
        insert_at: line_start(source, node.start_byte()),
        line: byte_line(source, node.start_byte()),
    }
}

/// `(receiver.property, receiver, property)` for a plain two-identifier
/// navigation. Deeper chains, safe calls, calls and subscripts are refused: the
/// property's OWNER cannot be resolved from the file's own bindings, and an
/// unproven owner must not be repaired behind the retention decision's back.
fn chain_of_opt(node: Node, source: &str) -> Option<(String, String, String)> {
    if node.kind() != "navigation_expression" {
        return None;
    }
    let parts: Vec<Node> = node.named_children(&mut node.walk()).collect();
    if parts.len() != 2 || parts[0].kind() != "identifier" || parts[1].kind() != "identifier" {
        return None;
    }
    let receiver = kt::text(parts[0], source).to_string();
    let property = kt::text(parts[1], source).to_string();
    let chain = format!("{receiver}.{property}");
    // Reconstructing the chain from its parts would also swallow a safe call
    // (`h?.payload` has the same two identifiers): text equality is what keeps
    // the rewrite from turning a null-safe read into a plain one.
    (kt::text(node, source) == chain).then_some((chain, receiver, property))
}

/// Assemble the site record. `ranges` holds the test's own chain plus every use
/// it dominates, so `uses` is one less than the range count.
fn finish(
    node: Node,
    source: &str,
    chain: (String, String, String),
    shape: SmartCastShape,
    ranges: Vec<(usize, usize)>,
    supported: bool,
) -> SmartCastSite {
    let mut ranges = ranges;
    ranges.sort_unstable();
    ranges.dedup();
    let uses = ranges.len().saturating_sub(1);
    SmartCastSite {
        chain: chain.0,
        receiver: chain.1,
        property: chain.2,
        shape,
        repairable: supported && uses > 0,
        uses,
        ranges: if supported { ranges } else { Vec::new() },
        insert_at: line_start(source, node.start_byte()),
        line: byte_line(source, node.start_byte()),
    }
}

fn range_of(node: Node) -> (usize, usize) {
    (node.start_byte(), node.end_byte())
}

/// `(then, else)` bodies of an `if_expression`: the first named node after the
/// condition, and the first named node after the `else` token.
fn if_branches<'t>(node: Node<'t>, condition: Node<'_>) -> (Option<Node<'t>>, Option<Node<'t>>) {
    let mut then_branch = None;
    let mut else_branch = None;
    let mut seen_condition = false;
    let mut seen_else = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.id() == condition.id() {
            seen_condition = true;
            continue;
        }
        if child.kind() == "else" {
            seen_else = true;
            continue;
        }
        if !child.is_named() {
            continue;
        }
        if seen_else {
            if else_branch.is_none() {
                else_branch = Some(child);
            }
        } else if seen_condition && then_branch.is_none() {
            then_branch = Some(child);
        }
    }
    (then_branch, else_branch)
}

/// The `&&` operands of a conjunction, left to right.
fn conjunction_operands(condition: Node) -> Vec<Node> {
    let mut operands = Vec::new();
    collect_operands(condition, &mut operands);
    operands
}

fn collect_operands<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    let is_conjunction = node.kind() == "binary_expression"
        && kt::field(node, "operator").is_some_and(|operator| operator.kind() == "&&");
    if is_conjunction
        && let Some(left) = kt::field(node, "left")
        && let Some(right) = kt::field(node, "right")
    {
        collect_operands(left, out);
        collect_operands(right, out);
        return;
    }
    out.push(node);
}

/// True when the branch always leaves the enclosing function or loop, so the code
/// after the `if` runs only when the `!is` test did not hold.
fn always_exits(branch: Node, source: &str) -> bool {
    match branch.kind() {
        "return_expression" | "throw_expression" | "break_expression" | "continue_expression" => {
            true
        }
        "block" => branch
            .named_children(&mut branch.walk())
            .last()
            .is_some_and(|last| always_exits(last, source)),
        _ => kt::text(branch, source).trim_start().starts_with("return"),
    }
}

/// The block the `if` lives in, so the statements after it can be walked.
fn following_block(node: Node) -> Option<Node> {
    let parent = node.parent()?;
    (parent.kind() == "block" || parent.kind() == "class_body").then_some(parent)
}

/// Collect the chain's occurrences in the siblings AFTER `node` inside `block` —
/// the region a `!is`-with-early-exit test dominates.
fn collect_after(
    block: Node,
    node: Node,
    source: &str,
    chain: &(String, String, String),
    ranges: &mut Vec<(usize, usize)>,
) {
    let mut seen = false;
    let mut cursor = block.walk();
    for child in block.named_children(&mut cursor) {
        if child.id() == node.id() {
            seen = true;
            continue;
        }
        if seen {
            collect_occurrences(child, source, chain, ranges);
        }
    }
}

/// Every occurrence of the exact chain text inside `node`, refusing to descend
/// past a nested declaration that shadows the receiver — a shadowed name is a
/// different value, and rewriting it would change the program.
fn collect_occurrences(
    node: Node,
    source: &str,
    chain: &(String, String, String),
    out: &mut Vec<(usize, usize)>,
) {
    if declares(node, source, &chain.1) {
        return;
    }
    if node.kind() == "navigation_expression" && kt::text(node, source) == chain.0 {
        out.push((node.start_byte(), node.end_byte()));
        return;
    }
    if node.kind() == "block" || node.kind() == "class_body" {
        // A local that shadows the receiver ends the narrowed region: in Kotlin
        // the shadow makes every later occurrence a different value.
        for child in node.named_children(&mut node.walk()) {
            if declares(child, source, &chain.1) {
                break;
            }
            collect_occurrences(child, source, chain, out);
        }
        return;
    }
    for child in node.named_children(&mut node.walk()) {
        collect_occurrences(child, source, chain, out);
    }
}

/// True when `node` itself declares `name` as a parameter or a local — the point
/// where the narrowed region ends, because every later occurrence is a different
/// value.
fn declares(node: Node, source: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let declared = match node.kind() {
        "property_declaration" | "variable_declaration" => Some(node),
        // A closure or function shadows through ITS parameters, so the whole
        // body is out of the narrowed region once a parameter takes the name.
        "lambda_literal"
        | "annotated_lambda"
        | "anonymous_function"
        | "function_declaration"
        | "class_declaration"
        | "secondary_constructor" => kt::child(node, "lambda_parameters")
            .or_else(|| kt::child(node, "function_value_parameters"))
            .or_else(|| kt::child(node, "class_parameters")),
        "lambda_parameters" | "function_value_parameters" | "class_parameters" => Some(node),
        _ => None,
    };
    let Some(declared) = declared else {
        return false;
    };
    let mut stack = vec![declared];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier" && kt::text(node, source) == name {
            return true;
        }
        for child in node.named_children(&mut node.walk()) {
            stack.push(child);
        }
    }
    false
}
