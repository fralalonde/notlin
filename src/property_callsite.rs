//! Rewrite retained Kotlin property syntax when a workspace contract has been
//! repaired to explicit JavaBean methods or translated to Java.
//!
//! The pass is deliberately conservative. It handles `name.property` for a
//! simple, explicitly typed receiver whose type is proven to inherit the
//! listed contract, plus bare reads in executable bodies of retained Kotlin
//! declarations that inherit the contract. Parameters and locals that shadow a bare
//! property, safe calls, callables, compound writes, unknown receiver types,
//! and ambiguous contracts are left as-is. Passing a [`SourceIndex`] built
//! with speculative overlays works the same as passing a disk-backed index.

use crate::smart_cast;
use crate::transpiler::kt;
use crate::workspace::{Declaration, SourceFile, SourceIndex, SourceLanguage};
use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tree_sitter::Node;

/// One Kotlin property whose call sites must use its JavaBean ABI.
///
/// `owner_type` should be fully qualified when possible. The getter is the JVM
/// name of the translated/repaired method (`isEnabled` or `getName`). A setter
/// is supplied only when the contract has a writable JavaBean method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyAccessorContract {
    pub owner_type: String,
    pub property: String,
    pub getter: String,
    pub setter: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IncrementalRepairStats {
    pub candidate_files: usize,
    pub rewritten_accesses: usize,
}

/// State shared by the speculative workspace rounds.
///
/// Call-site rewrites are monotone within one migration: once a property read
/// becomes an accessor call, later rounds retain that call. The cache records
/// the exact output last seen for each retained Kotlin path. An intermediate
/// round therefore needs to revisit only changed source text, or files that
/// can mention a newly changed accessor contract. The caller requests a full
/// relevant-file sweep before accepting convergence, which catches resolution
/// changes caused by declarations in other overlay files.
#[derive(Debug, Default)]
pub struct IncrementalRepairState {
    output_hashes: HashMap<PathBuf, blake3::Hash>,
    contracts: Vec<PropertyAccessorContract>,
}

impl IncrementalRepairState {
    pub fn requires_index(
        &self,
        sources: &[(PathBuf, String)],
        contracts: &[PropertyAccessorContract],
        full_sweep: bool,
    ) -> bool {
        let current_contracts = canonical_contracts(contracts);
        self.candidate_flags(sources, &current_contracts, full_sweep)
            .into_iter()
            .any(|candidate| candidate)
    }

    pub fn rewrite_files(
        &mut self,
        index: &SourceIndex,
        sources: &mut [(PathBuf, String)],
        contracts: &[PropertyAccessorContract],
        full_sweep: bool,
    ) -> IncrementalRepairStats {
        let current_contracts = canonical_contracts(contracts);
        let candidates = self.candidate_flags(sources, &current_contracts, full_sweep);
        let candidate_files = candidates.iter().filter(|candidate| **candidate).count();
        let rewritten_accesses = if current_contracts.is_empty() || candidate_files == 0 {
            0
        } else {
            crate::transpiler::fixpoint::install_parallel(|| {
                sources
                    .par_iter_mut()
                    .zip(&candidates)
                    .filter_map(|((path, source), candidate)| {
                        if !candidate {
                            return None;
                        }
                        let (rewritten, count) =
                            rewrite_file(index, path, source, &current_contracts);
                        *source = rewritten;
                        Some(count)
                    })
                    .sum()
            })
        };
        self.output_hashes.clear();
        self.output_hashes.extend(
            sources
                .iter()
                .map(|(path, source)| (path.clone(), blake3::hash(source.as_bytes()))),
        );
        self.contracts = current_contracts;
        IncrementalRepairStats {
            candidate_files,
            rewritten_accesses,
        }
    }

    fn candidate_flags(
        &self,
        sources: &[(PathBuf, String)],
        current_contracts: &[PropertyAccessorContract],
        full_sweep: bool,
    ) -> Vec<bool> {
        let properties = current_contracts
            .iter()
            .map(|contract| contract.property.as_str())
            .collect::<HashSet<_>>();
        let changed_properties = current_contracts
            .iter()
            .filter(|contract| !self.contracts.contains(contract))
            .map(|contract| contract.property.as_str())
            .collect::<HashSet<_>>();
        sources
            .iter()
            .map(|(path, source)| {
                let source_changed =
                    self.output_hashes.get(path) != Some(&blake3::hash(source.as_bytes()));
                ((full_sweep || source_changed) && contains_any_identifier(source, &properties))
                    || contains_any_identifier(source, &changed_properties)
            })
            .collect()
    }
}

fn canonical_contracts(contracts: &[PropertyAccessorContract]) -> Vec<PropertyAccessorContract> {
    let mut contracts = contracts.to_vec();
    contracts.sort_by(|left, right| {
        (&left.owner_type, &left.property, &left.getter, &left.setter).cmp(&(
            &right.owner_type,
            &right.property,
            &right.getter,
            &right.setter,
        ))
    });
    contracts.dedup();
    contracts
}

fn contains_any_identifier(source: &str, names: &HashSet<&str>) -> bool {
    if names.is_empty() {
        return false;
    }

    // Normal Kotlin identifiers can be found in one pass over the source,
    // including in comments, strings, and backticks. Those contexts are
    // intentionally conservative: the former substring matcher considered
    // them candidates too. Keep the substring-based check for unusual names
    // so punctuation and other spellings retain the old boundary behavior.
    let mut identifiers = HashSet::<&str>::new();
    let mut unusual = Vec::new();
    for &name in names {
        if !name.is_empty() && name.chars().all(is_kotlin_identifier_continue) {
            identifiers.insert(name);
        } else {
            unusual.push(name);
        }
    }
    let mut token_start = None;
    for (end, character) in source.char_indices() {
        if is_kotlin_identifier_continue(character) {
            token_start.get_or_insert(end);
        } else if let Some(start) = token_start.take()
            && identifiers.contains(&source[start..end])
        {
            return true;
        }
    }
    if let Some(start) = token_start
        && identifiers.contains(&source[start..])
    {
        return true;
    }
    unusual
        .into_iter()
        .any(|name| contains_identifier(source, name))
}

fn contains_identifier(source: &str, name: &str) -> bool {
    source.match_indices(name).any(|(start, _)| {
        let before = source[..start].chars().next_back();
        let end = start + name.len();
        let after = source[end..].chars().next();
        before.is_none_or(|ch| !is_kotlin_identifier_continue(ch))
            && after.is_none_or(|ch| !is_kotlin_identifier_continue(ch))
    })
}

/// Rewrite typed property reads and safe, plain assignments in one retained
/// Kotlin source file, plus unqualified reads in the owning interface's executable
/// bodies. Returns `(updated_source, rewritten_access_count)`.
///
/// Binding types come from the workspace index's source file and only simple
/// unique bindings are used. Receiver expressions must be a single identifier;
/// this avoids changing evaluation order or guessing at extension/side-effect
/// receiver shapes. A write is converted only for the exact plain `=` form and
/// when its contract has a setter.
pub fn rewrite_file(
    index: &SourceIndex,
    path: &Path,
    source: &str,
    contracts: &[PropertyAccessorContract],
) -> (String, usize) {
    let Some(source_file) = index.source_file(path) else {
        return (source.to_string(), 0);
    };
    if contracts.is_empty() {
        return (source.to_string(), 0);
    }

    let tree = crate::transpiler::parse_tree(source);
    // The caller text can itself come from a speculative source overlay (or a
    // just-applied preceding repair), so derive its local types from the exact
    // text being rewritten instead of relying on a possibly older cached
    // caller record.
    let bindings = smart_cast::bindings_in(&tree, source);
    let mut contracts_by_property = HashMap::<&str, Vec<&PropertyAccessorContract>>::new();
    for contract in contracts {
        contracts_by_property
            .entry(&contract.property)
            .or_default()
            .push(contract);
    }
    let context = RewriteContext {
        index,
        source_file,
        source,
        bindings: &bindings,
        contracts_by_property,
        inheritance_cache: RefCell::new(HashMap::new()),
    };
    let mut edits = Vec::new();
    let mut rewritten = 0;
    let mut stack = vec![tree.root_node()];

    // Handle assignments first, so a property lvalue is not also rewritten as
    // a getter call. Nested assignments in an RHS remain eligible.
    while let Some(node) = stack.pop() {
        for child in node.named_children(&mut node.walk()) {
            stack.push(child);
        }
        if node.kind() != "assignment" {
            continue;
        }
        let Some(left) = node.child_by_field_name("left") else {
            continue;
        };
        let Some(right) = node.child_by_field_name("right") else {
            continue;
        };
        let Some((receiver, property)) = property_access(left, source) else {
            continue;
        };
        let contract = if receiver == "super" {
            contract_for_super(index, source_file, source, node, &property, contracts)
        } else {
            contract_for(&context, node, &receiver, &property)
        };
        let Some(contract) = contract else {
            continue;
        };
        let Some(setter) = contract.setter.as_deref() else {
            continue;
        };
        let between = &source[left.end_byte()..right.start_byte()];
        let Some(equal_at) = between.rfind('=') else {
            continue;
        };
        if between[..equal_at]
            .chars()
            .any(|ch| matches!(ch, '+' | '-' | '*' | '/' | '%' | '&' | '|' | '^' | '?'))
            || between[equal_at + 1..].contains('=')
        {
            continue;
        }
        if !between[..equal_at].trim().is_empty() || !between[equal_at + 1..].trim().is_empty() {
            continue;
        }
        if between.contains("//") || between.contains("/*") {
            continue;
        }
        edits.push(Edit {
            start: left.start_byte(),
            end: right.start_byte(),
            replacement: format!("{receiver}.{setter}("),
        });
        edits.push(Edit {
            start: right.end_byte(),
            end: right.end_byte(),
            replacement: ")".to_string(),
        });
        rewritten += 1;
    }

    // Repaired Kotlin properties no longer provide property syntax to default
    // members on their owner or retained descendants. Those references are
    // unqualified, so the receiver-type pass above cannot resolve them.
    rewritten +=
        rewrite_implicit_this_reads(index, source_file, source, &tree, contracts, &mut edits);

    // Read edits replace only the member token, preserving whitespace and
    // comments around the receiver and dot. Reads inside a converted lvalue are
    // skipped by checking whether they are the direct left side of assignment.
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        for child in node.named_children(&mut node.walk()) {
            stack.push(child);
        }
        if node.kind() != "navigation_expression" || is_assignment_lvalue(node) {
            continue;
        }
        let Some((receiver_node, property, member)) = property_access_nodes(node, source) else {
            continue;
        };
        // A navigation followed by call arguments is a function call, not a
        // property read. Do not turn `receiver.property()` into `getProperty()()`.
        if node
            .parent()
            .is_some_and(|parent| parent.kind() == "call_expression")
        {
            continue;
        }
        let contract = if receiver_node.kind() == "super_expression" {
            contract_for_super(index, source_file, source, node, &property, contracts)
        } else {
            contract_for_read_receiver(&context, node, receiver_node, &property)
        };
        let Some(contract) = contract else {
            if !edits
                .iter()
                .any(|edit| edit.start < node.end_byte() && edit.end > node.start_byte())
                && receiver_property_fallback_is_safe(&context, node, receiver_node, &property)
                && let Some(operator) = safe_navigation_operator(source, receiver_node, member)
            {
                let receiver = kt::text(receiver_node, source);
                edits.push(Edit {
                    start: node.start_byte(),
                    end: node.end_byte(),
                    replacement: format!("{receiver}{operator}let {{ it.{property} }}"),
                });
                rewritten += 1;
            }
            continue;
        };
        edits.push(Edit {
            start: member.start_byte(),
            end: member.end_byte(),
            replacement: format!("{}()", contract.getter),
        });
        rewritten += 1;
    }

    if rewritten == 0 {
        return (source.to_string(), 0);
    }
    edits.sort_by_key(|edit| (edit.start, edit.end));
    let mut output = String::with_capacity(source.len() + edits.len() * 12);
    let mut cursor = 0;
    for edit in edits {
        if edit.start < cursor || edit.end > source.len() || edit.end < edit.start {
            continue;
        }
        output.push_str(&source[cursor..edit.start]);
        output.push_str(&edit.replacement);
        cursor = edit.end;
    }
    output.push_str(&source[cursor..]);
    (output, rewritten)
}

struct Edit {
    start: usize,
    end: usize,
    replacement: String,
}

fn property_access(node: Node, source: &str) -> Option<(String, String)> {
    if node.kind() != "navigation_expression" {
        return None;
    }
    let parts: Vec<Node> = node.named_children(&mut node.walk()).collect();
    if parts.len() != 2
        || !matches!(parts[0].kind(), "identifier" | "super_expression")
        || parts[1].kind() != "identifier"
    {
        return None;
    }
    let receiver = kt::text(parts[0], source).to_string();
    let property = kt::text(parts[1], source).to_string();
    (kt::text(node, source) == format!("{receiver}.{property}")).then_some((receiver, property))
}

fn property_access_nodes<'tree>(
    node: Node<'tree>,
    source: &str,
) -> Option<(Node<'tree>, String, Node<'tree>)> {
    if node.kind() != "navigation_expression" {
        return None;
    }
    let parts: Vec<Node> = node.named_children(&mut node.walk()).collect();
    if parts.len() != 2 || parts[1].kind() != "identifier" {
        return None;
    }
    let between = &source[parts[0].end_byte()..parts[1].start_byte()];
    if !matches!(between.trim(), "." | "?.") {
        return None;
    }
    Some((parts[0], kt::text(parts[1], source).to_string(), parts[1]))
}

fn contract_for_super<'a>(
    index: &SourceIndex,
    source_file: &SourceFile,
    source: &str,
    node: Node,
    property: &str,
    contracts: &'a [PropertyAccessorContract],
) -> Option<&'a PropertyAccessorContract> {
    let mut enclosing = node.parent();
    let declaration_node = loop {
        let candidate = enclosing?;
        if matches!(
            candidate.kind(),
            "class_declaration" | "enum_class_declaration" | "object_declaration"
        ) {
            break candidate;
        }
        enclosing = candidate.parent();
    };
    let name = declaration_node.child_by_field_name("name")?;
    let declaration = source_file
        .declarations
        .iter()
        .find(|declaration| declaration.name == kt::text(name, source))?;
    let direct_parents = declaration
        .supertypes
        .iter()
        .filter_map(|supertype| index.resolve_type(source_file, supertype))
        .collect::<Vec<_>>();
    let matching = contracts
        .iter()
        .filter(|contract| contract.property == property)
        .filter(|contract| {
            hierarchy_declares_method(index, source_file, &direct_parents, &contract.getter)
        })
        .collect::<Vec<_>>();
    let first = *matching.first()?;
    matching
        .iter()
        .all(|candidate| candidate.getter == first.getter && candidate.setter == first.setter)
        .then_some(first)
}

fn hierarchy_declares_method(
    index: &SourceIndex,
    fallback_file: &SourceFile,
    roots: &[&Declaration],
    method_name: &str,
) -> bool {
    let mut stack = roots.to_vec();
    let mut visited = HashSet::new();
    while let Some(declaration) = stack.pop() {
        if !visited.insert(declaration as *const Declaration as usize) {
            continue;
        }
        if declaration.members.iter().any(|member| {
            member.kind == crate::workspace::MemberKind::Method
                && member.name == method_name
                && member.parameter_types.is_empty()
        }) {
            return true;
        }
        let declaring_file = index
            .declaration_source_file(declaration)
            .unwrap_or(fallback_file);
        stack.extend(
            declaration
                .supertypes
                .iter()
                .filter_map(|supertype| index.resolve_type(declaring_file, supertype)),
        );
    }
    false
}

fn is_assignment_lvalue(node: Node) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "assignment"
            && parent
                .child_by_field_name("left")
                .is_some_and(|left| left.id() == node.id())
    })
}

fn rewrite_implicit_this_reads(
    index: &SourceIndex,
    source_file: &SourceFile,
    source: &str,
    tree: &tree_sitter::Tree,
    contracts: &[PropertyAccessorContract],
    edits: &mut Vec<Edit>,
) -> usize {
    let mut declaration_nodes = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "class_declaration" | "enum_class_declaration" | "object_declaration"
        ) && let Some(name) = node.child_by_field_name("name")
        {
            let name = kt::text(name, source);
            let mut declarations = source_file
                .declarations
                .iter()
                .filter(|declaration| declaration.name == name);
            if let Some(declaration) = declarations.next()
                && declarations.next().is_none()
            {
                declaration_nodes.push((node, declaration));
            }
        }
        for child in node.named_children(&mut node.walk()) {
            stack.push(child);
        }
    }

    let mut changed = 0;
    for (declaration_node, declaration) in declaration_nodes {
        let declaration_contracts = contracts
            .iter()
            .filter(|contract| {
                index
                    .resolve_type(source_file, &contract.owner_type)
                    .is_some_and(|owner| inherits_exactly(index, source_file, declaration, owner))
            })
            .collect::<Vec<_>>();
        if declaration_contracts.is_empty() {
            continue;
        }
        let mut bodies = Vec::new();
        collect_interface_bodies(declaration_node, source, &mut bodies);
        for (body, scope, method_name) in bodies {
            for contract in &declaration_contracts {
                // Conflicting plans for one property do not identify a safe
                // accessor. Identical duplicate plans are already deduplicated.
                if declaration_contracts.iter().any(|other| {
                    other.property == contract.property
                        && (other.getter != contract.getter || other.setter != contract.setter)
                }) {
                    continue;
                }
                // If a parameter, local, or nested local declaration shadows
                // this property anywhere in the method, fail closed for it.
                if method_name.as_deref() == Some(contract.property.as_str())
                    || method_name.as_deref() == Some(contract.getter.as_str())
                    || contract
                        .setter
                        .as_deref()
                        .is_some_and(|setter| method_name.as_deref() == Some(setter))
                    || scope_declares(scope, source, &contract.property)
                {
                    continue;
                }
                let mut body_stack = vec![body];
                while let Some(node) = body_stack.pop() {
                    if matches!(node.kind(), "class_declaration" | "object_declaration") {
                        continue;
                    }
                    if node.kind() == "identifier"
                        && kt::text(node, source) == contract.property
                        && is_bare_expression_identifier(node)
                    {
                        edits.push(Edit {
                            start: node.start_byte(),
                            end: node.end_byte(),
                            replacement: format!("{}()", contract.getter),
                        });
                        changed += 1;
                        continue;
                    }
                    for child in node.named_children(&mut node.walk()) {
                        body_stack.push(child);
                    }
                }
                changed += rewrite_short_string_templates(body, source, contract, edits);
            }
        }
    }
    changed
}

fn rewrite_short_string_templates(
    body: Node,
    source: &str,
    contract: &PropertyAccessorContract,
    edits: &mut Vec<Edit>,
) -> usize {
    let mut changed = 0;
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node.kind() == "string_literal" {
            let content = &source[node.start_byte()..node.end_byte()];
            let needle = format!("${}", contract.property);
            let mut offset = 0;
            while let Some(relative) = content[offset..].find(&needle) {
                let start = offset + relative;
                let end = start + needle.len();
                let following_is_identifier = content[end..]
                    .chars()
                    .next()
                    .is_some_and(is_kotlin_identifier_continue);
                let is_braced = content[end..].starts_with('{');
                let escaping_slashes = content[..start]
                    .chars()
                    .rev()
                    .take_while(|ch| *ch == '\\')
                    .count();
                if !following_is_identifier && !is_braced && escaping_slashes % 2 == 0 {
                    let absolute_start = node.start_byte() + start;
                    edits.push(Edit {
                        start: absolute_start,
                        end: absolute_start + needle.len(),
                        replacement: format!("${{{}()}}", contract.getter),
                    });
                    changed += 1;
                }
                offset = end;
            }
            continue;
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    changed
}

fn is_kotlin_identifier_continue(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn collect_interface_bodies<'tree>(
    interface: Node<'tree>,
    source: &str,
    out: &mut Vec<(Node<'tree>, Node<'tree>, Option<String>)>,
) {
    let mut stack = vec![interface];
    while let Some(node) = stack.pop() {
        if node.id() != interface.id()
            && matches!(node.kind(), "class_declaration" | "object_declaration")
        {
            continue;
        }
        if node.kind() == "function_declaration" {
            if let Some(body) = kt::child(node, "function_body") {
                out.push((
                    body,
                    node,
                    node.child_by_field_name("name")
                        .map(|name| kt::text(name, source).to_string()),
                ));
            }
            continue;
        }
        if node.kind() == "getter" {
            out.push((node, node, None));
            continue;
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
}

fn scope_declares(scope: Node, source: &str, name: &str) -> bool {
    let mut stack = vec![scope];
    while let Some(node) = stack.pop() {
        if node.id() != scope.id()
            && matches!(node.kind(), "class_declaration" | "object_declaration")
        {
            continue;
        }
        let declared = match node.kind() {
            "variable_declaration" | "parameter" | "class_parameter" => node
                .named_children(&mut node.walk())
                .find(|child| child.kind() == "identifier"),
            "function_declaration" if node.id() != scope.id() => node.child_by_field_name("name"),
            _ => None,
        };
        if declared.is_some_and(|declared| kt::text(declared, source) == name) {
            return true;
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    false
}

fn is_bare_expression_identifier(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    // Exclude declarations, qualified members, type names, and callable
    // names. A bare property may be the receiver of a chain (`payload.size`),
    // but must be the first named child of that navigation expression.
    !matches!(
        parent.kind(),
        "variable_declaration"
            | "parameter"
            | "class_parameter"
            | "function_declaration"
            | "user_type"
            | "nullable_type"
            | "type_arguments"
            | "type_parameters"
            | "annotation"
            | "call_expression"
    ) && !(parent.kind() == "navigation_expression"
        && parent
            .named_children(&mut parent.walk())
            .next()
            .is_none_or(|receiver| receiver.id() != node.id()))
        && !(parent.kind() == "assignment"
            && parent
                .child_by_field_name("left")
                .is_some_and(|left| left.id() == node.id()))
        && !(parent.kind() == "value_argument"
            && parent
                .named_children(&mut parent.walk())
                .last()
                .is_some_and(|value| value.id() != node.id()))
}

struct RewriteContext<'a> {
    index: &'a SourceIndex,
    source_file: &'a SourceFile,
    source: &'a str,
    bindings: &'a HashMap<String, String>,
    contracts_by_property: HashMap<&'a str, Vec<&'a PropertyAccessorContract>>,
    inheritance_cache: RefCell<HashMap<(usize, usize), bool>>,
}

impl<'a> RewriteContext<'a> {
    fn inherits_exactly(&self, receiver: &Declaration, owner: &Declaration) -> bool {
        let key = (
            receiver as *const Declaration as usize,
            owner as *const Declaration as usize,
        );
        if let Some(result) = self.inheritance_cache.borrow().get(&key) {
            return *result;
        }
        let result = inherits_exactly(self.index, self.source_file, receiver, owner);
        self.inheritance_cache.borrow_mut().insert(key, result);
        result
    }

    fn contracts_for(
        &self,
        property: &str,
    ) -> impl Iterator<Item = &'a PropertyAccessorContract> + '_ {
        self.contracts_by_property
            .get(property)
            .into_iter()
            .flatten()
            .copied()
    }
}

fn contract_for<'a>(
    context: &RewriteContext<'a>,
    node: Node,
    receiver: &str,
    property: &str,
) -> Option<&'a PropertyAccessorContract> {
    let receiver_type = receiver_type_at(context, node, receiver)?;
    let receiver_decl = context
        .index
        .resolve_type(context.source_file, &receiver_type)?;
    let matching = context
        .contracts_for(property)
        .filter(|contract| {
            let Some(owner) = context
                .index
                .resolve_type(context.source_file, &contract.owner_type)
            else {
                return false;
            };
            context.inherits_exactly(receiver_decl, owner)
        })
        .collect::<Vec<_>>();
    let first = *matching.first()?;
    matching
        .iter()
        .all(|candidate| candidate.getter == first.getter && candidate.setter == first.setter)
        .then_some(first)
}

fn contract_for_read_receiver<'a>(
    context: &RewriteContext<'a>,
    node: Node,
    receiver: Node,
    property: &str,
) -> Option<&'a PropertyAccessorContract> {
    if receiver.kind() == "identifier" {
        return contract_for(context, node, kt::text(receiver, context.source), property);
    }
    let receiver_type = expression_type(context, node, receiver)?;
    let receiver_decl = context
        .index
        .resolve_type(context.source_file, &receiver_type)?;
    let matching = context
        .contracts_for(property)
        .filter(|contract| {
            context
                .index
                .resolve_type(context.source_file, &contract.owner_type)
                .is_some_and(|owner| context.inherits_exactly(receiver_decl, owner))
        })
        .collect::<Vec<_>>();
    let first = *matching.first()?;
    matching
        .iter()
        .all(|candidate| candidate.getter == first.getter && candidate.setter == first.setter)
        .then_some(first)
}

fn receiver_property_fallback_is_safe(
    context: &RewriteContext<'_>,
    node: Node,
    receiver: Node,
    property: &str,
) -> bool {
    let Some(receiver_type) = expression_type(context, node, receiver) else {
        return false;
    };
    let Some(receiver_decl) = context
        .index
        .resolve_type(context.source_file, &receiver_type)
    else {
        return false;
    };
    if receiver_decl.language != SourceLanguage::Kotlin
        || member_property_type(context.index, context.source_file, receiver_decl, property)
            .is_none()
    {
        return false;
    }
    if context.contracts_for(property).any(|contract| {
        context
            .index
            .resolve_type(context.source_file, &contract.owner_type)
            .is_some_and(|owner| context.inherits_exactly(receiver_decl, owner))
    }) {
        return false;
    }

    let mut current = Some(node);
    let function = loop {
        let Some(candidate) = current else {
            return false;
        };
        if candidate.kind() == "function_declaration" {
            break candidate;
        }
        current = candidate.parent();
    };
    let Some(name) = function.child_by_field_name("name") else {
        return false;
    };
    let name = kt::text(name, context.source);
    if !context.contracts_for(property).any(|contract| {
        contract.getter == name
            && context
                .index
                .resolve_type(context.source_file, &contract.owner_type)
                .is_some_and(|owner| {
                    enclosing_declaration(context, node)
                        .is_some_and(|declaration| context.inherits_exactly(declaration, owner))
                })
    }) {
        return false;
    }
    true
}

fn safe_navigation_operator(source: &str, receiver: Node, member: Node) -> Option<&'static str> {
    match source[receiver.end_byte()..member.start_byte()].trim() {
        "." => Some("."),
        "?." => Some("?."),
        _ => None,
    }
}

fn enclosing_declaration<'a>(
    context: &'a RewriteContext<'_>,
    node: Node,
) -> Option<&'a Declaration> {
    let mut current = Some(node);
    let enclosing = loop {
        let candidate = current?;
        if matches!(
            candidate.kind(),
            "class_declaration" | "enum_class_declaration" | "object_declaration"
        ) {
            break candidate;
        }
        current = candidate.parent();
    };
    let name = enclosing.child_by_field_name("name")?;
    let name = kt::text(name, context.source);
    let mut declarations = context
        .source_file
        .declarations
        .iter()
        .filter(|declaration| declaration.name == name);
    let declaration = declarations.next()?;
    declarations.next().is_none().then_some(declaration)
}

fn expression_type(context: &RewriteContext<'_>, node: Node, expression: Node) -> Option<String> {
    match expression.kind() {
        "this_expression" => {
            enclosing_declaration_type(context.source_file, context.source, expression)
        }
        "identifier" => {
            let name = kt::text(expression, context.source);
            receiver_type_at(context, node, name)
        }
        "navigation_expression" => {
            let (receiver, property, _) = property_access_nodes(expression, context.source)?;
            let receiver_type = expression_type(context, node, receiver)?;
            let receiver_decl = context
                .index
                .resolve_type(context.source_file, &receiver_type)?;
            member_property_type(context.index, context.source_file, receiver_decl, &property)
        }
        "call_expression" => {
            let mut stack = vec![expression];
            while let Some(candidate) = stack.pop() {
                if candidate.kind() == "type_arguments" {
                    let mut cursor = candidate.walk();
                    let mut args = candidate.named_children(&mut cursor);
                    if let Some(argument) = args.next() {
                        let ty = kt::text(argument, context.source).trim();
                        if !ty.is_empty() {
                            return Some(ty.to_string());
                        }
                    }
                    return None;
                }
                stack.extend(candidate.named_children(&mut candidate.walk()));
            }
            let callee = expression
                .named_children(&mut expression.walk())
                .find(|child| child.kind() != "value_arguments")?;
            let arity = expression
                .named_children(&mut expression.walk())
                .find(|child| child.kind() == "value_arguments")
                .map(|arguments| {
                    arguments
                        .named_children(&mut arguments.walk())
                        .filter(|argument| argument.kind() == "value_argument")
                        .count()
                })
                .unwrap_or(0);
            match callee.kind() {
                "identifier" => {
                    let method = kt::text(callee, context.source);
                    let owner = enclosing_declaration(context, node)?;
                    method_return_type(context.index, context.source_file, owner, method, arity)
                }
                "navigation_expression" => {
                    let (receiver, method, _) = property_access_nodes(callee, context.source)?;
                    let receiver_type = expression_type(context, node, receiver)?;
                    let receiver_decl = context
                        .index
                        .resolve_type(context.source_file, &receiver_type)?;
                    method_return_type(
                        context.index,
                        context.source_file,
                        receiver_decl,
                        &method,
                        arity,
                    )
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn method_return_type(
    index: &SourceIndex,
    fallback_file: &SourceFile,
    owner: &Declaration,
    method_name: &str,
    arity: usize,
) -> Option<String> {
    let mut pending = vec![owner];
    let mut visited = HashSet::new();
    let mut returns = HashSet::new();
    while let Some(declaration) = pending.pop() {
        if !visited.insert(declaration as *const Declaration as usize) {
            continue;
        }
        let local = declaration
            .members
            .iter()
            .filter(|member| {
                member.kind == crate::workspace::MemberKind::Method
                    && member.name == method_name
                    && member.parameter_types.len() == arity
            })
            .filter_map(|member| member.type_name.as_deref())
            .collect::<HashSet<_>>();
        if !local.is_empty() {
            if local.len() != 1 {
                return None;
            }
            returns.extend(local.into_iter().map(str::to_owned));
            continue;
        }
        let declaring_file = index
            .declaration_source_file(declaration)
            .unwrap_or(fallback_file);
        pending.extend(
            declaration
                .supertypes
                .iter()
                .filter_map(|supertype| index.resolve_type(declaring_file, supertype)),
        );
    }
    if returns.len() != 1 {
        return None;
    }
    let resolved = returns.into_iter().next()?;
    let resolved = resolved.trim();
    let resolved_declaration = index
        .declaration_source_file(owner)
        .and_then(|file| index.resolve_type(file, resolved))?;
    resolved_declaration
        .package
        .as_ref()
        .map(|package| format!("{package}.{}", resolved_declaration.name))
        .or_else(|| Some(resolved_declaration.name.clone()))
        .or_else(|| Some(resolved.to_string()))
}

fn receiver_type_at(context: &RewriteContext<'_>, node: Node, name: &str) -> Option<String> {
    match scoped_binding_at(context, node, name) {
        Some(Some(ty)) => return Some(ty),
        Some(None) => return None,
        None => {}
    }
    enclosing_member_receiver_type(
        context.index,
        context.source_file,
        context.source,
        node,
        name,
    )
    .or_else(|| context.bindings.get(name).cloned())
}

/// `Some(None)` means a nearer untyped declaration shadows the name, so an
/// outer class property or the file-wide binding table must not be used.
fn scoped_binding_at(
    context: &RewriteContext<'_>,
    node: Node,
    name: &str,
) -> Option<Option<String>> {
    let source = context.source;
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "block" => {
                let mut bindings = parent
                    .named_children(&mut parent.walk())
                    .filter(|child| child.end_byte() < node.start_byte())
                    .filter_map(|child| declared_binding(child, source))
                    .filter(|(declared, _)| declared == name)
                    .collect::<Vec<_>>();
                if let Some((_, ty)) = bindings.pop() {
                    return Some(ty);
                }
            }
            "function_declaration" | "secondary_constructor" => {
                if let Some(parameters) = parent.child_by_field_name("parameters").or_else(|| {
                    parent
                        .named_children(&mut parent.walk())
                        .find(|child| child.kind() == "function_value_parameters")
                }) {
                    for parameter in parameters.named_children(&mut parameters.walk()) {
                        if let Some((declared, ty)) = declared_binding(parameter, source)
                            && declared == name
                        {
                            return Some(ty);
                        }
                    }
                }
            }
            "getter" | "setter" => {
                if parent.kind() == "setter" {
                    for parameter in parent.named_children(&mut parent.walk()) {
                        if let Some((declared, ty)) = declared_binding(parameter, source)
                            && declared == name
                        {
                            return Some(ty);
                        }
                    }
                }
            }
            "lambda_literal" => {
                let mut bindings = parent
                    .named_children(&mut parent.walk())
                    .filter(|child| child.end_byte() < node.start_byte())
                    .filter_map(|child| declared_binding(child, source))
                    .filter(|(declared, _)| declared == name)
                    .collect::<Vec<_>>();
                if let Some((_, ty)) = bindings.pop() {
                    return Some(ty);
                }
                for child in parent.named_children(&mut parent.walk()) {
                    if child.kind() == "lambda_parameters" {
                        for parameter in child.named_children(&mut child.walk()) {
                            if let Some((declared, ty)) = declared_binding(parameter, source)
                                && declared == name
                            {
                                return Some(ty);
                            }
                        }
                    }
                }
            }
            "for_statement" => {
                let mut cursor = parent.walk();
                let mut children = parent.named_children(&mut cursor);
                let variable = children.next();
                let Some((declared, _)) =
                    variable.and_then(|child| declared_binding(child, source))
                else {
                    current = parent;
                    continue;
                };
                if declared == name {
                    return Some(for_loop_element_type(context, parent));
                }
            }
            _ => {}
        }
        current = parent;
    }
    None
}

fn declared_binding(node: Node, source: &str) -> Option<(String, Option<String>)> {
    let declaration = if node.kind() == "property_declaration" {
        kt::child(node, "variable_declaration")?
    } else {
        node
    };
    let mut name = None;
    let mut ty = None;
    for child in declaration.named_children(&mut declaration.walk()) {
        match child.kind() {
            "identifier" if name.is_none() => name = Some(kt::text(child, source).to_string()),
            "user_type" | "nullable_type" if ty.is_none() => {
                let simple = crate::smart_cast::simple_type(kt::text(child, source));
                if !simple.is_empty() {
                    ty = Some(simple);
                }
            }
            _ => {}
        }
    }
    Some((name?, ty))
}

fn for_loop_element_type(context: &RewriteContext<'_>, loop_node: Node) -> Option<String> {
    let children = loop_node
        .named_children(&mut loop_node.walk())
        .collect::<Vec<_>>();
    let variable = children.first()?;
    let body = children
        .iter()
        .rev()
        .find(|child| child.kind() == "block")?;
    let iterable = children
        .iter()
        .rfind(|child| {
            child.start_byte() > variable.end_byte()
                && child.end_byte() < body.start_byte()
                && kt::text(**child, context.source) != "in"
        })
        .copied()?;
    sequence_element_type(context, loop_node, iterable, &mut HashSet::new())
}

fn sequence_element_type(
    context: &RewriteContext<'_>,
    at: Node,
    expression: Node,
    visiting: &mut HashSet<String>,
) -> Option<String> {
    match expression.kind() {
        "this_expression" => {
            enclosing_declaration_type(context.source_file, context.source, expression)
        }
        "identifier" => {
            let name = kt::text(expression, context.source).to_string();
            if !visiting.insert(name.clone()) {
                return None;
            }
            let initializer = local_initializer_before(context.source, at, &name)?;
            sequence_element_type(context, at, initializer, visiting)
        }
        "call_expression" => {
            let children = expression
                .named_children(&mut expression.walk())
                .collect::<Vec<_>>();
            let callee = *children.first()?;
            let args = children
                .iter()
                .find(|child| child.kind() == "value_arguments")
                .copied();
            let method = if callee.kind() == "identifier" {
                kt::text(callee, context.source).to_string()
            } else if callee.kind() == "navigation_expression" {
                property_access_nodes(callee, context.source).map(|(_, method, _)| method)?
            } else {
                return None;
            };
            match method.as_str() {
                "sequenceOf" => {
                    let argument = first_call_argument(args?, context.source)?;
                    expression_type(context, at, argument)
                }
                "plus" => {
                    let (receiver, _, _) = property_access_nodes(callee, context.source)?;
                    let left = sequence_element_type(context, at, receiver, visiting)?;
                    let right = sequence_element_type(
                        context,
                        at,
                        first_call_argument(args?, context.source)?,
                        visiting,
                    )?;
                    let left_decl = context.index.resolve_type(context.source_file, &left)?;
                    let right_decl = context.index.resolve_type(context.source_file, &right)?;
                    std::ptr::eq(left_decl, right_decl).then_some(left)
                }
                "asSequence" => {
                    let (receiver, _, _) = property_access_nodes(callee, context.source)?;
                    let name = (receiver.kind() == "identifier")
                        .then(|| kt::text(receiver, context.source))?;
                    declared_sequence_element(context, at, name)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn first_call_argument<'tree>(args: Node<'tree>, source: &str) -> Option<Node<'tree>> {
    let argument = args
        .named_children(&mut args.walk())
        .find(|child| child.kind() == "value_argument")?;
    argument
        .named_children(&mut argument.walk())
        .last()
        .filter(|child| !kt::text(*child, source).is_empty())
}

fn local_initializer_before<'tree>(
    source: &str,
    node: Node<'tree>,
    name: &str,
) -> Option<Node<'tree>> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "block" {
            let property = parent
                .named_children(&mut parent.walk())
                .filter(|child| child.end_byte() < node.start_byte())
                .filter(|child| {
                    declared_binding(*child, source).is_some_and(|(declared, _)| declared == name)
                })
                .last()?;
            return property
                .named_children(&mut property.walk())
                .filter(|child| child.kind() != "variable_declaration")
                .last();
        }
        current = parent;
    }
    None
}

fn declared_sequence_element(
    context: &RewriteContext<'_>,
    node: Node,
    name: &str,
) -> Option<String> {
    let mut current = Some(node);
    while let Some(parent) = current {
        if parent.kind() == "function_declaration" {
            let parameters = parent.child_by_field_name("parameters").or_else(|| {
                parent
                    .named_children(&mut parent.walk())
                    .find(|child| child.kind() == "function_value_parameters")
            });
            if let Some(parameters) = parameters {
                for parameter in parameters.named_children(&mut parameters.walk()) {
                    if declared_binding(parameter, context.source)
                        .is_some_and(|(declared, _)| declared == name)
                    {
                        let declared_type = parameter
                            .named_children(&mut parameter.walk())
                            .find(|child| matches!(child.kind(), "user_type" | "nullable_type"))?;
                        let mut stack = vec![parameter];
                        while let Some(candidate) = stack.pop() {
                            if candidate.kind() == "type_arguments" {
                                let argument =
                                    candidate.named_children(&mut candidate.walk()).next()?;
                                let mut nested = vec![argument];
                                while let Some(ty) = nested.pop() {
                                    if ty.kind() == "user_type" {
                                        return Some(crate::smart_cast::simple_type(kt::text(
                                            ty,
                                            context.source,
                                        )));
                                    }
                                    nested.extend(ty.named_children(&mut ty.walk()));
                                }
                            }
                            stack.extend(candidate.named_children(&mut candidate.walk()));
                        }
                        if context.source[parameters.start_byte()..parameter.start_byte()]
                            .contains("vararg")
                        {
                            return Some(crate::smart_cast::simple_type(kt::text(
                                declared_type,
                                context.source,
                            )));
                        }
                    }
                }
            }
        }
        current = parent.parent();
    }
    None
}

fn member_property_type(
    index: &SourceIndex,
    fallback_file: &SourceFile,
    declaration: &Declaration,
    property: &str,
) -> Option<String> {
    let mut pending = vec![declaration];
    let mut visited = HashSet::new();
    let mut found = None::<String>;
    while let Some(current) = pending.pop() {
        if !visited.insert(current as *const Declaration as usize) {
            continue;
        }
        for member in current.members.iter().filter(|member| {
            member.kind == crate::workspace::MemberKind::Property
                && member.name == property
                && !member.is_static
        }) {
            let ty = member.type_name.as_deref()?;
            if found.as_deref().is_some_and(|prior| prior != ty) {
                return None;
            }
            found.get_or_insert_with(|| ty.to_string());
        }
        let current_file = index
            .declaration_source_file(current)
            .unwrap_or(fallback_file);
        pending.extend(
            current
                .supertypes
                .iter()
                .filter_map(|supertype| index.resolve_type(current_file, supertype)),
        );
    }
    found
}

fn enclosing_declaration_type(
    source_file: &SourceFile,
    source: &str,
    node: Node,
) -> Option<String> {
    let mut current = Some(node);
    let enclosing = loop {
        let candidate = current?;
        if matches!(
            candidate.kind(),
            "class_declaration" | "enum_class_declaration" | "object_declaration"
        ) {
            break candidate;
        }
        current = candidate.parent();
    };
    let name = enclosing.child_by_field_name("name")?;
    let name = kt::text(name, source);
    let mut declarations = source_file
        .declarations
        .iter()
        .filter(|declaration| declaration.name == name);
    let declaration = declarations.next()?;
    if declarations.next().is_some() {
        return None;
    }
    Some(
        declaration
            .package
            .as_deref()
            .map(|package| format!("{package}.{}", declaration.name))
            .unwrap_or_else(|| declaration.name.clone()),
    )
}

/// Resolve a receiver that is an implicit-this property of the containing
/// declaration. Only an explicit property type is useful here; inferred types
/// and ambiguous/shadowed simple names remain unchanged.
fn enclosing_member_receiver_type(
    index: &SourceIndex,
    source_file: &SourceFile,
    source: &str,
    node: Node,
    receiver: &str,
) -> Option<String> {
    let mut current = Some(node);
    let enclosing = loop {
        let candidate = current?;
        if matches!(
            candidate.kind(),
            "class_declaration" | "enum_class_declaration" | "object_declaration"
        ) {
            break candidate;
        }
        current = candidate.parent();
    };
    let name = enclosing.child_by_field_name("name")?;
    let name = kt::text(name, source);
    let mut declarations = source_file
        .declarations
        .iter()
        .filter(|declaration| declaration.name == name);
    let declaration = declarations.next()?;
    if declarations.next().is_some() {
        return None;
    }

    let direct_members = declaration
        .members
        .iter()
        .filter(|member| {
            member.kind == crate::workspace::MemberKind::Property
                && member.name == receiver
                && !member.is_static
        })
        .collect::<Vec<_>>();
    if !direct_members.is_empty() {
        let mut resolved_type = None::<(&Declaration, String)>;
        for member in direct_members {
            let ty = member.type_name.as_deref()?;
            let context = index
                .declaration_source_file(declaration)
                .unwrap_or(source_file);
            let resolved = index.resolve_type(context, ty)?;
            if resolved_type
                .as_ref()
                .is_some_and(|(prior, _)| !std::ptr::eq(*prior, resolved))
            {
                return None;
            }
            let qualified = resolved
                .package
                .as_deref()
                .map(|package| format!("{package}.{}", resolved.name))
                .unwrap_or_else(|| resolved.name.clone());
            resolved_type.get_or_insert((resolved, qualified));
        }
        return resolved_type.map(|(_, ty)| ty);
    }

    let mut pending = vec![declaration];
    let mut visited = HashSet::new();
    let mut inherited_types = Vec::new();
    while let Some(current) = pending.pop() {
        if !visited.insert(current as *const Declaration as usize) {
            continue;
        }
        for member in current.members.iter().filter(|member| {
            member.kind == crate::workspace::MemberKind::Property
                && member.name == receiver
                && !member.is_static
        }) {
            let ty = member.type_name.as_deref()?;
            let context = index
                .declaration_source_file(current)
                .unwrap_or(source_file);
            let resolved = index.resolve_type(context, ty)?;
            let qualified = resolved
                .package
                .as_deref()
                .map(|package| format!("{package}.{}", resolved.name))
                .unwrap_or_else(|| resolved.name.clone());
            inherited_types.push((resolved, qualified, member.is_mutable, ty.contains('<')));
        }
        let context = index
            .declaration_source_file(current)
            .unwrap_or(source_file);
        pending.extend(
            current
                .supertypes
                .iter()
                .filter_map(|supertype| index.resolve_type(context, supertype)),
        );
    }
    let first = inherited_types.first()?;
    if inherited_types
        .iter()
        .all(|candidate| std::ptr::eq(candidate.0, first.0))
    {
        return Some(first.1.clone());
    }
    // Read-only properties may refine their inherited return type. Require
    // an indexed subtype of every competing type, rather than relying on
    // parent traversal order. Generic substitution is not established here.
    if inherited_types
        .iter()
        .any(|candidate| candidate.2 || candidate.3)
    {
        return None;
    }
    let mut most_specific = inherited_types.iter().filter(|candidate| {
        inherited_types
            .iter()
            .all(|other| inherits_exactly(index, source_file, candidate.0, other.0))
    });
    let selected = most_specific.next()?;
    most_specific
        .all(|other| std::ptr::eq(other.0, selected.0))
        .then(|| selected.1.clone())
}

fn inherits_exactly(
    index: &SourceIndex,
    use_file: &SourceFile,
    receiver: &Declaration,
    owner: &Declaration,
) -> bool {
    let mut pending = vec![receiver];
    let mut visited = Vec::new();
    while let Some(current) = pending.pop() {
        if std::ptr::eq(current, owner) {
            return true;
        }
        if visited.iter().any(|seen| std::ptr::eq(*seen, current)) {
            continue;
        }
        visited.push(current);
        let context = index.declaration_source_file(current).unwrap_or(use_file);
        pending.extend(
            current
                .supertypes
                .iter()
                .filter_map(|supertype| index.resolve_type(context, supertype)),
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{SourceIndex, SourceLanguage, SourceOverlay};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn candidate_scan_matches_identifier_boundaries_in_one_pass() {
        let names = HashSet::from(["enabled", "label"]);
        assert!(contains_any_identifier(
            "// enabled\nval text = \"label\"; val `enabled` = true",
            &names
        ));
        assert!(contains_any_identifier(
            "café.enabledSuffix + enabled",
            &names
        ));
        assert!(!contains_any_identifier(
            "enabledSuffix + xlabel + caféenabled",
            &names
        ));
    }

    #[test]
    fn candidate_scan_falls_back_for_unusual_property_names() {
        let names = HashSet::from(["with space", "odd-name"]);
        assert!(contains_any_identifier("obj.odd-name", &names));
        assert!(contains_any_identifier("obj.`with space`", &names));
        assert!(!contains_any_identifier("obj.odd-nameSuffix", &names));
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "notlin-property-callsite-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, rel: &str, source: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, source).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn contract() -> PropertyAccessorContract {
        PropertyAccessorContract {
            owner_type: "sample.Api".into(),
            property: "enabled".into(),
            getter: "isEnabled".into(),
            setter: Some("setEnabled".into()),
        }
    }

    #[test]
    fn rewrites_property_reads_on_resolved_method_return_receivers() {
        let fixture = Fixture::new();
        let category = fixture.write(
            "Category.java",
            "// NOTLIN: generated from Category.kt\npackage sample; public enum Category implements HasBaseAlias { INSTANCE; public String getBaseAlias() { return \"category\"; } }\n",
        );
        let java_base_alias = fixture.write(
            "JavaBaseAlias.java",
            "// NOTLIN: generated from JavaBaseAlias.kt\npackage sample; public interface JavaBaseAlias { String getBaseAlias(); }\n",
        );
        let properties = fixture.write(
            "Properties.java",
            "// NOTLIN: generated from Properties.kt\npackage sample; public interface Properties extends HasType { String getType(); }\n",
        );
        let java_base_type = fixture.write(
            "JavaBaseType.java",
            "// NOTLIN: generated from JavaBaseType.kt\npackage sample; public interface JavaBaseType { String getType(); }\n",
        );
        let holder = fixture.write(
            "Holder.java",
            "// NOTLIN: generated from Holder.kt\npackage sample; public class Holder { public String getPayload() { return \"payload\"; } }\n",
        );
        let caller = fixture.write(
            "Api.kt",
            "package sample\ninterface Api {\n    fun getCategory(): Category\n    fun getProperties(): Properties\n    fun alias() = getCategory().baseAlias\n    fun typeName() = getProperties().type\n    fun ordinary(holder: Holder) = holder.payload\n}\n",
        );
        fixture.write(
            "HasBaseAlias.kt",
            "package sample\ninterface HasBaseAlias : JavaBaseAlias {\n    val baseAlias: String\n}\n",
        );
        fixture.write(
            "HasType.kt",
            "package sample\ninterface HasType : JavaBaseType {\n    val type: String\n}\n",
        );
        let caller_tree = crate::transpiler::parse_tree(&fs::read_to_string(&caller).unwrap());
        assert!(
            !caller_tree.root_node().has_error(),
            "Api.kt should parse cleanly"
        );
        for (name, source) in [
            (
                "HasBaseAlias.kt",
                "package sample\ninterface HasBaseAlias : JavaBaseAlias {\n    val baseAlias: String\n}\n",
            ),
            (
                "HasType.kt",
                "package sample\ninterface HasType : JavaBaseType {\n    val type: String\n}\n",
            ),
        ] {
            let tree = crate::transpiler::parse_tree(source);
            assert!(!tree.root_node().has_error(), "{name} should parse cleanly");
        }
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&caller).unwrap();
        let generated_java = [
            category,
            java_base_alias,
            properties,
            java_base_type,
            holder,
        ]
        .into_iter()
        .map(|path| fs::canonicalize(path).unwrap())
        .collect();
        let contracts = crate::property_abi::repaired_callsite_contracts(&index, &generated_java);
        let api = index
            .kotlin_files()
            .flat_map(|file| file.declarations.iter())
            .find(|declaration| declaration.name == "Api")
            .expect("Api declaration must be indexed");
        for expected in ["Api", "HasBaseAlias", "HasType"] {
            assert!(
                index.kotlin_files().any(|file| {
                    file.declarations
                        .iter()
                        .any(|declaration| declaration.name == expected)
                }),
                "{expected} declaration must be indexed"
            );
        }
        assert!(
            api.members.iter().any(|member| {
                member.name == "getCategory"
                    && member.type_name.as_deref() == Some("Category")
                    && member.parameter_types.is_empty()
            }),
            "indexed Api members: {:?}",
            api.members
        );
        assert!(
            api.members.iter().any(|member| {
                member.name == "getProperties"
                    && member.type_name.as_deref() == Some("Properties")
                    && member.parameter_types.is_empty()
            }),
            "indexed Api members: {:?}",
            api.members
        );
        let caller_file = index
            .source_file(&caller)
            .expect("caller file must be indexed");
        assert_eq!(
            method_return_type(&index, caller_file, api, "getCategory", 0).as_deref(),
            Some("sample.Category"),
            "indexed Api members: {:?}",
            api.members
        );
        assert_eq!(
            method_return_type(&index, caller_file, api, "getProperties", 0).as_deref(),
            Some("sample.Properties"),
            "indexed Api members: {:?}",
            api.members
        );
        assert!(
            contracts.iter().any(|contract| {
                contract.owner_type == "sample.HasBaseAlias"
                    && contract.property == "baseAlias"
                    && contract.getter == "getBaseAlias"
            }),
            "callsite contracts: {contracts:?}"
        );
        assert!(
            contracts.iter().any(|contract| {
                contract.owner_type == "sample.HasType"
                    && contract.property == "type"
                    && contract.getter == "getType"
            }),
            "callsite contracts: {contracts:?}"
        );
        let (rewritten, count) = rewrite_file(&index, &caller, &source, &contracts);
        assert_eq!(count, 2);
        assert!(
            rewritten.contains("getCategory().getBaseAlias()"),
            "{rewritten}"
        );
        assert!(
            rewritten.contains("getProperties().getType()"),
            "{rewritten}"
        );
        assert!(rewritten.contains("holder.payload"), "{rewritten}");
        assert!(!rewritten.contains("holder.getPayload()"), "{rewritten}");
    }

    #[test]
    fn rewrites_typed_inherited_reads_and_plain_writes() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface Api\nclass Impl : Api\nfun use(api: Api, impl: Impl) {\n    println(api.enabled)\n    impl.enabled = true\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract()]);
        assert_eq!(count, 2);
        assert!(updated.contains("println(api.isEnabled())"), "{updated}");
        assert!(updated.contains("impl.setEnabled(true)"), "{updated}");
    }

    #[test]
    fn resolves_receiver_binding_in_its_function_scope() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface Api {\n    val enabled: Boolean\n}\nfun use(source: Api): Boolean {\n    val outer = source.enabled\n    run {\n        val source: String = \"\"\n        println(source.enabled)\n    }\n    return outer\n}\nclass Other {\n    fun use(source: String): Int = source.length\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract()]);
        assert_eq!(count, 1, "{updated}");
        assert!(
            updated.contains("val outer = source.isEnabled()"),
            "{updated}"
        );
        assert!(updated.contains("println(source.enabled)"), "{updated}");
        assert!(updated.contains("fun use(source: String): Int = source.length"));
    }

    #[test]
    fn resolves_receiver_binding_in_secondary_constructor_scope() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface Api { val enabled: Boolean }\nclass Use {\n    constructor(api: Api) { println(api.enabled) }\n    constructor(api: String, fallback: Boolean) { println(api.length); println(fallback) }\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract()]);
        assert_eq!(count, 1, "{updated}");
        assert!(
            updated.contains("constructor(api: Api) { println(api.isEnabled()) }"),
            "{updated}"
        );
        assert!(
            updated.contains("constructor(api: String, fallback: Boolean) { println(api.length); println(fallback) }"),
            "{updated}"
        );
    }

    #[test]
    fn refuses_ambiguous_untyped_shadowed_and_compound_accesses() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface Api\nfun use(api: Api, other: Any, enabled: Boolean) {\n    println(api.enabled)\n    println(other.enabled)\n    enabled = api.enabled\n    api.enabled += true\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract()]);
        assert_eq!(count, 2);
        assert!(updated.contains("println(api.isEnabled())"), "{updated}");
        assert!(updated.contains("other.enabled"), "{updated}");
        assert!(updated.contains("enabled = api.isEnabled()"), "{updated}");
        assert!(updated.contains("api.enabled += true"), "{updated}");
    }

    #[test]
    fn sees_contracts_from_speculative_overlay() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\nfun use(api: Api) { println(api.enabled) }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let overlay = SourceOverlay::Replace {
            path: fixture.0.join("Api.java"),
            language: SourceLanguage::Java,
            source: "package sample;\npublic interface Api { boolean isEnabled(); }\n".into(),
        };
        let virtual_index = index.with_overlays(&[overlay]).unwrap();
        assert!(!fixture.0.join("Api.java").exists());
        let source = fs::read_to_string(&path).unwrap();
        let (updated, count) = rewrite_file(&virtual_index, &path, &source, &[contract()]);
        assert_eq!(count, 1);
        assert!(updated.contains("api.isEnabled()"), "{updated}");
    }

    #[test]
    fn rewrites_only_interface_self_reads_and_respects_local_shadows() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Api.kt",
            "package sample\ninterface Api {\n    fun isEnabled(): Boolean\n    fun getLabel(): String\n    fun read(): Boolean = enabled\n    fun readLabel(): String = label\n    val state: Boolean get() = enabled\n    fun withParameter(enabled: Boolean): Boolean = enabled\n    fun withLocal(): Boolean { val enabled = false; return enabled }\n}\nclass Impl(val enabled: Boolean) { fun read(): Boolean = enabled }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let label = PropertyAccessorContract {
            owner_type: "sample.Api".into(),
            property: "label".into(),
            getter: "getLabel".into(),
            setter: None,
        };
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract(), label]);
        assert_eq!(count, 3, "{updated}");
        assert!(
            updated.contains("fun read(): Boolean = isEnabled()"),
            "{updated}"
        );
        assert!(
            updated.contains("fun readLabel(): String = getLabel()"),
            "{updated}"
        );
        assert!(updated.contains("get() = isEnabled()"), "{updated}");
        assert!(
            updated.contains("fun withParameter(enabled: Boolean): Boolean = enabled"),
            "{updated}"
        );
        assert!(
            updated.contains("val enabled = false; return enabled"),
            "{updated}"
        );
        assert!(
            updated.contains("class Impl(val enabled: Boolean) { fun read(): Boolean = enabled }"),
            "{updated}"
        );
    }

    #[test]
    fn inherited_readonly_receiver_uses_unique_covariant_type() {
        let fixture = Fixture::new();
        let path = fixture.write("Use.kt", "package sample\ninterface Base {}\ninterface Detail : Base {\n fun getCategory(): String\n}\ninterface Wide {\n val variant: Base\n}\ninterface Narrow {\n val variant: Detail\n}\ninterface Use : Narrow, Wide {\n val category: String\n get() = variant.category\n}\ninterface Other {}\ninterface Conflicting {\n val variant: Other\n}\ninterface Ambiguous : Narrow, Conflicting {\n val category: String\n get() = variant.category\n}\n");
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let contract = PropertyAccessorContract {
            owner_type: "sample.Detail".into(),
            property: "category".into(),
            getter: "getCategory".into(),
            setter: None,
        };
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract]);
        assert_eq!(count, 1, "{updated}");
        assert!(updated.contains("interface Use : Narrow, Wide {\n val category: String\n get() = variant.getCategory()\n}"), "{updated}");
        assert!(updated.contains("interface Ambiguous : Narrow, Conflicting {\n val category: String\n get() = variant.category\n}"), "{updated}");
    }

    #[test]
    fn preserves_named_argument_labels_while_rewriting_the_value() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Api.kt",
            "package sample\ninterface Api { fun isEnabled(): Boolean }\ndata class Impl(val marker: Boolean) : Api {\n    fun copy(enabled: Boolean): Impl = this\n    fun update(): Impl = copy(enabled = enabled)\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract()]);
        assert_eq!(count, 1, "{updated}");
        assert!(updated.contains("copy(enabled = isEnabled())"), "{updated}");
    }

    #[test]
    fn rewrites_bare_reads_in_descendants_chains_and_interpolations() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Api.kt",
            "package sample\ninterface Api {\n    var payload: String\n    fun rendered(): String = \"$payload:${payload.length}\"\n    fun chained(): Int = payload.length\n    fun update(value: String) { payload = value }\n}\nabstract class Child : Api {\n    fun childRead(): Int = payload.length\n    fun local(payload: String): String = payload\n    fun localBlock(): String { val payload = \"local\"; return payload }\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let contract = PropertyAccessorContract {
            owner_type: "sample.Api".into(),
            property: "payload".into(),
            getter: "getPayload".into(),
            setter: Some("setPayload".into()),
        };
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract]);
        assert_eq!(count, 4, "{updated}");
        assert!(
            updated.contains("\"${getPayload()}:${getPayload().length}\""),
            "{updated}"
        );
        assert!(
            updated.contains("fun chained(): Int = getPayload().length"),
            "{updated}"
        );
        assert!(updated.contains("payload = value"), "{updated}");
        assert!(
            updated.contains("fun childRead(): Int = getPayload().length"),
            "{updated}"
        );
        assert!(
            updated.contains("fun local(payload: String): String = payload"),
            "{updated}"
        );
        assert!(
            updated.contains("val payload = \"local\"; return payload"),
            "{updated}"
        );
    }

    #[test]
    fn rewrites_a_property_receiver_typed_as_a_contract_descendant() {
        let fixture = Fixture::new();
        fixture.write(
            "Types.kt",
            "package sample\ninterface WorkflowInfo { val workflow: String }\ninterface WorkflowAssetInfo : WorkflowInfo\n",
        );
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface Use { val asset: WorkflowAssetInfo\nfun id(): String = asset.workflow\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let contract = PropertyAccessorContract {
            owner_type: "sample.WorkflowInfo".into(),
            property: "workflow".into(),
            getter: "getWorkflow".into(),
            setter: None,
        };
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract]);
        assert_eq!(count, 1, "{updated}");
        assert!(updated.contains("asset.getWorkflow()"), "{updated}");
    }

    #[test]
    fn rewrites_safe_navigation_after_a_typed_generic_call() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface Resource\ninterface Holder { val resource: Resource?; fun read(): String? = resource?.id }\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let contract = PropertyAccessorContract {
            owner_type: "sample.Resource".into(),
            property: "id".into(),
            getter: "getId".into(),
            setter: None,
        };
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract]);
        assert_eq!(count, 1, "{updated}");
        assert!(updated.contains("resource?.getId()"), "{updated}");
    }

    #[test]
    fn rewrites_receiver_from_a_generic_safe_navigation_call() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface Resource {\n    val id: String\n}\ninterface ResourceContext {\n    val resource: Resource?\n}\ninterface Context {\n    fun <T> get(key: String): T\n    fun read(): String? = get<ResourceContext>(\"resource\")?.resource?.id\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let contracts = [
            PropertyAccessorContract {
                owner_type: "sample.Resource".into(),
                property: "id".into(),
                getter: "getId".into(),
                setter: None,
            },
            PropertyAccessorContract {
                owner_type: "sample.ResourceContext".into(),
                property: "resource".into(),
                getter: "getResource".into(),
                setter: None,
            },
        ];
        let (updated, count) = rewrite_file(&index, &path, &source, &contracts);
        assert_eq!(count, 2, "{updated}");
        assert!(
            updated.contains("get<ResourceContext>(\"resource\")?.getResource()?.getId()"),
            "{updated}"
        );
    }

    #[test]
    fn scopes_retained_kotlin_property_reads_inside_repaired_getters() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface ResourceInfo\ninterface WorkContext\ninterface ResourceContext : WorkContext {\n    val resource: ResourceInfo\n}\ninterface Context {\n    fun <T> get(key: String): T\n}\ninterface ResourceAware {\n    fun getResource(): ResourceInfo\n}\ninterface ContextAware : ResourceAware {\n    val context: Context\n    override fun getResource(): ResourceInfo = context.get<ResourceContext>(\"resource\")?.resource\n    fun preview(): ResourceInfo? = context.get<ResourceContext>(\"resource\")?.resource\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let contract = PropertyAccessorContract {
            owner_type: "sample.ResourceAware".into(),
            property: "resource".into(),
            getter: "getResource".into(),
            setter: None,
        };
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract]);
        assert_eq!(count, 1, "{updated}");
        assert!(
            updated.contains("context.get<ResourceContext>(\"resource\")?.let { it.resource }"),
            "{updated}"
        );
        assert!(
            updated.contains("fun preview(): ResourceInfo? = context.get<ResourceContext>(\"resource\")?.resource"),
            "{updated}"
        );
    }

    #[test]
    fn infers_loop_receiver_from_sequence_of_this_plus_fallbacks() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface Properties {\n    val properties: String\n    fun read(vararg fallbacks: Properties): String {\n        val chain = sequenceOf(this).plus(fallbacks.asSequence())\n        for (src in chain) {\n            return src.properties\n        }\n        return \"\"\n    }\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let contract = PropertyAccessorContract {
            owner_type: "sample.Properties".into(),
            property: "properties".into(),
            getter: "getProperties".into(),
            setter: None,
        };
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract]);
        assert_eq!(count, 1, "{updated}");
        assert!(updated.contains("return src.getProperties()"), "{updated}");
    }

    #[test]
    fn rewrites_this_and_inherited_member_property_contracts() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface WorkflowInfo {\n    val workflow: String\n    fun own(): String = this.workflow\n}\ninterface WorkflowOrderInfo : WorkflowInfo\ninterface Holder {\n    val workflowOrder: WorkflowOrderInfo\n    fun inherited(): String = workflowOrder.workflow\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let contract = PropertyAccessorContract {
            owner_type: "sample.WorkflowInfo".into(),
            property: "workflow".into(),
            getter: "getWorkflow".into(),
            setter: None,
        };
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract]);
        assert_eq!(count, 2, "{updated}");
        assert!(updated.contains("this.getWorkflow()"), "{updated}");
        assert!(updated.contains("workflowOrder.getWorkflow()"), "{updated}");
    }

    #[test]
    fn prefers_direct_override_type_over_conflicting_inherited_property_type() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface Identifier\ninterface WorkflowInfo {\n    val workflow: String\n}\ninterface WorkflowOrderInfo : WorkflowInfo, Identifier\ninterface WorkOrder {\n    val workflowOrder: Identifier\n}\ninterface ConcreteWorkOrder : WorkOrder {\n    override val workflowOrder: WorkflowOrderInfo\n    fun read(): String = workflowOrder.workflow\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let contract = PropertyAccessorContract {
            owner_type: "sample.WorkflowInfo".into(),
            property: "workflow".into(),
            getter: "getWorkflow".into(),
            setter: None,
        };
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract]);
        assert_eq!(count, 1, "{updated}");
        assert!(updated.contains("workflowOrder.getWorkflow()"), "{updated}");
    }

    #[test]
    fn resolves_inherited_member_property_receiver_types() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Use.kt",
            "package sample\ninterface Api {\n    val enabled: Boolean\n}\ninterface Holder {\n    val variant: Api\n}\nclass Use : Holder {\n    fun read(): Boolean = variant.enabled\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract()]);
        assert_eq!(count, 1, "{updated}");
        assert!(updated.contains("variant.isEnabled()"), "{updated}");
    }

    #[test]
    fn rewrites_super_property_when_the_repaired_parent_declares_the_getter() {
        let fixture = Fixture::new();
        let path = fixture.write(
            "Types.kt",
            "package sample\n\ninterface Api {\n    fun isEnabled(): Boolean\n}\n\ninterface Grandparent : Api {\n    override fun isEnabled(): Boolean = true\n}\n\ninterface Parent : Grandparent\n\nenum class Child : Parent {\n    ONE;\n\n    val local: Boolean\n        get() = super.enabled\n}\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let (updated, count) = rewrite_file(&index, &path, &source, &[contract()]);
        assert_eq!(count, 1, "{updated}");
        assert!(updated.contains("super.isEnabled()"), "{updated}");
    }

    #[test]
    fn incremental_repair_skips_unchanged_and_irrelevant_sources() {
        let fixture = Fixture::new();
        fixture.write("Api.kt", "package sample\ninterface Api\n");
        let use_path = fixture.write(
            "Use.kt",
            "package sample\nfun use(api: Api): Boolean = api.enabled\n",
        );
        let other_path = fixture.write("Other.kt", "package sample\nfun other(): Int = 42\n");
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let mut sources = vec![
            (
                use_path,
                "package sample\nfun use(api: Api): Boolean = api.enabled\n".into(),
            ),
            (other_path, "package sample\nfun other(): Int = 42\n".into()),
        ];
        let mut state = IncrementalRepairState::default();

        let first = state.rewrite_files(&index, &mut sources, &[contract()], false);
        assert_eq!(first.candidate_files, 1);
        assert_eq!(first.rewritten_accesses, 1);
        assert!(sources[0].1.contains("api.isEnabled()"));

        let unchanged = state.rewrite_files(&index, &mut sources, &[contract()], false);
        assert_eq!(unchanged, IncrementalRepairStats::default());

        sources[1].1 = "package sample\nfun other(): Int = 43\n".into();
        let irrelevant = state.rewrite_files(&index, &mut sources, &[contract()], false);
        assert_eq!(irrelevant, IncrementalRepairStats::default());
    }

    #[test]
    fn convergence_sweep_catches_external_hierarchy_changes() {
        let fixture = Fixture::new();
        fixture.write("Api.kt", "package sample\ninterface Api\n");
        let child_path = fixture.write("Child.kt", "package sample\nclass Child\n");
        let use_path = fixture.write(
            "Use.kt",
            "package sample\nfun use(child: Child): Boolean = child.enabled\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let mut sources = vec![(
            use_path,
            "package sample\nfun use(child: Child): Boolean = child.enabled\n".into(),
        )];
        let mut state = IncrementalRepairState::default();
        let first = state.rewrite_files(&index, &mut sources, &[contract()], false);
        assert_eq!(first.candidate_files, 1);
        assert_eq!(first.rewritten_accesses, 0);

        fs::write(&child_path, "package sample\nclass Child : Api {}\n").unwrap();
        let changed_index = SourceIndex::discover(&fixture.0).unwrap();
        let changed_child = changed_index.declarations_named("Child").next().unwrap();
        assert_eq!(changed_child.supertypes, vec!["Api"]);
        let changed_api = changed_index.declarations_named("Api").next().unwrap();
        let changed_child_file = changed_index
            .declaration_source_file(changed_child)
            .unwrap();
        assert!(inherits_exactly(
            &changed_index,
            changed_child_file,
            changed_child,
            changed_api
        ));
        let incremental = state.rewrite_files(&changed_index, &mut sources, &[contract()], false);
        assert_eq!(incremental, IncrementalRepairStats::default());

        let sweep = state.rewrite_files(&changed_index, &mut sources, &[contract()], true);
        assert_eq!(sweep.candidate_files, 1);
        assert_eq!(sweep.rewritten_accesses, 1);
        assert!(sources[0].1.contains("child.isEnabled()"));
    }

    #[test]
    fn added_contract_invalidates_only_its_property_referencers() {
        let fixture = Fixture::new();
        fixture.write(
            "Api.kt",
            "package sample\ninterface Api\ninterface Labels\n",
        );
        let enabled_path = fixture.write(
            "Enabled.kt",
            "package sample\nfun enabled(api: Api): Boolean = api.enabled\n",
        );
        let label_path = fixture.write(
            "Label.kt",
            "package sample\nfun label(labels: Labels): String = labels.label\n",
        );
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let mut sources = vec![
            (
                enabled_path,
                "package sample\nfun enabled(api: Api): Boolean = api.enabled\n".into(),
            ),
            (
                label_path,
                "package sample\nfun label(labels: Labels): String = labels.label\n".into(),
            ),
        ];
        let mut state = IncrementalRepairState::default();
        let first = state.rewrite_files(&index, &mut sources, &[contract()], false);
        assert_eq!(first.candidate_files, 1);
        assert_eq!(first.rewritten_accesses, 1);

        let label = PropertyAccessorContract {
            owner_type: "sample.Labels".into(),
            property: "label".into(),
            getter: "getLabel".into(),
            setter: None,
        };
        let added = state.rewrite_files(&index, &mut sources, &[contract(), label], false);
        assert_eq!(added.candidate_files, 1);
        assert_eq!(added.rewritten_accesses, 1);
        assert!(sources[1].1.contains("labels.getLabel()"));
    }
}
