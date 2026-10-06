//! Rewrite retained Kotlin property syntax when a workspace contract has been
//! repaired to explicit JavaBean methods or translated to Java.
//!
//! The pass is deliberately conservative. It handles `name.property` for a
//! simple, explicitly typed receiver whose type is proven to inherit the
//! listed contract, plus bare reads in executable bodies of the exact retained
//! interface that owns the contract. Parameters and locals that shadow a bare
//! property, safe calls, callables, compound writes, unknown receiver types,
//! and ambiguous contracts are left as-is. Passing a [`SourceIndex`] built
//! with speculative overlays works the same as passing a disk-backed index.

use crate::smart_cast;
use crate::transpiler::kt;
use crate::workspace::{Declaration, DeclarationKind, SourceFile, SourceIndex};
use std::collections::{HashMap, HashSet};
use std::path::Path;
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
    let bindings = smart_cast::bindings(source);
    let context = RewriteContext {
        index,
        source_file,
        source,
        bindings: &bindings,
        contracts,
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

    // Repaired interface properties no longer provide Kotlin property syntax
    // to their own default method bodies. Those references are unqualified, so
    // the receiver-type pass above cannot resolve them. Limit this pass to the
    // exact retained interface that owns each contract.
    rewritten +=
        rewrite_interface_self_reads(index, source_file, source, &tree, contracts, &mut edits);

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
        let Some((receiver, property)) = property_access(node, source) else {
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
        let contract = if receiver == "super" {
            contract_for_super(index, source_file, source, node, &property, contracts)
        } else {
            contract_for(&context, node, &receiver, &property)
        };
        let Some(contract) = contract else {
            continue;
        };
        let Some(member) = node.named_children(&mut node.walk()).last() else {
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
    // This exact reconstruction rejects safe navigation and trivia shapes that
    // are not the ordinary `name.member` expression.
    (kt::text(node, source) == format!("{receiver}.{property}")).then_some((receiver, property))
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

fn rewrite_interface_self_reads(
    index: &SourceIndex,
    source_file: &SourceFile,
    source: &str,
    tree: &tree_sitter::Tree,
    contracts: &[PropertyAccessorContract],
    edits: &mut Vec<Edit>,
) -> usize {
    let mut matching_owners: Vec<(&Declaration, &PropertyAccessorContract)> = Vec::new();
    for contract in contracts {
        let Some(owner) = index.resolve_type(source_file, &contract.owner_type) else {
            continue;
        };
        if owner.kind != DeclarationKind::Interface
            || !index
                .declaration_source_file(owner)
                .is_some_and(|owner_file| std::ptr::eq(owner_file, source_file))
        {
            continue;
        }
        if !matching_owners.iter().any(|(seen, seen_contract)| {
            std::ptr::eq(*seen, owner)
                && seen_contract.property == contract.property
                && seen_contract.getter == contract.getter
                && seen_contract.setter == contract.setter
        }) {
            matching_owners.push((owner, contract));
        }
    }

    let mut interface_nodes = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "class_declaration"
            && node
                .children(&mut node.walk())
                .any(|child| child.kind() == "interface")
            && let Some(name) = node.child_by_field_name("name")
        {
            let name = kt::text(name, source);
            if matching_owners.iter().any(|(owner, _)| owner.name == name) {
                interface_nodes.push((node, name.to_string()));
            }
        }
        for child in node.named_children(&mut node.walk()) {
            stack.push(child);
        }
    }

    // Duplicate same-named interface declarations make a simple-name AST match
    // ambiguous. In that case, leave them unchanged.
    let mut changed = 0;
    for (interface, owner_name) in interface_nodes {
        if interface_nodes_count_for_name(tree.root_node(), source, &owner_name) != 1 {
            continue;
        }
        let owner_contracts = matching_owners
            .iter()
            .filter(|(owner, _)| owner.name == owner_name)
            .map(|(_, contract)| *contract)
            .collect::<Vec<_>>();
        if owner_contracts.is_empty() {
            continue;
        }
        let mut bodies = Vec::new();
        collect_interface_bodies(interface, source, &mut bodies);
        for (body, scope, method_name) in bodies {
            for contract in &owner_contracts {
                // Conflicting plans for one property do not identify a safe
                // accessor. Identical duplicate plans are already deduplicated.
                if owner_contracts.iter().any(|other| {
                    other.property == contract.property
                        && (other.getter != contract.getter || other.setter != contract.setter)
                }) {
                    continue;
                }
                // If a parameter, local, or nested local declaration shadows
                // this property anywhere in the method, fail closed for it.
                if method_name.as_deref() == Some(contract.property.as_str())
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
            }
        }
    }
    changed
}

fn interface_nodes_count_for_name(root: Node, source: &str, expected: &str) -> usize {
    let mut count = 0;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "class_declaration"
            && node
                .children(&mut node.walk())
                .any(|child| child.kind() == "interface")
            && node
                .child_by_field_name("name")
                .is_some_and(|name| kt::text(name, source) == expected)
        {
            count += 1;
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    count
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
    // Exclude declarations, qualified members/receivers, type names, and
    // callable names. This keeps the edit to bare expression reads only.
    !matches!(
        parent.kind(),
        "variable_declaration"
            | "parameter"
            | "class_parameter"
            | "function_declaration"
            | "navigation_expression"
            | "user_type"
            | "nullable_type"
            | "type_arguments"
            | "type_parameters"
            | "annotation"
            | "call_expression"
    ) && !(parent.kind() == "assignment"
        && parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()))
}

struct RewriteContext<'a> {
    index: &'a SourceIndex,
    source_file: &'a SourceFile,
    source: &'a str,
    bindings: &'a HashMap<String, String>,
    contracts: &'a [PropertyAccessorContract],
}

fn contract_for<'a>(
    context: &RewriteContext<'a>,
    node: Node,
    receiver: &str,
    property: &str,
) -> Option<&'a PropertyAccessorContract> {
    let receiver_type = context.bindings.get(receiver).cloned().or_else(|| {
        enclosing_member_receiver_type(
            context.index,
            context.source_file,
            context.source,
            node,
            receiver,
        )
    })?;
    let receiver_decl = context
        .index
        .resolve_type(context.source_file, &receiver_type)?;
    let matching = context
        .contracts
        .iter()
        .filter(|contract| contract.property == property)
        .filter(|contract| {
            let Some(owner) = context
                .index
                .resolve_type(context.source_file, &contract.owner_type)
            else {
                return false;
            };
            inherits_exactly(context.index, context.source_file, receiver_decl, owner)
        })
        .collect::<Vec<_>>();
    let first = *matching.first()?;
    matching
        .iter()
        .all(|candidate| candidate.getter == first.getter && candidate.setter == first.setter)
        .then_some(first)
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

    // A function parameter or local with the same spelling takes precedence
    // over the implicit receiver property. Avoid using the class property when
    // such a shadow exists anywhere in the containing function body.
    let mut scope = node;
    while let Some(parent) = scope.parent() {
        if matches!(parent.kind(), "function_declaration" | "getter" | "setter") {
            if scope_declares(parent, source, receiver) {
                return None;
            }
            break;
        }
        scope = parent;
    }

    let mut pending = vec![declaration];
    let mut visited = HashSet::new();
    let mut found_type: Option<String> = None;
    let mut found_decl: Option<*const Declaration> = None;
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
            let resolved_ptr = resolved as *const Declaration;
            if found_decl.is_some_and(|prior| prior != resolved_ptr) {
                return None;
            }
            found_decl = Some(resolved_ptr);
            let qualified = resolved
                .package
                .as_deref()
                .map(|package| format!("{package}.{}", resolved.name))
                .unwrap_or_else(|| resolved.name.clone());
            found_type.get_or_insert(qualified);
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
    found_type
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
}
