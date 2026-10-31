//! Class/object/interface/record emission.

use super::Unit;
use super::capitalize;
use crate::diagnostics::{DiagnosticKind, RetentionKind};
use crate::transpiler::expr::Expr;
use crate::transpiler::java::JavaOut;
use crate::transpiler::kt;
use std::collections::HashSet;

/// Retention closure over the type hierarchy: a retained declaration forces
/// every declaration it shares a hierarchy edge with to stay Kotlin. It exists
/// because the translator refuses mixed-language hierarchies, so it is ON by
/// default and can be switched off (`NOTLIN_RETENTION_HIERARCHY=off`) to probe
/// what the closure is actually buying.
fn hierarchy_closure() -> bool {
    !std::env::var("NOTLIN_RETENTION_HIERARCHY").is_ok_and(|value| value == "off")
}

fn primary_constructor(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    node.child_by_field_name("primary_constructor")
        .or_else(|| kt::child(node, "primary_constructor"))
}

fn constructor_parameter_names(constructor: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let Some(parameters) = kt::child(constructor, "class_parameters") else {
        return Vec::new();
    };
    parameters
        .named_children(&mut parameters.walk())
        .filter(|parameter| parameter.kind() == "class_parameter")
        .filter_map(|parameter| {
            parameter
                .child_by_field_name("name")
                .or_else(|| {
                    parameter
                        .named_children(&mut parameter.walk())
                        .find(|child| matches!(child.kind(), "identifier" | "simple_identifier"))
                })
                .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                .map(str::to_owned)
        })
        .collect()
}

fn constructor_parameter_defaults(constructor: tree_sitter::Node<'_>) -> Vec<bool> {
    let Some(parameters) = kt::child(constructor, "class_parameters") else {
        return Vec::new();
    };
    parameters
        .named_children(&mut parameters.walk())
        .filter(|parameter| parameter.kind() == "class_parameter")
        .map(|parameter| {
            parameter.child_by_field_name("default_value").is_some()
                || parameter
                    .children(&mut parameter.walk())
                    .any(|child| child.kind() == "=")
        })
        .collect()
}

fn is_descendant_of(node: tree_sitter::Node<'_>, ancestor: tree_sitter::Node<'_>) -> bool {
    let mut parent = Some(node);
    while let Some(current) = parent {
        if current.id() == ancestor.id() {
            return true;
        }
        parent = current.parent();
    }
    false
}

fn annotation_fq_name(annotation: tree_sitter::Node<'_>, source: &str) -> Option<String> {
    let mut stack = vec![annotation];
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "user_type" | "scoped_type_identifier") {
            return node
                .utf8_text(source.as_bytes())
                .ok()
                .map(|name| name.chars().filter(|c| !c.is_whitespace()).collect());
        }
        let mut walk = node.walk();
        let children = node.named_children(&mut walk).collect::<Vec<_>>();
        stack.extend(children.into_iter().rev());
    }
    None
}

fn annotation_imports(source: &str) -> impl Iterator<Item = (&str, Option<&str>)> {
    source.lines().filter_map(|line| {
        let import = line
            .trim()
            .strip_prefix("import ")?
            .trim_end_matches(';')
            .trim();
        Some(
            import
                .split_once(" as ")
                .map_or((import, None), |(path, alias)| {
                    (path.trim(), Some(alias.trim()))
                }),
        )
    })
}

fn annotation_type_is_local(
    workspace: Option<&crate::workspace::SourceIndex>,
    file: Option<&crate::workspace::SourceFile>,
    name: &str,
) -> bool {
    workspace.zip(file).is_some_and(|(index, file)| {
        index.declarations_named(name).any(|declaration| {
            declaration.package == file.package
                || index
                    .declaration_source_file(declaration)
                    .is_some_and(|owner| owner.path == file.path)
        })
    })
}

fn annotation_import_name(name: String, source: &str) -> String {
    annotation_imports(source)
        .find(|(_, alias)| *alias == Some(name.as_str()))
        .map(|(path, _)| path.to_string())
        .unwrap_or(name)
}

fn kotlin_annotation_meta_name(
    annotation: tree_sitter::Node<'_>,
    source: &str,
    workspace: Option<&crate::workspace::SourceIndex>,
    file: Option<&crate::workspace::SourceFile>,
) -> Option<&'static str> {
    const META: [(&str, &str); 3] = [
        ("kotlin.annotation.Target", "Target"),
        ("kotlin.annotation.Retention", "Retention"),
        ("kotlin.annotation.MustBeDocumented", "MustBeDocumented"),
    ];
    let name = annotation_fq_name(annotation, source)?;
    if let Some((_, simple)) = META.iter().find(|(path, _)| *path == name) {
        return Some(simple);
    }
    let imports = annotation_imports(source).collect::<Vec<_>>();
    let explicit = imports
        .iter()
        .filter(|(path, alias)| {
            alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == name
        })
        .collect::<Vec<_>>();
    match explicit.as_slice() {
        [(path, _)] => META
            .iter()
            .find(|(known, _)| known == path)
            .map(|(_, simple)| *simple),
        [] if !imports
            .iter()
            .any(|(path, _)| path.ends_with(".*") && *path != "kotlin.annotation.*")
            && !annotation_type_is_local(workspace, file, &name) =>
        {
            META.iter()
                .find(|(_, simple)| *simple == name)
                .map(|(_, simple)| *simple)
        }
        _ => None,
    }
}
fn annotation_argument_values(annotation: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let Some(arguments) = kt::child(annotation, "constructor_invocation")
        .and_then(|invocation| kt::child(invocation, "value_arguments"))
    else {
        return Vec::new();
    };
    arguments
        .children(&mut arguments.walk())
        .filter(|child| child.kind() == "value_argument")
        .map(|argument| source[argument.byte_range()].trim().to_string())
        .collect()
}

fn known_mapped_superclass_annotation(
    name: &str,
    file: &crate::workspace::SourceFile,
    workspace: &crate::workspace::SourceIndex,
) -> bool {
    const KNOWN: [&str; 2] = [
        "jakarta.persistence.MappedSuperclass",
        "javax.persistence.MappedSuperclass",
    ];
    if KNOWN.contains(&name) {
        return true;
    }
    let imported_as = file.imports.iter().any(|import| {
        let import = import.trim();
        let (path, alias) = import
            .split_once(" as ")
            .map_or((import, None), |(path, alias)| {
                (path.trim(), Some(alias.trim()))
            });
        KNOWN.contains(&path)
            && alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == name
    });
    if imported_as {
        return workspace.resolve_type(file, name).is_none_or(|resolved| {
            resolved.package.as_deref() == Some("jakarta.persistence")
                || resolved.package.as_deref() == Some("javax.persistence")
        });
    }
    if name != "MappedSuperclass" {
        return false;
    }
    if let Some(resolved) = workspace.resolve_type(file, name)
        && resolved.package.as_deref() != Some("jakarta.persistence")
        && resolved.package.as_deref() != Some("javax.persistence")
    {
        return false;
    }
    let wildcard = file.imports.iter().any(|import| {
        matches!(
            import.trim(),
            "jakarta.persistence.*" | "javax.persistence.*"
        )
    });
    let conflicting_explicit = file.imports.iter().any(|import| {
        let import = import.trim();
        if import.ends_with(".*") {
            return false;
        }
        let (path, alias) = import
            .split_once(" as ")
            .map_or((import, None), |(path, alias)| {
                (path.trim(), Some(alias.trim()))
            });
        let visible_name = alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path));
        visible_name == name && !KNOWN.contains(&path)
    });
    wildcard && !conflicting_explicit
}

fn known_jvm_overloads_annotation(
    name: &str,
    file: &crate::workspace::SourceFile,
    workspace: &crate::workspace::SourceIndex,
) -> bool {
    if name == "kotlin.jvm.JvmOverloads" {
        return true;
    }
    if name != "JvmOverloads" || workspace.resolve_type(file, name).is_some() {
        return false;
    }
    !file.imports.iter().any(|import| {
        let import = import.trim();
        let (path, alias) = import
            .split_once(" as ")
            .map_or((import, None), |(path, alias)| {
                (path.trim(), Some(alias.trim()))
            });
        alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == name
            && path != "kotlin.jvm.JvmOverloads"
            && !path.ends_with(".*")
    })
}

fn superclass_annotations_are_compatible(
    class: tree_sitter::Node<'_>,
    header: &str,
    source: &str,
    file: &crate::workspace::SourceFile,
    workspace: &crate::workspace::SourceIndex,
) -> bool {
    let class_modifiers = kt::child(class, "modifiers");
    let constructor = primary_constructor(class);
    let constructor_modifiers =
        constructor.and_then(|constructor| kt::child(constructor, "modifiers"));
    let mut annotations = Vec::new();
    let mut pending = vec![class];
    while let Some(node) = pending.pop() {
        if node.start_byte() >= class.start_byte() + header.len() {
            continue;
        }
        if node.kind() == "annotation" {
            annotations.push(node);
            continue;
        }
        if node.kind() == "class_body" {
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    // A recovery node that hides an annotation from this traversal cannot be
    // used as evidence that a superclass annotation is harmless.
    if header.matches('@').count() != annotations.len() {
        return false;
    }
    for annotation in annotations {
        let Some(name) = annotation_fq_name(annotation, source) else {
            return false;
        };
        let class_annotation =
            class_modifiers.is_some_and(|modifiers| is_descendant_of(annotation, modifiers));
        let constructor_annotation =
            constructor_modifiers.is_some_and(|modifiers| is_descendant_of(annotation, modifiers));
        let safe_class_annotation =
            class_annotation && known_mapped_superclass_annotation(&name, file, workspace);
        let safe_constructor_annotation =
            constructor_annotation && known_jvm_overloads_annotation(&name, file, workspace);
        if !safe_class_annotation && !safe_constructor_annotation {
            return false;
        }
    }
    true
}

fn constructor_has_jvm_overloads(
    constructor: tree_sitter::Node<'_>,
    source: &str,
    file: &crate::workspace::SourceFile,
    workspace: &crate::workspace::SourceIndex,
) -> bool {
    let Some(modifiers) = kt::child(constructor, "modifiers") else {
        return false;
    };
    let mut pending = vec![modifiers];
    while let Some(node) = pending.pop() {
        if node.kind() == "annotation" {
            if annotation_fq_name(node, source)
                .is_some_and(|name| known_jvm_overloads_annotation(&name, file, workspace))
            {
                return true;
            }
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

/// One companion constant group: the enum member name, its DECLARED Java type
/// (`Set<Owner>` / `Collection<Owner>`) and the entries in source order.
struct EnumGroup {
    name: String,
    declared: String,
    element: String,
    entries: Vec<String>,
}

fn is_collection_base(base: &str) -> bool {
    matches!(
        base,
        "Set"
            | "kotlin.collections.Set"
            | "java.util.Set"
            | "Collection"
            | "kotlin.collections.Collection"
            | "java.util.Collection"
    )
}

/// `Base<A, B>` -> (`Base`, [`A`, `B`]); `Base` -> (`Base`, []). Only the
/// top-level argument list is split, so nested generics stay one argument.
fn split_generic(text: &str) -> Option<(String, Vec<String>)> {
    let text = text.trim().trim_end_matches('?');
    let Some((base, rest)) = text.split_once('<') else {
        return Some((text.trim().to_string(), Vec::new()));
    };
    let inner = rest.strip_suffix('>')?;
    let mut args = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for ch in inner.chars() {
        match ch {
            '<' => {
                depth += 1;
                current.push(ch);
            }
            '>' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                args.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        args.push(current.trim().to_string());
    }
    Some((base.trim().to_string(), args))
}

fn collect_kind<'t>(node: tree_sitter::Node<'t>, kind: &str, out: &mut Vec<tree_sitter::Node<'t>>) {
    if node.kind() == kind {
        out.push(node);
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_kind(child, kind, out);
    }
}

fn count_kind(node: tree_sitter::Node, kind: &str) -> usize {
    let mut found = Vec::new();
    collect_kind(node, kind, &mut found);
    found.len()
}

fn java_enum_companion_reference(source: &str, owner: &str, getters: &[String]) -> bool {
    if !source.contains(owner) || !source.contains("Companion") {
        return false;
    }
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .is_err()
    {
        return true;
    }
    let Some(tree) = parser.parse(source, None) else {
        return true;
    };
    fn visit(node: tree_sitter::Node<'_>, source: &str, owner: &str, getters: &[String]) -> bool {
        if matches!(
            node.kind(),
            "field_access" | "scoped_identifier" | "scoped_type_identifier"
        ) {
            fn names<'a>(node: tree_sitter::Node<'_>, source: &'a str, out: &mut Vec<&'a str>) {
                if matches!(node.kind(), "identifier" | "type_identifier") {
                    out.push(&source[node.byte_range()]);
                    return;
                }
                let mut c = node.walk();
                for child in node.named_children(&mut c) {
                    names(child, source, out);
                }
            }
            let mut identifiers = Vec::new();
            names(node, source, &mut identifiers);
            if identifiers
                .windows(2)
                .any(|pair| pair == [owner, "Companion"])
            {
                let supported_getter = node.parent().is_some_and(|call| {
                    call.kind() == "method_invocation"
                        && call.child_by_field_name("name").is_some_and(|name| {
                            getters
                                .iter()
                                .any(|getter| &source[name.byte_range()] == getter)
                        })
                        && call
                            .child_by_field_name("arguments")
                            .is_some_and(|args| args.named_child_count() == 0)
                });
                if !supported_getter {
                    return true;
                }
            }
        }
        let mut c = node.walk();
        node.named_children(&mut c)
            .any(|child| visit(child, source, owner, getters))
    }
    visit(tree.root_node(), source, owner, getters)
}

fn enum_group_consumer_requires_companion(source: &str, owner: &str) -> bool {
    if !source.contains(owner) {
        return false;
    }
    let tree = crate::transpiler::parse_tree(source);
    fn visit(node: tree_sitter::Node<'_>, source: &str, owner: &str) -> bool {
        let text = |n: tree_sitter::Node<'_>| &source[n.byte_range()];
        if node.kind() == "class_declaration"
            && kt::child(node, "identifier").is_some_and(|name| text(name) == owner)
            && text(node).contains("enum class")
        {
            return false;
        }
        if node.kind() == "identifier" && text(node).trim_matches('`') == owner {
            let mut current = node.parent();
            while let Some(parent) = current {
                match parent.kind() {
                    "import" => {
                        return crate::semantics::java_member_calls::import_header(source, parent)
                            .is_none_or(|header| {
                                header.alias.is_some() || !header.path.ends_with(owner)
                            });
                    }
                    "type_alias" => return true,
                    "user_type" => {
                        return text(parent)
                            .split('.')
                            .any(|part| part.trim() == "Companion");
                    }
                    "navigation_expression" => {
                        return text(parent)
                            .split('.')
                            .any(|part| part.trim() == "Companion");
                    }
                    "class_literal" => return false,
                    "callable_reference" if text(parent).contains("::class") => return false,
                    "property_declaration"
                    | "function_declaration"
                    | "call_expression"
                    | "value_argument" => return true,
                    _ => {}
                }
                current = parent.parent();
            }
            return true;
        }
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .any(|child| visit(child, source, owner))
    }
    visit(tree.root_node(), source, owner)
}

/// One companion fn captured for the nested `Companion` bridge.
struct BridgeSig {
    signature: String,
    call: String,
    returns_void: bool,
}

impl<'src, 'tree> Unit<'src, 'tree> {
    fn declaration_is_enum(&self, decl: tree_sitter::Node<'tree>) -> bool {
        let Some(modifiers) = kt::child(decl, "modifiers") else {
            return false;
        };
        modifiers
            .children(&mut modifiers.walk())
            .filter(|modifier| modifier.kind() == "class_modifier")
            .flat_map(|modifier| modifier.children(&mut modifier.walk()).collect::<Vec<_>>())
            .any(|token| self.text(token) == "enum")
    }

    fn declaration_references_kclass(&self, decl: tree_sitter::Node<'tree>) -> bool {
        let mut pending = vec![decl];
        while let Some(node) = pending.pop() {
            if node.kind() == "user_type"
                && self
                    .text(node)
                    .split(|character: char| !character.is_alphanumeric() && character != '_')
                    .any(|part| part == "KClass")
            {
                return true;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        false
    }

    pub(crate) fn collect_type_relations(&mut self, root: tree_sitter::Node<'tree>) {
        let mut stack: Vec<tree_sitter::Node<'tree>> = vec![root];
        while let Some(n) = stack.pop() {
            for c in n.children(&mut n.walk()) {
                stack.push(c);
            }
            // Top-level function names for `is_untranslated_file_function`:
            // a translated declaration must not emit a bare call to a file
            // function that remained Kotlin.
            if n.kind() == "function_declaration"
                && n.parent().is_some_and(|p| p.kind() == "source_file")
                && let Some(nm) = kt::field(n, "name")
            {
                let fname = self.text(nm).to_string();
                // A top-level function referenced by retained Kotlin source
                // will itself remain Kotlin (the loose-decl pass runs after
                // the type loop, too late for callee checks) — pre-mark it so
                // translated bodies taint instead of emitting bare calls.
                let retained = self.workspace_requires_top_level_retention(&fname);
                if retained {
                    self.retained_file_functions.insert(fname.clone());
                }
                self.top_level_functions.insert(fname);
            }
            if n.kind() != "class_declaration" {
                continue;
            }
            let Some(nm) = kt::field(n, "name") else {
                continue;
            };
            let tname = self.text(nm).to_string();
            let is_sealed = kt::child(n, "modifiers")
                .map(|m| self.text(m).contains("sealed"))
                .unwrap_or(false);
            if is_sealed {
                self.sealed_types.insert(tname.clone());
            }
            // Data classes: capture primary-constructor params as record
            // components for destructuring-site extraction (`val (a, b) = p`).
            let is_data = kt::child(n, "modifiers")
                .map(|m| self.text(m).contains("data"))
                .unwrap_or(false);
            if is_data && let Some(pvs) = kt::child(n, "primary_constructor") {
                let mut comps: Vec<(String, String)> = Vec::new();
                // Structure: primary_constructor > class_parameters > class_parameter
                let mut pvs_cur = pvs.walk();
                let cparams = pvs
                    .children(&mut pvs_cur)
                    .find(|c| c.kind() == "class_parameters");
                let params: Vec<tree_sitter::Node> = cparams
                    .map(|cp| {
                        cp.children(&mut cp.walk())
                            .filter(|c| c.is_named() && c.kind() == "class_parameter")
                            .collect()
                    })
                    .unwrap_or_default();
                for pm in params {
                    let mut pm_cur = pm.walk();
                    let named: Vec<tree_sitter::Node> =
                        pm.children(&mut pm_cur).filter(|c| c.is_named()).collect();
                    let pname = named
                        .iter()
                        .find(|c| c.kind() == "identifier")
                        .map(|x| self.text(*x).to_string())
                        .unwrap_or_default();
                    let ptype = named
                        .iter()
                        .find(|c| c.kind() == "user_type" || c.kind().ends_with("_type"))
                        .map(|t| kt::java_type(*t, self.source))
                        .unwrap_or_else(|| "Object".to_string());
                    if !pname.is_empty() {
                        comps.push((ptype, pname));
                    }
                }
                if !comps.is_empty() {
                    self.data_components.insert(tname.clone(), comps);
                    if self.data_class_can_be_record(n) {
                        self.record_types.insert(tname.clone());
                    }
                }
            }
            if let Some(sup) = self.superclass_name(n) {
                self.subclass_map.entry(sup).or_default().push(tname);
            }
        }
    }

    fn superclass_name(&self, decl: tree_sitter::Node<'tree>) -> Option<String> {
        let dc = kt::child(decl, "delegation_specifiers")?;
        let spec = dc
            .children(&mut dc.walk())
            .find(|s| s.kind() == "delegation_specifier")?;
        let ci = spec
            .children(&mut spec.walk())
            .find(|c| c.kind() == "constructor_invocation")?;
        let ut = ci
            .children(&mut ci.walk())
            .find(|c| c.kind() == "user_type")?;
        let mut last: Option<tree_sitter::Node<'tree>> = None;
        for ch in ut.children(&mut ut.walk()) {
            if ch.kind() == "identifier" {
                last = Some(ch);
            }
        }
        last.map(|n| self.text(n).to_string())
    }

    /// Records allow interfaces and delegating constructors, but no mutable
    /// components, class superclass or additional instance fields.
    fn data_class_can_be_record(&self, decl: tree_sitter::Node) -> bool {
        let constructor_parameters = kt::child(decl, "primary_constructor")
            .and_then(|ctor| kt::child(ctor, "class_parameters"))
            .map(|params| {
                params
                    .children(&mut params.walk())
                    .filter(|param| param.kind() == "class_parameter")
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mutable = constructor_parameters
            .iter()
            .any(|param| kt::child(*param, "var").is_some());
        // A Kotlin constructor-property annotation can target the backing
        // field, getter, or constructor parameter depending on its @Target.
        // A Java record component cannot reproduce that placement reliably,
        // so keep an ordinary field-backed class and use the existing
        // annotation routing for each generated member.
        let annotated = constructor_parameters.iter().any(|param| {
            let mut pending = vec![*param];
            while let Some(node) = pending.pop() {
                if node.kind() == "annotation" {
                    return true;
                }
                pending.extend(node.children(&mut node.walk()));
            }
            false
        });
        // Record component types also define their accessor return types and
        // canonical constructor parameters. Kotlin's covariant read-only
        // collection input needs a wildcard parameter but an invariant getter,
        // so this shape requires an ordinary class with a separate field.
        let has_covariant_collection = constructor_parameters.iter().any(|param| {
            let type_node =
                kt::child(*param, "user_type").or_else(|| kt::child(*param, "nullable_type"));
            let Some(type_node) = type_node else {
                return false;
            };
            let raw = self
                .text(type_node)
                .trim()
                .trim_start_matches("@Nullable ")
                .trim_end_matches('?')
                .split('<')
                .next()
                .unwrap_or_default()
                .rsplit('.')
                .next()
                .unwrap_or_default();
            matches!(
                raw,
                "List" | "Set" | "Collection" | "Iterable" | "Iterator" | "Sequence" | "Map"
            )
        });
        !mutable
            && !annotated
            && !has_covariant_collection
            && self.superclass_name(decl).is_none()
            && !kt::child(decl, "class_body").is_some_and(|body| {
                body.children(&mut body.walk()).any(|member| {
                    member.kind() == "property_declaration"
                        && self.property_emits_backing_field(member)
                })
            })
    }

    fn decl_contains(&self, decl: tree_sitter::Node<'tree>, name: &str) -> bool {
        let mut stack: Vec<tree_sitter::Node<'tree>> = vec![decl];
        while let Some(n) = stack.pop() {
            for c in n.children(&mut n.walk()) {
                stack.push(c);
            }
            if n.id() == decl.id()
                || !matches!(n.kind(), "class_declaration" | "object_declaration")
            {
                continue;
            }
            if let Some(nm) = kt::field(n, "name")
                && self.text(nm) == name
            {
                return true;
            }
        }
        false
    }

    pub(crate) fn transpile_type_decl_set(
        &mut self,
        out: &mut JavaOut,
        package: &str,
        imports: &[String],
        f: impl FnOnce(&mut Self, &mut JavaOut),
    ) {
        // Provenance header: every generated .java records which .kt produced
        // it (in-place migration trims the .kt, so the pair must stay matchable).
        // Forward slashes only: a raw backslash inside a Java comment is not a
        // unicode escape today, but `\c`-style prefixes would be rejected as
        // illegal escapes by javac on any path containing one.
        out.line(format!(
            "// NOTLIN: generated from {}",
            normalized_source_path(self.file)
        ));
        if !package.is_empty() {
            out.line(format!("package {};", package));
            out.blank();
        }
        let declaring_file = self.workspace_file.as_deref().unwrap_or(self.file);
        let own_names: Vec<String> = self
            .workspace
            .map(|workspace| workspace.decl_names_in_file(declaring_file))
            .unwrap_or_default();
        let import_anchor = out.buf.len();
        f(self, out);
        // Take the body out before anything rewrites the buffer as a whole.
        // `lower_visible_typealiases` substitutes every occurrence of an alias
        // name, and the generated header carries the source path (`// NOTLIN:
        // generated from …/ktor/src/…`), where an alias spelled like a path
        // fragment (`io`, `ktor`) matches and is replaced by a shorter target.
        // The buffer then ends up shorter than the anchor and slicing panics.
        // The header is a generated comment plus the package name, so nothing
        // there needs lowering anyway.
        let body = out.buf[import_anchor..].to_string();
        let mut lowered = body.clone();
        self.lower_visible_typealiases(&mut lowered);
        if let Err(error) = self.lower_proven_map_typealiases(&mut lowered) {
            self.preparation_failure.get_or_insert(error);
        }
        let body = lowered;
        // Imports are chosen AFTER the body exists: a source file can be split
        // (some declarations translate, others stay Kotlin), and the retained
        // Kotlin keeps its own `import com.example.KotlinOnlyFn` lines. Carrying
        // such an import into the generated Java is a hard javac error
        // ("cannot find symbol: class KotlinOnlyFn") even though the generated
        // code never mentions it. Emit only imports this Java actually uses.
        // Kotlin resolves a simple name to its import; Java gives the class's
        // OWN name priority inside its own body. So `class Length` with
        // `import javax.measure.quantity.Length` and a member typed
        // `Unit<Length>` (bound: `Unit<Q : Quantity<Q>>`) compiles as Kotlin
        // but javac rejects the type argument after migration — it reads the
        // enclosing `Length`. Qualify the colliding name with its imported
        // FQN inside type-argument lists, where the two resolutions differ.
        let qualified = qualify_shadowing_type_arguments(&body, &own_names, imports);
        // Always rewrite the body region: the alias lowering above already
        // produced a replacement string, so the buffer's copy is stale.
        out.buf.truncate(import_anchor);
        out.buf.push_str(&qualified);
        let body = qualified;
        let mut block = String::new();
        for imp in imports {
            if !imp.is_empty() {
                // Kotlin file-level declarations shadow single imports of the
                // same simple name (`class Length` + `import …quantity.Length`
                // is legal Kotlin, file decl wins) — javac rejects the import
                // with "X is already defined in this compilation unit". Drop
                // the colliding import: the same-file declaration wins every
                // plain-name use site (Kotlin resolution semantics).
                let simple = imp.rsplit('.').next().unwrap_or(imp);
                let collides = imports.iter().any(|other| {
                    other.rsplit('.').next() == Some(simple) && other.rsplit('.').count() == 1
                }) || own_names.iter().any(|n| n == simple);
                if collides || !import_is_referenced(imp, &body) {
                    continue;
                }
                if imp == "com.fasterxml.jackson.module.kotlin.jacksonObjectMapper" {
                    block.push_str(
                        "import static com.fasterxml.jackson.module.kotlin.ExtensionsKt.jacksonObjectMapper;\n",
                    );
                } else {
                    block.push_str(&format!("import {};\n", imp));
                }
            }
        }
        // A proved constant-field type is bound by a single-type import: the
        // source spelling it replaces was ambiguous under a wildcard import,
        // and a single-type import shadows type-import-on-demand declarations.
        // Nothing is added for a file whose plan proved no such field, so the
        // helper-free import block stays byte-identical.
        for want in &self.proved_field_imports {
            if imports.iter().any(|existing| existing == want) {
                continue;
            }
            block.push_str(&format!("import {};\n", want));
        }
        // Under --lombok the emitted @Data/@AllArgsConstructor need their
        // imports; user-declared lombok imports (Lombok-flagged source) may
        // already provide some — add only what's missing, exactly once.
        if self.lombok {
            for want in [
                "lombok.Data",
                "lombok.Getter",
                "lombok.Setter",
                "lombok.Value",
                "lombok.NonNull",
                "lombok.AllArgsConstructor",
                "lombok.EqualsAndHashCode",
            ] {
                if !imports.iter().any(|i| i == want) {
                    block.push_str(&format!("import {};\n", want));
                }
            }
        }
        // The generated body uses ArrayList/HashMap/HashSet/List/Map/Set from
        // stdlib collections; java.util.* covers them all in one line.
        block.push_str("import java.util.*;\n");
        block.push_str("import java.util.stream.Stream;\n");
        block.push('\n');
        if let Some(pkg) = crate::transpiler::types::nullable_import(self.annots) {
            // Generated nullability uses only this short annotation name;
            // non-null annotations are fully qualified. An unnecessary star
            // import makes later planning snapshots' type identities unknown.
            block.push_str(&format!("import {}.Nullable;\n", pkg));
            block.push('\n');
        }
        out.buf.insert_str(import_anchor, &block);
    }

    /// Lower one declaration annotation to Java text, or None when the
    /// annotation must stay in Kotlin. Java-native pass-through rules:
    ///   - the annotation NAME resolves to a pre-existing Java declaration in
    ///     the workspace index, OR the workspace is absent/unresolvable
    ///     (single-file probes; the target build proves the classpath) —
    ///   - the ARGUMENTS contain no Kotlin-only syntax: unresolved `::class`
    ///     references, string templates, lambdas, or named-value
    ///     forms the annotation node renders with Kotlin spellings.
    ///
    /// The use-site target (`@get:` / `@field:`) is dropped: Java annotates
    /// the element itself, and notlin's primary-constructor properties are
    /// fields with Lombok accessors.
    pub(crate) fn transpile_declaration_annotation(
        &self,
        node: tree_sitter::Node,
    ) -> Option<String> {
        // Wrapper shape: an annotated_expression node (grammar quirk) carries
        // the annotation in a child plus, sometimes, a sibling
        // parenthesized_expression holding the argument list
        // (`@JsonSubTypes(...)`); when the arguments live INSIDE the
        // annotation's own constructor_invocation (`@JsonTypeInfo(use = ...)`)
        // the sibling is absent. Descend to the annotation for the parts and
        // prefer the in-node argument text.
        let (annotation_node, argument_node) = if node.kind() == "annotated_expression" {
            let annotation = kt::child(node, "annotation");
            let args = kt::child(node, "parenthesized_expression");
            let in_node_args = annotation
                .and_then(|a| kt::child(a, "constructor_invocation"))
                .and_then(|a| kt::child(a, "value_arguments"));
            (annotation, in_node_args.or(args))
        } else {
            (
                Some(node),
                kt::child(node, "constructor_invocation")
                    .and_then(|a| kt::child(a, "value_arguments")),
            )
        };
        let argument_text = argument_node
            .map(|arguments| annotation_arguments_without_comments(arguments, self.source))
            .unwrap_or_default();
        let annotation_node = annotation_node?;
        // The annotation name: the `constructor_invocation`'s `user_type`
        // (handles `com.example.Mapping`); a bare `user_type` directly under
        // the annotation node (no arguments, e.g. `@NotNull`) is the
        // fallback. The use-site target child is skipped by kind.
        let invocation_user_type = kt::child(annotation_node, "constructor_invocation")
            .and_then(|a| kt::child(a, "user_type"));
        let direct_user_type = annotation_node
            .children(&mut annotation_node.walk())
            .find(|c| matches!(c.kind(), "user_type" | "identifier"));
        let name = invocation_user_type
            .or(direct_user_type)
            .map(|c| self.text(c).to_string())?;
        // Java has no import aliases. Spell an aliased annotation's complete
        // name so the alias cannot become an unresolved annotation or bind to
        // a different type with the same simple name.
        let name = annotation_import_name(name, self.source);
        // Kotlin's Suppress is SOURCE-retained compiler metadata, not a Java
        // annotation. Drop it only with an unambiguous Kotlin binding; never
        // erase a user annotation just because its short name matches.
        if self.kotlin_builtin_annotation_binding(node, "kotlin.Suppress") == Some(true) {
            return Some(String::new());
        }
        if self.kotlin_builtin_annotation_binding(node, "kotlin.Deprecated") == Some(true) {
            return Some("@java.lang.Deprecated".to_string());
        }
        // Kotlin-only argument shapes taint immediately — except a
        // `Name::class` class literal, which rewrites to `Name.class` in Java
        // when `Name` resolves to a Java-visible declaration (checked below);
        // an unknown or Kotlin-only name keeps the taint.
        if argument_text.contains("${")
            || argument_text.contains('{')
            || argument_text.contains("-> ")
        {
            return None;
        }
        if argument_text.contains("::class") {
            let mut lowered =
                lower_annotation_arrays(&lower_annotation_raw_strings(&argument_text));
            let class_literals = annotation_class_literal_names(node, self.source);
            if class_literals.is_empty()
                || class_literals.len() != argument_text.matches("::class").count()
            {
                return None;
            }
            for class_name in class_literals {
                let java_class = crate::semantics::annotation_reference_class(
                    self.source,
                    self.workspace_file.as_deref().unwrap_or(self.file),
                    node,
                    &class_name,
                    self.semantic_provider,
                )?;
                lowered = lowered.replace(
                    &format!("{class_name}::class"),
                    &format!("{java_class}.class"),
                );
            }
            if lowered.contains("::class") {
                return None;
            }
            let lowered = lower_known_annotation_positional_argument(&name, &lowered);
            return Some(
                format!(
                    "@{name}{}",
                    brace_wrap_unnamed_nested_annotations(&prefix_nested_annotations(&lowered))
                )
                .trim_end()
                .to_string(),
            );
        }
        // Kotlin annotation classes compile to JVM annotation interfaces before
        // javac runs, so Java emitted in the same module can reference them even
        // when their Kotlin declarations remain outside this translation set.
        // Rebuild from the parts: a use-site target (`@get:X(...)` -> `@X(...)`)
        // is dropped and the arguments ride on the bare name; the wrapper
        // shape (annotated_expression) holds its arguments separately from
        // the annotation node, so `text` is not usable there.
        // Both shapes pass ONLY the argument list (never the annotation name)
        // to the transforms: brace-wrapping the name would move it inside the
        // braces.
        let lowered = lower_annotation_arrays(&lower_annotation_raw_strings(&argument_text));
        let lowered = lower_known_annotation_positional_argument(&name, &lowered);
        let arguments = brace_wrap_unnamed_nested_annotations(&prefix_nested_annotations(&lowered));
        let rebuilt = format!("@{name}{arguments}");
        // Kotlin wildcard/star imports and `!` nullability assertions never
        // appear here (parsed as value arguments), but a trailing semicolon
        // or whitespace from multi-annotation lines would break Java.
        Some(rebuilt.trim_end().to_string())
    }

    /// Some(true) proves a builtin identity; None means a wildcard import
    /// could bind the short spelling to a different annotation.
    fn kotlin_builtin_annotation_binding(
        &self,
        annotation: tree_sitter::Node<'_>,
        canonical: &str,
    ) -> Option<bool> {
        let Some(name) = annotation_fq_name(annotation, self.source) else {
            return Some(false);
        };
        let name = annotation_import_name(name, self.source);
        if name == canonical {
            return Some(true);
        }
        let simple = canonical.rsplit('.').next()?;
        if name != simple {
            return Some(false);
        }
        let file = self.workspace.and_then(|index| {
            index.source_file(self.workspace_file.as_deref().unwrap_or(self.file))
        });
        if annotation_type_is_local(self.workspace, file, simple) {
            return Some(false);
        }
        let imports = annotation_imports(self.source).collect::<Vec<_>>();
        if imports.iter().any(|(path, alias)| {
            alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == simple
        }) {
            return Some(false);
        }
        if imports
            .iter()
            .any(|(path, _)| path.ends_with(".*") && *path != "kotlin.*")
        {
            return None;
        }
        Some(true)
    }

    pub(crate) fn check_builtin_annotation_contracts(&mut self, decl: tree_sitter::Node<'_>) {
        let mut nodes = vec![decl];
        // Parser-recovered annotation wrappers can be siblings of the owning
        // declaration rather than children of its modifiers.
        let mut previous = decl.prev_named_sibling();
        while let Some(wrapper) = previous.filter(|node| node.kind() == "annotated_expression") {
            nodes.push(wrapper);
            previous = wrapper.prev_named_sibling();
        }
        while let Some(node) = nodes.pop() {
            if node.kind() == "annotation" {
                for canonical in ["kotlin.Suppress", "kotlin.Deprecated"] {
                    match self.kotlin_builtin_annotation_binding(node, canonical) {
                        None => self.diag_untranslatable(node, format!(
                            "{} annotation binding is uncertain under wildcard imports; retained rather than guessing annotation identity",
                            canonical.rsplit('.').next().unwrap_or(canonical)
                        )),
                        Some(true) if canonical == "kotlin.Deprecated" => {
                            let message = "Kotlin deprecation metadata (message, replacement, and level) dropped; emitted Java @Deprecated";
                            self.diags.warn_approx(node, self.file, message);
                            self.diag_approx(node, message);
                        }
                        _ => {}
                    }
                }
            }
            nodes.extend(node.named_children(&mut node.walk()));
        }
    }

    /// Why this declaration cannot be translated away, or `None` when it can.
    /// The reason is part of the diagnostic so a run log is greppable by
    /// cause, not just by "retained".
    ///
    /// The pair is (blocker KIND, params): the kind is one of the fixed
    /// vocabulary entries in `RetentionKind` — it yields the stable code and
    /// the summary row — and params instantiate that kind's template with the
    /// specific element (the conflicting members, ...). The second half is the
    /// BLOCKERS: the retained declarations that have to translate first for
    /// this one to follow. Empty means the blocker is intrinsic — a fact about
    /// the source no other translation can clear — which is what makes it a
    /// root cause in the run-end report; a non-empty list makes this
    /// declaration fallout, to be credited to the roots behind those names
    /// rather than to a human.
    fn kotlin_retention_reason(
        &self,
        name: &str,
        decl: tree_sitter::Node<'_>,
    ) -> Option<(RetentionKind, Vec<String>, Vec<String>)> {
        // Returns `(kind, params, blockers)`: `params` instantiate the kind's
        // detail template (the specific blocked element a human has to look at),
        // `blockers` name the retained declarations this one waits on.
        let workspace = self.workspace?;
        let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
        let source_file = workspace.source_file(indexed_path)?;
        let target = source_file
            .declarations
            .iter()
            .find(|declaration| declaration.name == name)?;
        // A memberless interface adds no ABI of its own, so retained Kotlin
        // descendants can keep implementing it after it becomes Java.  This
        // also covers typed aliases such as `AgentEvent : IObjectEvent<...>`.
        // Retained Kotlin interface parents can keep their ABI and metadata;
        // a Java interface with no members only forwards that existing ABI.
        let memberless_interface = self.retained_hint.is_some()
            && target.kind == crate::workspace::DeclarationKind::Interface
            && target.members.iter().all(|member| member.is_static);
        let property_bridge_result = self.retained_hint.as_ref().and_then(|retained| {
            crate::property_abi::is_property_interface_candidate(target).then(|| {
                crate::property_abi::retained_subtypes_bridge(
                    workspace,
                    target,
                    retained,
                    self.translation_roots,
                )
            })
        });
        let property_interface_bridge = property_bridge_result.as_ref().is_some_and(Result::is_ok);
        let method_interface = self.retained_hint.is_some()
            && (crate::property_abi::is_method_interface_candidate(target)
                || crate::property_abi::is_accessor_method_interface_candidate(workspace, target));
        let safe_for_retained_subtypes =
            memberless_interface || property_interface_bridge || method_interface;
        if hierarchy_closure()
            && !safe_for_retained_subtypes
            && workspace.has_unselected_kotlin_subtype(target, self.translation_roots)
        {
            return Some((
                RetentionKind::SubtypeOutsideTranslationSet,
                Vec::new(),
                Vec::new(),
            ));
        }
        // Subtype rule, two modes:
        // - No fixpoint hint (single-file mode): retain on ANY Kotlin
        //   subtype - conservative, cannot know what translates later.
        // - With hint (fixpoint pass): retain only when a Kotlin subtype
        //   is ITSELF retained for an intrinsic reason; a retained
        //   Kotlin implementor cannot implement a translated-away
        //   supertype (enum entries ABI, KClass...). Monotone: seeds
        //   (intrinsically tainted decls) never shrink, so iteration
        //   reaches the least fixpoint.
        if hierarchy_closure()
            && !safe_for_retained_subtypes
            && target.kind == crate::workspace::DeclarationKind::Interface
        {
            let blockers = self.retained_subtypes(workspace, target);
            let retains = match self.retained_hint.as_ref() {
                Some(_) => !blockers.is_empty(),
                None => workspace.has_kotlin_subtype(target),
            };
            if retains {
                if let Some(Err(failure)) = property_bridge_result {
                    let failure_is_root = failure.declaration == target.name;
                    return Some((
                        RetentionKind::PropertyInterfaceBridge,
                        vec![failure.declaration.clone(), failure.reason],
                        if failure_is_root {
                            Vec::new()
                        } else {
                            vec![failure.declaration]
                        },
                    ));
                }
                return Some((
                    RetentionKind::InterfaceSubtypeRetained,
                    Vec::new(),
                    blockers,
                ));
            }
        }
        // Closed hierarchy, downward half: a retained Kotlin declaration must
        // not inherit from a supertype that was translated away. Only a Kotlin
        // supertype can supply what kotlinc needs from it — the JPA no-arg
        // plugin synthesizes the subclass's `super()` call from the
        // supertype's *default parameter values*, which a Java constructor
        // does not have ("No noarg super constructor"), and InterfaceLowering
        // resolves an inherited member as a real override against a Kotlin
        // declaration, not against a Java default method.
        if hierarchy_closure()
            && !safe_for_retained_subtypes
            && self
                .retained_hint
                .as_ref()
                .is_some_and(|retained| workspace.has_retained_kotlin_subtype(target, retained))
        {
            return Some((
                RetentionKind::RetainedInheritor,
                Vec::new(),
                self.retained_subtypes(workspace, target),
            ));
        }
        // Constructor default arguments (N87CB). A default on a middle
        // parameter is not a blocker by itself — what decides is whether a caller
        // omits one in a shape Java can be given. Java expresses an omission by
        // writing a language-neutral literal into the call, or by a delegating
        // overload for the exact pattern a call site uses; only a pattern the
        // emitter cannot write keeps the declaration in Kotlin.
        if target.has_default_constructor_parameter {
            let evidence = workspace.ctor_omission_evidence(target);
            if let Some(reason) = self.ctor_default_plan(decl, target).blocked {
                return Some((
                    RetentionKind::MiddleDefaultParameter,
                    vec![reason],
                    Vec::new(),
                ));
            }
            if let Some(site) = evidence.unresolvable.first() {
                return Some((
                    RetentionKind::MiddleDefaultParameter,
                    vec![format!(
                        "a caller's argument shape could not be read at {site}"
                    )],
                    Vec::new(),
                ));
            }
            if !self.in_place && evidence.named_callers {
                return Some((
                    RetentionKind::MiddleDefaultParameter,
                    vec![
                        "a caller names arguments and this run rewrites no retained Kotlin"
                            .to_string(),
                    ],
                    Vec::new(),
                ));
            }
        }
        // A translated Java object singleton cannot be referenced by
        // plain name from residual Kotlin (no companion object), so an
        // object referenced from a retained Kotlin file has to stay.
        if target.kind == crate::workspace::DeclarationKind::Object
            && self.retained_hint.as_ref().is_some_and(|retained| {
                workspace.has_retained_kotlin_reference(indexed_path, name, retained)
            })
        {
            return Some((
                RetentionKind::ReferencedFromRetainedKotlin,
                Vec::new(),
                self.retained_hint
                    .as_ref()
                    .map(|retained| {
                        workspace.retained_kotlin_referencers(indexed_path, name, retained)
                    })
                    .unwrap_or_default(),
            ));
        }
        if let Some((property, inherited_type, implementation_type)) =
            workspace.nullable_property_narrowing_conflict(source_file, target)
        {
            return Some((
                RetentionKind::NullableNarrowing,
                vec![property, inherited_type, implementation_type],
                Vec::new(),
            ));
        }
        let retained_property_ancestor = self.retained_hint.as_ref().and_then(|retained| {
            crate::property_abi::retained_kotlin_property_ancestor(workspace, target, retained)
        });
        // Cache the exact retained interface identities whose physical
        // property getters this class has proven compatible. The same proof
        // serves both the earlier property-boundary check and the final
        // hierarchy-closure gate below.
        let proven_property_interfaces = if self.in_place {
            self.retained_hint.as_ref().and_then(|retained| {
                workspace.retained_property_implementation_interfaces_java_compatible(
                    source_file,
                    target,
                    self.translation_roots,
                    retained,
                )
            })
        } else {
            None
        };
        if self.in_place {
            let retained_property_boundary = retained_property_ancestor.is_some()
                || workspace.inherits_retained_kotlin_property_interface(
                    source_file,
                    target,
                    self.translation_roots,
                );
            let proven_java_property_contracts = proven_property_interfaces.is_some();
            if self.retained_hint.is_some()
                && retained_property_boundary
                && !proven_java_property_contracts
            {
                return Some((
                    RetentionKind::RetainedPropertyInterface,
                    retained_property_ancestor.into_iter().collect(),
                    self.retained_supertypes(workspace, target),
                ));
            }
        }
        // Retained Kotlin that smart-casts one of its properties does not have
        // to keep the owner in Kotlin: the rewrite pass binds the property read
        // into a local first, and a local `val` smart-casts freely. Only a site
        // the rewrite pass cannot repair (an unsupported shape, or a receiver
        // whose owner it cannot prove) keeps the owner here. Not in place, the
        // rewrite pass never runs, so nothing is repaired and the owner stays.
        if self.in_place && !workspace.smart_cast_rewrites_cover(indexed_path, target) {
            return Some((RetentionKind::SmartCast, Vec::new(), Vec::new()));
        }
        if hierarchy_closure()
            && !self.declaration_is_enum(decl)
            && self.retained_hint.as_ref().is_some_and(|retained| {
                target.supertypes.iter()
                    .filter_map(|spelling| workspace.resolve_type(source_file, spelling)
                        .map(|parent| (spelling, parent)))
                    .filter(|(_, parent)| {
                        parent.language == crate::workspace::SourceLanguage::Kotlin
                            && workspace.declaration_retained(parent, retained)
                    })
                    .any(|(spelling, parent)| {
                        let is_interface = parent.kind == crate::workspace::DeclarationKind::Interface;
                        let forwards_interface = memberless_interface && is_interface;
                        let proven_property_interface = is_interface
                            && proven_property_interfaces.as_ref().is_some_and(|interfaces| {
                                interfaces.contains(&crate::semantics::workspace_symbol(workspace, parent))
                            });
                        let proven_method_interface = self.semantic_provider
                            .filter(|_| target.kind == crate::workspace::DeclarationKind::Interface)
                            .is_some_and(|provider| {
                                crate::property_abi::retained_method_interface_parent_is_java_compatible(
                                    workspace, target, parent, retained, provider,
                                )
                            });
                        let proven_forwarding_interface = is_interface
                            && self.forwarding_bridge_covers_retained_interface(
                                workspace, target, parent,
                            );
                        !(forwards_interface
                            || proven_property_interface
                            || proven_method_interface
                            || proven_forwarding_interface)
                            && !self.retained_class_super_is_java_compatible(source_file, decl, spelling)
                    })
            })
        {
            return Some((
                RetentionKind::RetainedSupertype,
                Vec::new(),
                self.retained_supertypes(workspace, target),
            ));
        }
        None
    }

    /// A translated class may implement a retained Kotlin interface when the
    /// accepted plan re-creates every method obligation with proven
    /// source-forwarding bridges, and the class itself declares matching public
    /// readonly properties for every property obligation. The class then
    /// *implements* the interface's JVM obligations instead of inheriting them
    /// from Kotlin metadata, which makes the retained provider harmless.
    ///
    /// Any method without such a bridge (an extra default method, a `suspend`
    /// member, a renamed accessor) leaves the parent incompatible:
    /// whether such a member is abstract in the JVM interface depends on the
    /// retained provider's `-jvm-default` mode, which this run cannot know.
    fn forwarding_bridge_covers_retained_interface(
        &self,
        workspace: &crate::workspace::SourceIndex,
        target: &crate::workspace::Declaration,
        parent: &crate::workspace::Declaration,
    ) -> bool {
        self.semantic_provider.is_some_and(|provider| {
            workspace.concrete_getters_cover_retained_interface(
                target,
                parent,
                &self.default_getter_bridges,
                provider,
            )
        })
    }

    /// A JVM class with an explicit open/abstract boundary and a complete
    /// constructor call can remain Kotlin while its Java subclass migrates.
    /// Interface defaults, plugin-open classes and omitted Kotlin constructor
    /// arguments still require their existing compatibility rules.
    fn retained_class_super_is_java_compatible(
        &self,
        source_file: &crate::workspace::SourceFile,
        decl: tree_sitter::Node,
        name: &str,
    ) -> bool {
        let Some(workspace) = self.workspace else {
            return false;
        };
        let Some(supertype) = workspace.resolve_type(source_file, name) else {
            return false;
        };
        if supertype.kind != crate::workspace::DeclarationKind::Class {
            return false;
        }
        let Some(provider) = self.semantic_provider else {
            return false;
        };
        let id = crate::semantics::workspace_symbol(workspace, supertype);
        let Some(symbol) = provider
            .symbols_named(&id.name)
            .into_iter()
            .find(|symbol| symbol.id == id)
        else {
            return false;
        };
        let Some(snapshot) = workspace.source_file(&symbol.location.file) else {
            return false;
        };
        if snapshot.language != crate::workspace::SourceLanguage::Kotlin {
            return false;
        }
        let source = snapshot.source_text();
        let mut parser = tree_sitter::Parser::new();
        if parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .is_err()
        {
            return false;
        }
        let Some(tree) = parser.parse(source, None) else {
            return false;
        };
        let mut pending = vec![tree.root_node()];
        let mut super_node = None;
        while let Some(node) = pending.pop() {
            if node.kind() == "class_declaration"
                && node.start_byte() == symbol.location.start_byte
                && node.end_byte() == symbol.location.end_byte
            {
                super_node = Some(node);
                break;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        let Some(super_node) = super_node else {
            return false;
        };
        let header_end = kt::child(super_node, "class_body")
            .map(|body| body.start_byte())
            .unwrap_or(super_node.end_byte());
        let Some(header) = source.get(super_node.start_byte()..header_end) else {
            return false;
        };
        let explicitly_open_or_abstract =
            kt::child(super_node, "modifiers").is_some_and(|modifiers| {
                let mut pending = vec![modifiers];
                while let Some(node) = pending.pop() {
                    if matches!(node.kind(), "abstract" | "open") {
                        return true;
                    }
                    let mut cursor = node.walk();
                    pending.extend(node.children(&mut cursor));
                }
                false
            });
        if !explicitly_open_or_abstract
            || !superclass_annotations_are_compatible(
                super_node, header, source, snapshot, workspace,
            )
        {
            return false;
        }
        let parent_constructor = primary_constructor(super_node);
        let parent_signature = parent_constructor.map(|constructor| {
            crate::semantics::symbol_id_for_node(source, constructor, &snapshot.path)
        });
        let parent_defaults = parent_constructor
            .map(constructor_parameter_defaults)
            .unwrap_or_default();
        let parent_parameter_count = parent_signature
            .as_ref()
            .map(|signature| signature.parameters.len())
            .unwrap_or(0);
        if parent_parameter_count != supertype.constructor_param_count {
            return false;
        }
        let parent_has_jvm_overloads = parent_constructor.is_some_and(|constructor| {
            constructor_has_jvm_overloads(constructor, source, snapshot, workspace)
        });
        let child_constructor = primary_constructor(decl);
        let child_signature = child_constructor.map(|constructor| {
            crate::semantics::symbol_id_for_node(
                self.source,
                constructor,
                self.workspace_file.as_deref().unwrap_or(self.file),
            )
        });
        let child_names = child_constructor
            .map(|constructor| constructor_parameter_names(constructor, self.source))
            .unwrap_or_default();
        let Some(specifiers) = kt::child(decl, "delegation_specifiers") else {
            return false;
        };
        let mut cursor = specifiers.walk();
        specifiers
            .named_children(&mut cursor)
            .filter(|specifier| specifier.kind() == "delegation_specifier")
            .any(|specifier| {
                let raw = self.text(specifier).trim();
                if raw
                    .split(['(', '<'])
                    .next()
                    .unwrap_or(raw)
                    .trim()
                    .rsplit('.')
                    .next()
                    != Some(supertype.name.as_str())
                {
                    return false;
                }
                let arguments = kt::child(specifier, "constructor_invocation")
                    .and_then(|invocation| kt::child(invocation, "value_arguments"))
                    .map(|arguments| {
                        arguments
                            .named_children(&mut arguments.walk())
                            .filter(|argument| argument.kind() == "value_argument")
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let count = arguments.len();
                if count > parent_parameter_count {
                    return false;
                }
                if count < parent_parameter_count
                    && (!parent_has_jvm_overloads
                        || !parent_defaults
                            .get(count..parent_parameter_count)
                            .is_some_and(|suffix| suffix.iter().all(|is_default| *is_default)))
                {
                    return false;
                }
                let (Some(parent_signature), Some(child_signature)) =
                    (parent_signature.as_ref(), child_signature.as_ref())
                else {
                    return count == 0 && parent_parameter_count == 0;
                };
                if child_signature.parameters.len() != child_names.len()
                    || parent_signature.parameters.len() != parent_parameter_count
                {
                    return false;
                }
                arguments.iter().enumerate().all(|(index, argument)| {
                    let parameter = &child_names[index];
                    let argument_text = self.text(*argument).trim();
                    argument_text == parameter
                        && child_signature.parameters[index] == parent_signature.parameters[index]
                        && !parent_signature.parameters[index].starts_with("vararg:")
                })
            })
    }

    /// Retained Kotlin subtypes of `target`, by simple name — the blame edges
    /// behind the subtype-based retention reasons. Empty without a fixpoint
    /// hint: the run-end table is built from the hinted final pass, and the
    /// unhinted single-file mode has no retained set to attribute against.
    fn retained_subtypes(
        &self,
        workspace: &crate::workspace::SourceIndex,
        target: &crate::workspace::Declaration,
    ) -> Vec<String> {
        self.retained_hint
            .as_ref()
            .map(|retained| workspace.retained_kotlin_subtype_names(target, retained))
            .unwrap_or_default()
    }

    /// Retained Kotlin supertypes of `target`, by simple name — the blame edges
    /// behind the supertype-based retention reasons.
    fn retained_supertypes(
        &self,
        workspace: &crate::workspace::SourceIndex,
        target: &crate::workspace::Declaration,
    ) -> Vec<String> {
        self.retained_hint
            .as_ref()
            .map(|retained| workspace.retained_supertypes_of(target, retained))
            .unwrap_or_default()
    }

    /// A Kotlin companion `operator fun invoke` gives the enclosing type a
    /// class-call ABI (`Type(...)`) that Java cannot represent: Kotlin binds
    /// that syntax to a Java constructor after migration, never to a static
    /// factory. Only residual Kotlin callers make this a retention boundary.
    fn has_companion_operator_invoke(&self, decl: tree_sitter::Node) -> bool {
        let Some(body) = kt::child(decl, "class_body") else {
            return false;
        };
        let mut stack: Vec<tree_sitter::Node> = body
            .children(&mut body.walk())
            .filter(|node| node.kind() == "companion_object")
            .collect();
        while let Some(node) = stack.pop() {
            if node.kind() == "function_declaration"
                && kt::field(node, "name").is_some_and(|name| self.text(name) == "invoke")
                && self.text(node).contains("operator")
            {
                return true;
            }
            stack.extend(node.children(&mut node.walk()));
        }
        false
    }

    pub(crate) fn transpile_type_decl(&mut self, decl: tree_sitter::Node, out: &mut JavaOut) {
        if self.workspace.is_some_and(|workspace| {
            workspace.kotlin_files().any(|file| {
                file.declarations.iter().any(|candidate| {
                    candidate.kind == crate::workspace::DeclarationKind::Enum
                        && enum_group_consumer_requires_companion(
                            self.text(decl),
                            candidate.name.as_str(),
                        )
                })
            })
        }) {
            self.diag_untranslatable(
                decl,
                "enum companion object is used as a value; retain its consumer in Kotlin",
            );
            return;
        }
        self.check_builtin_annotation_contracts(decl);
        // `KClass<T>` type references lower to Java `Class<T>` (kt.rs /
        // types.rs interop mapping). That ABI change is only compatible
        // when no RESIDUAL Kotlin file consumes this declaration: a
        // retained Kotlin caller passing `X::class` (KClass) to the
        // translated Java `Class` overload no longer compiles, and
        // override/property type agreement breaks. The index proves it:
        // retention when residual Kotlin references the declaration,
        // translation when only Java (or nobody) does — Kotlin callers ad
        // apt via `X::class.java`, which is legal against a `Class` param.
        if self.declaration_references_kclass(decl) {
            let name = kt::field(decl, "name")
                .map(|n| self.text(n).to_string())
                .unwrap_or_default();
            let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
            if self
                .workspace
                .map(|w| w.has_external_kotlin_reference(indexed_path, &name))
                .unwrap_or(true)
            {
                self.diag_untranslatable(
                    decl,
                    "declaration references kotlin.reflect.KClass consumed by residual Kotlin; retained in Kotlin",
                );
                return;
            }
        }
        let mut is_data = false;
        let mut is_sealed = false;
        // Kotlin `open`: the class is subclassable, so the Java form must NOT
        // be final. `open` itself is not Java and must not be emitted.
        let mut is_open = false;
        let mut is_explicit_final = false;
        let mut is_enum = false;
        let mut is_annotation = false;
        let is_fun_interface = decl
            .children(&mut decl.walk())
            .any(|child| child.kind() == "fun");
        // Kotlin `interface` parses as class_declaration with an unnamed
        // `interface` keyword child.
        let is_interface = decl
            .children(&mut decl.walk())
            .any(|c| c.kind() == "interface");
        let mut modifiers = String::new();
        let mut annotations: Vec<String> = Vec::new();
        // Java-native annotations hoisted from a preceding top-level
        // annotated_expression wrapper (grammar quirk) ride first.
        let hoisted_annotations = self
            .hoisted_annotations
            .remove(&decl.id())
            .unwrap_or_default();
        annotations.extend(hoisted_annotations.iter().cloned());
        if let Some(mods) = kt::child(decl, "modifiers") {
            let mut cursor = mods.walk();
            for m in mods.children(&mut cursor) {
                match m.kind() {
                    "annotation" => {
                        if let Some(text) = self.transpile_declaration_annotation(m) {
                            annotations.push(text);
                        } else {
                            self.diag_untranslatable_coded(
                                m,
                                format!("declaration annotation is retained in Kotlin; annotation: `{}`; its type or argument expression cannot yet be lowered in this run", self.text(m)),
                                Some(crate::diagnostics::warning_code("declaration annotation is retained in Kotlin")),
                            );
                        }
                    }
                    "class_modifier" => {
                        let mut inner = m.walk();
                        for cm in m.children(&mut inner) {
                            match cm.kind() {
                                "data" => is_data = true,
                                "open" | "abstract" | "sealed" => {
                                    let word = self.text(cm).trim();
                                    match word {
                                        // `sealed`/`abstract` are Java words;
                                        // they are emitted as-is.
                                        "sealed" => is_sealed = true,
                                        // `open` has no Java keyword. Its Java
                                        // form is a class WITHOUT `final`
                                        // (see the final_kw rule below), so the
                                        // flag is recorded and the word is
                                        // never emitted — `open class` is not
                                        // Java.
                                        "open" => {
                                            is_open = true;
                                            continue;
                                        }
                                        _ => {}
                                    }
                                    // Guarded: the same keyword can arrive as
                                    // either node kind depending on the grammar,
                                    // and `abstract abstract class` is not Java.
                                    if !modifiers.contains(word) {
                                        modifiers.push_str(word);
                                        modifiers.push(' ');
                                    }
                                }
                                "enum" => is_enum = true,
                                "annotation" => is_annotation = true,
                                "companion" | "inline" | "value" | "expect" | "actual"
                                | "external" | "inner" => {
                                    self.diag_untranslatable(
                                        cm,
                                        format!("class modifier not supported: {}", self.text(cm)),
                                    );
                                }
                                _ => {}
                            }
                        }
                    }
                    "visibility_modifier" => {
                        // handled below via text
                    }
                    "inheritance_modifier" => {
                        // Kotlin's inheritance keywords parse as their OWN node
                        // kind, not as `class_modifier` children:
                        //  - `open` means subclassable, and Java expresses that
                        //    as the ABSENCE of `final` (see the final_kw rule
                        //    below). `open` is not a Java keyword and must never
                        //    be emitted.
                        //  - `abstract`/`sealed` are Java words, emitted as-is
                        //    (`sealed`'s own handling drops it when no subclass
                        //    is known in this file).
                        //  - `final` is Kotlin's default, which the Java form
                        //    states explicitly anyway; recording the keyword
                        //    itself matters because the source is authoritative
                        //    (see the final_kw rule below).
                        for word in self.text(m).split_whitespace() {
                            match word {
                                "open" => is_open = true,
                                "final" => is_explicit_final = true,
                                "sealed" => {
                                    is_sealed = true;
                                    if !modifiers.contains("sealed") {
                                        modifiers.push_str("sealed ");
                                    }
                                }
                                // Guarded: the same keyword can arrive as
                                // either node kind depending on the grammar,
                                // and `abstract abstract class` is not Java.
                                "abstract" if !modifiers.contains("abstract") => {
                                    modifiers.push_str("abstract ");
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if is_fun_interface {
            annotations.push("@FunctionalInterface".to_string());
        }
        // Kotlin nested classes are static (no enclosing instance) unless
        // explicitly `inner`. Java requires the `static` modifier on the
        // declaration, otherwise `new Outer.Inner(...)` (valid Kotlin shape)
        // is rejected by javac ("enclosing instance required").
        if decl.parent().is_some_and(|p| p.kind() == "class_body")
            && (decl.kind() == "class_declaration" || decl.kind() == "object_declaration")
            && !is_enum
            && !modifiers.contains("static")
            && !self.text(decl).contains("inner class")
        {
            modifiers = format!("static {}", modifiers);
        }
        // Kotlin permits repeating the same annotation on one declaration
        // (a stack of `@X(...)` lines); plain Java needs `@Repeatable` on the
        // annotation type, which notlin cannot verify — taint instead of
        // emitting an uncompilable duplicate.
        {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for annotation in &annotations {
                let annotation_name = annotation
                    .trim_start_matches('@')
                    .split(['(', ' ', '\n'])
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if !seen.insert(annotation_name.clone()) {
                    self.diag_untranslatable(
                        decl,
                        format!(
                            "annotation {annotation_name} is repeated on this declaration; Java needs @Repeatable which cannot be verified"
                        ),
                    );
                    return;
                }
            }
        }
        // visibility: Kotlin default = public; java default = package-private
        // We emit `public ` for Kotlin public (default) and nothing for private etc.
        let visibility = self.visibility_of(decl);

        let name = kt::field(decl, "name")
            .map(|n| self.text(n).to_string())
            .unwrap_or_else(|| "Anonymous".to_string());
        // Register every instance property before emitting any body member.
        // Kotlin permits a default method to read a property declared before
        // or after it; Java must spell that implicit receiver as a getter.
        // Waiting for transpile_property() is order-sensitive and leaves a
        // bare identifier when function bodies are lowered first.
        if let Some(body) = kt::child(decl, "class_body") {
            for member in body.children(&mut body.walk()) {
                if member.kind() == "function_declaration"
                    && kt::child(member, "function_value_parameters").is_some_and(|parameters| {
                        !parameters
                            .children(&mut parameters.walk())
                            .any(|parameter| parameter.kind() == "parameter")
                    })
                    && let Some(method) = kt::field(member, "name").map(|name| self.text(name))
                    && let Some(stem) = method.strip_prefix("get").filter(|stem| !stem.is_empty())
                {
                    let mut chars = stem.chars();
                    if let Some(first) = chars.next() {
                        let property = first.to_lowercase().collect::<String>() + chars.as_str();
                        self.self_getters.insert(property, method.to_string());
                    }
                }
                if member.kind() != "property_declaration" {
                    continue;
                }
                if let Some(variable) = kt::child(member, "variable_declaration")
                    && let Some(identifier) = kt::child(variable, "identifier")
                {
                    let property = self.text(identifier).to_string();
                    let is_static = kt::parent_of(member)
                        .is_some_and(|parent| parent.kind() == "class_body")
                        && kt::parent_of(member)
                            .and_then(kt::parent_of)
                            .is_some_and(|parent| parent.kind() == "object_declaration");
                    if !is_static {
                        self.self_getters
                            .insert(property.clone(), format!("get{}", capitalize(&property)));
                    }
                }
            }
        }
        if let Some(workspace) = self.workspace
            && let Some(name) = kt::field(decl, "name").map(|node| self.text(node).to_string())
        {
            let declaring = self.workspace_file.as_deref().unwrap_or(self.file);
            for property in workspace.inherited_property_names_in_file(declaring, &name) {
                self.self_getters
                    .entry(property.clone())
                    .or_insert_with(|| format!("get{}", capitalize(&property)));
            }
        }
        let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
        if self.has_companion_operator_invoke(decl)
            && self.workspace.is_some_and(|workspace| {
                // Kotlin class-call ABI `Type(...)`: only Kotlin that stays
                // Kotlin after this run can still consume the operator, so
                // retention follows the surviving-reference rule. Without a
                // fixpoint hint (single-file mode) any residual Kotlin counts,
                // because Java statics can't upgrade to the class-call form.
                match self.retained_hint {
                    Some(_) => self.referenced_by_surviving_kotlin(&name),
                    None => {
                        workspace.kotlin_files().next().is_some()
                            || workspace.has_external_kotlin_reference(indexed_path, &name)
                    }
                }
            })
        {
            self.diag_untranslatable(
                decl,
                "companion operator `invoke` is consumed by residual Kotlin; Java constructors cannot preserve the Kotlin class-call ABI",
            );
            return;
        }
        if let Some((kind, params, blockers)) = self.kotlin_retention_reason(&name, decl) {
            self.retain_decl(decl, &name, kind, &params, &blockers);
            return;
        }

        if decl.kind() == "object_declaration" {
            self.transpile_object(decl, &name, &visibility, &annotations, out);
            return;
        }
        if is_annotation {
            self.transpile_annotation_decl(decl, &name, &visibility, &hoisted_annotations, out);
            return;
        }
        if is_enum {
            self.transpile_enum(
                decl,
                &name,
                &visibility,
                &modifiers,
                is_sealed,
                &annotations,
                out,
            );
            return;
        }
        if let Some(deserializer) = self.jackson_default_deserializer_name(decl) {
            annotations.push(format!("@com.fasterxml.jackson.databind.annotation.JsonDeserialize(using = {name}.{deserializer}.class)"));
        }
        // primary constructor parameters -> fields + constructor
        // Class type parameters `class Gen<T : Bound>(...)` must be emitted or
        // field/ctor references to them won't resolve (P0 audit finding).
        let mut type_params = String::new();
        if let Some(tp) = kt::child(decl, "type_parameters") {
            let mut cursor = tp.walk();
            let mut parts: Vec<String> = Vec::new();
            for t in tp.children(&mut cursor) {
                if t.kind() != "type_parameter" {
                    continue;
                }
                if let Some(id) = kt::child(t, "identifier") {
                    let id_text = self.text(id).to_string();
                    match kt::child(t, "user_type").or_else(|| kt::child(t, "nullable_type")) {
                        Some(bound) => {
                            // A nullable Kotlin bound (`T : Foo?`) erases to
                            // the same Java bound as `T : Foo`. Declaration
                            // annotations are not valid between `extends` and
                            // the bound for common annotation sets.
                            let b = kt::java_type(bound, self.source);
                            if b == "Object" || b == "Any" {
                                parts.push(id_text);
                            } else {
                                parts.push(format!("{} extends {}", id_text, b));
                            }
                        }
                        None => parts.push(id_text),
                    }
                }
                // variance/reified modifiers inside type_parameter: untranslatable
                for extra in t.children(&mut t.walk()) {
                    if extra.kind() == "type_parameter_modifiers" {
                        self.diag_untranslatable(
                            extra,
                            format!(
                                "type-parameter modifier '{}' has no Java counterpart",
                                self.text(extra).trim()
                            ),
                        );
                    }
                }
            }
            if !parts.is_empty() {
                type_params = format!("<{}> ", parts.join(", "));
            }
        }
        let params = self.class_params(decl);
        // Primary-constructor properties are emitted as Java getters too, but
        // they bypass transpile_property(). Register them before translating
        // any member body so a Kotlin implicit receiver (`parent.key`) does
        // not leak the property name as an unresolved Java local.
        for (is_property, is_mutable, member_name, _) in &params {
            if !*is_property {
                continue;
            }
            let getter = crate::workspace::property_accessor_name(member_name);
            self.class_props.insert(
                member_name.clone(),
                if *is_mutable {
                    crate::property_abi::setter_name(member_name)
                } else {
                    String::new()
                },
            );
            // A retained Kotlin property repaired for a Java interface has an
            // explicit `getX()` bridge. If that class translates on a later
            // speculative pass, its own field reads must stay field reads;
            // routing the bridge body back through `getX()` creates recursion.
            if self.class_declares_method(decl, &getter) {
                continue;
            }
            self.self_getters.insert(member_name.clone(), getter);
        }
        let has_secondary_constructor = kt::child(decl, "class_body").is_some_and(|body| {
            body.children(&mut body.walk())
                .any(|member| member.kind() == "secondary_constructor")
        });

        // superclass / interfaces
        // The grammar wraps each supertype in `delegation_specifier`
        // (constructor_invocation / explicit_delegation / user_type).
        let mut extends = String::new();
        let mut superclass: Option<String> = None;
        let mut super_ctor_args: Option<String> = None;
        if let Some(dc) = kt::child(decl, "delegation_specifiers") {
            let mut parts: Vec<String> = Vec::new();
            let mut cursor = dc.walk();
            for spec in dc.children(&mut cursor) {
                if spec.kind() != "delegation_specifier" {
                    continue;
                }
                // unwrap: take the first named inner node
                let inner = spec.children(&mut spec.walk()).find(|c| c.is_named());
                let Some(inner) = inner else {
                    continue;
                };
                match inner.kind() {
                    // `: Parent(args)` — constructor invocation => class
                    // superclass with ctor ARGUMENTS that must be forwarded
                    // (`: RuntimeException(message)`); dropping them makes
                    // the Java parent's required ctor unreachable.
                    "constructor_invocation" => {
                        if let Some(ut) = inner
                            .children(&mut inner.walk())
                            .find(|c| c.kind() == "user_type")
                        {
                            // raw text keeps Kotlin spellings (`Any`); run
                            // through the Kotlin->Java type-name mapping.
                            let t_j = kt::java_type(ut, self.source).replace(" ", "");
                            parts.push(format!("class:{}", t_j));
                            let mut args_collected: Vec<String> = Vec::new();
                            if let Some(va) = inner
                                .children(&mut inner.walk())
                                .find(|c| c.kind() == "value_arguments")
                            {
                                let mut acur = va.walk();
                                for a in va
                                    .children(&mut acur)
                                    .filter(|c| c.kind() == "value_argument")
                                {
                                    let expr_node = a
                                        .children(&mut a.walk())
                                        .find(|c| c.is_named())
                                        .unwrap_or(a);
                                    let raw_expr = self.text(expr_node).trim().to_string();
                                    let mut e = Expr { unit: self };
                                    let rendered = if expr_node.kind() == "navigation_expression" {
                                        let raw = raw_expr.as_str();
                                        let mut pieces = raw.split('.');
                                        let base = pieces.next().unwrap_or(raw).trim();
                                        let member = pieces.next().unwrap_or("").trim();
                                        if params.iter().any(|(_, _, pname, _)| pname == base)
                                            && !member.is_empty()
                                        {
                                            format!("{}.get{}()", base, capitalize(member))
                                        } else {
                                            e.transpile(expr_node)
                                        }
                                    } else if expr_node.kind() == "call" {
                                        e.transpile(expr_node)
                                    } else {
                                        self.text(expr_node).trim().to_string()
                                    };
                                    let head = rendered.split('(').next().unwrap_or("");
                                    let rendered = if rendered.ends_with(')')
                                        && head
                                            .chars()
                                            .next()
                                            .is_some_and(|c| c.is_ascii_uppercase())
                                        && !rendered.starts_with("new ")
                                    {
                                        format!("new {}", rendered)
                                    } else {
                                        rendered
                                    };
                                    args_collected.push(rendered);
                                }
                            }
                            if !args_collected.is_empty() {
                                superclass = Some(t_j.clone());
                                super_ctor_args = Some(args_collected.join(", "));
                            }
                        }
                    }
                    // `: Greeter by Parent2()` — delegation: implement the
                    // interface; the `by` delegate is approximated (warned).
                    "explicit_delegation" => {
                        if let Some(ut) = inner
                            .children(&mut inner.walk())
                            .find(|c| c.kind() == "user_type")
                        {
                            parts.push(format!(
                                "iface:{}",
                                kt::java_type(ut, self.source).replace(" ", "")
                            ));
                        }
                        self.diag_approx(
                            inner,
                            "interface delegation `by` has no Java counterpart; emitted as plain implements",
                        );
                    }
                    // `: Greeter` — bare supertype; can't tell class vs
                    // interface without cross-file metadata, assume interface
                    "user_type" | "nullable_type" => {
                        let t = kt::java_type(inner, self.source).replace(" ", "");
                        if !t.starts_with("@") {
                            parts.push(format!("iface:{}", t));
                        }
                    }
                    other => {
                        self.diag_untranslatable(
                            inner,
                            format!("supertype form not supported: {}", other),
                        );
                    }
                }
            }
            if !parts.is_empty() {
                // constructor_invocation => extends; everything else =>
                // implements (a class superclass must come first, which the
                // Kotlin grammar guarantees).
                let classes: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| p.strip_prefix("class:"))
                    .collect();
                let ifaces: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| p.strip_prefix("iface:"))
                    .collect();
                if let Some(c) = classes.first() {
                    superclass = Some(c.to_string());
                }
                let mut j = String::new();
                if let Some(c) = classes.first() {
                    j.push_str(&format!(" extends {c}"));
                }
                if !ifaces.is_empty() {
                    if is_interface {
                        j.push_str(&format!(" extends {}", ifaces.join(", ")));
                    } else {
                        j.push_str(&format!(" implements {}", ifaces.join(", ")));
                    }
                }
                if !j.is_empty() {
                    extends = j;
                }
            }
        }

        // Sealed classes: Java needs a `permits` clause for direct subclasses
        // that land in other files; same-file nested subclasses share this
        // compilation unit and need none. With no in-file subclasses at all,
        // `sealed` has no legal Java form — fall back to a plain class.
        let mut permits = String::new();
        if is_sealed {
            let subs = self
                .subclass_map
                .get(name.as_str())
                .cloned()
                .unwrap_or_default();
            if subs.is_empty() {
                modifiers = modifiers.replace("sealed ", "");
                self.diag_approx(
                    decl,
                    format!(
                        "sealed class '{}': no subclasses declared in this file; emitted as a plain class (sealed restriction lost)",
                        name
                    ),
                );
            } else {
                let top: Vec<String> = subs
                    .iter()
                    .filter(|s| !self.decl_contains(decl, s))
                    .cloned()
                    .collect();
                if !top.is_empty() {
                    permits = format!(" permits {}", top.join(", "));
                }
            }
        }

        let kind_word = if is_interface {
            "interface"
        } else if is_data {
            "record"
        } else {
            "class"
        };
        // Kotlin's `kotlin-jpa` plugin gives @Entity/@Embeddable/
        // @MappedSuperclass classes a zero-argument constructor so the ORM can
        // instantiate them. The Kotlin source never spells that constructor
        // out, so the Java form has to be synthesized here.
        let jpa_no_arg = annotations.iter().any(|a| is_jpa_no_arg_annotation(a));

        if is_interface {
            // Interfaces declared type parameters too (`interface
            // IActivityUpdatedEvent<T : Activity>`) — emit them or the
            // `T` references in extends/implies clauses won't resolve.
            // Java-native declaration annotations pass through verbatim
            // (collected during the modifiers scan above).
            for annotation in &annotations {
                out.line(annotation.clone());
            }
            out.open(format!(
                "{}{}interface {}{}{}",
                visibility, modifiers, name, type_params, extends
            ));
            if let Some(body) = kt::child(decl, "class_body") {
                for property in body
                    .children(&mut body.walk())
                    .filter(|member| member.kind() == "property_declaration")
                {
                    if let Some(property_name) = kt::child(property, "variable_declaration")
                        .and_then(|variable| kt::child(variable, "identifier"))
                    {
                        let property_name = self.text(property_name).to_string();
                        self.self_getters.insert(
                            property_name.clone(),
                            crate::workspace::property_accessor_name(&property_name),
                        );
                        self.class_props.insert(
                            property_name.clone(),
                            if kt::child(property, "var").is_some() {
                                crate::property_abi::setter_name(&property_name)
                            } else {
                                String::new()
                            },
                        );
                    }
                }
                let mut cursor = body.walk();
                for member in body.children(&mut cursor) {
                    match member.kind() {
                        "function_declaration" => {
                            self.transpile_function(member, true, out);
                            out.blank();
                        }
                        "property_declaration" => {
                            // interface property: abstract accessor unless a
                            // getter/setter body is declared -> default method
                            let vd = kt::child(member, "variable_declaration");
                            if let Some(vd) = vd {
                                let ident = kt::child(vd, "identifier");
                                if let Some(ident) = ident {
                                    let pname = self.text(ident).to_string();
                                    // OVERIDE rule: `override val x` without a
                                    // declared type narrows the SUPERTYPE's
                                    // declared type (Java cannot bridge Object
                                    // against a type-variable getter).
                                    let mut pty = kt::child(vd, "user_type")
                                        .or_else(|| kt::child(vd, "nullable_type"))
                                        .map(|t| kt::java_type_ann(t, self.source, self.annots))
                                        .or_else(|| {
                                            let is_override = kt::child(member, "modifiers")
                                                .map(|m| self.text(m).contains("override"))
                                                .unwrap_or(false);
                                            if !is_override {
                                                return None;
                                            }
                                            self.workspace.and_then(|ws| {
                                                let declaring = self
                                                    .workspace_file
                                                    .as_deref()
                                                    .unwrap_or(self.file);
                                                ws.inherited_property_type_in_file(
                                                    declaring, &name, &pname,
                                                )
                                            })
                                        })
                                        .or_else(|| self.inferred_getter_type(member))
                                        .unwrap_or_else(|| "Object".to_string());
                                    let explicit_non_null_override =
                                        kt::child(member, "modifiers").is_some_and(|modifiers| {
                                            self.text(modifiers).contains("override")
                                        }) && kt::child(vd, "nullable_type").is_none()
                                            && kt::child(vd, "user_type").is_some()
                                            && !matches!(
                                                pty.as_str(),
                                                "boolean"
                                                    | "byte"
                                                    | "short"
                                                    | "int"
                                                    | "long"
                                                    | "float"
                                                    | "double"
                                                    | "char"
                                            );
                                    if kt::child(member, "val").is_some() {
                                        pty = crate::transpiler::types::covariant_interface_return(
                                            &pty,
                                        );
                                    }
                                    let getter_name =
                                        crate::workspace::property_accessor_name(&pname);
                                    let setter_name = crate::property_abi::setter_name(&pname);
                                    let getter = kt::child(member, "getter");
                                    // Interface properties have no backing
                                    // field, so use-site getter annotations
                                    // must be emitted on the Java accessor.
                                    // The general property emitter handles
                                    // this through `annotation_split`; this
                                    // specialized interface path must do the
                                    // same or runtime annotations disappear.
                                    let (_, mut getter_annotations) = self.annotation_split(member);
                                    if let Some(getter_node) = getter
                                        && let Some(modifiers) = kt::child(getter_node, "modifiers")
                                    {
                                        let (accessor_annotations, targeted) =
                                            self.annotation_split(modifiers);
                                        getter_annotations.extend(accessor_annotations);
                                        getter_annotations.extend(targeted);
                                    }
                                    for annotation in getter_annotations {
                                        out.line(annotation);
                                    }
                                    match getter.and_then(|g| kt::child(g, "function_body")) {
                                        Some(gb) => {
                                            // Fresh scope for the accessor body:
                                            // locals must not leak into later
                                            // interface members.
                                            self.var_types.clear();
                                            out.open(format!("default {} {}()", pty, getter_name));
                                            self.transpile_function_body(gb, out, true);
                                            out.close();
                                        }
                                        None => {
                                            if explicit_non_null_override
                                                && let Some(annotation) =
                                                    crate::transpiler::types::non_null_annotation(
                                                        self.annots,
                                                    )
                                            {
                                                out.line(annotation);
                                            }
                                            out.line(format!("{} {}();", pty, getter_name));
                                        }
                                    }
                                    if kt::child(member, "val").is_none() {
                                        let setter = kt::child(member, "setter");
                                        match setter.and_then(|s| kt::child(s, "function_body")) {
                                            Some(sb) => {
                                                self.var_types.clear();
                                                let parameter = setter
                                                    .and_then(|setter| {
                                                        kt::child(setter, "identifier")
                                                    })
                                                    .map(|name| self.text(name).to_string())
                                                    .unwrap_or_else(|| "value".into());
                                                self.var_types
                                                    .insert(parameter.clone(), pty.clone());
                                                out.open(format!(
                                                    "default void {}({} {})",
                                                    setter_name, pty, parameter
                                                ));
                                                self.transpile_function_body(sb, out, false);
                                                out.close();
                                            }
                                            None => out.line(format!(
                                                "void {}({} value);",
                                                setter_name, pty
                                            )),
                                        }
                                    }
                                    out.blank();
                                }
                            }
                        }
                        "companion_object" => {
                            self.transpile_interface_companion(member, out);
                            out.blank();
                        }
                        "line_comment" | "block_comment" | ";" | "{" | "}" => {}
                        _ => {
                            if member.is_named() {
                                self.diag_untranslatable(
                                    member,
                                    format!("interface member not supported: {}", member.kind()),
                                );
                            }
                        }
                    }
                }
            }
            out.close();
            return;
        }
        if is_data
            && !params.is_empty()
            && self.lombok
            // Lombok constructors cannot run Kotlin initialization after
            // assigning primary properties. Use explicit Java for this class.
            && !kt::child(decl, "class_body").is_some_and(|body| {
                body.children(&mut body.walk()).any(|member| {
                    member.kind() == "anonymous_initializer"
                        || (member.kind() == "property_declaration"
                            && self.property_initializer(member).is_some())
                })
            })
            // Lombok would generate a second equals/hashCode/toString beside
            // the user-defined one already in the body.
            && !self.class_body_defines_lombok_generated(decl)
        {
            // --lombok: a data class is a VALUE type. All-`val` properties
            // become Lombok's immutable `@Value` (private final fields,
            // getters, equals/hashCode/toString, all-args constructor, and no
            // setters), which is what Kotlin's data class actually is. A
            // writable `var` — or any body instance field, whose presence
            // changes the constructor arity `@Value`'s implied
            // all-args constructor would settle on — keeps mutable `@Data`.
            let lombok_annotation = self.data_class_lombok_annotation(
                decl,
                params.iter().all(|(_, is_mutable, _, _)| !*is_mutable),
            );
            let primary_constructor_annotations = self.primary_constructor_annotations(decl);
            let use_lombok_generated_members = lombok_annotation == "@Value"
                && !self.primary_constructor_has_defaults(decl)
                && primary_constructor_annotations.is_empty()
                && !jpa_no_arg
                && !has_secondary_constructor
                && superclass.is_none()
                && !self.constructor_has_covariant_input(decl, &params);
            out.line(lombok_annotation);
            self.emit_lombok_equals_call_super(decl, out);
            out.blank();
            // Java-native declaration annotations pass through verbatim
            // (collected during the modifiers scan above).
            for annotation in &annotations {
                out.line(annotation.clone());
            }
            let tp = type_params.trim_end();
            out.open(format!(
                "{}{}class {}{}{}",
                visibility, modifiers, name, tp, extends
            ));
            let nullable_parameters: std::collections::HashSet<_> =
                kt::child(decl, "primary_constructor")
                    .and_then(|constructor| kt::child(constructor, "class_parameters"))
                    .map(|parameters| {
                        parameters
                            .named_children(&mut parameters.walk())
                            .filter(|parameter| kt::child(*parameter, "nullable_type").is_some())
                            .filter_map(|parameter| kt::child(parameter, "identifier"))
                            .map(|identifier| self.text(identifier).to_string())
                            .collect()
                    })
                    .unwrap_or_default();
            let is_non_null_reference = |fname: &str, ftype: &str| {
                !nullable_parameters.contains(fname)
                    && !ftype.starts_with('@')
                    && !matches!(
                        ftype,
                        "boolean" | "byte" | "short" | "int" | "long" | "char" | "float" | "double"
                    )
            };
            for (is_property, is_mutable, fname, ftype) in &params {
                if !*is_property {
                    continue;
                }
                // final val fields: @Data omits the setter automatically
                let final_kw = if !*is_mutable { "final " } else { "" };
                // Kotlin writes persistence and metadata annotations on the
                // constructor property; the Java side must carry them on the
                // FIELD, which is the element the ORM reads.
                // This path emits no explicit getter (Lombok generates the
                // accessors), so a `@get:` annotation has nowhere better to go
                // than the field it is written on.
                for annotation in self
                    .param_annotations(decl, fname)
                    .into_iter()
                    .chain(self.param_getter_annotations(decl, fname))
                {
                    out.line(annotation);
                }
                let nullability = if is_non_null_reference(fname, ftype) {
                    "@NonNull "
                } else {
                    ""
                };
                if use_lombok_generated_members {
                    out.line(format!("{}{} {};", nullability, ftype, fname));
                } else {
                    out.line(format!(
                        "{}private {}{} {};",
                        nullability, final_kw, ftype, fname
                    ));
                }
            }
            out.blank();
            // Kotlin metadata makes primary-constructor parameter names available
            // to reflection-based binders (notably Jackson's Kotlin module). The
            // translated Java class has no kotlin.Metadata, so preserve the same
            // constructor-property contract with the JDK annotation rather than
            // requiring every consumer to install a Java parameter-name module.
            for annotation in &primary_constructor_annotations {
                out.line(annotation.clone());
            }
            if self.constructor_has_covariant_input(decl, &params) {
                out.line("@SuppressWarnings(\"unchecked\")");
            }
            self.emit_constructor_property_contract(decl, &params, out);
            if self.jackson_default_factory_supported(decl, &params) {
                // ConstructorProperties is also an implicit Jackson creator.
                // Route Jackson through the factory that applies defaults.
                out.line("@com.fasterxml.jackson.annotation.JsonCreator(mode = com.fasterxml.jackson.annotation.JsonCreator.Mode.DISABLED)");
            } else if self.jackson_default_deserializer_name(decl).is_some() {
                out.line("@com.fasterxml.jackson.annotation.JsonCreator(mode = com.fasterxml.jackson.annotation.JsonCreator.Mode.PROPERTIES)");
            }
            out.open(format!("public {}({})", name, {
                params
                    .iter()
                    .map(|(_, _, n, t)| {
                        let nullability = if is_non_null_reference(n, t) {
                            "@NonNull "
                        } else {
                            ""
                        };
                        self.java_constructor_parameter(decl, n, t, nullability)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }));
            for (is_property, _, fname, ftype) in &params {
                if *is_property {
                    let value = self.constructor_property_assignment(decl, fname, ftype);
                    out.line(format!("this.{} = {};", fname, value));
                }
            }
            out.close();
            if !use_lombok_generated_members {
                // NOTLIN: emit explicit accessors mirroring the original Kotlin ABI.
                // Relying on @Data's synthesized getters breaks cross-language member
                // resolution: kotlinc (reading our generated Java via the kotlin
                // lombok plugin) merges the implemented interface's @Nullable getter
                // into the lookup and reports T? where the original Kotlin member was
                // non-nullable, so call sites fail to typecheck. An explicit getter
                // overrides synthesis and keeps the declared member visible.
                for (is_property, _, fname, ftype) in &params {
                    if !*is_property {
                        continue;
                    }
                    // Kotlin `val authenticated: Boolean` has a Java-style
                    // `getAuthenticated()` accessor. Only a property whose name
                    // itself starts with `is` uses that name as its getter.
                    let getter = if ftype == "boolean"
                        && fname.starts_with("is")
                        && fname.chars().nth(2).is_some_and(|c| c.is_ascii_uppercase())
                    {
                        fname.clone()
                    } else {
                        format!("get{}", capitalize(fname))
                    };
                    // A Kotlin supertype's member function of the same getter
                    // name (e.g. `fun getId(): LookupEntityId`) IS the accessor
                    // the JDK sees for this property: synthesizing one with the
                    // wider property type breaks the override and javac rejects
                    // the return-type clash. Let the inherited accessor stand.
                    if self.workspace.is_some_and(|w| {
                        w.inherited_fun_getter_conflicts(
                            self.workspace_file.as_deref().unwrap_or(self.file),
                            &name,
                            &getter,
                        )
                    }) {
                        continue;
                    }
                    // @NotNull pins the getter's nullability to the (non-null)
                    // field: kotlinc otherwise merges the implemented interface's
                    // nullable property into the member lookup and reports T?
                    // (verified with the jlombok-probe fixture). A source
                    // annotation on the type (e.g. `@Nullable Boolean` from a
                    // `Boolean?` property) already states nullability — keep it
                    // verbatim instead of prepending ours.
                    let nullability = if ftype.starts_with('@') {
                        String::new()
                    } else {
                        crate::transpiler::types::non_null_annotation(self.annots)
                            .map(|annotation| format!("{annotation} "))
                            .unwrap_or_default()
                    };
                    out.line(format!(
                        "{}public {} {}() {{ return {}; }}",
                        nullability, ftype, getter, fname
                    ));
                }

                for (_, is_mutable, fname, ftype) in &params {
                    if *is_mutable && ftype != "boolean" {
                        let nullability = if is_non_null_reference(fname, ftype) {
                            "@NonNull "
                        } else {
                            ""
                        };
                        out.line(format!(
                            "public void set{}({}{} {}) {{ this.{} = {}; }}",
                            capitalize(fname),
                            nullability,
                            ftype,
                            fname,
                            fname,
                            fname
                        ));
                    }
                }
            } else {
                // Lombok's @Value emits `isEnabled()` for a primitive boolean
                // field, while Kotlin callers and overrides use
                // `getEnabled()`. Preserve Kotlin's getter ABI alongside
                // Lombok's convenience `isEnabled()` accessor.
                for (is_property, _, field_name, field_type) in &params {
                    if !*is_property
                        || field_type != "boolean"
                        || (field_name.starts_with("is")
                            && field_name
                                .chars()
                                .nth(2)
                                .is_some_and(|ch| ch.is_ascii_uppercase()))
                    {
                        continue;
                    }
                    let getter = format!("get{}", capitalize(field_name));
                    if self.class_declares_method(decl, &getter)
                        || self.workspace.is_some_and(|workspace| {
                            workspace.inherited_fun_getter_conflicts(
                                self.workspace_file.as_deref().unwrap_or(self.file),
                                &name,
                                &getter,
                            )
                        })
                    {
                        continue;
                    }
                    out.line(format!(
                        "public boolean {}() {{ return {}; }}",
                        getter, field_name
                    ));
                }
            }
            self.emit_jvm_overloads_primary_constructors(decl, &name, &params, out);
            self.emit_jackson_default_factory(decl, &name, &params, out);
            if jpa_no_arg {
                self.emit_jpa_no_arg_constructor(decl, &name, &params, superclass.is_some(), out);
            }
            out.blank();
            self.emit_planned_default_getters(decl, out);
            if let Some(body) = kt::child(decl, "class_body") {
                self.transpile_class_body(body, out);
            }
            out.close();
            return;
        }
        if is_data && !params.is_empty() {
            let as_record = !self.lombok && self.data_class_can_be_record(decl);
            let jackson_defaults = self.jackson_default_factory_supported(decl, &params);
            let jackson_creator = self.jackson_default_deserializer_name(decl).is_some();
            let has_initialization = kt::child(decl, "class_body").is_some_and(|body| {
                body.children(&mut body.walk()).any(|member| {
                    member.kind() == "anonymous_initializer"
                        || (member.kind() == "property_declaration"
                            && self.property_initializer(member).is_some())
                })
            });
            let mut init_method = "__notlin_initializeData".to_string();
            while kt::child(decl, "class_body").is_some_and(|body| {
                body.children(&mut body.walk()).any(|member| {
                    member.kind() == "function_declaration"
                        && kt::field(member, "name").is_some_and(|n| self.text(n) == init_method)
                })
            }) {
                init_method.push('_');
            }
            if let Some(body) = kt::child(decl, "class_body") {
                for member in body.children(&mut body.walk()) {
                    if member.kind() == "property_declaration"
                        && self.property_emits_backing_field(member)
                    {
                        self.deferred_property_initializers.insert(member.id());
                    }
                }
            }
            let comps: Vec<String> = params
                .iter()
                .map(|(_, _, n, t)| {
                    format!("{} {}", self.constructor_parameter_java_type(decl, n, t), n)
                })
                .collect();
            let tp = type_params.trim_end(); // "<T> " / ""
            for annotation in &annotations {
                out.line(annotation.clone());
            }
            if as_record {
                out.open(format!(
                    "{}record {}{}({}){}",
                    visibility,
                    name,
                    tp.trim_end(),
                    comps.join(", "),
                    extends
                ));
                let has_initializers = kt::child(decl, "class_body").is_some_and(|body| {
                    body.children(&mut body.walk())
                        .any(|member| member.kind() == "anonymous_initializer")
                });
                if has_initializers
                    || !self.primary_constructor_annotations(decl).is_empty()
                    || jackson_defaults
                    || jackson_creator
                {
                    for annotation in self.primary_constructor_annotations(decl) {
                        out.line(annotation);
                    }
                    if jackson_defaults {
                        out.line("@com.fasterxml.jackson.annotation.JsonCreator(mode = com.fasterxml.jackson.annotation.JsonCreator.Mode.DISABLED)");
                    } else if jackson_creator {
                        out.line("@com.fasterxml.jackson.annotation.JsonCreator(mode = com.fasterxml.jackson.annotation.JsonCreator.Mode.PROPERTIES)");
                    }
                    let constructor_params = params
                        .iter()
                        .map(|(_, _, param, ty)| {
                            self.java_constructor_parameter(decl, param, ty, "")
                        })
                        .collect::<Vec<_>>();
                    out.open(format!(
                        "public {}({})",
                        name,
                        constructor_params.join(", ")
                    ));
                    for (_, _, field, _) in &params {
                        out.line(format!("this.{field} = {field};"));
                    }
                    self.emit_data_initialization(decl, &params, out);
                    out.close();
                }
                if jpa_no_arg {
                    self.emit_jpa_no_arg_record_constructor(decl, &name, &params, out);
                }
                for (is_property, _, fname, ftype) in &params {
                    if !*is_property {
                        continue;
                    }
                    let getter_name = format!("get{}", capitalize(fname));
                    let declares_getter = kt::child(decl, "class_body").is_some_and(|body| {
                        body.children(&mut body.walk()).any(|member| {
                            member.kind() == "function_declaration"
                                && kt::field(member, "name")
                                    .is_some_and(|method| self.text(method) == getter_name)
                        })
                    });
                    if declares_getter {
                        continue;
                    }
                    out.blank();
                    for annotation in self.param_getter_annotations(decl, fname) {
                        out.line(annotation);
                    }
                    out.open(format!("public {} {}()", ftype, getter_name));
                    out.line(format!("return {}();", fname));
                    out.close();
                }
            } else {
                let inner = if extends.is_empty() {
                    format!("{}final class {}{}", visibility, name, tp)
                } else {
                    // must nest inside `extends X` clause: nested class
                    // inheritance in one line
                    // static nested: no enclosing instance needed at `new` —
                    // but only legal INSIDE a class (top-level classes reject
                    // the `static` modifier).
                    let static_kw = if decl.parent().is_some_and(|p| p.kind() == "class_body") {
                        "static "
                    } else {
                        ""
                    };
                    format!(
                        "{}{}final class {}{} {}",
                        visibility, static_kw, name, tp, extends
                    )
                };
                out.open(inner);
                out.blank();
                for (is_property, is_mutable, fname, ftype) in &params {
                    if !*is_property {
                        continue;
                    }
                    let final_kw = if !*is_mutable { "final " } else { "" };
                    // Kotlin writes persistence and metadata annotations on the
                    // constructor property; the Java side must carry them on the
                    // FIELD, which is the element the ORM reads.
                    for annotation in self.param_annotations(decl, fname) {
                        out.line(annotation);
                    }
                    out.line(format!("private {}{} {};", final_kw, ftype, fname));
                }
                out.blank();
                // `data class Layout @Default constructor(...)` — the marker
                // must ride on the generated constructor: the default-argument
                // overloads below give the class several constructors, and
                // MapStruct selects between them by an annotation named
                // `@Default`.
                for annotation in self.primary_constructor_annotations(decl) {
                    out.line(annotation);
                }
                self.emit_constructor_property_contract(decl, &params, out);
                if jackson_defaults {
                    out.line("@com.fasterxml.jackson.annotation.JsonCreator(mode = com.fasterxml.jackson.annotation.JsonCreator.Mode.DISABLED)");
                } else if jackson_creator {
                    out.line("@com.fasterxml.jackson.annotation.JsonCreator(mode = com.fasterxml.jackson.annotation.JsonCreator.Mode.PROPERTIES)");
                }
                if self.constructor_has_covariant_input(decl, &params) {
                    out.line("@SuppressWarnings(\"unchecked\")");
                }
                let constructor_params = params
                    .iter()
                    .map(|(_, _, param, ty)| {
                        self.java_constructor_parameter(
                            decl,
                            param,
                            &self.constructor_parameter_java_type(decl, param, ty),
                            "",
                        )
                    })
                    .collect::<Vec<_>>();
                out.open(format!(
                    "public {}({})",
                    name,
                    constructor_params.join(", ")
                ));
                if let Some(args) = &super_ctor_args {
                    out.line(format!("super({args});"));
                }
                for (is_property, _, fname, ftype) in &params {
                    if *is_property {
                        let value = self.constructor_property_assignment(decl, fname, ftype);
                        out.line(format!("this.{} = {};", fname, value));
                    }
                }
                if has_initialization {
                    out.line(format!("{init_method}();"));
                }
                out.close();
                if has_initialization {
                    out.open(format!("private void {init_method}()"));
                    self.emit_data_initialization(decl, &params, out);
                    out.close();
                }
                if jpa_no_arg {
                    self.emit_jpa_no_arg_constructor(
                        decl,
                        &name,
                        &params,
                        superclass.is_some(),
                        out,
                    );
                }
                out.blank();
                for (_, _, fname, ftype) in &params {
                    let getter = crate::workspace::property_accessor_name(fname);
                    let setter = crate::property_abi::setter_name(fname);
                    // `@get:`-targeted annotations belong on the generated
                    // getter, which this path emits.
                    if !self.class_declares_method(decl, &getter) {
                        for annotation in self.param_getter_annotations(decl, fname) {
                            out.line(annotation);
                        }
                        out.open(format!("public {} {}()", ftype, getter));
                        out.line(format!("return {};", fname));
                        out.close();
                    }
                    if params
                        .iter()
                        .any(|(_, mutable, n, _)| *mutable && n == fname)
                        && !self.class_declares_method_signature(decl, &setter, &[ftype.as_str()])
                    {
                        out.open(format!("public void {}({} {})", setter, ftype, fname));
                        out.line(format!("this.{} = {};", fname, fname));
                        out.close();
                    }
                }
                out.blank();
            }
            if let Some(body) = kt::child(decl, "class_body") {
                self.transpile_class_body(body, out);
            }
            self.emit_planned_default_getters(decl, out);
            self.emit_data_members(decl, &name, &params, as_record, out);
            if let Some(body) = kt::child(decl, "class_body") {
                for member in body.children(&mut body.walk()) {
                    self.deferred_property_initializers.remove(&member.id());
                }
            }
            // A record's canonical constructor is its only implicit one,
            // extra constructors may delegate to it with `this(...)`. Kotlin
            // callers omit trailing defaults, so a translated record needs the
            // same delegating overloads a plain class gets — without them the
            // retained Kotlin that called `Configuration()` no longer resolves.
            self.emit_jvm_overloads_primary_constructors(decl, &name, &params, out);
            self.emit_jackson_default_factory(decl, &name, &params, out);
            out.close();
        } else {
            // Java places type params after the class name: `class Name<T>`.
            let tp = type_params.trim_end(); // "<T>" or "" (no space needed before '{')
            // Kotlin classes are final by DEFAULT; Java's default is the
            // opposite. Emitting an ordinary Kotlin class as a plain Java class
            // silently grants an extensibility the source never had, so the
            // faithful Java form is `final`. Deliberate opt-outs, in order:
            //  - a subclass of a FILE-sealed type: Java requires a permitted
            //    subtype to be final, sealed or non-sealed;
            //  - the source says so (`open`/`abstract`/`sealed`);
            //  - something in the workspace extends it: both compilers reject
            //    inheriting a final class, so any subclass not visible here
            //    would turn this into an error at the other end;
            //  - a persistence class, which the ORM subclasses at runtime for
            //    lazy proxies. Such a Kotlin source is `open` only via the
            //    all-open plugin, so there is no `open` token in the AST to
            //    tell us and `final` fails at STARTUP, not at compile time.
            //
            // An EXPLICIT `final` in the source outranks every opt-out below
            // and is never traded away: valid Kotlin cannot extend a `final`
            // class at all, so "something extends it" is a false positive (or
            // a stray Java file that was already broken, which silently
            // un-finaling the class would hide), and a `final` persistence
            // class is one the all-open plugin did NOT open, i.e. not proxied.
            // Dropping the keyword would hand callers an extensibility the
            // Kotlin source explicitly refused them.
            let sealed_parent = superclass
                .as_ref()
                .is_some_and(|parent| self.sealed_types.contains(parent));
            let final_kw = if sealed_parent || is_explicit_final {
                "final "
            } else if is_sealed
                || is_open
                || modifiers.contains("abstract")
                || self
                    .workspace
                    .is_some_and(|workspace| workspace.has_subtype_named(&name))
                || annotations.iter().any(|a| is_orm_proxied_annotation(a))
            {
                ""
            } else {
                "final "
            };
            // --lombok: hand-rolled accessors/equals/hashCode/toString become
            // Lombok annotations placed BEFORE the class declaration.
            // @AllArgsConstructor synthesizes a ctor over EVERY field, so it
            // only stands in for the Kotlin primary constructor when the class
            // has no other instance fields: `class C(val site: SiteId) { var
            // zone: SearchValue? = null }` has a one-parameter Kotlin ctor, but
            // Lombok would demand `(site, zone, …)` and every caller using the
            // Kotlin arity stops compiling. Emit the explicit ctor instead —
            // the body properties keep their inline initializers as fields.
            //
            // The JPA no-arg constructor is a second reason never to lean on
            // Lombok here: Lombok generates NO constructor at all once the
            // class declares one (measured with lombok 1.18.42: @Data +
            // @AllArgsConstructor beside an explicit `public T()` yields only
            // that one), so relying on the synthesized all-args ctor would
            // silently DELETE it and break every call site — the explicit ctor
            // has to be written beside the no-arg one.
            let lombok_supplies_all_args = !jpa_no_arg
                && !self.class_body_declares_instance_fields(decl)
                && !self.constructor_has_covariant_input(decl, &params)
                && params
                    .iter()
                    .all(|(_, _, name, _)| self.param_parameter_annotations(decl, name).is_empty());
            let lombok_accessors = self.lombok
                && !params.is_empty()
                && !self.class_body_defines_lombok_generated(decl);
            if lombok_accessors {
                // Ordinary Kotlin classes inherit identity equality unless
                // they explicitly override it. @Data would introduce value
                // equality, hashing, and toString as an unintended side effect.
                out.line("@Getter");
                // @AllArgsConstructor's synthesized ctor collides with the
                // explicit super-forwarding ctor emitted below — only
                // annotate when the explicit one is not being written.
                if super_ctor_args.is_none()
                    && !has_secondary_constructor
                    && lombok_supplies_all_args
                {
                    out.line("@AllArgsConstructor");
                }
                out.blank();
            }
            // Java-native declaration annotations pass through verbatim
            // (collected during the modifiers scan above).
            for annotation in &annotations {
                out.line(annotation.clone());
            }
            out.open(format!(
                "{}{}{}{} {}{}{}{}",
                visibility, final_kw, modifiers, kind_word, name, tp, extends, permits
            ));
            // fields (final for val: @Data skips the setter on a final field)
            for (is_property, is_mutable, fname, ftype) in &params {
                if !*is_property {
                    continue;
                }
                let final_kw = if !*is_mutable { "final " } else { "" };
                // Kotlin writes persistence and metadata annotations on the
                // constructor property; the Java side must carry them on the
                // FIELD, which is the element the ORM reads.
                // The getter below is emitted only when this is NOT --lombok
                // (`accessors (skipped under --lombok: @Data generates
                // them)`), so under Lombok a `@get:` annotation has nowhere
                // to land and must stay on the field it is written on:
                // dropping it would lose the very metadata the ORM reads.
                let mut annotations = self.param_annotations(decl, fname);
                if lombok_accessors && *is_mutable {
                    annotations.push("@Setter".into());
                }
                let lombok_needs_kotlin_boolean_getter = lombok_accessors
                    && ftype == "boolean"
                    && !(fname.starts_with("is")
                        && fname.chars().nth(2).is_some_and(|c| c.is_ascii_uppercase()));
                if lombok_accessors && !lombok_needs_kotlin_boolean_getter {
                    annotations.extend(self.param_getter_annotations(decl, fname));
                }
                for annotation in annotations {
                    out.line(annotation);
                }
                out.line(format!("private {}{} {};", final_kw, ftype, fname));
            }
            if !params.is_empty() {
                out.blank();
            }
            // constructor (redundant under --lombok: AllArgsConstructor)
            let ctor_annotations = self.primary_constructor_annotations(decl);
            if let Some(sargs) = &super_ctor_args {
                // superclass ctor needs arguments: emit an explicit ctor
                // forwarding them (`: Parent("template")` -> super("template")).
                // @AllArgsConstructor's generated ctor cannot express the
                // super-call, so the explicit one is required regardless.
                for annotation in &ctor_annotations {
                    out.line(annotation.clone());
                }
                if self.constructor_has_covariant_input(decl, &params) {
                    out.line("@SuppressWarnings(\"unchecked\")");
                }
                out.open(format!("public {}({})", name, {
                    params
                        .iter()
                        .map(|(_, _, n, t)| self.java_constructor_parameter(decl, n, t, ""))
                        .collect::<Vec<_>>()
                        .join(", ")
                }));
                out.line(format!("super({});", sargs));
                for (is_property, _, fname, ftype) in &params {
                    if *is_property {
                        let value = self.constructor_property_assignment(decl, fname, ftype);
                        out.line(format!("this.{} = {};", fname, value));
                    }
                }
                out.close();
                out.blank();
            }
            if !params.is_empty()
                && (!lombok_accessors || has_secondary_constructor || !lombok_supplies_all_args)
                && super_ctor_args.is_none()
            {
                for annotation in &ctor_annotations {
                    out.line(annotation.clone());
                }
                if self.constructor_has_covariant_input(decl, &params) {
                    out.line("@SuppressWarnings(\"unchecked\")");
                }
                out.open(format!("public {}({})", name, {
                    params
                        .iter()
                        .map(|(_, _, n, t)| self.java_constructor_parameter(decl, n, t, ""))
                        .collect::<Vec<_>>()
                        .join(", ")
                }));
                for (is_property, _, fname, ftype) in &params {
                    if *is_property {
                        let value = self.constructor_property_assignment(decl, fname, ftype);
                        out.line(format!("this.{} = {};", fname, value));
                    }
                }
                out.close();
                out.blank();
            }
            self.emit_jvm_overloads_primary_constructors(decl, &name, &params, out);
            if jpa_no_arg {
                self.emit_jpa_no_arg_constructor(decl, &name, &params, superclass.is_some(), out);
            }
            // Only omit accessors when the annotations above actually supply
            // them. A custom method may have disabled Lombok for this class.
            if !lombok_accessors {
                for (is_property, is_mutable, fname, ftype) in &params {
                    // A plain constructor parameter is not a property: Kotlin
                    // declares no member for it, so an emitted accessor would
                    // read a field that was never written (`return x;` ->
                    // "cannot find symbol x"; for a name that shadows an
                    // inherited property, "id has private access in Base").
                    if !*is_property {
                        continue;
                    }
                    let getter = crate::workspace::property_accessor_name(fname);
                    let setter = crate::property_abi::setter_name(fname);
                    // `@get:`-targeted annotations belong on the generated
                    // getter, which this path emits.
                    if !self.class_declares_method(decl, &getter) {
                        for annotation in self.param_getter_annotations(decl, fname) {
                            out.line(annotation);
                        }
                        out.open(format!("public {} {}()", ftype, getter));
                        out.line(format!("return {};", fname));
                        out.close();
                    }
                    if *is_mutable
                        && !self.class_declares_method_signature(decl, &setter, &[ftype.as_str()])
                    {
                        out.blank();
                        for annotation in self.param_setter_annotations(decl, fname) {
                            out.line(annotation);
                        }
                        out.open(format!("public void {}({} {})", setter, ftype, fname));
                        out.line(format!("this.{} = {};", fname, fname));
                        out.close();
                    }
                    out.blank();
                }
            } else {
                // Lombok names a primitive-boolean getter `isEnabled()`, while
                // Kotlin's JVM ABI for `val enabled: Boolean` is
                // `getEnabled()`. Keep the Kotlin getter explicitly; Lombok
                // suppresses only the same method name and may still add its
                // convenience `isEnabled()` without breaking callers.
                for (is_property, _, fname, ftype) in &params {
                    if !*is_property
                        || ftype != "boolean"
                        || (fname.starts_with("is")
                            && fname.chars().nth(2).is_some_and(|c| c.is_ascii_uppercase()))
                    {
                        continue;
                    }
                    let getter = format!("get{}", capitalize(fname));
                    if self.class_declares_method(decl, &getter)
                        || self.workspace.is_some_and(|workspace| {
                            workspace.inherited_fun_getter_conflicts(
                                self.workspace_file.as_deref().unwrap_or(self.file),
                                &name,
                                &getter,
                            )
                        })
                    {
                        continue;
                    }
                    for annotation in self.param_getter_annotations(decl, fname) {
                        out.line(annotation);
                    }
                    out.line(format!(
                        "public boolean {}() {{ return {}; }}",
                        getter, fname
                    ));
                }
                // Lombok suppresses an @Data setter when a user method has
                // the same name and arity, even if the parameter type differs.
                // Preserve the constructor property's exact Java ABI with an
                // explicit bridge in that case.
                for (is_property, is_mutable, fname, ftype) in &params {
                    if !*is_property || !*is_mutable {
                        continue;
                    }
                    let setter = format!("set{}", capitalize(fname));
                    let setter_annotations = self.param_setter_annotations(decl, fname);
                    let has_wrong_arity_peer = self.class_declares_method_arity(decl, &setter, 1)
                        && !self.class_declares_method_signature(decl, &setter, &[ftype.as_str()]);
                    if !setter_annotations.is_empty() || has_wrong_arity_peer {
                        if self.class_declares_method_signature(decl, &setter, &[ftype.as_str()]) {
                            continue;
                        }
                        for annotation in setter_annotations {
                            out.line(annotation);
                        }
                        out.open(format!("public void {}({} {})", setter, ftype, fname));
                        out.line(format!("this.{} = {};", fname, fname));
                        out.close();
                        out.blank();
                    }
                }
            } // body members
            self.emit_planned_default_getters(decl, out);
            if let Some(body) = kt::child(decl, "class_body") {
                self.transpile_class_body(body, out);
            }
            out.close();
        }
    }

    fn transpile_annotation_decl(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        visibility: &str,
        hoisted_annotations: &[String],
        out: &mut JavaOut,
    ) {
        let declaring = self.workspace_file.as_deref().unwrap_or(self.file);
        let source_file = self
            .workspace
            .and_then(|workspace| workspace.source_file(declaring));
        let mut metadata = hoisted_annotations.to_vec();
        let mut java_targets = Vec::new();
        let mut retention = "RUNTIME";
        let mut has_target = false;
        let mut has_java_retention = false;
        let mut has_documented = false;
        if let Some(modifiers) = kt::child(decl, "modifiers") {
            for annotation in modifiers.children(&mut modifiers.walk()) {
                if annotation.kind() != "annotation" {
                    continue;
                }
                if let Some(raw_name) = annotation_fq_name(annotation, self.source) {
                    let imported = annotation_import_name(raw_name, self.source);
                    has_java_retention |= imported == "java.lang.annotation.Retention"
                        || (imported == "Retention"
                            && annotation_imports(self.source).any(|(path, alias)| {
                                alias.is_none() && path == "java.lang.annotation.Retention"
                            }));
                }
                match kotlin_annotation_meta_name(
                    annotation,
                    self.source,
                    self.workspace,
                    source_file,
                ) {
                    Some("Target") => {
                        has_target = true;
                        for value in annotation_argument_values(annotation, self.source) {
                            let Some((owner, target)) = value.rsplit_once('.') else {
                                self.diag_untranslatable(
                                    decl,
                                    "Kotlin annotation target cannot be represented in Java",
                                );
                                return;
                            };
                            if !matches!(
                                owner,
                                "AnnotationTarget" | "kotlin.annotation.AnnotationTarget"
                            ) {
                                self.diag_untranslatable(
                                    decl,
                                    "Kotlin annotation target cannot be resolved safely",
                                );
                                return;
                            }
                            let Some(mapped) = (match target {
                                "CLASS" => Some("TYPE"),
                                "ANNOTATION_CLASS" => Some("ANNOTATION_TYPE"),
                                "FIELD" => Some("FIELD"),
                                _ => None,
                            }) else {
                                self.diag_untranslatable(decl, format!("Kotlin annotation target {target} cannot be represented in Java"));
                                return;
                            };
                            if !java_targets.contains(&mapped) {
                                java_targets.push(mapped);
                            }
                        }
                    }
                    Some("Retention") => {
                        let values = annotation_argument_values(annotation, self.source);
                        let mapped = match values.as_slice() {
                            [] => Some("RUNTIME"),
                            [value] => value.rsplit_once('.').and_then(|(owner, member)| {
                                if !matches!(
                                    owner,
                                    "AnnotationRetention" | "kotlin.annotation.AnnotationRetention"
                                ) {
                                    return None;
                                }
                                match member {
                                    "SOURCE" => Some("SOURCE"),
                                    "BINARY" => Some("CLASS"),
                                    "RUNTIME" => Some("RUNTIME"),
                                    _ => None,
                                }
                            }),
                            _ => None,
                        };
                        let Some(mapped) = mapped else {
                            self.diag_untranslatable(
                                decl,
                                "Kotlin annotation retention cannot be represented in Java",
                            );
                            return;
                        };
                        retention = mapped;
                    }
                    Some("MustBeDocumented") => has_documented = true,
                    _ => {
                        if let Some(lowered) = self.transpile_declaration_annotation(annotation) {
                            metadata.push(lowered);
                        } else {
                            self.diag_untranslatable(
                                decl,
                                "annotation metadata cannot be represented in Java",
                            );
                            return;
                        }
                    }
                }
            }
        }
        if has_target {
            metadata.push(format!(
                "@java.lang.annotation.Target({{{}}})",
                java_targets
                    .iter()
                    .map(|target| format!("java.lang.annotation.ElementType.{target}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !has_java_retention {
            metadata.push(format!(
                "@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.{retention})"
            ));
        }
        if has_documented {
            metadata.push("@java.lang.annotation.Documented".to_string());
        }
        for annotation in metadata {
            out.line(annotation);
        }
        let params = self.class_params(decl);
        let defaults = self.class_param_defaults(decl);
        out.open(format!("{visibility}@interface {name}"));
        for ((_, _, param_name, param_type), default) in params.into_iter().zip(defaults) {
            let suffix = default
                .map(|value| format!(" default {value}"))
                .unwrap_or_default();
            out.line(format!("{param_type} {param_name}(){suffix};"));
        }
        out.close();
    }

    /// Preserve the canonical property constructor's names when Kotlin metadata
    /// is replaced by an ordinary Java class with explicit constructors.
    fn emit_constructor_property_contract(
        &self,
        decl: tree_sitter::Node,
        params: &[(bool, bool, String, String)],
        out: &mut JavaOut,
    ) {
        if params.is_empty() || params.iter().any(|(property, _, _, _)| !property) {
            return;
        }
        let imported_names = self
            .source
            .lines()
            .filter_map(|line| {
                let import = line.trim().strip_prefix("import ")?.trim_end_matches(';');
                let (path, alias) = import
                    .split_once(" as ")
                    .map_or((import, None), |(path, alias)| {
                        (path.trim(), Some(alias.trim()))
                    });
                matches!(path, "java.beans.ConstructorProperties" | "java.beans.*")
                    .then_some(alias.unwrap_or("ConstructorProperties"))
            })
            .collect::<Vec<_>>();
        let already_present = self
            .primary_constructor_annotations(decl)
            .iter()
            .any(|annotation| {
                annotation.starts_with("@java.beans.ConstructorProperties(")
                    || imported_names.iter().any(|name| {
                        annotation
                            .strip_prefix('@')
                            .and_then(|text| text.split_once('('))
                            .is_some_and(|(actual, _)| actual == *name)
                    })
            });
        if already_present {
            return;
        }
        // Primary data-class property names survive in the existing Lombok
        // path and must also survive when custom members require plain Java.
        // Secondary/default overloads deliberately receive no creator marker.
        out.line(format!(
            "@java.beans.ConstructorProperties({{{}}})",
            params
                .iter()
                .map(|(_, _, name, _)| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    /// Preserve primary-constructor annotations, including MapStruct's `Default`
    /// selection when default arguments introduce additional Java overloads.
    fn primary_constructor_annotations(&self, decl: tree_sitter::Node) -> Vec<String> {
        // A constructor with no annotations at all has no `modifiers` child;
        // the marker below must still be considered, so this is a plain
        // iterator, not an early return.
        let mut annotations: Vec<String> = kt::child(decl, "primary_constructor")
            .and_then(|constructor| kt::child(constructor, "modifiers"))
            .map(|modifiers| {
                let mut cursor = modifiers.walk();
                modifiers
                    .children(&mut cursor)
                    .filter(|node| matches!(node.kind(), "annotation" | "annotated_expression"))
                    .filter_map(|node| self.transpile_declaration_annotation(node))
                    // `@JvmOverloads` and its family describe the Kotlin ABI;
                    // the overloads themselves are emitted explicitly, so the
                    // annotations have no Java counterpart to carry over.
                    .filter(|annotation| {
                        ![
                            "JvmOverloads",
                            "JvmName",
                            "JvmStatic",
                            "JvmField",
                            "JvmSuppressWildcards",
                        ]
                        .iter()
                        .any(|jvm_only| annotation.contains(jvm_only))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // A constructor with Kotlin default arguments becomes several Java
        // constructors. Kotlin metadata told MapStruct which of them was the
        // primary one; in Java the same consumer disambiguates only by an
        // annotation named `Default`. Keep the marker the source wrote, and
        // supply it (fully qualified — the declaring file rarely imports it)
        // whenever the translation introduces the extra constructors.
        if self.in_place
            && self.primary_constructor_has_defaults(decl)
            && let Some(marker) = self.workspace_constructor_marker()
        {
            let simple = marker.rsplit('.').next().unwrap_or_default();
            if !annotations.iter().any(|written| written.contains(simple)) {
                annotations.push(format!("@{marker}"));
            }
        }
        annotations
    }

    /// Whether the primary constructor declares any default argument, i.e.
    /// whether the migration will add delegating constructor overloads that a
    /// reflection-based consumer then has to disambiguate.
    fn primary_constructor_has_defaults(&self, decl: tree_sitter::Node) -> bool {
        kt::child(decl, "primary_constructor")
            .and_then(|constructor| kt::child(constructor, "class_parameters"))
            .is_some_and(|parameters| {
                parameters
                    .children(&mut parameters.walk())
                    .filter(|parameter| parameter.kind() == "class_parameter")
                    .any(|parameter| {
                        parameter
                            .children(&mut parameter.walk())
                            .any(|child| !child.is_named() && child.kind() == "=")
                    })
            })
    }

    /// The workspace's `Default`-named constructor marker annotation, as a
    /// fully qualified name, when it declares one.
    fn workspace_constructor_marker(&self) -> Option<String> {
        let workspace = self.workspace?;
        let found: Vec<_> = workspace
            .declarations_named("Default")
            .map(|declaration| (declaration.kind, declaration.package.clone()))
            .collect();
        if std::env::var("NOTLIN_DEBUG_MARKER").is_ok() {
            eprintln!("dbg marker: candidates={found:?}");
        }
        workspace
            .declarations_named("Default")
            .find(|declaration| declaration.kind == crate::workspace::DeclarationKind::Annotation)
            .and_then(|declaration| {
                declaration
                    .package
                    .as_deref()
                    .map(|package| format!("{package}.{}", declaration.name))
            })
    }

    /// Body properties (`var zone: SearchValue? = null`) become instance fields
    /// with inline initializers; they are NOT primary-constructor parameters.
    /// Lombok's @AllArgsConstructor covers every field, so it only matches the
    /// Kotlin constructor when the body adds no field of its own.
    /// Lombok's annotation for a translated Kotlin data class: `@Value` when
    /// nothing is writable, `@Data` as soon as something is.
    ///
    /// Kotlin's `data class X(val a: T)` is an immutable value type, and
    /// `@Value` is its faithful Java form. A `var` property makes the Java
    /// object writable, which only `@Data` serves. A body instance field also
    /// forces `@Data`: `@Value` implies `@AllArgsConstructor` over EVERY field,
    /// so the constructor arity Kotlin callers use would change.
    fn data_class_lombok_annotation(
        &self,
        decl: tree_sitter::Node,
        ctor_props_all_final: bool,
    ) -> &'static str {
        if ctor_props_all_final && !self.class_body_declares_instance_fields(decl) {
            "@Value"
        } else {
            "@Data"
        }
    }

    /// Lombok's generated `equals`/`hashCode` cover this class's own fields
    /// only. A value type with a superclass must fold the inherited state in —
    /// and without the call Lombok warns "generating equals/hashCode
    /// implementation but without a call to superclass" on every such class.
    ///
    /// The predicate is `superclass_name`, which only matches a delegation
    /// specifier shaped like `: Base(...)`, i.e. a real class supertype —
    /// interfaces take no parentheses. Keying on the emitted `extends` text
    /// instead would catch interface-only classes and buy the opposite
    /// complaint, Lombok's "supercall to java.lang.Object is pointless".
    fn emit_lombok_equals_call_super(&self, decl: tree_sitter::Node<'tree>, out: &mut JavaOut) {
        if self.superclass_name(decl).is_some() {
            out.line("@EqualsAndHashCode(callSuper = true)");
        }
    }

    /// Annotations written on one declaration, split by what Java can do with
    /// them: a `@get:`-targeted annotation belongs on the generated getter when
    /// the emitter writes one, and everything else on the field. The use-site
    /// prefix itself is dropped either way — Java annotates the element, and
    /// `@JsonProperty` is not Java syntax.
    pub(crate) fn annotation_split(&self, node: tree_sitter::Node) -> (Vec<String>, Vec<String>) {
        let mut field = Vec::new();
        let mut getter = Vec::new();
        // Annotations sit under `modifiers`, but walk the subtree so the
        // grammar's exact nesting cannot matter.
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let mut cursor = current.walk();
            for child in current.children(&mut cursor) {
                if child.kind() == "annotation" {
                    let annotation = self.text(child);
                    // JVM control annotations describe the Kotlin ABI, not a
                    // Java field annotation. Static companion properties already
                    // receive an explicit Java field and static getter.
                    if annotation.contains("notlinProperty")
                        || annotation_simple_name(annotation) == "JvmField"
                        || self.is_kotlin_jvm_static_annotation(child)
                    {
                        continue;
                    }
                    if let Some(text) = self.transpile_declaration_annotation(child) {
                        if use_site_target(annotation) == Some("get") {
                            getter.push(text);
                        } else {
                            field.push(text);
                        }
                    }
                } else {
                    stack.push(child);
                }
            }
        }
        if let Some((owner, property_node, property)) = self.property_annotation_owner(node) {
            for annotation in
                self.property_repair_bridge_annotations(&owner, property_node, &property, true)
            {
                if !getter.contains(&annotation) {
                    getter.push(annotation);
                }
            }
        }
        (field, getter)
    }

    /// Annotations moved from a property onto a temporary JavaBean bridge by
    /// retained-property repair still belong to the property's generated
    /// accessor if the owning class becomes Java on a later fixpoint pass.
    /// Only consume annotations from the exact generated bridge for this
    /// property; ordinary user methods keep their own annotation semantics.
    pub(crate) fn property_repair_bridge_annotations(
        &self,
        owner: &tree_sitter::Node,
        property_node: tree_sitter::Node,
        property: &str,
        getter: bool,
    ) -> Vec<String> {
        let Some(body) =
            kt::child(*owner, "class_body").or_else(|| kt::child(*owner, "enum_class_body"))
        else {
            return Vec::new();
        };
        let name = format!(
            "{}{}",
            if getter { "get" } else { "set" },
            capitalize(property)
        );
        // The method-name marker must be attached to this exact property, not
        // merely somewhere in the owning class (which could be an overload).
        let property_marker = self
            .text(property_node)
            .contains(&format!("notlinProperty{name}"));
        let Some(bridge) = body.children(&mut body.walk()).find(|member| {
            member.kind() == "function_declaration"
                && kt::field(*member, "name").is_some_and(|method| self.text(method) == name)
                && (property_marker && self.is_property_repair_bridge(*member)
                    || self.constructor_property_bridge(*member).as_deref() == Some(property))
                && self.property_repair_bridge_for(*member, property, getter)
        }) else {
            return Vec::new();
        };
        let root = kt::child(bridge, "modifiers").unwrap_or(bridge);
        let mut stack = vec![root];
        let mut annotations = Vec::new();
        while let Some(current) = stack.pop() {
            for child in current.children(&mut current.walk()) {
                if child.kind() == "annotation" {
                    let text = self.text(child);
                    if text.contains("notlinProperty")
                        || annotation_simple_name(text) == "JvmField"
                        || self.is_kotlin_jvm_static_annotation(child)
                    {
                        continue;
                    }
                    if let Some(annotation) = self.transpile_declaration_annotation(child) {
                        annotations.push(annotation);
                    }
                } else {
                    stack.push(child);
                }
            }
        }
        annotations
    }

    fn property_repair_bridge_for(
        &self,
        function: tree_sitter::Node,
        property: &str,
        getter: bool,
    ) -> bool {
        let Some(parameters) = kt::child(function, "function_value_parameters") else {
            return false;
        };
        let parameter_count = parameters
            .children(&mut parameters.walk())
            .filter(|child| child.kind() == "parameter")
            .count();
        let Some(body) = kt::child(function, "function_body") else {
            return false;
        };
        let normalized = self
            .text(body)
            .chars()
            .filter(|ch| !ch.is_whitespace() && !matches!(ch, '{' | '}' | ';'))
            .collect::<String>()
            .replace("this.", "");
        if getter {
            parameter_count == 0 && normalized.strip_prefix('=') == Some(property)
        } else {
            parameter_count == 1 && normalized == format!("{property}=value")
        }
    }

    pub(crate) fn property_annotation_owner<'a>(
        &self,
        node: tree_sitter::Node<'a>,
    ) -> Option<(tree_sitter::Node<'a>, tree_sitter::Node<'a>, String)> {
        let (mut owner, property_node, property) = match node.kind() {
            "property_declaration" => {
                let variable = kt::child(node, "variable_declaration")?;
                let property = self.text(kt::child(variable, "identifier")?).to_string();
                (kt::parent_of(node), node, property)
            }
            "class_parameter" => {
                if kt::child(node, "val").is_none() && kt::child(node, "var").is_none() {
                    return None;
                }
                let property = self.text(kt::child(node, "identifier")?).to_string();
                (kt::parent_of(node), node, property)
            }
            _ => return None,
        };
        while let Some(current) = owner {
            if matches!(
                current.kind(),
                "class_declaration"
                    | "enum_class_declaration"
                    | "object_declaration"
                    | "companion_object"
            ) {
                return Some((current, property_node, property));
            }
            owner = kt::parent_of(current);
        }
        None
    }

    fn is_kotlin_jvm_static_annotation(&self, annotation: tree_sitter::Node) -> bool {
        let Some(name) = annotation_fq_name(annotation, self.source) else {
            return false;
        };
        if name == "kotlin.jvm.JvmStatic" {
            return true;
        }
        if name != "JvmStatic" && !self.source.contains("kotlin.jvm.JvmStatic") {
            return false;
        }
        let declaring = self.workspace_file.as_deref().unwrap_or(self.file);
        if self.semantic_provider.is_some_and(|provider| {
            let symbols = provider.symbols_in_file(declaring);
            symbols.first().is_some_and(|symbol| {
                !matches!(
                    provider.resolve_type_in_file(declaring, &symbol.id.package, &name),
                    crate::semantics::FactStatus::Unknown
                )
            })
        }) {
            return false;
        }
        if self.workspace.is_some_and(|workspace| {
            workspace
                .source_file(declaring)
                .is_some_and(|file| workspace.resolve_type(file, &name).is_some())
        }) {
            // A real source annotation with the same spelling keeps its meaning.
            return false;
        }
        let explicit = self
            .source
            .lines()
            .filter_map(|line| {
                let import = line.trim().strip_prefix("import ")?.trim_end_matches(';');
                let (path, alias) = import
                    .split_once(" as ")
                    .map_or((import, None), |(path, alias)| {
                        (path.trim(), Some(alias.trim()))
                    });
                (alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == name)
                    .then_some(path)
            })
            .collect::<Vec<_>>();
        if !explicit.is_empty() {
            return explicit.len() == 1 && explicit[0] == "kotlin.jvm.JvmStatic";
        }
        name == "JvmStatic"
    }

    /// The `class_parameter` for a primary-constructor property, by name.
    fn param_node(
        &self,
        decl: tree_sitter::Node<'tree>,
        property: &str,
    ) -> Option<tree_sitter::Node<'tree>> {
        let parameters = kt::child(decl, "primary_constructor")
            .and_then(|ctor| kt::child(ctor, "class_parameters"))?;
        let mut cursor = parameters.walk();
        parameters
            .children(&mut cursor)
            .filter(|child| child.kind() == "class_parameter")
            .find(|child| {
                kt::child(*child, "identifier").is_some_and(|name| self.text(name) == property)
            })
    }

    /// Annotations for a constructor property that belong on the FIELD.
    fn param_annotations(&self, decl: tree_sitter::Node, property: &str) -> Vec<String> {
        self.param_node(decl, property)
            .map(|parameter| {
                self.parameter_annotations(parameter)
                    .into_iter()
                    .filter(|(target, annotation)| {
                        !annotation.contains("JvmField")
                            && match target.as_deref() {
                                Some("get" | "param") => false,
                                Some("field") => true,
                                _ => {
                                    self.java_annotation_allows(annotation, "PARAMETER")
                                        != Some(true)
                                }
                            }
                    })
                    .map(|(_, annotation)| annotation)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Untargeted Java annotations that Kotlin places on the primary
    /// constructor parameter because their `@Target` admits PARAMETER, plus
    /// explicit `@param:` annotations.
    fn param_parameter_annotations(&self, decl: tree_sitter::Node, property: &str) -> Vec<String> {
        self.param_node(decl, property)
            .map(|parameter| {
                self.parameter_annotations(parameter)
                    .into_iter()
                    .filter(|(target, annotation)| match target.as_deref() {
                        Some("param") => true,
                        Some(_) => false,
                        None => self.java_annotation_allows(annotation, "PARAMETER") == Some(true),
                    })
                    .map(|(_, annotation)| annotation)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn parameter_annotations(&self, parameter: tree_sitter::Node) -> Vec<(Option<String>, String)> {
        let mut annotations = Vec::new();
        let mut stack = vec![parameter];
        while let Some(current) = stack.pop() {
            for child in current.children(&mut current.walk()) {
                if child.kind() == "annotation" {
                    if self.text(child).contains("notlinProperty") {
                        continue;
                    }
                    if let Some(annotation) = self.transpile_declaration_annotation(child) {
                        annotations.push((
                            use_site_target(self.text(child)).map(str::to_string),
                            annotation,
                        ));
                    }
                } else {
                    stack.push(child);
                }
            }
        }
        annotations
    }

    fn java_annotation_allows(&self, annotation: &str, element_type: &str) -> Option<bool> {
        let workspace = self.workspace?;
        let context = workspace.source_file(self.workspace_file.as_deref().unwrap_or(self.file))?;
        let name = annotation
            .trim_start_matches('@')
            .split(|ch: char| ch == '(' || ch.is_whitespace())
            .next()?;
        workspace.java_annotation_allows(context, name, element_type)
    }

    fn java_constructor_parameter(
        &self,
        decl: tree_sitter::Node,
        name: &str,
        java_type: &str,
        prefix: &str,
    ) -> String {
        let annotations = self.param_parameter_annotations(decl, name);
        let annotations = if annotations.is_empty() {
            String::new()
        } else {
            format!("{} ", annotations.join(" "))
        };
        let parameter_type = self.constructor_parameter_java_type(decl, name, java_type);
        format!("{annotations}{prefix}{parameter_type} {name}")
    }

    fn jackson_default_factory_supported(
        &self,
        decl: tree_sitter::Node,
        params: &[(bool, bool, String, String)],
    ) -> bool {
        let defaults = self.class_param_default_nodes(decl);
        if !defaults.iter().any(Option::is_some) || kt::child(decl, "type_parameters").is_some() {
            return false;
        }
        if self.jackson_default_deserializer_name(decl).is_some() {
            return false;
        }
        // Only introduce a Jackson dependency where the same source set already
        // imports its creator API. Another module's test dependency is insufficient.
        fn source_set(path: &std::path::Path) -> &std::path::Path {
            path.ancestors()
                .find(|ancestor| {
                    ancestor.parent().and_then(std::path::Path::file_name)
                        == Some(std::ffi::OsStr::new("src"))
                })
                .unwrap_or_else(|| path.parent().unwrap_or(path))
        }
        let file = self.workspace_file.as_deref().unwrap_or(self.file);
        if !self.workspace.is_some_and(|workspace| {
            let imports = || {
                workspace
                    .files
                    .iter()
                    .filter(|candidate| source_set(&candidate.path) == source_set(file))
                    .flat_map(|candidate| &candidate.imports)
            };
            imports().any(|import| {
                matches!(
                    import.as_str(),
                    "com.fasterxml.jackson.annotation.JsonCreator"
                        | "com.fasterxml.jackson.annotation.*"
                )
            }) && imports().any(|import| {
                matches!(
                    import.as_str(),
                    "com.fasterxml.jackson.databind.annotation.JsonDeserialize"
                        | "com.fasterxml.jackson.databind.annotation.*"
                )
            })
        }) {
            return false;
        }
        let mut nodes = vec![decl];
        while let Some(node) = nodes.pop() {
            if node.kind() == "annotation"
                && matches!(
                    annotation_simple_name(self.text(node)),
                    "JsonCreator" | "JsonDeserialize"
                )
            {
                return false;
            }
            nodes.extend(node.named_children(&mut node.walk()));
        }
        // A nullable non-null default needs presence tracking: explicit null is
        // meaningful. Leave that shape to the existing metadata retention gate.
        if params
            .iter()
            .zip(&defaults)
            .any(|((_, _, param, _), default)| {
                default.is_some_and(|value| self.text(value).trim() != "null")
                    && self
                        .param_node(decl, param)
                        .is_some_and(|node| kt::child(node, "nullable_type").is_some())
            })
        {
            return false;
        }
        true
    }

    /// Java constructor parameters cannot distinguish an omitted primitive
    /// from its zero value. A separate Jackson creator boxes defaulted inputs,
    /// applies defaults in declaration order, and calls the ordinary constructor.
    /// The constructor's ABI and Lombok null checks remain unchanged.
    fn emit_jackson_default_factory(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        params: &[(bool, bool, String, String)],
        out: &mut JavaOut,
    ) {
        if let Some(builder) = self.jackson_default_deserializer_name(decl) {
            self.emit_jackson_constructor_defaults(decl, name, params, &builder, out);
            return;
        }
        if !self.jackson_default_factory_supported(decl, params) {
            return;
        }
        let defaults = self.class_param_default_nodes(decl);
        let mut factory_name = "__notlinCreate".to_string();
        while self.class_declares_method(decl, &factory_name) {
            factory_name.push('_');
        }
        let mut parameter_deserializer = "__notlinDefaultedParameter".to_string();
        while self.text(decl).contains(&parameter_deserializer) {
            parameter_deserializer.push('_');
        }
        let factory_params = params
            .iter()
            .zip(&defaults)
            .map(|((_, _, param, ty), default)| {
                let original = self.param_node(decl, param);
                let annotation = original
                    .and_then(|node| {
                        self.parameter_annotations(node)
                            .into_iter()
                            .map(|(_, annotation)| annotation)
                            .find(|annotation| annotation_simple_name(annotation) == "JsonProperty")
                    })
                    .unwrap_or_else(|| {
                        let property = original
                            .and_then(|node| kt::child(node, "identifier"))
                            .map(|identifier| self.text(identifier).trim_matches('`'))
                            .unwrap_or(param);
                        format!("@com.fasterxml.jackson.annotation.JsonProperty(\"{property}\")")
                    });
                let parameter_type = self.constructor_parameter_java_type(decl, param, ty);
                let parameter_type = if default.is_some() {
                    crate::transpiler::types::boxed_name(&parameter_type).unwrap_or(&parameter_type)
                } else {
                    &parameter_type
                };
                let mut annotations = self.param_parameter_annotations(decl, param)
                    .into_iter()
                    .filter(|annotation| annotation_simple_name(annotation) != "JsonProperty")
                    .collect::<Vec<_>>();
                annotations.insert(0, annotation);
                if default.is_some_and(|node| self.text(node).trim() != "null") {
                    annotations.push(format!("@com.fasterxml.jackson.databind.annotation.JsonDeserialize(using = {parameter_deserializer}.class)"));
                }
                format!("{} {parameter_type} {param}", annotations.join(" "))
            })
            .collect::<Vec<_>>()
            .join(", ");
        out.blank();
        out.line("@com.fasterxml.jackson.annotation.JsonCreator");
        out.open(format!(
            "public static {name} {factory_name}({factory_params})"
        ));
        let previous_variables = std::mem::take(&mut self.var_types);
        for (_, _, param, ty) in params {
            self.var_types.insert(param.clone(), ty.clone());
        }
        for ((_, _, param, _), default) in params.iter().zip(defaults) {
            if let Some(default) = default {
                if self.text(default).trim() == "null" {
                    continue;
                }
                let expression = Expr { unit: self }.transpile(default);
                out.line(format!(
                    "if ({param} == null) {{ {param} = {expression}; }}"
                ));
            }
        }
        for (_, _, param, ty) in params {
            if !self.lombok
                && crate::transpiler::types::boxed_name(ty).is_none()
                && self
                    .param_node(decl, param)
                    .is_some_and(|node| kt::child(node, "nullable_type").is_none())
            {
                out.line(format!(
                    "java.util.Objects.requireNonNull({param}, \"{param}\");"
                ));
            }
        }
        self.var_types = previous_variables;
        let arguments = params
            .iter()
            .map(|(_, _, param, _)| param.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        out.line(format!("return new {name}({arguments});"));
        out.close();
        // Jackson invokes getAbsentValue for omitted creator arguments and
        // getNullValue for explicit null. Keep that distinction until the
        // factory applies defaults; primitives retain Jackson's null policy.
        out.blank();
        let helper = r#"public static final class __PARAMETER__ extends com.fasterxml.jackson.databind.JsonDeserializer<Object> implements com.fasterxml.jackson.databind.deser.ContextualDeserializer {
    private final com.fasterxml.jackson.databind.JsonDeserializer<Object> delegate;
    private final boolean primitive;
    public __PARAMETER__() { this(null, false); }
    private __PARAMETER__(com.fasterxml.jackson.databind.JsonDeserializer<Object> delegate, boolean primitive) {
        this.delegate = delegate;
        this.primitive = primitive;
    }

    @Override
    public com.fasterxml.jackson.databind.JsonDeserializer<?> createContextual(com.fasterxml.jackson.databind.DeserializationContext context, com.fasterxml.jackson.databind.BeanProperty property) throws com.fasterxml.jackson.databind.JsonMappingException {
        Class<?> raw = property.getType().getRawClass();
        Class<?> primitiveType = raw == java.lang.Boolean.class ? boolean.class
            : raw == java.lang.Byte.class ? byte.class
            : raw == java.lang.Short.class ? short.class
            : raw == java.lang.Integer.class ? int.class
            : raw == java.lang.Long.class ? long.class
            : raw == java.lang.Float.class ? float.class
            : raw == java.lang.Double.class ? double.class
            : raw == java.lang.Character.class ? char.class : null;
        com.fasterxml.jackson.databind.JavaType type = primitiveType == null ? property.getType() : context.constructType(primitiveType);
        return new __PARAMETER__(context.findContextualValueDeserializer(type, property), primitiveType != null);
    }
    @Override
    public Object deserialize(com.fasterxml.jackson.core.JsonParser parser, com.fasterxml.jackson.databind.DeserializationContext context) throws java.io.IOException {
        return delegate.deserialize(parser, context);
    }
    @Override
    public Object getAbsentValue(com.fasterxml.jackson.databind.DeserializationContext context) { return null; }
    @Override
    public Object getNullValue(com.fasterxml.jackson.databind.DeserializationContext context) throws com.fasterxml.jackson.databind.JsonMappingException {
        if (primitive) return delegate.getNullValue(context);
        return context.reportInputMismatch(Object.class, "Null is not allowed for a defaulted non-null property");
    }
}"#.replace("__PARAMETER__", &parameter_deserializer);
        for line in helper.lines() {
            out.line(line);
        }
    }

    fn jackson_default_deserializer_name(&self, decl: tree_sitter::Node) -> Option<String> {
        let file = self.workspace_file.as_deref().unwrap_or(self.file);
        let supported = self.semantic_provider.is_some_and(|provider| {
            provider.symbols_in_file(file).iter().any(|symbol| {
                symbol.location.start_byte == decl.start_byte()
                    && symbol.location.end_byte == decl.end_byte()
                    && symbol.location.snapshot_hash == self.source_hash
                    && provider.has_jackson_default_factory(&symbol.id)
            })
        });
        if !supported {
            return None;
        }
        let mut name = "__notlinDefaults".to_owned();
        while self.text(decl).contains(&name) {
            name.push('_');
        }
        Some(name)
    }

    /// Keep Jackson's creator-property binding intact. The instantiator hook
    /// sees its presence buffer, supplies only omitted Kotlin defaults, then
    /// invokes the ordinary constructor. Parameter annotations and mapper
    /// deserializers remain attached to that constructor's real arguments.
    fn emit_jackson_constructor_defaults(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        params: &[(bool, bool, String, String)],
        deserializer: &str,
        out: &mut JavaOut,
    ) {
        let defaults = self.class_param_default_nodes(decl);
        let values = format!("{deserializer}Values");
        let present = format!("{deserializer}Present");
        let create = format!("{deserializer}Create");
        out.blank();
        out.line("@SuppressWarnings(\"unchecked\")");
        out.open(format!(
            "private static {name} {create}(Object[] {values}, boolean[] {present})"
        ));
        // Validate supplied non-null values and missing required references
        // before default expressions, just as Kotlin's argument binding does.
        for (index, ((_, _, param, ty), default)) in params.iter().zip(&defaults).enumerate() {
            if crate::transpiler::types::boxed_name(ty).is_none()
                && self
                    .param_node(decl, param)
                    .is_some_and(|node| kt::child(node, "nullable_type").is_none())
            {
                let check =
                    format!("java.util.Objects.requireNonNull({values}[{index}], \"{param}\");");
                if default.is_some() {
                    out.line(format!("if ({present}[{index}]) {{ {check} }}"));
                } else {
                    out.line(check);
                }
            }
        }
        let previous = std::mem::take(&mut self.var_types);
        for (index, ((_, _, param, ty), default)) in params.iter().zip(&defaults).enumerate() {
            let ty = self.constructor_parameter_java_type(decl, param, ty);
            let cast = crate::transpiler::types::cast_type(&ty);
            let cast = crate::transpiler::types::boxed_name(&cast).unwrap_or(&cast);
            let provided = format!("({cast}) {values}[{index}]");
            let value = if let Some(default) = default {
                let expression = Expr { unit: self }.transpile(*default);
                format!("{present}[{index}] ? {provided} : {expression}")
            } else {
                provided
            };
            out.line(format!("{ty} {param} = {value};"));
            self.var_types.insert(param.clone(), ty);
        }
        out.line(format!(
            "return new {name}({});",
            params
                .iter()
                .map(|(_, _, param, _)| param.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        self.var_types = previous;
        out.close();
        out.blank();
        // Annotation deserializers resolve before Jackson's incomplete-cache
        // registration. Share recursive resolution only on this thread and
        // context, then clear it in finally. Prefetch contexts have no per-call
        // attribute storage, so DeserializationContext attributes cannot hold it.
        let helper = r#"public static final class __DESER__ extends com.fasterxml.jackson.databind.JsonDeserializer<__DTO__> implements com.fasterxml.jackson.databind.deser.ResolvableDeserializer, com.fasterxml.jackson.databind.deser.ContextualDeserializer {
    private static final ThreadLocal<java.util.IdentityHashMap<com.fasterxml.jackson.databind.DeserializationContext, com.fasterxml.jackson.databind.JsonDeserializer<Object>>> RESOLVING = new ThreadLocal<>();
    private com.fasterxml.jackson.databind.JsonDeserializer<Object> delegate;
    private boolean cacheable;
    public __DESER__() {}
    private __DESER__(com.fasterxml.jackson.databind.JsonDeserializer<Object> delegate) { this.delegate = delegate; }
    @Override
    public Class<?> handledType() { return __DTO__.class; }
    @Override
    public boolean isCachable() { return cacheable; }
    @Override
    public void resolve(com.fasterxml.jackson.databind.DeserializationContext context) throws com.fasterxml.jackson.databind.JsonMappingException {
        if (delegate != null) return;
        var resolving = RESOLVING.get();
        com.fasterxml.jackson.databind.JsonDeserializer<Object> active = resolving == null ? null : resolving.get(context);
        if (active != null) { delegate = active; return; }
        com.fasterxml.jackson.databind.JavaType type = context.constructType(__DTO__.class);
        com.fasterxml.jackson.databind.deser.DeserializerFactory factory = context.getFactory().withValueInstantiators(new com.fasterxml.jackson.databind.deser.ValueInstantiators.Base() {
            @Override
            public com.fasterxml.jackson.databind.deser.ValueInstantiator findValueInstantiator(com.fasterxml.jackson.databind.DeserializationConfig config, com.fasterxml.jackson.databind.BeanDescription bean, com.fasterxml.jackson.databind.deser.ValueInstantiator original) {
                return bean.getBeanClass() == __DTO__.class ? new __DESER__Instantiator(original) : original;
            }
        });
        delegate = factory.createBeanDeserializer(context, type, context.getConfig().introspect(type));
        if (resolving == null) { resolving = new java.util.IdentityHashMap<>(); RESOLVING.set(resolving); }
        resolving.put(context, delegate);
        try {
            if (delegate instanceof com.fasterxml.jackson.databind.deser.ResolvableDeserializer resolvable) resolvable.resolve(context);
            cacheable = true;
        } finally {
            resolving.remove(context);
            if (resolving.isEmpty()) RESOLVING.remove();
        }
    }
    @Override
    @SuppressWarnings("unchecked")
    public com.fasterxml.jackson.databind.JsonDeserializer<?> createContextual(com.fasterxml.jackson.databind.DeserializationContext context, com.fasterxml.jackson.databind.BeanProperty property) throws com.fasterxml.jackson.databind.JsonMappingException {
        return new __DESER__((com.fasterxml.jackson.databind.JsonDeserializer<Object>) context.handleSecondaryContextualization(delegate, property, context.constructType(__DTO__.class)));
    }
    @Override
    public __DTO__ deserialize(com.fasterxml.jackson.core.JsonParser parser, com.fasterxml.jackson.databind.DeserializationContext context) throws java.io.IOException {
        return (__DTO__) delegate.deserialize(parser, context);
    }
    @Override
    public __DTO__ deserialize(com.fasterxml.jackson.core.JsonParser parser, com.fasterxml.jackson.databind.DeserializationContext context, __DTO__ value) throws java.io.IOException {
        return (__DTO__) delegate.deserialize(parser, context, value);
    }
    @Override
    public Object deserializeWithType(com.fasterxml.jackson.core.JsonParser parser, com.fasterxml.jackson.databind.DeserializationContext context, com.fasterxml.jackson.databind.jsontype.TypeDeserializer type) throws java.io.IOException {
        return delegate.deserializeWithType(parser, context, type);
    }
    @Override
    public com.fasterxml.jackson.databind.deser.impl.ObjectIdReader getObjectIdReader() { return delegate == null ? null : delegate.getObjectIdReader(); }
    @Override
    public com.fasterxml.jackson.databind.deser.SettableBeanProperty findBackReference(String name) { return delegate.findBackReference(name); }
}
private static final class __DESER__Instantiator extends com.fasterxml.jackson.databind.deser.ValueInstantiator.Delegating {
    private __DESER__Instantiator(com.fasterxml.jackson.databind.deser.ValueInstantiator delegate) { super(delegate); }
    @Override
    public Object createFromObjectWith(com.fasterxml.jackson.databind.DeserializationContext context, com.fasterxml.jackson.databind.deser.SettableBeanProperty[] properties, com.fasterxml.jackson.databind.deser.impl.PropertyValueBuffer buffer) throws java.io.IOException {
        if (properties.length != __COUNT__) return super.createFromObjectWith(context, properties, buffer);
        Object[] values = new Object[__COUNT__];
        boolean[] present = new boolean[__COUNT__];
        boolean[] defaulted = new boolean[] {__DEFAULTED__};
        for (int index = 0; index < __COUNT__; index++) {
            present[index] = buffer.hasParameter(properties[index]);
            if (present[index] || !defaulted[index]) values[index] = buffer.getParameter(properties[index]);
        }
        return __CREATE__(values, present);
    }
    @Override
    public Object createFromObjectWith(com.fasterxml.jackson.databind.DeserializationContext context, Object[] values) throws java.io.IOException {
        boolean[] present = new boolean[__COUNT__];
        java.util.Arrays.fill(present, true);
        return __CREATE__(values, present);
    }
}"#.replace("__DESER__", deserializer).replace("__DTO__", name)
            .replace("__CREATE__", &create).replace("__COUNT__", &params.len().to_string())
            .replace("__DEFAULTED__", &defaults.iter().map(|default| default.is_some().to_string()).collect::<Vec<_>>().join(", "));
        for line in helper.lines() {
            out.line(line);
        }
    }

    /// Kotlin read-only collections are covariant at call boundaries, but a
    /// constructor property still exposes an invariant getter. Keep the field
    /// and getter type exact, widening only the Java constructor parameter.
    fn constructor_parameter_java_type(
        &self,
        decl: tree_sitter::Node,
        name: &str,
        field_type: &str,
    ) -> String {
        let Some(parameter) = self.param_node(decl, name) else {
            return field_type.to_string();
        };
        if kt::child(parameter, "var").is_some() {
            return field_type.to_string();
        }
        let Some(type_node) =
            kt::child(parameter, "user_type").or_else(|| kt::child(parameter, "nullable_type"))
        else {
            return field_type.to_string();
        };
        crate::transpiler::types::covariant_readonly_parameter(self.text(type_node), field_type)
    }

    /// Assigning a covariant Java constructor argument to Kotlin's invariant
    /// property type needs an unchecked generic cast. The cast is erased and
    /// keeps the same collection object, matching Kotlin's declaration-site
    /// covariance without copying or widening the getter signature.
    fn constructor_property_assignment(
        &self,
        decl: tree_sitter::Node,
        name: &str,
        field_type: &str,
    ) -> String {
        let parameter_type = self.constructor_parameter_java_type(decl, name, field_type);
        if parameter_type == field_type {
            name.to_string()
        } else {
            let cast_type = crate::transpiler::types::cast_type(field_type);
            format!("({cast_type}) (Object) {name}")
        }
    }

    fn constructor_has_covariant_input(
        &self,
        decl: tree_sitter::Node,
        params: &[(bool, bool, String, String)],
    ) -> bool {
        params.iter().any(|(_, _, name, field_type)| {
            self.constructor_parameter_java_type(decl, name, field_type) != field_type.as_str()
        })
    }

    /// Annotations for a constructor property that belong on the GETTER —
    /// `@get:`-targeted ones, which say so themselves.
    fn param_getter_annotations(&self, decl: tree_sitter::Node, property: &str) -> Vec<String> {
        self.param_node(decl, property)
            .map(|parameter| self.annotation_split(parameter).1)
            .unwrap_or_default()
    }

    fn param_setter_annotations(&self, decl: tree_sitter::Node, property: &str) -> Vec<String> {
        let Some(parameter) = self.param_node(decl, property) else {
            return Vec::new();
        };
        let Some((owner, property_node, name)) = self.property_annotation_owner(parameter) else {
            return Vec::new();
        };
        self.property_repair_bridge_annotations(&owner, property_node, &name, false)
    }

    fn class_body_declares_instance_fields(&self, decl: tree_sitter::Node) -> bool {
        let Some(body) = kt::child(decl, "class_body") else {
            return false;
        };
        let mut cursor = body.walk();
        body.children(&mut cursor).any(|member| {
            matches!(
                member.kind(),
                "property_declaration" | "variable_declaration"
            )
        })
    }

    fn class_body_defines_lombok_generated(&self, decl: tree_sitter::Node) -> bool {
        let Some(body) = kt::child(decl, "class_body") else {
            return false;
        };
        body.children(&mut body.walk())
            .filter(|m| m.kind() == "function_declaration")
            .any(|m| {
                kt::field(m, "name")
                    .map(|n| matches!(self.text(n), "equals" | "hashCode" | "toString"))
                    .unwrap_or(false)
            })
    }

    /// Primary constructor parameters -> (is_property, is_mutable, name,
    /// java_type). A parameter WITHOUT `val`/`var` is a plain constructor
    /// argument in Kotlin: it forwards to the superclass and exists only
    /// during initialization — it must NOT become a Java field.
    /// Shared by the class/enum/record paths so ctor-param handling stays in
    /// one place.
    fn class_params(&mut self, decl: tree_sitter::Node) -> Vec<(bool, bool, String, String)> {
        kt::child(decl, "primary_constructor")
            .and_then(|pc| kt::child(pc, "class_parameters"))
            .map(|cps| {
                let mut cursor = cps.walk();
                cps.children(&mut cursor)
                    .filter(|c| c.kind() == "class_parameter")
                    .filter_map(|cp| {
                        let is_property = kt::child(cp, "val").is_some()
                            || kt::child(cp, "var").is_some();
                        let is_mutable = kt::child(cp, "var").is_some();
                        let ident = kt::child(cp, "identifier")?;
                        let ty = kt::child(cp, "user_type")
                            .or_else(|| kt::child(cp, "nullable_type"))
                            .or_else(|| kt::child(cp, "function_type"))
                            .or_else(|| kt::child(cp, "parenthesized_type"));
                        let ty_java = match ty {
                            Some(t) => {
                                let t_java = kt::java_type_ann(t, self.source, self.annots);
                                if t_java == crate::transpiler::types::FUNCTION_TYPE_PLACEHOLDER {
                                    self.diag_untranslatable(
                                        t,
                                        format!(
                                            "function type on param '{}' has no Java counterpart (functional-interface mapping not implemented)",
                                            self.text(ident)
                                        ),
                                    );
                                    "Object".to_string()
                                } else {
                                    t_java
                                }
                            }
                            None => {
                                // Param with unrecognized type shape: flag it
                                // instead of silently dropping the field.
                                self.diags.push(crate::diagnostics::Diagnostic {
                                    severity: crate::diagnostics::Severity::Warning,
                                    kind: DiagnosticKind::Approximated,
                                    message: format!(
                                        "primary-ctor param '{}' has unsupported type shape ({}); emitted as Object",
                                        self.text(ident),
                                        cp.kind()
                                    ),
                                    file: self.file.to_path_buf(),
                                    line: cp.start_position().row + 1,
                                    col: cp.start_position().column + 1,
                                    code: None,
                                });
                                "Object".to_string()
                            }
                        };
                        // A field/param cannot be `void`: Kotlin `Unit`
                        // members become boxed `Void` on the Java side.
                        let ty_java = if ty_java == "void" {
                            "Void".to_string()
                        } else {
                            ty_java
                        };
                        // Java reserved words (e.g. Kotlin `default`) cannot
                        // name a field: append '_' for the Java side. Accessor
                        // names derive from the escaped name, so the class is
                        // self-consistent; retained Kotlin callers relying on
                        // the exact bean name surface next compilation pass.
                        let param_name = self.text(ident).to_string();
                        let param_name = if java_reserved(param_name.as_str()) {
                            format!("{param_name}_")
                        } else {
                            param_name
                        };
                        Some((is_property, is_mutable, param_name, ty_java))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn class_param_default_nodes<'node>(
        &self,
        decl: tree_sitter::Node<'node>,
    ) -> Vec<Option<tree_sitter::Node<'node>>> {
        kt::child(decl, "primary_constructor")
            .and_then(|pc| kt::child(pc, "class_parameters"))
            .map(|cps| {
                let mut cursor = cps.walk();
                cps.children(&mut cursor)
                    .filter(|c| c.kind() == "class_parameter")
                    .map(|cp| {
                        let has_default = cp
                            .children(&mut cp.walk())
                            .any(|c| !c.is_named() && c.kind() == "=");
                        has_default
                            .then(|| cp.children(&mut cp.walk()).filter(|c| c.is_named()).last())
                            .flatten()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Transpiled default expressions for primary-ctor parameters, aligned
    /// by index (`None` where the parameter has no default). Used to fill
    /// enum-constant call sites, since Java has no default arguments.
    fn class_param_defaults(&mut self, decl: tree_sitter::Node) -> Vec<Option<String>> {
        // All consumers (overloads, enum entries, JPA constructors and creators)
        // lower defaults before an instance exists. Earlier parameters are
        // local values, with their declared types, rather than member reads.
        let parameters = self.class_params(decl);
        let prior = std::mem::replace(
            &mut self.var_types,
            parameters
                .into_iter()
                .map(|(_, _, name, ty)| (name, ty))
                .collect(),
        );
        let result = self
            .class_param_default_nodes(decl)
            .into_iter()
            .map(|default| default.map(|ex| Expr { unit: self }.transpile(ex)))
            .collect();
        self.var_types = prior;
        result
    }

    /// `@JvmOverloads` on the primary constructor: the annotation that asks
    /// for Java overloads of the omitted trailing defaults.
    fn primary_constructor_has_jvm_overloads(&self, decl: tree_sitter::Node) -> bool {
        kt::child(decl, "primary_constructor")
            .and_then(|constructor| kt::child(constructor, "modifiers"))
            .is_some_and(|modifiers| {
                modifiers
                    .children(&mut modifiers.walk())
                    .filter(|node| node.kind() == "annotation")
                    .any(|annotation| self.text(annotation).contains("JvmOverloads"))
            })
    }

    /// The declaration the index holds for `name` in this file, when there is
    /// one. The retention rule and the emitter both read the INDEX's record of
    /// the constructor — parameter names, default texts — instead of re-reading
    /// the syntax, so the two cannot disagree about what a call site's shape is.
    fn indexed_target(&self, name: &str) -> Option<&crate::workspace::Declaration> {
        let workspace = self.workspace?;
        let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
        let source_file = workspace.source_file(indexed_path)?;
        source_file
            .declarations
            .iter()
            .find(|declaration| declaration.name == name)
    }

    fn constructor_overload_patterns(
        defaults: &[Option<String>],
        has_jvm_overloads: bool,
        in_place: bool,
    ) -> Vec<Vec<usize>> {
        if has_jvm_overloads {
            crate::ctor_defaults::jvm_overload_patterns(defaults)
        } else if in_place {
            crate::ctor_defaults::trailing_patterns(defaults)
        } else {
            Vec::new()
        }
    }

    /// Whether the delegating overloads for this declaration's default
    /// arguments can be written, and which omission patterns need one.
    ///
    /// Reads the index's omission evidence for the declaration: a pattern no
    /// caller uses costs nothing, so only the shapes call sites actually write
    /// become constructors. `blocked` is why none can be written.
    fn ctor_default_plan(
        &self,
        decl: tree_sitter::Node<'_>,
        target: &crate::workspace::Declaration,
    ) -> crate::ctor_defaults::CtorDefaultPlan {
        let defaults = &target.constructor_param_defaults;
        // The ladder is what the emitter writes for Kotlin's own positional ABI
        // (any in-place run) or for an explicit `@JvmOverloads`; the plan must
        // not write it twice.
        let has_jvm_overloads = self.primary_constructor_has_jvm_overloads(decl);
        let ladder =
            Self::constructor_overload_patterns(defaults, has_jvm_overloads, self.in_place);
        let patterns = self
            .workspace
            .map(|workspace| workspace.ctor_omission_evidence(target).patterns)
            .unwrap_or_default();
        let param_types = crate::ctor_defaults::class_param_types(decl, self.source);
        crate::ctor_defaults::plan_ctor_defaults(&crate::ctor_defaults::CtorShape {
            param_names: &target.constructor_param_names,
            defaults,
            param_types: &param_types,
            patterns: &patterns,
            ladder: &ladder,
            has_secondary_constructor: target
                .members
                .iter()
                .any(|member| member.kind == crate::workspace::MemberKind::Constructor),
        })
    }

    /// `@JvmOverloads` exposes Java overloads by omitting defaulted
    /// primary-constructor parameters from right to left, keeping required ones.
    /// Emit those overloads to preserve its Java ABI.
    ///
    /// Two sets of overloads are written, and only these:
    ///
    /// - the annotation ladder, or trailing defaults for an in-place run: Kotlin's
    ///   own positional constructor ABI, which a retained caller may use;
    /// - one overload per omission pattern a call site actually writes
    ///   ([`Self::ctor_default_plan`]) — a named-argument call omitting a middle
    ///   parameter, or one whose default cannot be written into the call.
    ///
    /// Never every subset: middle defaults add no overloads unless an
    /// annotation or an observed caller requests them.
    fn emit_jvm_overloads_primary_constructors(
        &mut self,
        decl: tree_sitter::Node<'_>,
        name: &str,
        params: &[(bool, bool, String, String)],
        out: &mut JavaOut,
    ) {
        if params.is_empty() {
            return;
        }
        // Only a defaulted parameter can be delegated, and only a call site that
        // omits one needs an overload. Without either there is nothing to write —
        // and nothing to lower, which matters because lowering a default
        // expression can carry diagnostics.
        let has_jvm_overloads = self.primary_constructor_has_jvm_overloads(decl);
        let ladder_requested = self.in_place || has_jvm_overloads;
        let has_defaults = self
            .indexed_target(name)
            .is_some_and(|target| target.has_default_constructor_parameter);
        if !has_defaults && !ladder_requested {
            return;
        }
        let defaults = self.class_param_defaults(decl);
        let mut patterns =
            Self::constructor_overload_patterns(&defaults, has_jvm_overloads, self.in_place);
        let ladder_patterns = patterns.clone();
        let target = self.indexed_target(name);
        let secondary_arities = target
            .map(|target| {
                target
                    .secondary_ctors
                    .iter()
                    .map(Vec::len)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(target) = target {
            let plan = self.ctor_default_plan(decl, target);
            if plan.blocked.is_none() {
                patterns.extend(plan.overloads);
            }
        }
        for pattern in &patterns {
            // Defensive: a pattern the index derived must line up with the
            // parameters the emitter lowered, and every omitted parameter must
            // have a lowered default to delegate with.
            if pattern.iter().any(|index| *index >= params.len())
                || pattern
                    .iter()
                    .any(|index| defaults.get(*index).is_none_or(Option::is_none))
            {
                continue;
            }
            let signature = (0..params.len())
                .filter(|index| !pattern.contains(index))
                .map(|index| {
                    let (_, _, param_name, param_type) = &params[index];
                    let parameter_type =
                        self.constructor_parameter_java_type(decl, param_name, param_type);
                    format!("{parameter_type} {param_name}")
                })
                .collect::<Vec<_>>()
                .join(", ");
            // Each trailing-default or @JvmOverloads rung supplies the next default in declaration
            // order, then delegates to the larger rung. A later default can
            // therefore use an earlier omitted parameter without evaluating it
            // twice or referencing a name absent from this overload.
            let ladder_rung = ladder_patterns
                .iter()
                .position(|candidate| candidate == pattern);
            let delegate_omissions = ladder_rung
                .and_then(|index| index.checked_sub(1))
                .map(|index| ladder_patterns[index].as_slice())
                .unwrap_or_default();
            let delegate_arity = params.len() - delegate_omissions.len();
            let competing_constructor = patterns.iter().any(|candidate| {
                candidate.len() == delegate_omissions.len()
                    && candidate.as_slice() != delegate_omissions
            }) || secondary_arities.contains(&delegate_arity);
            let values = (0..params.len())
                .filter(|index| !delegate_omissions.contains(index))
                .map(|index| {
                    if pattern.contains(&index) {
                        let value = defaults[index]
                            .clone()
                            .unwrap_or_else(|| "null".to_string());
                        // Kotlin delegates to the primary constructor. A bare
                        // null can select several Java reference overloads.
                        if ladder_rung.is_some() && competing_constructor {
                            // A default's expression can have a narrower type
                            // than the parameter. Anchor overload selection to
                            // the intended rung, including around secondary
                            // constructors accepting that narrower type.
                            let cast_type = self.constructor_parameter_java_type(
                                decl,
                                &params[index].2,
                                &params[index].3,
                            );
                            let cast_type = crate::transpiler::types::cast_type(&cast_type);
                            if value.trim() == "null" {
                                format!("({cast_type}) null")
                            } else {
                                format!("({cast_type}) ({value})")
                            }
                        } else if value.trim() == "null"
                            && secondary_arities.contains(&params.len())
                        {
                            let cast_type = crate::transpiler::types::cast_type(&params[index].3);
                            format!("({cast_type}) null")
                        } else {
                            value
                        }
                    } else {
                        params[index].2.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.blank();
            out.open(format!("public {}({})", name, signature));
            out.line(format!("this({});", values));
            out.close();
        }
    }

    /// Whether the emitter writes a zero-argument constructor for this class
    /// for a reason OTHER than the JPA rule: every primary-constructor
    /// parameter has a default and the delegating overloads are being written
    /// (`@JvmOverloads`, or an in-place run that keeps Kotlin's default
    /// arguments callable). Kotlin has that constructor too — so the JPA rule
    /// must not add a second one, and its body (a `this(...)` delegation) is
    /// the one Kotlin's own bytecode holds.
    fn emits_zero_arg_constructor_overload(
        &self,
        decl: tree_sitter::Node,
        params: &[(bool, bool, String, String)],
        all_params_defaulted: bool,
    ) -> bool {
        !params.is_empty()
            && all_params_defaulted
            && (self.in_place || self.primary_constructor_has_jvm_overloads(decl))
    }

    /// An explicit `constructor()` (zero parameters) already gives the class
    /// the constructor JPA needs. Measured: with one declared, kotlinc's
    /// no-arg plugin emits exactly one zero-argument constructor (the declared
    /// one delegating to the primary), not two.
    fn declares_zero_arg_secondary_constructor(&self, decl: tree_sitter::Node) -> bool {
        let Some(body) = kt::child(decl, "class_body") else {
            return false;
        };
        body.children(&mut body.walk())
            .filter(|member| member.kind() == "secondary_constructor")
            .any(
                |constructor| match kt::child(constructor, "function_value_parameters") {
                    Some(parameters) => !parameters
                        .children(&mut parameters.walk())
                        .any(|child| child.kind() == "parameter"),
                    None => true,
                },
            )
    }

    /// (field name, JVM default literal) for every instance field the class
    /// emitter writes for `decl`: the primary-constructor properties, plus the
    /// class-body properties that emit a backing field AND declare their type.
    ///
    /// A body property whose type is only inferred is skipped: property.rs
    /// resolves those through a cross-file/inherited-type path this pass does
    /// not reproduce, and guessing a primitive wrong would emit `null` into an
    /// `int` field. Skipping costs fidelity (that field keeps its Java
    /// initializer where the plugin's constructor would leave it default);
    /// guessing costs the compile.
    ///
    /// The field predicate is SHARED with property.rs — a property that emits
    /// no field must not be assigned here.
    fn jpa_instance_field_defaults(
        &mut self,
        decl: tree_sitter::Node,
        params: &[(bool, bool, String, String)],
    ) -> Vec<(String, String)> {
        let mut fields: Vec<(String, String)> = params
            .iter()
            .filter(|(is_property, _, _, _)| *is_property)
            .map(|(_, _, name, java_type)| {
                (name.clone(), jvm_default_literal(java_type).to_string())
            })
            .collect();
        let Some(body) = kt::child(decl, "class_body") else {
            return fields;
        };
        let mut cursor = body.walk();
        let members: Vec<tree_sitter::Node> = body.children(&mut cursor).collect();
        for member in members {
            if member.kind() != "property_declaration" || !self.property_emits_backing_field(member)
            {
                continue;
            }
            let Some(variable) = kt::child(member, "variable_declaration") else {
                continue;
            };
            let declared =
                kt::child(variable, "user_type").or_else(|| kt::child(variable, "nullable_type"));
            let (Some(declared), Some(identifier)) = (declared, kt::child(variable, "identifier"))
            else {
                continue;
            };
            let java_type = kt::java_type_ann(declared, self.source, self.annots);
            fields.push((
                self.text(identifier).to_string(),
                jvm_default_literal(&java_type).to_string(),
            ));
        }
        fields
    }

    /// Kotlin's `kotlin-jpa` plugin (= the `no-arg` plugin with the JPA
    /// preset) adds a zero-argument constructor to every @Entity/@Embeddable/
    /// @MappedSuperclass class, so the ORM can instantiate it by reflection.
    /// Nothing in the source spells that constructor out, and the flag that
    /// turns it on lives in the BUILD FILE — not in the AST — so the Java side
    /// has to synthesize it from the annotation alone.
    ///
    /// Measured on the reference module's own bytecode (kotlinc 2.3.21 +
    /// `org.jetbrains.kotlin.plugin.jpa`, `javap -p -c` on the compiled
    /// entities):
    ///
    /// ```text
    /// public AuthenticatedConnectorRefEntity();
    ///   Code: aload_0
    ///         invokespecial .../NameLookupEntity."<init>":()V
    ///         return
    /// ```
    ///
    /// Three properties of that constructor decide the Java emission:
    ///   * it is `public` (NOT synthetic), so Java source may call it;
    ///   * it runs NO property initializer and no `init` block: the fields come
    ///     out at their JVM defaults, even when the source initializes them
    ///     (`WorkView.items` is null, not an empty list; `WorkEntity.status` is
    ///     null, not `CREATED`). That is the plugin's default
    ///     (`invokeInitializers = false`);
    ///   * it forwards to `super()` with NO arguments, so the superclass must
    ///     itself be zero-arg constructible. kotlinc enforces that — `error:
    ///     zero-argument constructor was not found in the superclass` — and a
    ///     JPA superclass gets its constructor from this SAME rule, which is
    ///     why the emitted `super()` resolves for `: NameLookupEntity(id,
    ///     lookupId)` even though those arguments do not exist in a
    ///     zero-argument constructor.
    ///
    /// Java has no synthetic constructor and does not clear a field on its own,
    /// so the faithful form is an explicit constructor: `super()`, then every
    /// instance field assigned its type's default. `final` fields force the
    /// question — an empty constructor body would not compile.
    fn emit_jpa_no_arg_constructor(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        params: &[(bool, bool, String, String)],
        has_class_supertype: bool,
        out: &mut JavaOut,
    ) {
        if params.is_empty() || self.declares_zero_arg_secondary_constructor(decl) {
            return;
        }
        let defaults = self.class_param_defaults(decl);
        let all_defaulted = defaults.len() == params.len()
            && !defaults.is_empty()
            && defaults.iter().all(|default| default.is_some());
        if self.emits_zero_arg_constructor_overload(decl, params, all_defaulted) {
            return;
        }
        if !out.buf.ends_with("\n\n") {
            out.blank();
        }
        out.open(format!("public {}()", name));
        if all_defaulted {
            // Kotlin gives this class a no-argument constructor of its own
            // (every parameter defaulted), and THAT one delegates to the
            // primary constructor — so its initializers do run. Reproduce
            // Kotlin's constructor, not the plugin's field-clearing form.
            let values: Vec<String> = defaults.into_iter().flatten().collect();
            out.line(format!("this({});", values.join(", ")));
        } else {
            if has_class_supertype {
                out.line("super();");
            }
            for (field, literal) in self.jpa_instance_field_defaults(decl, params) {
                out.line(format!("this.{} = {};", field, literal));
            }
        }
        out.close();
    }

    /// The record form of the same constructor. A record's components are
    /// `final`, so a body that assigns them itself does not exist — the
    /// constructor has to delegate to the canonical one (`this(...)`), passing
    /// each component its type's default. That is the same instance Kotlin's
    /// plugin constructor produces: no initializer runs, so a `val name:
    /// String` component is null and an `Int` component is 0.
    fn emit_jpa_no_arg_record_constructor(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        params: &[(bool, bool, String, String)],
        out: &mut JavaOut,
    ) {
        if params.is_empty() || self.declares_zero_arg_secondary_constructor(decl) {
            return;
        }
        let defaults = self.class_param_defaults(decl);
        let all_defaulted = defaults.len() == params.len()
            && !defaults.is_empty()
            && defaults.iter().all(|default| default.is_some());
        if self.emits_zero_arg_constructor_overload(decl, params, all_defaulted) {
            return;
        }
        let values: Vec<String> = if all_defaulted {
            defaults.into_iter().flatten().collect()
        } else {
            params
                .iter()
                .map(|(_, _, _, java_type)| jvm_default_literal(java_type).to_string())
                .collect()
        };
        out.blank();
        out.open(format!("public {}()", name));
        out.line(format!("this({});", values.join(", ")));
        out.close();
    }

    /// `enum class` -> native Java enum. Constants become enum constants;
    /// primary-ctor params (`val rgb: Int`) become private final fields +
    /// accessors + a private constructor; body members (functions, properties
    /// incl. `get() =` accessor shapes) become enum members.
    ///
    /// Shapes beyond javac's capability TAINT with N001 instead of emitting
    /// broken Java: generic enums, enum superclass delegation (Java enums
    /// implicitly extend Enum), abstract/bodyless enum methods (they require
    /// per-constant bodies the grammar can't even parse), and class modifiers
    /// like `sealed`. Plain supertypes are fine — Java enums may implement
    /// interfaces.
    #[allow(clippy::too_many_arguments)]
    fn transpile_enum(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        visibility: &str,
        modifiers: &str,
        is_sealed: bool,
        annotations: &[String],
        out: &mut JavaOut,
    ) {
        self.enum_types.insert(name.to_string());
        let mut default_property_bridges = Vec::new();
        if let Some(workspace) = self.workspace {
            let declaring = self.workspace_file.as_deref().unwrap_or(self.file);
            default_property_bridges = workspace.enum_default_property_bridges(declaring, name);
            let bridged = default_property_bridges
                .iter()
                .map(|bridge| bridge.method_name.as_str())
                .collect::<HashSet<_>>();
            let obligations = workspace
                .enum_default_obligations(declaring, name)
                .into_iter()
                .filter(|obligation| !bridged.contains(obligation.as_str()))
                .collect::<Vec<_>>();
            if !obligations.is_empty() {
                self.diag_untranslatable(
                    decl,
                    format!(
                        "inherited interface members require explicit Java default bridges: {}",
                        obligations.join(", ")
                    ),
                );
                return;
            }
        }
        // Only visibility + (implicitly final) enum is legal Java. Any other
        // class modifier (sealed/abstract/open) on an enum taints.
        let extra = modifiers.trim();
        if !extra.is_empty() || is_sealed {
            self.diag_untranslatable(
                decl,
                format!(
                    "enum '{}' has class modifier(s) '{}' that Java enums cannot express",
                    name, extra
                ),
            );
            return;
        }
        // Java enums cannot be generic.
        if let Some(tp) = kt::child(decl, "type_parameters") {
            self.diag_untranslatable(
                tp,
                format!(
                    "enum '{}' declares type parameters; Java enums cannot be generic",
                    name
                ),
            );
            return;
        }
        // Supertypes: constructor_invocation would mean `extends` — illegal
        // for a Java enum (implicit Enum). Bare/`by` supertypes are
        // interfaces and can be `implements`.
        let mut implements = String::new();
        if let Some(dc) = kt::child(decl, "delegation_specifiers") {
            let mut ifaces: Vec<String> = Vec::new();
            let mut cursor = dc.walk();
            for spec in dc.children(&mut cursor) {
                if spec.kind() != "delegation_specifier" {
                    continue;
                }
                let inner = spec.children(&mut spec.walk()).find(|c| c.is_named());
                match inner.map(|n| n.kind()) {
                    Some("constructor_invocation") => {
                        self.diag_untranslatable(
                            spec,
                            format!(
                                "enum '{}' extends a superclass; Java enums implicitly extend java.lang.Enum and cannot extend another class",
                                name
                            ),
                        );
                        return;
                    }
                    Some("user_type") | Some("nullable_type") | Some("explicit_delegation") => {
                        let t = self
                            .text(inner.unwrap())
                            .replace(" ", "")
                            .trim_start_matches('@')
                            .to_string();
                        if !t.is_empty() {
                            ifaces.push(t);
                        }
                    }
                    other => {
                        self.diag_untranslatable(
                            spec,
                            format!(
                                "enum '{}' supertype form not supported: {}",
                                name,
                                other.unwrap_or("?")
                            ),
                        );
                        return;
                    }
                }
            }
            if !ifaces.is_empty() {
                implements = format!(" implements {}", ifaces.join(", "));
            }
        }

        let params = self.class_params(decl);
        // Register ctor-param names/types so bodies (`rgb.toString(16)`) get
        // primitive-receiver rewrites and type inference like class fields do.
        // `pending_field_types` re-seeds them inside each member's scope
        // (transpile_function clears var_types per declaration).
        self.pending_field_types = params
            .iter()
            .map(|(_, _, fname, ftype)| (fname.clone(), ftype.clone()))
            .collect();
        // Defaulted ctor params: Java enum constants must pass every
        // trailing argument; a constant that omits a defaulted param gets
        // the default expression transpiled in.
        let param_defaults = self.class_param_defaults(decl);
        let body = kt::child(decl, "enum_class_body").or_else(|| kt::child(decl, "class_body"));

        // Split the body: constants first (Java requires them before any
        // member), then `;`, then members in declaration order.
        let mut entries: Vec<String> = Vec::new();
        let mut members: Vec<tree_sitter::Node> = Vec::new();
        if let Some(body) = body {
            let mut cursor = body.walk();
            for member in body.children(&mut cursor) {
                match member.kind() {
                    "enum_entry" => {
                        let mut e = String::new();
                        let mut has_class_body = false;
                        let mut ec = member.walk();
                        for c in member.children(&mut ec) {
                            match c.kind() {
                                "identifier" => e.push_str(self.text(c)),
                                "class_body" => {
                                    // Per-constant override body: Java enum
                                    // constants accept `NAME { ... }`.
                                    has_class_body = true;
                                    let mut bc = JavaOut::new();
                                    bc.open("");
                                    self.transpile_class_body(c, &mut bc);
                                    bc.close();
                                    e.push_str(&format!(" {}", bc.finish().trim()));
                                }
                                "value_arguments" => {
                                    let mut args: Vec<String> = Vec::new();
                                    let mut ac = c.walk();
                                    for arg in c.children(&mut ac) {
                                        if arg.kind() != "value_argument" {
                                            continue;
                                        }
                                        if let Some(ex) =
                                            arg.children(&mut arg.walk()).find(|x| x.is_named())
                                        {
                                            let mut e2 = Expr { unit: self };
                                            let mut rendered = e2.transpile(ex);
                                            let expects_object =
                                                params.get(args.len()).is_some_and(
                                                    |(_, _, _, ty)| ty.ends_with("Object"),
                                                );
                                            if expects_object && rendered == "() -> {}" {
                                                rendered = "(kotlin.jvm.functions.Function0<kotlin.Unit>) () -> kotlin.Unit.INSTANCE".to_string();
                                            }
                                            args.push(rendered);
                                        }
                                    }
                                    // Fill omitted defaulted parameters so
                                    // the call site compiles in Java.
                                    while args.len() < param_defaults.len() {
                                        match &param_defaults[args.len()] {
                                            Some(default_expr) => {
                                                args.push(default_expr.clone());
                                            }
                                            None => break,
                                        }
                                    }
                                    e.push_str(&format!("({})", args.join(", ")));
                                }
                                _ => {}
                            }
                        }
                        let _ = has_class_body;
                        entries.push(e);
                    }
                    "line_comment" | "block_comment" | ";" => {}
                    "function_declaration"
                    | "property_declaration"
                    | "class_declaration"
                    | "object_declaration"
                    | "anonymous_initializer"
                    | "secondary_constructor"
                    | "companion_object" => {
                        if member.is_named() {
                            members.push(member);
                        }
                    }
                    _ => {
                        if member.is_named() {
                            self.diag_untranslatable(
                                member,
                                format!("enum member not supported: {}", member.kind()),
                            );
                        }
                    }
                }
            }
        }
        // Bodyless (abstract) enum methods are fine ONLY when every entry
        // carries an override body (checked after entries are built).
        let abstract_fns: Vec<String> = members
            .iter()
            .filter(|m| {
                m.kind() == "function_declaration" && kt::child(**m, "function_body").is_none()
            })
            .map(|m| {
                kt::field(*m, "name")
                    .map(|n| self.text(n).to_string())
                    .unwrap_or_else(|| "?".to_string())
            })
            .collect();
        if !abstract_fns.is_empty() {
            // Every constant must implement each abstract method.
            let bodies_ok = entries.iter().all(|e| e.contains("{"));
            if !bodies_ok {
                self.diag_untranslatable(
                    decl,
                    format!(
                        "enum '{}' declares abstract method(s) {} but not every constant has an override body",
                        name,
                        abstract_fns.join(", ")
                    ),
                );
                return;
            }
        }

        let emit_entries_bridge = self
            .workspace
            .is_some_and(|workspace| workspace.has_enum_entries_consumer(name));
        // Java-native declaration annotations pass through verbatim
        // (collected during the modifiers scan above).
        for annotation in annotations {
            out.line(annotation.clone());
        }
        out.open(format!("{}enum {}{}", visibility, name, implements));
        // constants
        out.line(entries.join(",\n"));
        if entries.is_empty()
            || !members.is_empty()
            || !params.is_empty()
            || emit_entries_bridge
            || !default_property_bridges.is_empty()
        {
            out.line(";");
        }
        if emit_entries_bridge {
            out.blank();
            out.open(format!(
                "public static kotlin.enums.EnumEntries<{}> getEntries()",
                name
            ));
            out.line("return kotlin.enums.EnumEntriesKt.enumEntries(values());");
            out.close();
        }
        for bridge in &default_property_bridges {
            out.blank();
            let return_type =
                crate::transpiler::types::map_type_name(bridge.return_type.trim_end_matches('?'));
            out.line("@Override");
            out.open(format!("public {} {}()", return_type, bridge.method_name));
            out.line(format!(
                "return {}.super.{}();",
                bridge.provider, bridge.method_name
            ));
            out.close();
        }
        // ctor params -> fields + accessors + private ctor
        if !params.is_empty() {
            out.blank();
            for (is_property, is_mutable, fname, ftype) in &params {
                if !*is_property {
                    continue;
                }
                let final_kw = if !*is_mutable { "final " } else { "" };
                // Kotlin writes persistence and metadata annotations on the
                // constructor property; the Java side must carry them on the
                // FIELD, which is the element the ORM reads.
                for annotation in self.param_annotations(decl, fname) {
                    out.line(annotation);
                }
                out.line(format!("private {}{} {};", final_kw, ftype, fname));
            }
            out.blank();
            for (_is_property, is_mutable, fname, ftype) in &params {
                let getter_name = crate::workspace::property_accessor_name(fname);
                let setter_name = crate::property_abi::setter_name(fname);
                let declares_getter = members.iter().any(|member| {
                    member.kind() == "function_declaration"
                        && kt::field(*member, "name")
                            .is_some_and(|name| self.text(name) == getter_name)
                });
                let declares_setter = members.iter().any(|member| {
                    member.kind() == "function_declaration"
                        && kt::field(*member, "name")
                            .is_some_and(|name| self.text(name) == setter_name)
                });
                // `@get:`-targeted annotations belong on the generated
                // getter, which this path emits.
                if !declares_getter {
                    for annotation in self.param_getter_annotations(decl, fname) {
                        out.line(annotation);
                    }
                    out.open(format!("public {} {}()", ftype, getter_name));
                    out.line(format!("return {};", fname));
                    out.close();
                }
                if *is_mutable && !declares_setter {
                    out.blank();
                    for annotation in self.param_setter_annotations(decl, fname) {
                        out.line(annotation);
                    }
                    out.open(format!("public void {}({} {})", setter_name, ftype, fname));
                    out.line(format!("this.{} = {};", fname, fname));
                    out.close();
                }
                out.blank();
            }
            // Kotlin enum constructors are private; Java requires the same.
            if self.constructor_has_covariant_input(decl, &params) {
                out.line("@SuppressWarnings(\"unchecked\")");
            }
            out.open(format!(
                "private {}({})",
                name,
                params
                    .iter()
                    .map(|(_, _, n, t)| {
                        format!("{} {}", self.constructor_parameter_java_type(decl, n, t), n)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            for (is_property, _, fname, ftype) in &params {
                if *is_property {
                    let value = self.constructor_property_assignment(decl, fname, ftype);
                    out.line(format!("this.{} = {};", fname, value));
                }
            }
            out.close();
            out.blank();
        }
        // Computed properties have no backing Java field. Register their
        // accessors before any method, including methods declared before them.
        for member in &members {
            if member.kind() == "property_declaration"
                && kt::child(*member, "getter").is_some()
                && let Some(variable) = kt::child(*member, "variable_declaration")
                && let Some(identifier) = kt::child(variable, "identifier")
            {
                let property = self.text(identifier).to_owned();
                self.self_getters.insert(
                    property.clone(),
                    crate::workspace::property_accessor_name(&property),
                );
            }
        }
        // body members
        for m in members {
            match m.kind() {
                "function_declaration" => {
                    self.transpile_function(m, true, out);
                    out.blank();
                }
                "property_declaration" => {
                    self.transpile_property(m, out);
                    out.blank();
                }
                "class_declaration" | "object_declaration" => {
                    self.transpile_type_decl(m, out);
                    out.blank();
                }
                "companion_object" => {
                    if !enum_group_consumer_requires_companion(self.source, name)
                        && !self.workspace.is_some_and(|workspace| {
                            workspace.kotlin_files().any(|file| {
                                enum_group_consumer_requires_companion(file.source_text(), name)
                            })
                        })
                    {
                        self.transpile_companion(m, name, out);
                        out.blank();
                    } else {
                        self.diag_untranslatable(
                            m,
                            "enum companion is consumed as a value; a static namespace cannot represent it",
                        );
                    }
                }
                "line_comment" | "block_comment" | ";" | "{" | "}" => {}
                _ => {
                    if m.is_named() {
                        self.diag_untranslatable(
                            m,
                            format!("enum member not supported: {}", m.kind()),
                        );
                    }
                }
            }
        }
        self.pending_field_types.clear();
        out.close();
    }

    /// The declared type of a companion constant group, as the Java type Kotlin
    /// exposes. Same-file `typealias` chains are expanded because Kotlin erases
    /// them; the DECLARED type is returned, never a type narrowed from the
    /// initializer (`Collection<Kind>` must not become `Set<Kind>`).
    fn declared_collection_type(&self, declared: &str, owner: &str) -> Option<(String, String)> {
        let (base, args) = split_generic(declared)?;
        let (base, args) = if is_collection_base(&base) {
            (base, args)
        } else {
            let alias = self
                .source
                .lines()
                .find_map(|line| {
                    let rest = line.trim().strip_prefix("typealias ")?;
                    let (head, target) = rest.split_once('=')?;
                    let alias_name = head.trim().split(['<', ' ']).next()?;
                    (alias_name == base).then(|| target.trim().to_string())
                })
                .or_else(|| {
                    // A same-package alias in another source file is just as
                    // erased; the workspace snapshot is the only place its
                    // target is recorded.
                    let workspace = self.workspace?;
                    let file = self
                        .workspace_file
                        .clone()
                        .unwrap_or_else(|| self.file.to_path_buf());
                    workspace
                        .type_aliases_for(&file)
                        .into_iter()
                        .find(|(name, _)| name == &base)
                        .map(|(_, target)| target)
                })?;
            let (target_base, target_params) = split_generic(&alias)?;
            if target_params.len() != args.len() || !is_collection_base(&target_base) {
                return None;
            }
            (target_base, args)
        };
        if args.len() != 1 || args[0].trim() != owner {
            return None;
        }
        let element = args[0].trim().to_string();
        let java_base = match base.as_str() {
            "Set" | "kotlin.collections.Set" | "java.util.Set" => "java.util.Set",
            "Collection" | "kotlin.collections.Collection" | "java.util.Collection" => {
                "java.util.Collection"
            }
            _ => return None,
        };
        Some((format!("{java_base}<{element}>"), element))
    }

    /// Companion collection groups over the owning enum's entries. Constructor
    /// arguments and ordinary enum members do not affect group initialization;
    /// declared collection types, entry order and shared group identity do.
    fn enum_companion_groups(
        &self,
        companion: tree_sitter::Node,
        owner: &str,
    ) -> Option<Vec<EnumGroup>> {
        if enum_group_consumer_requires_companion(self.source, owner)
            || self.workspace.is_some_and(|workspace| {
                workspace
                    .kotlin_files()
                    .any(|file| enum_group_consumer_requires_companion(file.source_text(), owner))
            })
        {
            return None;
        }
        let enum_body = kt::parent_of(companion)?;
        if !matches!(enum_body.kind(), "enum_class_body" | "class_body") {
            return None;
        }
        let mut entry_nodes = Vec::new();
        collect_kind(enum_body, "enum_entry", &mut entry_nodes);
        if entry_nodes.is_empty() {
            return None;
        }
        let entries = entry_nodes
            .into_iter()
            .filter_map(|node| {
                kt::child(node, "identifier").map(|name| self.text(name).to_string())
            })
            .collect::<Vec<_>>();
        let companion_body = kt::child(companion, "class_body")?;
        for kind in [
            "function_declaration",
            "object_declaration",
            "class_declaration",
            "anonymous_initializer",
            "secondary_constructor",
        ] {
            if count_kind(companion_body, kind) > 0 {
                return None;
            }
        }
        let mut properties = Vec::new();
        collect_kind(companion_body, "property_declaration", &mut properties);
        if properties.is_empty() {
            return None;
        }
        let mut groups = Vec::new();
        for property in properties {
            if !self.text(property).trim_start().starts_with("val ")
                || kt::child(property, "getter").is_some()
                || kt::child(property, "setter").is_some()
                || kt::child(property, "property_delegate").is_some()
            {
                return None;
            }
            let mut identifiers = Vec::new();
            collect_kind(property, "identifier", &mut identifiers);
            let name = self.text(*identifiers.first()?).to_string();
            let mut type_nodes = Vec::new();
            collect_kind(property, "user_type", &mut type_nodes);
            let (declared, element) =
                self.declared_collection_type(self.text(*type_nodes.first()?), owner)?;
            let mut calls = Vec::new();
            collect_kind(property, "call_expression", &mut calls);
            let init = *calls.first()?;
            let callee = init.named_child(0)?;
            if self.text(callee) != "setOf"
                || self
                    .semantic_provider
                    .is_some_and(|provider| !provider.symbols_named("setOf").is_empty())
            {
                return None;
            }
            if count_kind(init, "type_arguments") > 0 {
                return None;
            }
            let mut argument_nodes = Vec::new();
            collect_kind(init, "value_argument", &mut argument_nodes);
            if argument_nodes.is_empty() {
                return None;
            }
            let mut listed: Vec<String> = Vec::new();
            for argument in argument_nodes {
                let spelling = self.text(argument).trim().to_string();
                if !entries.contains(&spelling) {
                    return None;
                }
                listed.push(spelling);
            }
            groups.push(EnumGroup {
                name,
                declared,
                element,
                entries: listed,
            });
        }
        let getters = groups
            .iter()
            .map(|group| format!("get{}", capitalize(&group.name)))
            .collect::<Vec<_>>();
        if self.workspace.is_some_and(|workspace| {
            workspace
                .java_files()
                .any(|file| java_enum_companion_reference(file.source_text(), owner, &getters))
        }) {
            return None;
        }
        Some(groups)
    }

    /// `companion object { ... }` -> static members of the enclosing class.
    ///
    /// Kotlin companion member call sites (`Outer.member`) resolve the same
    /// way as Java statics, so functions become `static` methods, `val`
    /// properties become `static final` fields (with accessors), and `var`
    /// properties become static fields + accessors sharing that storage.
    /// Nested Companion bridges preserve member call paths for source callers.
    /// Named companions and companions holding init logic
    /// or other non-member constructs TAINT with N001 — there is no Java
    /// shape that preserves reachability (`Outer.Factory` vs statics).
    fn transpile_companion(
        &mut self,
        companion: tree_sitter::Node,
        owner: &str,
        out: &mut JavaOut,
    ) {
        // Named companion: `companion object Foo { ... }` — Kotlin reaches
        // members via `Outer.Foo.member`; statics on Outer would change the
        // call path. Taint rather than silently rename.
        let named = companion
            .children(&mut companion.walk())
            .any(|c| c.kind() == "identifier");
        if named {
            self.diag_untranslatable(
                companion,
                "named companion object has no Java counterpart (callers use Outer.CompanionName.member; cannot be redirected to Outer statics)",
            );
            return;
        }
        // The one #5 shape this backend proves: a literal enum plus a companion
        // `val GROUP: Set<Owner> = setOf(ENTRY, ...)`. Emit the Kotlin `setOf`
        // semantics as a shared, ordered, mutable instance behind a static field
        // whose type is the DECLARED type (aliases expanded, never narrowed from
        // the initializer). Every other companion shape keeps the existing path.
        if let Some(groups) = self.enum_companion_groups(companion, owner) {
            for group in &groups {
                let cap = capitalize(&group.name);
                let initializer = if group.entries.len() == 1 {
                    format!("java.util.Set.<{}>of({})", group.element, group.entries[0])
                } else {
                    format!(
                        "new java.util.LinkedHashSet<{}>(java.util.Arrays.<{}>asList({}))",
                        group.element,
                        group.element,
                        group.entries.join(", ")
                    )
                };
                out.line(format!(
                    "public static final {} {} = {};",
                    group.declared, group.name, initializer
                ));
                out.open(format!("public static {} get{}()", group.declared, cap));
                out.line(format!("return {};", group.name));
                out.close();
                out.blank();
                self.companion_members
                    .insert(group.name.clone(), format!("get{}()", cap));
                self.static_member_types
                    .insert(group.name.clone(), group.declared.clone());
            }
            out.open("public static final class Companion");
            out.line("private Companion() {}");
            for group in &groups {
                out.open(format!(
                    "public static {} get{}()",
                    group.declared,
                    capitalize(&group.name)
                ));
                out.line(format!("return {};", group.name));
                out.close();
            }
            out.close();
            out.blank();
            return;
        }
        let Some(body) = kt::child(companion, "class_body") else {
            return;
        };
        let mut bridge_sigs: Vec<BridgeSig> = Vec::new();
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            match member.kind() {
                "property_declaration" => {
                    if let Some(vd) = kt::child(member, "variable_declaration")
                        && let Some(n) = kt::child(vd, "identifier")
                    {
                        let pname = self.text(n).to_string();
                        let cap: String = capitalize(&pname);
                        self.companion_members
                            .insert(pname.clone(), format!("get{}()", cap));
                    }
                    let mut property_out = JavaOut::new();
                    self.transpile_property_opts(member, &mut property_out, true, Some(owner));
                    let emitted = property_out.finish();
                    bridge_sigs.extend(self.companion_method_bridges(member, &emitted));
                    out.line(emitted);
                    out.blank();
                }
                "function_declaration" => {
                    bridge_sigs.extend(self.transpile_companion_function(member, out));
                    out.blank();
                }
                // Trivia nodes are named by tree-sitter but are not companion
                // members and must not make an otherwise valid companion fail.
                "line_comment" | "block_comment" | ";" | "{" | "}" => {}
                _ => {
                    if member.is_named() {
                        self.diag_untranslatable(
                            member,
                            format!("companion object member not supported: {}", member.kind()),
                        );
                    }
                }
            }
        }
        if !bridge_sigs.is_empty() {
            out.blank();
            out.open(String::from("public static final class Companion"));
            for sig in &bridge_sigs {
                out.open(format!("public {}", sig.signature));
                let prefix = if sig.returns_void { "" } else { "return " };
                out.line(format!("{prefix}{}.{};", owner, sig.call));
                out.close();
            }
            out.close();
            out.blank();
        }
    }

    /// Build companion bridges from the actual emitted Java methods. This
    /// shares return inference, generic bounds, parameter types and overloads
    /// with the function emitter instead of maintaining another Kotlin renderer.
    fn transpile_companion_function(
        &mut self,
        member: tree_sitter::Node,
        out: &mut JavaOut,
    ) -> Vec<BridgeSig> {
        let mut method_out = JavaOut::new();
        self.transpile_function_opts(member, true, true, false, &mut method_out);
        let emitted = method_out.finish();
        out.line(&emitted);
        self.companion_method_bridges(member, &emitted)
    }

    fn companion_method_bridges(
        &mut self,
        member: tree_sitter::Node,
        emitted: &str,
    ) -> Vec<BridgeSig> {
        let wrapped = format!("class __Companion {{\n{emitted}\n}}");
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .expect("Java grammar");
        let Some(tree) = parser
            .parse(&wrapped, None)
            .filter(|tree| !tree.root_node().has_error())
        else {
            self.diag_untranslatable(member, "companion method did not produce valid Java");
            return Vec::new();
        };
        let root = tree.root_node();
        let mut methods = Vec::new();
        if let Some(class) = root.named_child(0)
            && let Some(body) = class.child_by_field_name("body")
        {
            methods.extend(
                body.named_children(&mut body.walk())
                    .filter(|node| node.kind() == "method_declaration"),
            );
        }
        let mut bridges = Vec::new();
        for method in methods {
            let Some(body) = method.child_by_field_name("body") else {
                continue;
            };
            let Some(name) = method.child_by_field_name("name") else {
                continue;
            };
            let Some(parameters) = method.child_by_field_name("parameters") else {
                continue;
            };
            let mut arguments = Vec::new();
            for parameter in parameters.named_children(&mut parameters.walk()) {
                if let Some(name) = parameter.child_by_field_name("name") {
                    arguments.push(wrapped[name.byte_range()].to_string());
                } else {
                    self.diag_untranslatable(member, "companion bridge parameter has no Java name");
                    return Vec::new();
                }
            }
            let mut signature = wrapped[method.start_byte()..body.start_byte()].to_string();
            if let Some(modifiers) = method
                .named_children(&mut method.walk())
                .find(|node| node.kind() == "modifiers")
            {
                let mut edits = modifiers
                    .children(&mut modifiers.walk())
                    .filter(|node| {
                        matches!(node.kind(), "public" | "protected" | "private" | "static")
                    })
                    .map(|node| {
                        (
                            node.start_byte() - method.start_byte(),
                            node.end_byte() - method.start_byte(),
                        )
                    })
                    .collect::<Vec<_>>();
                edits.sort_unstable_by_key(|edit| std::cmp::Reverse(edit.0));
                for (start, end) in edits {
                    signature.replace_range(start..end, "");
                }
            }
            bridges.push(BridgeSig {
                signature: format!("static {}", signature.trim()),
                call: format!("{}({})", &wrapped[name.byte_range()], arguments.join(", ")),
                returns_void: method
                    .child_by_field_name("type")
                    .is_some_and(|ty| &wrapped[ty.byte_range()] == "void"),
            });
        }
        bridges
    }

    fn transpile_object(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        visibility: &str,
        annotations: &[String],
        out: &mut JavaOut,
    ) {
        // Supertypes: `object Idle : State()` — the nested class must extend
        // a class supertype or implement an interface. Kotlin uses the same
        // syntax for both, so consult the workspace index when available.
        let mut obj_supertype = String::new();
        if let Some(ds) = kt::child(decl, "delegation_specifiers") {
            let mut dcur = ds.walk();
            for spec in ds.children(&mut dcur) {
                if spec.kind() == "delegation_specifier" {
                    let sup = kt::child(spec, "super_type")
                        .or_else(|| spec.children(&mut spec.walk()).find(|c| c.is_named()));
                    if let Some(st) = sup {
                        let mut scur = st.walk();
                        let base = st.children(&mut scur).find(|c| c.is_named()).unwrap_or(st);
                        let ty = kt::text(base, self.source).trim().replace(" ", "");
                        let bare = ty
                            .split('<')
                            .next()
                            .unwrap_or(&ty)
                            .rsplit('.')
                            .next()
                            .unwrap_or(&ty);
                        let is_interface = self.workspace.is_some_and(|workspace| {
                            workspace.declarations_named(bare).any(|declaration| {
                                declaration.name == bare
                                    && declaration.kind
                                        == crate::workspace::DeclarationKind::Interface
                            })
                        });
                        obj_supertype = format!(
                            " {} {}",
                            if is_interface {
                                "implements"
                            } else {
                                "extends"
                            },
                            ty
                        );
                    }
                }
            }
        }
        // `static` only legal for a NESTED class (inner inside class_body);
        // a top-level object is a plain public final class.
        let static_kw = if decl.parent().is_some_and(|p| p.kind() == "class_body") {
            "static "
        } else {
            ""
        };
        for annotation in annotations {
            out.line(annotation.clone());
        }
        out.open(format!(
            "{}{}final class {}{}",
            visibility, static_kw, name, obj_supertype
        ));
        out.line(format!(
            "public static final {} INSTANCE = new {}();",
            name, name
        ));
        out.line(format!("private {}() {{}}", name));
        out.blank();
        // Self-references inside the body (`val self = Registry`) point at
        // the singleton in Java (`Registry.INSTANCE`).
        self.current_object = Some(name.to_string());
        if let Some(body) = kt::child(decl, "class_body") {
            let mut cursor = body.walk();
            for member in body.children(&mut cursor) {
                match member.kind() {
                    "function_declaration" => {
                        // Ordinary object methods are static helpers reached
                        // through the singleton class. An override implements
                        // an interface instance method, including a property
                        // getter materialized by the ABI repair pass, so it
                        // must remain an instance method.
                        let is_override = kt::child(member, "modifiers")
                            .is_some_and(|modifiers| self.text(modifiers).contains("override"));
                        self.transpile_function_opts(member, true, !is_override, false, out);
                        out.blank();
                    }
                    "property_declaration" => {
                        // object properties behave like static fields; record
                        // the accessor so `Cfg.TAG` call sites read via the
                        // generated getter (fields are private).
                        if let Some(vd) = kt::child(member, "variable_declaration")
                            && let Some(n) = kt::child(vd, "identifier")
                        {
                            let pname = self.text(n).to_string();
                            let cap: String = capitalize(&pname);
                            self.companion_members
                                .insert(pname, format!("get{}()", cap));
                        }
                        // An overridden object property implements an
                        // interface accessor. ABI repair may already have
                        // removed `override` from the property and added a
                        // sibling forwarding function, so recognize that
                        // bridge too before choosing static object accessors.
                        let is_override = kt::child(member, "modifiers")
                            .is_some_and(|modifiers| self.text(modifiers).contains("override"));
                        let has_repaired_getter = kt::child(member, "variable_declaration")
                            .and_then(|variable| kt::child(variable, "identifier"))
                            .is_some_and(|property_name| {
                                let getter_name =
                                    format!("get{}", capitalize(self.text(property_name)));
                                body.children(&mut body.walk()).any(|sibling| {
                                    sibling.kind() == "function_declaration"
                                        && kt::field(sibling, "name").is_some_and(|sibling_name| {
                                            self.text(sibling_name) == getter_name
                                                && self.is_property_repair_bridge(sibling)
                                        })
                                })
                            });
                        self.transpile_property_opts(
                            member,
                            out,
                            !(is_override || has_repaired_getter),
                            Some(name),
                        );
                        out.blank();
                    }
                    "companion_object" => {
                        self.transpile_companion(member, name, out);
                        out.blank();
                    }
                    "secondary_constructor" => {
                        self.diag_untranslatable(
                            member,
                            "secondary constructors not yet supported",
                        );
                    }
                    "line_comment" | "block_comment" | ";" | "{" | "}" => {}
                    _ => {
                        if member.is_named() {
                            self.diag_untranslatable(
                                member,
                                format!("object member not supported: {}", member.kind()),
                            );
                        }
                    }
                }
            }
        }
        self.current_object = None;
        out.close();
    }

    /// Lower a secondary constructor with a direct `this(...)`/`super(...)`
    /// delegation and, optionally, a simple assignment-only body. Java requires
    /// the delegation call to be the constructor's first statement; bodies
    /// with control flow or defaults remain Kotlin until a wider flow and
    /// call-site analysis exists. A trailing vararg keeps its Java varargs ABI.
    fn transpile_secondary_constructor(
        &mut self,
        member: tree_sitter::Node,
        class_name: &str,
        out: &mut JavaOut,
    ) -> bool {
        let body = kt::child(member, "function_body").or_else(|| kt::child(member, "block"));
        if body.is_some_and(|body| !self.is_safe_secondary_constructor_body(body)) {
            return false;
        }
        let Some(delegation) = kt::child(member, "constructor_delegation_call") else {
            return false;
        };
        let delegation_target = if self.text(delegation).trim_start().starts_with("this") {
            "this"
        } else if self.text(delegation).trim_start().starts_with("super") {
            "super"
        } else {
            return false;
        };
        let Some(params_node) = kt::child(member, "function_value_parameters") else {
            return false;
        };
        let mut params = Vec::new();
        let prior_var_types = std::mem::take(&mut self.var_types);
        for param in params_node.children(&mut params_node.walk()) {
            if param.kind() != "parameter" {
                continue;
            }
            let children: Vec<_> = param.children(&mut param.walk()).collect();
            if children
                .iter()
                .any(|child| !child.is_named() && child.kind() == "=")
            {
                self.var_types = prior_var_types;
                return false;
            }
            let Some(name_node) = children.iter().find(|child| child.kind() == "identifier") else {
                self.var_types = prior_var_types;
                return false;
            };
            let Some(type_node) = children.iter().find(|child| {
                matches!(
                    child.kind(),
                    "user_type" | "nullable_type" | "type_reference" | "type"
                )
            }) else {
                self.var_types = prior_var_types;
                return false;
            };
            let name = self.text(*name_node).to_string();
            let ty = kt::java_type_ann(*type_node, self.source, self.annots);
            let vararg = param.prev_named_sibling().is_some_and(|node| {
                node.kind() == "parameter_modifiers"
                    && self
                        .text(node)
                        .split_whitespace()
                        .any(|word| word == "vararg")
            }) || children.iter().any(|node| node.kind() == "vararg");
            self.var_types.insert(
                name.clone(),
                if vararg {
                    format!("{ty}[]")
                } else {
                    ty.clone()
                },
            );
            params.push((name, if vararg { format!("{ty}...") } else { ty }));
        }
        let Some(args_node) = kt::child(delegation, "value_arguments") else {
            self.var_types = prior_var_types;
            return false;
        };
        let mut written_args = Vec::new();
        for arg in args_node.children(&mut args_node.walk()) {
            if arg.kind() != "value_argument" {
                continue;
            }
            let children: Vec<_> = arg.children(&mut arg.walk()).collect();
            let has_name = children
                .iter()
                .any(|child| !child.is_named() && child.kind() == "=");
            let named: Vec<_> = children
                .iter()
                .copied()
                .filter(|child| child.is_named())
                .collect();
            let Some(value) = named.last().copied() else {
                self.var_types = prior_var_types;
                return false;
            };
            if !self.is_safe_secondary_constructor_argument(value) {
                self.var_types = prior_var_types;
                return false;
            }
            let name = if has_name {
                let Some(name) = named.first().filter(|node| node.kind() == "identifier") else {
                    self.var_types = prior_var_types;
                    return false;
                };
                Some(self.text(*name).to_string())
            } else {
                None
            };
            written_args.push((name, Expr { unit: self }.transpile(value)));
        }
        let args = if delegation_target == "this" {
            let Some(class_decl) = kt::parent_of(member)
                .and_then(kt::parent_of)
                .filter(|node| node.kind() == "class_declaration")
            else {
                self.var_types = prior_var_types;
                return false;
            };
            if let Some(fact) = self
                .constructor_delegations
                .get(&(member.start_byte(), member.end_byte()))
            {
                // Planning has selected one secondary constructor, including
                // every competing arity. Keep source argument order: Java
                // cannot introduce temporaries before this(...).
                let target = kt::child(class_decl, "class_body").and_then(|body| {
                    body.named_children(&mut body.walk()).find(|node| {
                        node.kind() == "secondary_constructor"
                            && node.start_byte() == fact.target.start_byte
                            && node.end_byte() == fact.target.end_byte
                    })
                });
                let target_types = target
                    .and_then(|target| kt::child(target, "function_value_parameters"))
                    .map(|params| {
                        params
                            .named_children(&mut params.walk())
                            .filter(|node| node.kind() == "parameter")
                            .filter_map(|param| {
                                param.named_children(&mut param.walk()).find(|node| {
                                    matches!(node.kind(), "user_type" | "type_reference" | "type")
                                })
                            })
                            .map(|ty| self.text(ty).trim().to_string())
                            .collect::<Vec<_>>()
                    });
                assert!(
                    fact.location.snapshot_hash == self.source_hash
                        && fact.target.snapshot_hash == self.source_hash
                        && target_types.as_ref() == Some(&fact.parameter_types)
                        && written_args.len() == fact.parameter_types.len()
                        && written_args.iter().all(|(name, _)| name.is_none()),
                    "constructor emission requires the accepted delegation target"
                );
                written_args.into_iter().map(|(_, value)| value).collect()
            } else {
                let primary = self.class_params(class_decl);
                let names: Vec<String> =
                    primary.iter().map(|(_, _, name, _)| name.clone()).collect();
                let Some(resolved) = crate::ctor_defaults::resolve_args(&names, &written_args)
                else {
                    self.var_types = prior_var_types;
                    return false;
                };
                let defaults = self.class_param_default_nodes(class_decl);
                let mut args = Vec::with_capacity(names.len());
                for (index, slot) in resolved.slots.into_iter().enumerate() {
                    if let Some(value) = slot {
                        args.push(value);
                        continue;
                    }
                    let Some(default) = defaults.get(index).copied().flatten() else {
                        self.var_types = prior_var_types;
                        return false;
                    };
                    if !self.is_safe_secondary_constructor_argument(default)
                        || crate::ctor_defaults::unsupplied_reference(
                            self.text(default),
                            &names,
                            &[],
                        )
                        .is_some()
                    {
                        self.var_types = prior_var_types;
                        return false;
                    }
                    args.push(Expr { unit: self }.transpile(default));
                }
                // This source call resolves to the primary constructor. Preserve
                // that choice when null also matches a Java secondary overload.
                for (index, value) in args.iter_mut().enumerate() {
                    if value.trim() == "null" {
                        let cast_type = primary[index]
                            .3
                            .replace("@Nullable ", "")
                            .replace("@NotNull ", "")
                            .replace("@NonNull ", "");
                        *value = format!("({cast_type}) null");
                    }
                }
                args
            }
        } else {
            if written_args.iter().any(|(name, _)| name.is_some()) {
                self.var_types = prior_var_types;
                return false;
            }
            written_args.into_iter().map(|(_, value)| value).collect()
        };
        out.open(format!(
            "public {}({})",
            class_name,
            params
                .iter()
                .map(|(name, ty)| format!("{} {}", ty, name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        out.line(format!("{}({});", delegation_target, args.join(", ")));
        if let Some(body) = body {
            let mut cursor = body.walk();
            for stmt in body.children(&mut cursor) {
                if stmt.is_named() {
                    self.transpile_statement(stmt, out);
                }
            }
        }
        self.var_types = prior_var_types;
        out.close();
        out.blank();
        true
    }

    /// A deliberately narrow constructor-body subset: assignments to fields
    /// on `this`, with a value that can be emitted unchanged in Java. This
    /// preserves Java's required first-statement delegation invariant without
    /// accepting control flow or Kotlin-specific call semantics.
    fn is_safe_secondary_constructor_body(&self, body: tree_sitter::Node) -> bool {
        if body.kind() != "block" {
            return false;
        }
        body.children(&mut body.walk())
            .filter(|stmt| stmt.is_named())
            .all(|stmt| {
                if stmt.kind() != "assignment" {
                    return false;
                }
                let named: Vec<_> = stmt
                    .children(&mut stmt.walk())
                    .filter(|child| child.is_named())
                    .collect();
                let (Some(target), Some(value)) = (named.first(), named.get(1)) else {
                    return false;
                };
                target.kind() == "navigation_expression"
                    && self.text(*target).trim_start().starts_with("this.")
                    && !self.text(*target).contains('(')
                    && self.is_safe_secondary_constructor_argument(*value)
            })
    }

    /// Delegated constructor arguments are narrower than the general expression
    /// lowerer. Admit constructor calls and the Java-compatible library calls
    /// needed by constructor defaults and shared established member lowerings.
    fn is_safe_secondary_constructor_argument(&self, node: tree_sitter::Node) -> bool {
        if self
            .reference_string_calls
            .get(&(node.start_byte(), node.end_byte()))
            .is_some_and(|fact| {
                fact.location.snapshot_hash == self.source_hash
                    && fact.location.end_byte == node.end_byte()
            })
        {
            return true;
        }
        match node.kind() {
            "identifier" | "this_expression" | "number_literal" | "boolean_literal"
            | "hex_literal" | "long_literal" | "real_literal" | "float_literal"
            | "null_literal" => true,
            "string_literal" => !self.text(node).contains('$'),
            "navigation_expression" | "parenthesized_expression" | "value_arguments" => node
                .children(&mut node.walk())
                .filter(|child| child.is_named())
                .all(|child| self.is_safe_secondary_constructor_argument(child)),
            "value_argument" => node
                .children(&mut node.walk())
                .filter(|child| child.is_named())
                .last()
                .is_some_and(|value| self.is_safe_secondary_constructor_argument(value)),
            "call_expression" => {
                let named: Vec<_> = node
                    .children(&mut node.walk())
                    .filter(|child| child.is_named())
                    .collect();
                let Some(callee) = named.first().copied() else {
                    return false;
                };
                let callee_text = self.text(callee).trim();
                let method = callee_text
                    .rsplit_once('.')
                    .map_or(callee_text, |(_, method)| method);
                let is_constructor = method
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_uppercase());
                // Reuse the planner's source-scoped platform proof. A method
                // merely named randomUUID (or a shadowed UUID qualifier) does
                // not establish the java.util.UUID factory contract.
                let value_receiver = callee_text
                    .rsplit_once('.')
                    .is_some_and(|(receiver, _)| self.var_types.contains_key(receiver.trim()));
                let platform_uuid = !value_receiver
                    && self.semantic_provider.is_some_and(|provider| {
                        crate::semantics::established_platform_static_call_return_type(
                            self.source,
                            self.workspace_file.as_deref().unwrap_or(self.file),
                            node,
                            provider,
                        ) == Some("java.util.UUID")
                    });
                let is_supported_method = platform_uuid
                    || self.semantic_provider.is_some_and(|provider| {
                        crate::semantics::has_standard_member_lowering(
                            self.source,
                            self.workspace_file.as_deref().unwrap_or(self.file),
                            node,
                            provider,
                        )
                    })
                    || matches!(method, "getOrDefault" | "now" | "toInstant")
                    || self.semantic_provider.is_some_and(|provider| {
                        crate::semantics::established_own_companion_delegation_call(
                            self.source,
                            node,
                            self.workspace_file.as_deref().unwrap_or(self.file),
                            provider,
                        )
                    })
                    || self.is_declared_zero_arg_instance_call(node, callee_text);
                (is_constructor || is_supported_method)
                    && named
                        .iter()
                        .skip(1)
                        .all(|child| self.is_safe_secondary_constructor_argument(*child))
            }
            _ => false,
        }
    }

    fn is_declared_zero_arg_instance_call(
        &self,
        call: tree_sitter::Node,
        _callee_text: &str,
    ) -> bool {
        let Some(arguments) = kt::child(call, "value_arguments") else {
            return false;
        };
        if arguments
            .children(&mut arguments.walk())
            .any(|child| child.kind() == "value_argument")
        {
            return false;
        }
        let Some(callee) = crate::semantics::call_callee_node(call) else {
            return false;
        };
        let Some(receiver_node) = crate::semantics::explicit_receiver_node(call, callee) else {
            return false;
        };
        let receiver = self.text(receiver_node).trim();
        let method = self.text(callee).trim();
        if !matches!(receiver_node.kind(), "identifier" | "simple_identifier") {
            return self.semantic_provider.is_some_and(|provider| {
                crate::semantics::established_projected_receiver_instance_call(
                    self.source,
                    call,
                    self.workspace_file.as_deref().unwrap_or(self.file),
                    provider,
                )
            });
        }
        let Some(receiver_type) = self.var_types.get(receiver.trim()) else {
            return false;
        };
        let Some(workspace) = self.workspace else {
            return false;
        };
        let declaring = self.workspace_file.as_deref().unwrap_or(self.file);
        let Some(source_file) = workspace.source_file(declaring) else {
            return false;
        };
        workspace.type_declares_zero_arg_instance_method(source_file, receiver_type, method.trim())
    }

    fn transpile_interface_companion(&mut self, companion: tree_sitter::Node, out: &mut JavaOut) {
        let Some(body) = kt::child(companion, "class_body") else {
            return;
        };
        let owner = kt::parent_of(companion)
            .and_then(kt::parent_of)
            .and_then(|decl| kt::field(decl, "name"))
            .map(|name| self.text(name).to_string())
            .unwrap_or_default();
        let namespace = kt::child(companion, "identifier")
            .map(|name| self.text(name).to_string())
            .unwrap_or_else(|| "Companion".to_string());
        let mut bridges = Vec::new();
        let mut fields = Vec::new();
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            match member.kind() {
                "function_declaration" => {
                    bridges.extend(self.transpile_companion_function(member, out));
                    out.blank();
                }
                "property_declaration" => {
                    let mut property_out = JavaOut::new();
                    self.transpile_property_opts(
                        member,
                        &mut property_out,
                        true,
                        Some("__interface__"),
                    );
                    let emitted = property_out.finish();
                    bridges.extend(self.companion_method_bridges(member, &emitted));
                    if kt::child(member, "val").is_some() {
                        let wrapped = format!("class __Companion {{\n{emitted}\n}}");
                        let mut parser = tree_sitter::Parser::new();
                        parser
                            .set_language(&tree_sitter_java::LANGUAGE.into())
                            .expect("Java grammar");
                        if let Some(tree) = parser.parse(&wrapped, None)
                            && let Some(class) = tree.root_node().named_child(0)
                            && let Some(body) = class.child_by_field_name("body")
                        {
                            for field in body
                                .named_children(&mut body.walk())
                                .filter(|node| node.kind() == "field_declaration")
                            {
                                if let Some(ty) = field.child_by_field_name("type") {
                                    for variable in field
                                        .named_children(&mut field.walk())
                                        .filter(|node| node.kind() == "variable_declarator")
                                    {
                                        if let Some(name) = variable.child_by_field_name("name") {
                                            fields.push((
                                                wrapped[ty.byte_range()].to_owned(),
                                                wrapped[name.byte_range()].to_owned(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    out.line(emitted);
                    out.blank();
                }
                "line_comment" | "block_comment" | ";" | "{" | "}" => {}
                _ => {
                    if member.is_named() {
                        self.diag_untranslatable(
                            member,
                            format!(
                                "interface companion member not supported: {}",
                                member.kind()
                            ),
                        );
                    }
                }
            }
        }
        if !bridges.is_empty() {
            out.line(format!(
                "public static final {namespace} {namespace} = new {namespace}();"
            ));
            out.open(format!("public static final class {namespace}"));
            out.line(format!("private {namespace}() {{}}"));
            for (ty, name) in fields {
                out.line(format!("public static final {ty} {name} = {owner}.{name};"));
            }
            for bridge in bridges {
                // Instance methods keep Java/Kotlin callers of the singleton
                // working; the interface itself exposes the convenient statics.
                out.open(format!(
                    "public {}",
                    bridge.signature.trim_start_matches("static ")
                ));
                let prefix = if bridge.returns_void { "" } else { "return " };
                out.line(format!("{prefix}{owner}.{};", bridge.call));
                out.close();
            }
            out.close();
        }
    }

    fn emit_data_initialization(
        &mut self,
        decl: tree_sitter::Node,
        params: &[(bool, bool, String, String)],
        out: &mut JavaOut,
    ) {
        let prior = std::mem::take(&mut self.var_types);
        for (_, _, name, ty) in params {
            self.var_types.insert(name.clone(), ty.clone());
        }
        if let Some(body) = kt::child(decl, "class_body") {
            for member in body.children(&mut body.walk()) {
                if member.kind() == "property_declaration"
                    && self.deferred_property_initializers.contains(&member.id())
                {
                    if let Some(initializer) = self.property_initializer(member) {
                        let name = kt::child(member, "variable_declaration")
                            .and_then(|vd| kt::child(vd, "identifier"));
                        if let Some(name) = name {
                            let value = Expr { unit: self }.transpile(initializer);
                            out.line(format!("this.{} = {};", self.text(name), value));
                        }
                    }
                } else if member.kind() == "anonymous_initializer" {
                    if let Some(block) = kt::child(member, "block") {
                        // Each Kotlin init block has its own local scope.
                        let prior_block_types = self.var_types.clone();
                        out.open("");
                        for stmt in block
                            .children(&mut block.walk())
                            .filter(|stmt| stmt.is_named())
                        {
                            if !self.transpile_data_precondition(stmt, out) {
                                self.transpile_statement(stmt, out);
                            }
                        }
                        out.close();
                        self.var_types = prior_block_types;
                    } else {
                        self.diag_untranslatable(member, "unrecognized data-class init block");
                    }
                }
            }
        }
        self.var_types = prior;
    }

    /// Kotlin preconditions evaluate their message lazily, only on failure.
    fn transpile_data_precondition(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) -> bool {
        if stmt.kind() != "call_expression" {
            return false;
        }
        let mut call = stmt;
        while let Some(inner) = kt::child(call, "call_expression") {
            call = inner;
        }
        let Some(callee) = call.children(&mut call.walk()).find(|n| n.is_named()) else {
            return false;
        };
        let name = self.text(callee).to_string();
        if !matches!(name.as_str(), "require" | "check")
            || self.fn_rets.contains_key(&name)
            || self.source.lines().any(|line| {
                line.trim_start().starts_with("import ")
                    && line.trim_end().ends_with(&format!(".{name}"))
            })
        {
            return false;
        }
        let args = kt::child(call, "value_arguments")
            .map(|args| {
                args.children(&mut args.walk())
                    .filter(|a| a.kind() == "value_argument")
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if args.len() != 1 {
            return false;
        }
        let Some(condition) = args[0].children(&mut args[0].walk()).find(|n| n.is_named()) else {
            return false;
        };
        let lambda =
            kt::child(stmt, "annotated_lambda").and_then(|l| kt::child(l, "lambda_literal"));
        let message = if let Some(lambda) = lambda {
            let values = lambda
                .children(&mut lambda.walk())
                .filter(|n| n.is_named())
                .collect::<Vec<_>>();
            if values.len() != 1
                || matches!(
                    values[0].kind(),
                    "return_expression" | "property_declaration" | "lambda_parameters"
                )
            {
                self.diag_untranslatable(
                    stmt,
                    "data-class precondition message requires unsupported lambda control flow",
                );
                return true;
            }
            Expr { unit: self }.transpile(values[0])
        } else if name == "require" {
            "\"Failed requirement.\"".into()
        } else {
            "\"Check failed.\"".into()
        };
        let condition = Expr { unit: self }.transpile(condition);
        out.open(format!("if (!({condition}))"));
        out.line(format!(
            "throw new {}(String.valueOf({message}));",
            if name == "require" {
                "IllegalArgumentException"
            } else {
                "IllegalStateException"
            }
        ));
        out.close();
        true
    }

    /// Kotlin data-class operations use primary-constructor properties only.
    /// Records provide storage, but their default hash/toString differ from Kotlin.
    fn emit_data_members(
        &self,
        decl: tree_sitter::Node,
        name: &str,
        params: &[(bool, bool, String, String)],
        as_record: bool,
        out: &mut JavaOut,
    ) {
        let declares = |method: &str| {
            kt::child(decl, "class_body").is_some_and(|body| {
                body.children(&mut body.walk()).any(|member| {
                    member.kind() == "function_declaration"
                        && kt::field(member, "name").is_some_and(|n| self.text(n) == method)
                })
            })
        };
        fn raw_type(ty: &str) -> &str {
            ty.trim_start_matches("@Nullable ")
                .trim_start_matches("@NonNull ")
        }
        if !declares("equals") {
            out.open("@Override public boolean equals(Object other)");
            out.line("if (this == other) return true;");
            out.line(format!(
                "if (!(other instanceof {name} that)) return false;"
            ));
            let comparisons = params
                .iter()
                .map(|(_, _, field, ty)| match raw_type(ty) {
                    "float" => format!("Float.compare(this.{field}, that.{field}) == 0"),
                    "double" => format!("Double.compare(this.{field}, that.{field}) == 0"),
                    "boolean" | "byte" | "short" | "int" | "long" | "char" => {
                        format!("this.{field} == that.{field}")
                    }
                    _ => format!("java.util.Objects.equals(this.{field}, that.{field})"),
                })
                .collect::<Vec<_>>();
            out.line(format!("return {};", comparisons.join(" && ")));
            out.close();
        }
        if !declares("hashCode") {
            out.open("@Override public int hashCode()");
            out.line("int result = 0;");
            for (_, _, field, ty) in params {
                let ty = raw_type(ty);
                let hash = match ty {
                    "boolean" => format!("(this.{field} ? 1 : 0)"),
                    "byte" | "short" | "int" | "char" => format!("this.{field}"),
                    "long" => format!("Long.hashCode(this.{field})"),
                    "float" => format!("Float.hashCode(this.{field})"),
                    "double" => format!("Double.hashCode(this.{field})"),
                    _ if ty.ends_with("[]") => format!("java.util.Arrays.hashCode(this.{field})"),
                    _ => format!("java.util.Objects.hashCode(this.{field})"),
                };
                out.line(format!("result = 31 * result + {hash};"));
            }
            out.line("return result;");
            out.close();
        }
        if !declares("toString") {
            out.open("@Override public String toString()");
            let pieces = params
                .iter()
                .enumerate()
                .map(|(index, (_, _, field, ty))| {
                    let value = if raw_type(ty).ends_with("[]") {
                        format!("java.util.Arrays.toString(this.{field})")
                    } else {
                        format!("this.{field}")
                    };
                    format!(
                        "\"{}{field}=\" + {value}",
                        if index == 0 { "" } else { ", " }
                    )
                })
                .collect::<Vec<_>>();
            out.line(format!(
                "return \"{name}(\" + {} + \")\";",
                pieces.join(" + ")
            ));
            out.close();
        }
        let generic = kt::child(decl, "type_parameters")
            .map(|tp| {
                tp.children(&mut tp.walk())
                    .filter(|p| p.kind() == "type_parameter")
                    .filter_map(|p| kt::child(p, "identifier"))
                    .map(|n| self.text(n))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let self_type = if generic.is_empty() {
            name.to_string()
        } else {
            format!("{name}<{}>", generic.join(", "))
        };
        out.open(format!(
            "public {self_type} copy({})",
            params
                .iter()
                .map(|(_, _, field, ty)| format!("{ty} {field}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        out.line(format!(
            "return new {self_type}({});",
            params
                .iter()
                .map(|(_, _, field, _)| field.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        out.close();
        for (index, (_, _, field, ty)) in params.iter().enumerate() {
            out.open(format!("public {ty} component{}()", index + 1));
            out.line(format!(
                "return this.{field}{};",
                if as_record { "()" } else { "" }
            ));
            out.close();
        }
    }

    fn emit_planned_default_getters(&self, decl: tree_sitter::Node, out: &mut JavaOut) {
        for bridge in self
            .default_getter_bridges
            .iter()
            .filter(|bridge| bridge.location.start_byte == decl.start_byte())
        {
            assert_eq!(bridge.location.snapshot_hash, self.source_hash);
            assert_eq!(
                bridge.return_type_ref,
                crate::semantics::TypeRef::parse(&bridge.return_type),
                "accepted getter return spelling must match the structured contract"
            );
            let provider = &bridge.provider;
            let mut parts = Vec::new();
            if !provider.package.is_empty() {
                parts.push(provider.package.as_str());
            }
            parts.extend(provider.owner_path.iter().map(String::as_str));
            parts.push(provider.name.as_str());
            let provider_name = parts.join(".");
            let body = match &bridge.body {
                crate::translation_plan::PlannedDefaultGetterBody::JavaSuperDelegation => {
                    format!("return {provider_name}.super.{}();", bridge.method_name)
                }
                crate::translation_plan::PlannedDefaultGetterBody::SourceForwarding {
                    accessors,
                } => {
                    let mut call = String::from("this");
                    for accessor in accessors {
                        assert!(
                            !accessor.is_empty()
                                && accessor
                                    .chars()
                                    .all(|character| character.is_ascii_alphanumeric()
                                        || character == '_'),
                            "accepted forwarding accessor must be a plain method name"
                        );
                        call.push('.');
                        call.push_str(accessor);
                        call.push_str("()");
                    }
                    format!("return {call};")
                }
            };
            out.line("@Override");
            out.open(format!(
                "public {} {}()",
                bridge.return_type, bridge.method_name
            ));
            out.line(&body);
            out.close();
            out.blank();
        }
    }

    fn transpile_class_body(&mut self, body: tree_sitter::Node, out: &mut JavaOut) {
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            match member.kind() {
                "function_declaration" => {
                    // A retained Kotlin constructor property may have gained a
                    // mechanical `getX() = x` ABI bridge on an earlier pass.
                    // If the class now translates, the constructor-property
                    // accessor emitted above is the same Java method; omit the
                    // redundant bridge instead of producing a duplicate (and
                    // preserve non-trivial explicit getters normally).
                    if let Some(property) = self.constructor_property_bridge(member) {
                        let current_is_record = !self.lombok
                            && self
                                .current_decl
                                .and_then(|decl| kt::field(decl, "name"))
                                .is_some_and(|name| self.record_types.contains(self.text(name)));
                        if !current_is_record {
                            continue;
                        }
                        let getter = self.self_getters.remove(&property);
                        self.transpile_function(member, /*in_class=*/ true, out);
                        if let Some(getter) = getter {
                            self.self_getters.insert(property, getter);
                        }
                        out.blank();
                        continue;
                    }
                    self.transpile_function(member, /*in_class=*/ true, out);
                    out.blank();
                }
                "property_declaration" => {
                    self.transpile_property(member, out);
                    out.blank();
                }
                "companion_object" => {
                    let is_interface = kt::parent_of(body).is_some_and(|class| {
                        class
                            .children(&mut class.walk())
                            .any(|child| child.kind() == "interface")
                    });
                    if is_interface {
                        self.transpile_interface_companion(member, out);
                    } else if let Some(cls) = kt::parent_of(body)
                        && let Some(name) = kt::field(cls, "name")
                    {
                        self.transpile_companion(member, self.text(name), out);
                    }
                    out.blank();
                }
                "secondary_constructor" => {
                    let class_name = kt::parent_of(body)
                        .and_then(|class| kt::field(class, "name"))
                        .map(|name| self.text(name).to_string())
                        .unwrap_or_default();
                    if !self.transpile_secondary_constructor(member, &class_name, out) {
                        self.diag_untranslatable(
                            member,
                            "secondary constructor is not a direct `this(...)` or `super(...)` delegation with an empty or simple assignment-only body",
                        );
                    }
                }
                "anonymous_initializer"
                    if kt::parent_of(body).is_some_and(|decl| {
                        kt::child(decl, "modifiers").is_some_and(|m| self.text(m).contains("data"))
                    }) => {}
                "class_declaration" | "object_declaration" => {
                    self.transpile_type_decl(member, out);
                    out.blank();
                }
                "line_comment" | "block_comment" | ";" | "{" | "}" => {}
                _ => {
                    if member.is_named() {
                        self.diag_untranslatable(
                            member,
                            format!("class member not supported: {}", member.kind()),
                        );
                    }
                }
            }
        }
    }

    pub(crate) fn constructor_property_bridge(
        &self,
        function: tree_sitter::Node,
    ) -> Option<String> {
        let name_node = kt::field(function, "name")?;
        let name = self.text(name_node);
        let (stem, getter) = if let Some(stem) = name.strip_prefix("get") {
            (stem, true)
        } else {
            (name.strip_prefix("set")?, false)
        };
        let mut chars = stem.chars();
        let first = chars.next()?;
        let property = first.to_lowercase().collect::<String>() + chars.as_str();
        let parameters = kt::child(function, "function_value_parameters")?;
        let parameter_count = parameters
            .children(&mut parameters.walk())
            .filter(|child| child.kind() == "parameter")
            .count();
        let body = kt::child(function, "function_body")?;
        let mut owner = kt::parent_of(function);
        while owner.is_some_and(|candidate| {
            !matches!(
                candidate.kind(),
                "class_declaration" | "enum_class_declaration"
            )
        }) {
            owner = owner.and_then(kt::parent_of);
        }
        let constructor_property = owner
            .and_then(|class| kt::child(class, "primary_constructor"))
            .and_then(|constructor| kt::child(constructor, "class_parameters"))
            .is_some_and(|parameters| {
                parameters
                    .children(&mut parameters.walk())
                    .filter(|candidate| candidate.kind() == "class_parameter")
                    .any(|candidate| {
                        (kt::child(candidate, "val").is_some()
                            || kt::child(candidate, "var").is_some())
                            && kt::child(candidate, "identifier")
                                .is_some_and(|name| self.text(name) == property)
                    })
            });
        if getter {
            return (constructor_property
                && parameter_count == 0
                && body
                    .children(&mut body.walk())
                    .find(|child| child.is_named())
                    .is_some_and(|expression| self.text(expression).trim() == property))
            .then_some(property);
        }
        let setter_matches = constructor_property
            && self.self_getters.contains_key(&property)
            && name == format!("set{}", capitalize(&property));
        let normalized = self
            .text(body)
            .chars()
            .filter(|ch| !ch.is_whitespace() && !matches!(ch, '{' | '}' | ';'))
            .collect::<String>()
            .replace("this.", "");
        (setter_matches && parameter_count == 1 && normalized == format!("{property}=value"))
            .then_some(property)
    }
}

/// Render a source path for the provenance header: forward slashes only (javac
/// rejects `\uXXXX`-shaped sequences in comments on some paths) and without the
/// Windows verbatim prefix, which otherwise leaks a meaningless `//?/D:/…`.
fn normalized_source_path(path: &std::path::Path) -> String {
    let text = crate::paths::display(path).to_string();
    let text = text
        .strip_prefix("\\\\?\\")
        .or_else(|| text.strip_prefix("//?/"))
        .or_else(|| text.strip_prefix("\\\\.\\"))
        .or_else(|| text.strip_prefix("//./"))
        .unwrap_or(&text);
    text.replace('\\', "/")
}

/// Whether generated Java actually mentions `import`'s simple name. Unused
/// imports are not merely noise — when the name resolves only in Kotlin (a
/// dependency's top-level function, or a declaration that stayed Kotlin in a
/// split file) javac fails the whole compilation unit with "cannot find
/// symbol". Wildcard imports are kept: they are package-wide and there is no
/// name to match.
/// Qualify a simple type name that collides with the enclosing declaration's
/// own name, but ONLY where it sits in a type-argument list (`<Length>`,
/// `<Length,`, `List<Length[]>`): that is where Java's "the class's own name
/// outranks its imports" rule changes the meaning of migrated code. Kotlin
/// resolved such a name to the import, so `Unit<Length>` satisfies
/// `Unit<Q : Quantity<Q>>`; javac instead reads the enclosing `Length` and
/// rejects the type argument. Only the imported top-level FQN can be the
/// intended target, so the import list is the resolver.
fn qualify_shadowing_type_arguments(
    body: &str,
    own_names: &[String],
    imports: &[String],
) -> String {
    // A type declared by this very compilation unit outranks any import in
    // javac's resolution; Kotlin instead resolved the name to the import.
    // Each such collision needs the imported FQN spelled out.
    let mut shadows: Vec<(&str, &str)> = Vec::new();
    for import in imports {
        let path = match import.strip_prefix("static ") {
            Some(path) => path.trim(),
            None => import.trim(),
        };
        if path.contains(' ') || !path.contains('.') {
            continue;
        }
        let Some(last) = path.rsplit('.').next().filter(|last| !last.is_empty()) else {
            continue;
        };
        if own_names
            .iter()
            .any(|own| own.rsplit('.').next() == Some(last))
            && !shadows.iter().any(|(simple, _)| *simple == last)
        {
            shadows.push((last, path));
        }
    }
    if shadows.is_empty() {
        return body.to_string();
    }
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len() + 32);
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            let mut end = index;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            let word = &body[index..end];
            // The token before a type argument is `<` (first) or `,`
            // (later); the token after a type argument is `>`, `,` or `[`.
            // A method-argument list (`f(a, Length)`) never closes that way,
            // and `Length.member` / `new Length(` keep the class's own name.
            let opens = body[..index]
                .trim_end()
                .chars()
                .last()
                .is_some_and(|previous| previous == '<' || previous == ',');
            let closes = body[end..]
                .trim_start()
                .chars()
                .next()
                .is_some_and(|next| next == '>' || next == ',' || next == '[');
            if opens
                && closes
                && let Some((_, fqn)) = shadows.iter().find(|(simple, _)| *simple == word)
            {
                out.push_str(fqn);
            } else {
                out.push_str(word);
            }
            index = end;
            continue;
        }
        // Multi-byte UTF-8 (string literals) must be copied whole.
        if byte.is_ascii() {
            out.push(byte as char);
            index += 1;
        } else {
            let width = body[index..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&body[index..index + width]);
            index += width;
        }
    }
    out
}

fn import_is_referenced(import: &str, body: &str) -> bool {
    let path = import.strip_prefix("static ").unwrap_or(import).trim();
    if path.is_empty() || path.ends_with(".*") {
        return true;
    }
    let simple = path.rsplit('.').next().unwrap_or(path);
    if simple.is_empty() {
        return true;
    }
    let mut rest = body;
    while let Some(index) = rest.find(simple) {
        let before = rest[..index].chars().next_back();
        let after = rest[index + simple.len()..].chars().next();
        let boundary =
            |ch: Option<char>| ch.is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '$'));
        if boundary(before) && boundary(after) {
            return true;
        }
        rest = &rest[index + simple.len()..];
    }
    false
}

/// Drop only parsed comment nodes; comment-like text inside annotation string
/// values remains data. A disabled class literal cannot authorize or block code.
fn annotation_arguments_without_comments(root: tree_sitter::Node, source: &str) -> String {
    let mut comments = Vec::new();
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        if node.kind().ends_with("comment") {
            comments.push((
                node.start_byte() - root.start_byte(),
                node.end_byte() - root.start_byte(),
            ));
        } else {
            nodes.extend(node.children(&mut node.walk()));
        }
    }
    let mut arguments = source[root.byte_range()].to_owned();
    comments.sort_unstable();
    for (start, end) in comments.into_iter().rev() {
        arguments.replace_range(start..end, " ");
    }
    arguments
}

/// Kotlin auto-wraps nested annotation arguments into arrays; Java requires
/// braces around them. Named elements and ordinary values stay unchanged.
fn brace_wrap_unnamed_nested_annotations(prefixed: &str) -> String {
    // The argument text arrives wrapped in its enclosing parentheses (from
    // `value_arguments` or the wrapper's parenthesized_expression) — strip
    // them so the top-level element scan sees the bare element list.
    let trimmed = prefixed.trim();
    let trimmed = if trimmed.starts_with('(') && trimmed.ends_with(')') {
        trimmed[1..trimmed.len() - 1].trim()
    } else {
        trimmed
    };
    // An empty argument list or an existing array needs no wrapping.
    if trimmed.is_empty() || trimmed.starts_with('{') {
        return prefixed.to_string();
    }
    // Split the top-level elements at depth-0 commas.
    let mut elements: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for character in trimmed.chars() {
        match character {
            '(' => {
                depth += 1;
                current.push(character);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            '{' => {
                depth += 1;
                current.push(character);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 => {
                elements.push(current.clone());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    elements.push(current);
    // Wrap only when EVERY top-level element is a nested annotation
    // invocation (`@A.B(...)`): that is Kotlin's auto-wrapped array form
    // (a single one included). Any named element (`name = value`) or simple
    // value means the shape is already valid Java.
    let all_nested_annotations = elements
        .iter()
        .filter(|element| !element.trim().is_empty())
        .all(|element| element.trim_start().starts_with('@'));
    if !all_nested_annotations {
        return prefixed.to_string();
    }
    // Wrap the element list in braces, keeping the enclosing parentheses:
    // Java annotation array values are `@X({elem, elem})`.
    format!("({{{trimmed}}})")
}

/// Kotlin annotation arrays use `[a, b]`; Java annotation arrays use
/// `{a, b}`. Brackets inside string and character literals remain text.
fn lower_annotation_arrays(arguments: &str) -> String {
    let mut result = String::with_capacity(arguments.len());
    let mut quote = None;
    let mut escaped = false;
    for ch in arguments.chars() {
        if let Some(active) = quote {
            result.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        if matches!(ch, '\'' | '"') {
            quote = Some(ch);
            result.push(ch);
        } else if ch == '[' {
            result.push('{');
        } else if ch == ']' {
            result.push('}');
        } else {
            result.push(ch);
        }
    }
    result
}

/// Kotlin raw strings are not valid Java string literals. Annotation values
/// are compile-time constants, so lower each `"""..."""` segment to a normal
/// Java string with explicit escapes.
fn lower_annotation_raw_strings(arguments: &str) -> String {
    let mut result = String::with_capacity(arguments.len());
    let mut remaining = arguments;
    while let Some(start) = remaining.find("\"\"\"") {
        result.push_str(&remaining[..start]);
        let content = &remaining[start + 3..];
        let Some(end) = content.find("\"\"\"") else {
            result.push_str(&remaining[start..]);
            return result;
        };
        result.push('"');
        for character in content[..end].chars() {
            match character {
                '\\' => result.push_str("\\\\"),
                '"' => result.push_str("\\\""),
                '\r' => result.push_str("\\r"),
                '\n' => result.push_str("\\n"),
                character => result.push(character),
            }
        }
        result.push('"');
        remaining = &content[end + 3..];
    }
    result.push_str(remaining);
    result
}

/// Kotlin permits annotation arguments to be positional even when the Java
/// annotation's first element is not named `value`. Java accepts the shorthand
/// only for `value`, so external annotations whose bytecode is outside the
/// source index need a small signature bridge. Jakarta Persistence `Column`
/// declares `name` first; preserve the Kotlin meaning in emitted Java.
fn lower_known_annotation_positional_argument(name: &str, arguments: &str) -> String {
    if annotation_simple_name(name) != "Column"
        || !arguments.starts_with('(')
        || !arguments.ends_with(')')
    {
        return arguments.to_string();
    }
    let inner = arguments[1..arguments.len() - 1].trim();
    if inner.is_empty() || inner.contains('=') || inner.contains(',') {
        return arguments.to_string();
    }
    format!("(name = {inner})")
}

/// Java requires the `@` prefix on nested annotation types inside annotation
/// arguments (`@JsonSubTypes.Type(value = X.class)`), while Kotlin omits it
/// (`JsonSubTypes.Type(value = X::class)`). Walk the argument text and prefix
/// `@` on every qualified identifier chain that starts a call — `A.B(` where
/// the chain is preceded by nothing, `(`, `,`, `=`, or whitespace and not
/// already `@`. Only qualified chains (containing `.`) are prefixed: a bare
/// `name(` inside annotation values is not a nested annotation shape.
fn prefix_nested_annotations(argument_text: &str) -> String {
    if !argument_text.contains('(') {
        return argument_text.to_string();
    }
    let mut result = String::with_capacity(argument_text.len() + 8);
    let mut token_start: Option<usize> = None;
    let bytes = argument_text.as_bytes();
    let mut index = 0usize;
    // A `Name(` inside a STRING literal is text, not a nested annotation:
    // `@Anno(name = "a(b)")` must not come out as `"@a(b)"`.
    let mut in_string = false;
    while index < bytes.len() {
        let character = bytes[index] as char;
        if character == '"' {
            in_string = !in_string;
            token_start = None;
            result.push(character);
            index += 1;
            continue;
        }
        if in_string {
            result.push(character);
            index += 1;
            if character == '\\' && index < bytes.len() {
                result.push(bytes[index] as char);
                index += 1;
            }
            continue;
        }
        let is_identifier_byte = character.is_alphabetic()
            || character == '_'
            || (character.is_ascii_digit() && token_start.is_some());
        if is_identifier_byte || (character == '.' && token_start.is_some()) {
            if token_start.is_none() {
                token_start = Some(index);
            }
            result.push(character);
            index += 1;
            continue;
        }
        if character == '(' {
            if let Some(start) = token_start {
                let token = &argument_text[start..index];
                let already_annotated =
                    start > 0 && argument_text[..start].trim_end().ends_with('@');
                // Any `Name(` in an annotation argument is a nested annotation:
                // Java has no constructor calls or method calls there, only
                // constants and annotations. Kotlin writes the nested form
                // WITHOUT the `@`, so it has to be added — a bare `Index(...)`
                // left as-is is javac's "annotation value must be an
                // annotation".
                if !already_annotated {
                    let insert_at = result.len() - token.chars().count();
                    result.insert(insert_at, '@');
                }
                token_start = None;
            }
            result.push('(');
            index += 1;
            continue;
        }
        token_start = None;
        result.push(character);
        index += 1;
    }
    // A Kotlin argument list may end with a trailing comma
    // (`@Anno(a = 1,)`); Java annotation arrays reject it — drop a comma
    // immediately followed by the closing paren, across line breaks.
    let mut cleaned = String::with_capacity(result.len());
    let mut characters = result.chars().peekable();
    while let Some(character) = characters.next() {
        if character == ',' {
            let mut lookahead = characters.clone();
            let mut trimmed = String::new();
            while let Some(&next) = lookahead.peek() {
                if next.is_whitespace() {
                    trimmed.push(next);
                    lookahead.next();
                } else {
                    break;
                }
            }
            if matches!(lookahead.peek(), Some(&')') | Some(&'}')) {
                continue; // drop the comma (and surrounding whitespace stays)
            }
        }
        cleaned.push(character);
    }
    cleaned
}

/// Java reserved identifiers that cannot name a field or parameter.
/// A persistence annotation whose classes the ORM must be able to subclass at
/// runtime for lazy proxies. `final` forbids that, and the failure is at
/// STARTUP rather than compile time — Kotlin marks such sources `open` via the
/// all-open plugin, leaving no `open` token in the AST for us to read.
/// The use-site target of an annotation (`@get:Foo` -> `get`), or `None` when
/// it has none. Kotlin lets an annotation say which element it belongs to;
/// dropping the prefix is required (Java has no such syntax) but dropping the
/// TARGET is a decision, not a detail.
fn annotation_class_literal_names(root: tree_sitter::Node, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "identifier"
            && node
                .utf8_text(source.as_bytes())
                .is_ok_and(|text| text == "class")
            && let Some(separator) = node.prev_sibling().filter(|sibling| sibling.kind() == "::")
            && let Some(receiver) = separator.prev_sibling().filter(|receiver| {
                matches!(
                    receiver.kind(),
                    "identifier" | "type_identifier" | "navigation_expression" | "user_type"
                )
            })
            && let Ok(name) = receiver.utf8_text(source.as_bytes())
            && name.split('.').all(|part| {
                !part.is_empty()
                    && part.chars().enumerate().all(|(position, ch)| {
                        ch == '_'
                            || ch == '$'
                            || ch.is_alphabetic()
                            || (position > 0 && ch.is_ascii_digit())
                    })
            })
        {
            names.push(name.to_string());
        }
        stack.extend(node.children(&mut node.walk()));
    }
    names
}

fn use_site_target(annotation: &str) -> Option<&str> {
    let rest = annotation.trim_start().strip_prefix('@')?;
    let target = rest.split(':').next()?;
    if rest.contains(':') && !target.is_empty() && target.chars().all(|c| c.is_ascii_alphabetic()) {
        Some(target)
    } else {
        None
    }
}

/// The simple name of an annotation as it reached the emitted Java text:
/// `@jakarta.persistence.Entity` -> `Entity`, `@Type(IEnumerationType::class)`
/// -> `Type`.
fn annotation_simple_name(annotation: &str) -> &str {
    annotation
        .trim_start_matches('@')
        .split('(')
        .next()
        .unwrap_or("")
        .trim()
        .rsplit('.')
        .next()
        .unwrap_or("")
}

fn is_orm_proxied_annotation(annotation: &str) -> bool {
    matches!(
        annotation_simple_name(annotation),
        "Entity" | "MappedSuperclass"
    )
}

/// The no-arg trigger set of Kotlin's `kotlin-jpa` plugin — the `no-arg`
/// plugin with the JPA preset, which annotates exactly these three
/// annotations with "needs a zero-argument constructor".
fn is_jpa_no_arg_annotation(annotation: &str) -> bool {
    matches!(
        annotation_simple_name(annotation),
        "Entity" | "Embeddable" | "MappedSuperclass"
    )
}

/// The value a Java field of this declared type holds when no constructor
/// assigns it: the zero literal for a primitive, `null` for everything else
/// (references, arrays, boxed primitives, type variables). A leading
/// annotation (`@Nullable Boolean`) is not part of the type name, and a
/// generic/array suffix cannot turn a reference type into a primitive.
fn jvm_default_literal(java_type: &str) -> &'static str {
    let trimmed = java_type.trim();
    let bare = match trimmed.strip_prefix('@') {
        Some(rest) => rest.split_once(' ').map(|(_, ty)| ty.trim()).unwrap_or(""),
        None => trimmed,
    };
    let head = bare.split(['<', '[']).next().unwrap_or(bare).trim();
    match head {
        "boolean" => "false",
        "char" => "'\\0'",
        "byte" => "(byte) 0",
        "short" => "(short) 0",
        "int" => "0",
        "long" => "0L",
        "float" => "0.0f",
        "double" => "0.0",
        _ => "null",
    }
}

fn java_reserved(word: &str) -> bool {
    matches!(
        word,
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "var"
            | "void"
            | "volatile"
            | "while"
            | "sealed"
            | "permits"
            | "record"
    )
}
