//! Snapshot-checked accessor-layout edits. Caller migration is a separate
//! obligation; these edits alone are not a complete workspace migration.
use super::GetterFamily;
use crate::semantics::{self, SourceLocation, SymbolId};
use crate::smart_cast::Edit;
use crate::translation_plan::PlannedSourceEdit;
use crate::transpiler::{kt, unit::kotlin_builtin_annotation_binding};
use crate::workspace::{SourceFile, SourceIndex, SourceOverlay};
use serde::Serialize;
use std::collections::BTreeMap;
use tree_sitter::Node;

#[derive(Debug, Serialize)]
pub struct LayoutPlan {
    pub root: SymbolId,
    pub edits: Vec<PlannedSourceEdit>,
    pub bean_views: Vec<SymbolId>,
    pub java_adapters: Vec<SymbolId>,
    pub caller_migration_required: bool,
}

impl LayoutPlan {
    pub fn overlays(&self, index: &SourceIndex) -> Result<Vec<SourceOverlay>, String> {
        let mut grouped = BTreeMap::<_, Vec<_>>::new();
        for edit in &self.edits {
            grouped
                .entry(edit.location.file.clone())
                .or_default()
                .push(edit);
        }
        let mut overlays = Vec::new();
        for (path, mut edits) in grouped {
            let file = index.source_file(&path).ok_or("layout source vanished")?;
            let source = file.source_text();
            edits.sort_by_key(|edit| (edit.location.start_byte, edit.location.end_byte));
            for edit in &edits {
                if edit.location.snapshot_hash != *blake3::hash(source.as_bytes()).as_bytes()
                    || edit.location.start_byte > edit.location.end_byte
                    || edit.location.end_byte > source.len()
                {
                    return Err(format!(
                        "layout source snapshot/range changed: {}",
                        path.display()
                    ));
                }
            }
            if edits.windows(2).any(|pair| {
                pair[1].location.start_byte < pair[0].location.end_byte
                    || pair[1].location.start_byte == pair[0].location.start_byte
            }) {
                return Err(format!(
                    "overlapping getter layout edits: {}",
                    path.display()
                ));
            }
            let changed = crate::smart_cast::apply_edits(
                source,
                edits
                    .into_iter()
                    .map(|edit| Edit {
                        start: edit.location.start_byte,
                        end: edit.location.end_byte,
                        text: edit.replacement.clone(),
                    })
                    .collect(),
            );
            if file.language == crate::workspace::SourceLanguage::Kotlin {
                let before = crate::transpiler::parse_tree(source);
                let after = crate::transpiler::parse_tree(&changed);
                if after.root_node().has_error() && !before.root_node().has_error() {
                    return Err(format!(
                        "getter layout introduces a Kotlin parse error: {}",
                        path.display()
                    ));
                }
                let before = crate::translation_plan::analyze(source, before.root_node(), &path);
                let after = crate::translation_plan::analyze(&changed, after.root_node(), &path);
                let symbols = |plan: &crate::translation_plan::TranslationPlan| {
                    plan.declarations
                        .iter()
                        .map(|decision| decision.symbol_id.clone())
                        .collect::<std::collections::BTreeSet<_>>()
                };
                if symbols(&before) != symbols(&after) {
                    return Err(format!(
                        "getter layout changes Kotlin ownership boundaries: {}",
                        path.display()
                    ));
                }
            }
            overlays.push(SourceOverlay::Replace {
                path,
                source: changed,
                language: file.language,
            });
        }
        Ok(overlays)
    }
}

pub fn plan_layout(index: &SourceIndex, family: &GetterFamily) -> Result<LayoutPlan, String> {
    if !family.unresolved_edges.is_empty() {
        return Err("getter family has unresolved ancestry".into());
    }
    for descendant in &family.descendants {
        let owner = declaration(index, descendant)?;
        if owner.language != crate::workspace::SourceLanguage::Java {
            continue;
        }
        let mut path = descendant.owner_path.clone();
        path.push(descendant.name.clone());
        if owner.kind != crate::workspace::DeclarationKind::Class
            || !family
                .java_implementations
                .iter()
                .any(|method| method.owner_path == path && method.file == descendant.file)
        {
            // Lombok and inherited JVM bridges may supply an implementation
            // absent from these source headers. A source-only layout cannot
            // assign its property/independent role or invent an adapter.
            return Err("Java getter implementation requires compiled declaration facts".into());
        }
    }
    let mut plan = LayoutPlan {
        root: family.root.clone(),
        edits: Vec::new(),
        bean_views: Vec::new(),
        java_adapters: Vec::new(),
        caller_migration_required: true,
    };
    for property in &family.property_owners {
        if property
            .owner
            .file
            .extension()
            .is_none_or(|ext| ext != "kt")
        {
            return Err("property layout requires Kotlin declaration evidence".into());
        }
        if property.getter.as_deref() != Some(family.original_getter.as_str()) {
            return Err("getter family has different physical property getters".into());
        }
        let file = index
            .source_file(&property.owner.file)
            .ok_or("property source missing")?;
        let source = file.source_text();
        let tree = index.source_tree(file).ok_or("property parse missing")?;
        let owner = find_owner(tree.root_node(), source, &property.owner)?;
        let node =
            find_property(owner, source, &property.property).ok_or("property syntax missing")?;
        // Scope this compiler-only diagnostic to the owner. Putting it on a
        // property adds metadata to the property bridge's annotation contract;
        // on constructor parameters it also confuses adjacent use-site syntax.
        {
            let (suppress, existing) =
                crate::getter_namespace::name_suppression(index, file, owner, source)?;
            if let Some(existing) = existing {
                plan.edits
                    .push(edit(file, existing.start, existing.end, existing.text));
            }
            if suppress {
                plan.edits.push(edit(
                    file,
                    owner.start_byte(),
                    owner.start_byte(),
                    "@kotlin.Suppress(\"INAPPLICABLE_JVM_NAME\")\n".into(),
                ));
            }
        }
        let replacement = renamed_property(index, file, node, &family.proposed_getter)?;
        plan.edits
            .push(edit(file, node.start_byte(), node.end_byte(), replacement));
        // An existing source function already supplies this owner's bean view.
        if family
            .independent_methods
            .iter()
            .any(|function| function.owner_path == member_path(&property.owner))
        {
            continue;
        }
        let ty = property
            .declared_type
            .as_deref()
            .ok_or("property getter return type is not established")?;
        let inherited = super::ancestry(index, declaration(index, &property.owner)?)
            .iter()
            .any(|ancestor| {
                family
                    .property_owners
                    .iter()
                    .any(|p| p.owner == semantics::workspace_symbol(index, ancestor))
            });
        let modifiers = if inherited {
            "override "
        } else if property.owner.kind == "interface" {
            ""
        } else {
            "open "
        };
        let method = format!(
            "\n    {modifiers}fun {}(): {ty} = {}\n",
            family.original_getter, property.property
        );
        add_member(&mut plan.edits, file, owner, method)?;
        plan.bean_views.push(property.owner.clone());
    }
    for function in &family.independent_methods {
        let file = index
            .source_file(&function.file)
            .ok_or("method source missing")?;
        let tree = index.source_tree(file).ok_or("method parse missing")?;
        let node = find_owner(tree.root_node(), file.source_text(), function)?;
        if kt::child(node, "type_parameters").is_some() {
            return Err(
                "generic zero-argument bean override needs a separate signature adaptation".into(),
            );
        }
        let owner = file
            .declarations
            .iter()
            .find(|owner| {
                member_path(&semantics::workspace_symbol(index, owner)) == function.owner_path
            })
            .ok_or("method owner missing")?;
        let inherited = std::iter::once(owner)
            .chain(super::ancestry(index, owner))
            .map(|owner| semantics::workspace_symbol(index, owner))
            .collect::<std::collections::BTreeSet<_>>();
        let method = owner
            .members
            .iter()
            .find(|method| {
                method.kind == crate::workspace::MemberKind::Method
                    && method.name == function.name
                    && method.parameter_types.is_empty()
                    && !method.is_static
            })
            .ok_or("independent getter declaration missing")?;
        let return_type = method
            .type_name
            .as_deref()
            .ok_or("independent getter return type is not established")?;
        for property in family
            .property_owners
            .iter()
            .filter(|property| inherited.contains(&property.owner))
        {
            let property_type = property
                .declared_type
                .as_deref()
                .ok_or("property return type is not established")?;
            if !index.property_getter_return_compatible_in_files(
                &file.path,
                return_type,
                &property.owner.file,
                property_type,
            ) {
                return Err("independent getter has no proven compatible bean return type".into());
            }
        }
        let overrides_view = super::ancestry(index, owner).iter().any(|ancestor| {
            plan.bean_views
                .contains(&semantics::workspace_symbol(index, ancestor))
        });
        if overrides_view && !has_modifier(node, file.source_text(), "override") {
            let keyword = node
                .children(&mut node.walk())
                .find(|child| kt::text(*child, file.source_text()) == "fun")
                .ok_or("function keyword missing")?;
            plan.edits.push(edit(
                file,
                keyword.start_byte(),
                keyword.start_byte(),
                "override ".into(),
            ));
        }
    }
    for function in &family.java_implementations {
        let file = index
            .source_file(&function.file)
            .ok_or("Java getter source missing")?;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .map_err(|e| e.to_string())?;
        let tree = parser
            .parse(file.source_text(), None)
            .ok_or("Java parse missing")?;
        let mut owner = function.clone();
        owner.name = owner.owner_path.pop().ok_or("Java getter owner missing")?;
        owner.kind = "class".into();
        owner.parameters.clear();
        let node = find_owner(tree.root_node(), file.source_text(), &owner)?;
        let getter = find_owner(tree.root_node(), file.source_text(), function)?;
        let ty = getter
            .child_by_field_name("type")
            .ok_or("Java getter return syntax missing")?;
        let method = format!(
            "\n    @Override\n    public {} {}() {{ return {}(); }}\n",
            kt::text(ty, file.source_text()),
            family.proposed_getter,
            family.original_getter
        );
        add_member(&mut plan.edits, file, node, method)?;
        plan.java_adapters.push(function.clone());
    }
    // Check overlap and source hashes before exposing a usable edit bundle.
    plan.overlays(index)?;
    Ok(plan)
}

fn declaration<'a>(
    index: &'a SourceIndex,
    symbol: &SymbolId,
) -> Result<&'a crate::workspace::Declaration, String> {
    index
        .source_file(&symbol.file)
        .and_then(|file| {
            file.declarations
                .iter()
                .find(|d| semantics::workspace_symbol(index, d) == *symbol)
        })
        .ok_or_else(|| "layout owner declaration missing".into())
}
fn member_path(owner: &SymbolId) -> Vec<String> {
    let mut path = owner.owner_path.clone();
    path.push(owner.name.clone());
    path
}
fn edit(
    file: &SourceFile,
    start_byte: usize,
    end_byte: usize,
    replacement: String,
) -> PlannedSourceEdit {
    PlannedSourceEdit {
        location: SourceLocation {
            file: file.path.clone(),
            snapshot_hash: *blake3::hash(file.source_text().as_bytes()).as_bytes(),
            start_byte,
            end_byte,
        },
        replacement,
        speculative: true,
    }
}
fn find_owner<'t>(root: Node<'t>, source: &str, symbol: &SymbolId) -> Result<Node<'t>, String> {
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        if semantics::declaration_node_kind(node.kind())
            && semantics::symbol_id_for_node(source, node, &symbol.file) == *symbol
        {
            return Ok(node);
        }
        nodes.extend(node.named_children(&mut node.walk()));
    }
    Err(format!(
        "layout declaration syntax missing: {}",
        symbol.stable_key()
    ))
}
fn find_property<'t>(owner: Node<'t>, source: &str, name: &str) -> Option<Node<'t>> {
    let mut nodes = owner.named_children(&mut owner.walk()).collect::<Vec<_>>();
    while let Some(node) = nodes.pop() {
        if matches!(
            node.kind(),
            "class_body"
                | "enum_class_body"
                | "primary_constructor"
                | "class_parameters"
                | "enum_body_declarations"
        ) {
            nodes.extend(node.named_children(&mut node.walk()));
        }
        if matches!(node.kind(), "property_declaration" | "class_parameter") {
            let variable = kt::child(node, "variable_declaration").unwrap_or(node);
            if kt::child(variable, "identifier").is_some_and(|id| kt::text(id, source) == name) {
                return Some(node);
            }
        }
    }
    None
}
fn has_modifier(node: Node<'_>, source: &str, wanted: &str) -> bool {
    let Some(modifiers) = kt::child(node, "modifiers") else {
        return false;
    };
    let mut nodes = vec![modifiers];
    while let Some(node) = nodes.pop() {
        if node.kind() == "annotation" {
            continue;
        }
        if node.child_count() == 0 && kt::text(node, source) == wanted {
            return true;
        }
        nodes.extend(node.children(&mut node.walk()));
    }
    false
}
fn renamed_property(
    index: &SourceIndex,
    file: &SourceFile,
    node: Node<'_>,
    getter: &str,
) -> Result<String, String> {
    let source = file.source_text();
    for accessor in node
        .named_children(&mut node.walk())
        .filter(|child| child.kind() == "getter")
    {
        if kt::child(accessor, "modifiers").is_some_and(|modifiers| {
            modifiers
                .named_children(&mut modifiers.walk())
                .any(|child| child.kind() == "annotation")
        }) {
            return Err("getter-header annotation needs a bean/accessor role policy".into());
        }
    }
    let mut edits = Vec::new();
    let mut replaced = false;
    if let Some(modifiers) = kt::child(node, "modifiers") {
        for annotation in modifiers
            .named_children(&mut modifiers.walk())
            .filter(|node| node.kind() == "annotation")
        {
            let target = kt::child(annotation, "use_site_target")
                .map(|target| kt::text(target, source).trim().trim_end_matches(':'));
            if target != Some("get") {
                continue;
            }
            if kotlin_builtin_annotation_binding(
                annotation,
                "kotlin.jvm.JvmName",
                source,
                Some(index),
                Some(file),
            ) != Some(true)
            {
                return Err(format!(
                    "getter annotation needs a bean/accessor role policy: {}",
                    kt::text(annotation, source)
                ));
            }
            edits.push(Edit {
                start: annotation.start_byte() - node.start_byte(),
                end: annotation.end_byte() - node.start_byte(),
                text: format!("@get:kotlin.jvm.JvmName(\"{getter}\")"),
            });
            replaced = true;
        }
    }
    let body = crate::smart_cast::apply_edits(kt::text(node, source), edits);
    Ok(format!(
        "{}{}",
        if replaced {
            String::new()
        } else {
            format!("@get:kotlin.jvm.JvmName(\"{getter}\") ")
        },
        body
    ))
}
fn add_member(
    edits: &mut Vec<PlannedSourceEdit>,
    file: &SourceFile,
    owner: Node<'_>,
    method: String,
) -> Result<(), String> {
    let body = kt::child(owner, "class_body")
        .or_else(|| kt::child(owner, "enum_class_body"))
        .or_else(|| owner.child_by_field_name("body"));
    if let Some(body) = body {
        let close = body
            .children(&mut body.walk())
            .find(|child| kt::text(*child, file.source_text()) == "}")
            .ok_or("class closing brace missing")?;
        if let Some(previous) = edits.iter_mut().find(|edit| {
            edit.location.file == file.path
                && edit.location.start_byte == close.start_byte()
                && edit.location.end_byte == close.start_byte()
        }) {
            previous.replacement.push_str(&method);
        } else {
            edits.push(edit(file, close.start_byte(), close.start_byte(), method));
        }
    } else {
        edits.push(edit(
            file,
            owner.end_byte(),
            owner.end_byte(),
            format!(" {{{method}}}"),
        ));
    }
    Ok(())
}
