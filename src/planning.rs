//! Accepted Java output planning.
//!
//! The legacy lowering implementation is used only to prepare candidate Java
//! source. Candidate files enter this plan only after strict Java parsing, and
//! final output is rendered from the owned syntax trees.

use crate::java_ir::{JavaCompilationUnit, JavaIrError, JavaSyntaxNode, render};
use crate::semantics::{FactStatus, SemanticProvider, SymbolId};
use crate::translation_plan::TranslationPlan;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedJavaFile {
    pub path: String,
    pub unit: JavaCompilationUnit,
}

/// Candidate syntax bound to the source snapshot and declaration transactions
/// that produced it. This is preparation data, never final emitted output.
pub struct PreparedJavaFile {
    pub path: String,
    pub source: String,
    pub owners: Vec<SymbolId>,
    pub snapshot_hash: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedPlanError {
    pub path: String,
    pub error: JavaIrError,
}

impl std::fmt::Display for AcceptedPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "generated Java {} failed validation: {}",
            self.path, self.error
        )
    }
}
impl std::error::Error for AcceptedPlanError {}

/// The immutable set of Java units that passed validation and may be emitted.
/// On validation failure, no partial set is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedTranslationPlan {
    translation: TranslationPlan,
    files: Vec<AcceptedJavaFile>,
}

impl AcceptedTranslationPlan {
    pub fn accept(
        mut translation: TranslationPlan,
        candidates: Vec<PreparedJavaFile>,
        semantic_provider: Option<&dyn SemanticProvider>,
    ) -> Result<Self, AcceptedPlanError> {
        let facade_targets = candidates
            .iter()
            .flat_map(|candidate| {
                let facade = std::path::Path::new(&candidate.path)
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("")
                    .to_owned();
                candidate
                    .owners
                    .iter()
                    .filter(|owner| {
                        owner.kind == "function"
                            && owner.owner_path.is_empty()
                            && owner.receiver.is_none()
                    })
                    .map(move |owner| (facade.clone(), owner.clone()))
            })
            .collect::<Vec<_>>();
        let mut files = Vec::with_capacity(candidates.len());
        let mut assignments = Vec::new();
        for PreparedJavaFile {
            path,
            source,
            owners,
            snapshot_hash,
        } in candidates
        {
            let mut unit = crate::java_ir::parse_java(&source).map_err(|mut error| {
                let excerpt = source
                    .get(error.start_byte..error.end_byte)
                    .unwrap_or("")
                    .chars()
                    .take(160)
                    .collect::<String>();
                if !excerpt.is_empty() {
                    error.message.push_str(&format!(" near {excerpt:?}"));
                }
                AcceptedPlanError {
                    path: path.clone(),
                    error,
                }
            })?;
            crate::java_ir::remove_unused_lombok_imports(&mut unit);
            if let Some(provider) = semantic_provider {
                attach_resolved_targets(&mut unit.root, &[], provider, &facade_targets);
            }
            assignments.push((path.clone(), owners, snapshot_hash));
            files.push(AcceptedJavaFile { path, unit });
        }
        let mut assigned = std::collections::BTreeSet::new();
        let mut paths = std::collections::BTreeSet::new();
        for (path, owners, snapshot_hash) in &assignments {
            let fail = |message: &str| AcceptedPlanError {
                path: path.clone(),
                error: JavaIrError {
                    message: message.into(),
                    start_byte: 0,
                    end_byte: 0,
                },
            };
            if *snapshot_hash != translation.source_hash {
                return Err(fail("candidate belongs to a stale source snapshot"));
            }
            if !paths.insert(path) {
                return Err(fail("duplicate generated output path"));
            }
            if owners.is_empty() {
                return Err(fail("candidate has no declaration transaction"));
            }
            for owner in owners {
                if !translation.declarations.iter().any(|d| {
                    &d.symbol_id == owner
                        && d.final_owner == Some(crate::translation_plan::BackendOwner::Java)
                }) {
                    return Err(fail("candidate declaration is not Java-owned"));
                }
                if !assigned.insert(owner.clone()) {
                    return Err(fail(
                        "declaration is assigned to multiple generated outputs",
                    ));
                }
            }
            let file = files
                .iter()
                .find(|file| &file.path == path)
                .expect("assignment has a parsed file");
            let class_owners = owners
                .iter()
                .filter(|owner| {
                    matches!(
                        owner.kind.as_str(),
                        "class" | "interface" | "enum" | "annotation" | "object"
                    )
                })
                .collect::<Vec<_>>();
            if !class_owners.is_empty() {
                let types = file
                    .unit
                    .root
                    .children
                    .iter()
                    .filter_map(element_node)
                    .filter(|node| {
                        matches!(
                            node.kind.as_str(),
                            "class_declaration"
                                | "interface_declaration"
                                | "record_declaration"
                                | "enum_declaration"
                                | "annotation_type_declaration"
                        )
                    })
                    .filter_map(declaration_name)
                    .collect::<Vec<_>>();
                if types.len() != class_owners.len()
                    || class_owners
                        .iter()
                        .any(|owner| !types.contains(&owner.name))
                {
                    return Err(fail("generated type does not match its declaration owner"));
                }
            }
        }
        if let Some(unassigned) = translation.declarations.iter().find(|d| {
            d.final_owner == Some(crate::translation_plan::BackendOwner::Java)
                && !assigned.contains(&d.symbol_id)
        }) {
            return Err(AcceptedPlanError {
                path: unassigned.symbol_id.name.clone(),
                error: JavaIrError {
                    message: "Java-owned declaration has no prepared output".into(),
                    start_byte: 0,
                    end_byte: 0,
                },
            });
        }
        translation.outputs = files
            .iter()
            .map(|file| crate::translation_plan::PlannedOutput {
                path: std::path::PathBuf::from(&file.path),
                owner: crate::translation_plan::BackendOwner::Java,
            })
            .collect();
        record_generated_declarations(&mut translation, &files, semantic_provider);
        Ok(Self { translation, files })
    }

    /// The sole final emitter: all content comes from accepted structured IR.
    pub fn emit(&self) -> Vec<(String, String)> {
        self.files
            .iter()
            .map(|file| (file.path.clone(), render(&file.unit)))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
    pub fn files(&self) -> &[AcceptedJavaFile] {
        &self.files
    }
    pub fn translation(&self) -> &TranslationPlan {
        &self.translation
    }
    pub(crate) fn rejected(translation: TranslationPlan) -> Self {
        Self {
            translation,
            files: Vec::new(),
        }
    }
}

fn record_generated_declarations(
    plan: &mut TranslationPlan,
    files: &[AcceptedJavaFile],
    provider: Option<&dyn SemanticProvider>,
) {
    let source_files: std::collections::BTreeSet<_> = plan
        .declarations
        .iter()
        .map(|d| d.id.file.clone())
        .collect();
    let mut source_symbol_map = std::collections::BTreeMap::new();
    for decision in plan
        .declarations
        .iter()
        .filter(|d| d.final_owner == Some(crate::translation_plan::BackendOwner::Java))
    {
        source_symbol_map.insert(decision.symbol_id.stable_key(), decision.symbol_id.clone());
    }
    if let Some(provider) = provider {
        for symbol in provider
            .symbols()
            .iter()
            .filter(|s| source_files.contains(&s.id.file))
        {
            source_symbol_map.insert(symbol.id.stable_key(), symbol.id.clone());
        }
    }
    let source_symbols: Vec<_> = source_symbol_map.into_values().collect();
    let mut generated = Vec::new();
    let mut methods = Vec::new();
    for file in files {
        for node in &file.unit.root.children {
            if let Some(n) = element_node(node) {
                collect_generated(n, &[], &mut generated);
                collect_methods(n, &[], &mut methods);
            }
        }
    }
    let mut bridge_ids: std::collections::BTreeSet<_> =
        plan.bridges.iter().map(|b| b.id.stable_key()).collect();
    for decl in generated {
        if let Some((origin, kind, reason)) = bridge_origin(&decl, &source_symbols) {
            let generated_id = SymbolId::generated(&origin, &kind);
            if bridge_ids.insert(generated_id.stable_key()) {
                plan.record_generated_bridge(&origin, kind, reason);
            }
        }
    }
    for file in files {
        let lombok_imports = file
            .unit
            .root
            .children
            .iter()
            .filter_map(element_node)
            .filter(|node| node.category == crate::java_ir::JavaCategory::Import)
            .filter_map(|node| {
                crate::java_ir::render(&crate::java_ir::JavaCompilationUnit { root: node.clone() })
                    .trim()
                    .strip_prefix("import lombok.")
                    .and_then(|name| name.strip_suffix(';'))
                    .map(str::to_owned)
            })
            .collect::<std::collections::BTreeSet<_>>();
        for node in &file.unit.root.children {
            if let Some(node) = element_node(node) {
                record_lombok_bridges(
                    plan,
                    node,
                    &[],
                    &source_symbols,
                    &mut bridge_ids,
                    &lombok_imports,
                );
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for (method, owner) in methods {
        let Some(name) = declaration_name(method) else {
            continue;
        };
        let origin = unique_origin(
            source_symbols
                .iter()
                .filter(|s| s.kind == "function" && s.name == name && same_owner(s, &owner)),
        )
        .or_else(|| {
            unique_origin(
                source_symbols
                    .iter()
                    .filter(|s| s.kind == "class" && owner.last().is_some_and(|o| o == &s.name)),
            )
        });
        let Some(origin) = origin else {
            continue;
        };
        let mut targets = Vec::new();
        collect_resolved_references(method, &mut targets);
        for target in targets {
            let key = (origin.stable_key(), target.stable_key());
            if seen.insert(key) {
                plan.dependencies
                    .push(crate::translation_plan::SymbolDependency {
                        from: origin.clone(),
                        spelling: target.name.clone(),
                        resolution: FactStatus::Established(target),
                    });
            }
        }
    }
}

fn record_lombok_bridges(
    plan: &mut TranslationPlan,
    node: &JavaSyntaxNode,
    owner: &[String],
    sources: &[SymbolId],
    bridge_ids: &mut std::collections::BTreeSet<String>,
    lombok_imports: &std::collections::BTreeSet<String>,
) {
    let is_type = matches!(
        node.kind.as_str(),
        "class_declaration" | "record_declaration" | "enum_declaration"
    );
    let Some(class_name) = is_type.then(|| declaration_name(node)).flatten() else {
        for child in &node.children {
            if let Some(child) = element_node(child) {
                record_lombok_bridges(plan, child, owner, sources, bridge_ids, lombok_imports);
            }
        }
        return;
    };
    let class_origin = unique_origin(sources.iter().filter(|symbol| {
        symbol.kind == "class" && symbol.name == class_name && symbol.owner_path == owner
    }));
    let mut class_owner = owner.to_vec();
    class_owner.push(class_name.clone());

    if let Some(class_origin) = class_origin {
        let annotations = class_annotation_names(node, lombok_imports);
        let data = annotations.contains("Data");
        let value = annotations.contains("Value");
        let all_args_annotation = annotations.contains("AllArgsConstructor");
        if data || value {
            for (property_name, mutable, field_type) in class_fields(node) {
                let Some(property) = unique_origin(sources.iter().filter(|symbol| {
                    symbol.kind == "property"
                        && symbol.name == property_name
                        && symbol.owner_path == class_owner
                })) else {
                    continue;
                };
                let getter_name = if field_type == "boolean" && is_lombok_is_prefix(&property_name)
                {
                    property_name.clone()
                } else if field_type == "boolean" {
                    format!("is{}", capitalize_java(&property_name))
                } else {
                    format!("get{}", capitalize_java(&property_name))
                };
                let getter_candidates = lombok_getter_candidates(&property_name, &field_type);
                if !class_has_accessor_any(node, &getter_candidates, 0) {
                    add_lombok_bridge(
                        plan,
                        bridge_ids,
                        &property,
                        format!("lombok:getter:{getter_name}()"),
                        "Lombok-generated property getter",
                    );
                }
                if data && mutable {
                    let setter_suffix =
                        if field_type == "boolean" && is_lombok_is_prefix(&property_name) {
                            property_name[2..].to_owned()
                        } else {
                            property_name.clone()
                        };
                    let setter = format!("set{}", capitalize_java(&setter_suffix));
                    let setter_candidates = lombok_setter_candidates(&property_name, &field_type);
                    if !class_has_accessor_any(node, &setter_candidates, 1) {
                        add_lombok_bridge(
                            plan,
                            bridge_ids,
                            &property,
                            format!("lombok:setter:{setter}({field_type})"),
                            "Lombok-generated property setter",
                        );
                    }
                }
            }
            for (name, signature) in [
                ("equals", "equals(java.lang.Object)"),
                ("hashCode", "hashCode()"),
                ("toString", "toString()"),
            ] {
                let arity = usize::from(name == "equals");
                if !class_has_method(node, name, arity) {
                    add_lombok_bridge(
                        plan,
                        bridge_ids,
                        &class_origin,
                        format!("lombok:{signature}"),
                        "Lombok-generated data-class method",
                    );
                }
            }
        }
        let field_types = class_fields(node)
            .into_iter()
            .map(|(_, _, ty)| ty)
            .collect::<Vec<_>>();
        let value_constructor = value && !has_explicit_constructor(node);
        let all_args_constructor =
            all_args_annotation && !class_has_constructor(node, &field_types);
        if (value_constructor || all_args_constructor) && !field_types.is_empty() {
            let signature = format!("{}({})", class_name, field_types.join(","));
            let constructor_origin = unique_origin(sources.iter().filter(|symbol| {
                symbol.kind == "constructor"
                    && symbol.owner_path == class_owner
                    && symbol.parameters.len() == field_types.len()
                    && symbol
                        .parameters
                        .iter()
                        .zip(&field_types)
                        .all(|(kotlin, java)| compatible_parameter_type(java, kotlin))
            }))
            .unwrap_or_else(|| class_origin.clone());
            add_lombok_bridge(
                plan,
                bridge_ids,
                &constructor_origin,
                format!("lombok:all-args-constructor:{signature}"),
                "Lombok-generated all-arguments constructor",
            );
        }
    }
    for child in &node.children {
        if let Some(child) = element_node(child) {
            record_lombok_bridges(
                plan,
                child,
                &class_owner,
                sources,
                bridge_ids,
                lombok_imports,
            );
        }
    }
}

fn add_lombok_bridge(
    plan: &mut TranslationPlan,
    bridge_ids: &mut std::collections::BTreeSet<String>,
    origin: &SymbolId,
    kind: String,
    reason: &'static str,
) {
    let id = SymbolId::generated(origin, &kind);
    if bridge_ids.insert(id.stable_key()) {
        plan.record_generated_bridge(origin, kind, reason);
    }
}

fn class_annotation_names(
    node: &JavaSyntaxNode,
    lombok_imports: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<String> {
    let Some(modifiers) = direct_child_kind(node, "modifiers") else {
        return std::collections::BTreeSet::new();
    };
    fn collect(
        node: &JavaSyntaxNode,
        names: &mut std::collections::BTreeSet<String>,
        lombok_imports: &std::collections::BTreeSet<String>,
    ) {
        if matches!(node.kind.as_str(), "annotation" | "marker_annotation") {
            if let Some(annotation_name) = find_field(node, &["name"]).map(|name| {
                crate::java_ir::render(&crate::java_ir::JavaCompilationUnit { root: name.clone() })
                    .trim()
                    .to_owned()
            }) {
                let simple = annotation_name
                    .rsplit('.')
                    .next()
                    .unwrap_or(&annotation_name);
                if annotation_name == format!("lombok.{simple}")
                    || (annotation_name == simple && lombok_imports.contains(simple))
                {
                    names.insert(simple.to_owned());
                }
            }
            return;
        }
        for child in &node.children {
            if let Some(child) = element_node(child) {
                collect(child, names, lombok_imports);
            }
        }
    }
    let mut names = std::collections::BTreeSet::new();
    collect(modifiers, &mut names, lombok_imports);
    names
}

fn class_fields(node: &JavaSyntaxNode) -> Vec<(String, bool, String)> {
    fn collect_field(field: &JavaSyntaxNode, out: &mut Vec<(String, bool, String)>) {
        if direct_child_kind(field, "modifiers").is_some_and(|mods| contains_token(mods, "static"))
        {
            return;
        }
        let Some(ty) = direct_field(field, &["type"]) else {
            return;
        };
        let ty = crate::java_ir::render(&crate::java_ir::JavaCompilationUnit { root: ty.clone() })
            .split_whitespace()
            .collect::<String>();
        let is_final =
            direct_child_kind(field, "modifiers").is_some_and(|mods| contains_token(mods, "final"));
        fn variables(
            node: &JavaSyntaxNode,
            ty: &str,
            mutable: bool,
            out: &mut Vec<(String, bool, String)>,
        ) {
            if node.kind == "variable_declarator" {
                if let Some(name) = direct_field(node, &["name"]).and_then(last_identifier) {
                    out.push((name, !mutable, ty.to_owned()));
                }
                return;
            }
            for child in &node.children {
                if let Some(child) = element_node(child) {
                    variables(child, ty, mutable, out);
                }
            }
        }
        variables(field, &ty, is_final, out);
    }
    let mut fields = Vec::new();
    if let Some(body) = direct_field(node, &["body"]) {
        for child in &body.children {
            if let Some(child) = element_node(child)
                && child.kind == "field_declaration"
            {
                collect_field(child, &mut fields);
            }
        }
    }
    fields
}

fn contains_token(node: &JavaSyntaxNode, text: &str) -> bool {
    node.children.iter().any(|child| match child {
        crate::java_ir::JavaElement::Token(token) => token.text == text,
        _ => element_node(child).is_some_and(|child| contains_token(child, text)),
    })
}

fn direct_child_kind<'a>(node: &'a JavaSyntaxNode, kind: &str) -> Option<&'a JavaSyntaxNode> {
    node.children
        .iter()
        .filter_map(element_node)
        .find(|child| child.kind == kind)
}

fn class_has_method(node: &JavaSyntaxNode, name: &str, arity: usize) -> bool {
    let Some(body) = direct_field(node, &["body"]) else {
        return false;
    };
    body.children.iter().filter_map(element_node).any(|member| {
        member.kind == "method_declaration"
            && declaration_name(member).as_deref() == Some(name)
            && parameter_types(member).len() == arity
    })
}

fn class_has_accessor_any(node: &JavaSyntaxNode, names: &[String], arity: usize) -> bool {
    let Some(body) = direct_field(node, &["body"]) else {
        return false;
    };
    body.children.iter().filter_map(element_node).any(|member| {
        member.kind == "method_declaration"
            && declaration_name(member).is_some_and(|declared| {
                names.iter().any(|name| declared.eq_ignore_ascii_case(name))
            })
            && parameter_types(member).len() == arity
    })
}

fn is_lombok_is_prefix(field: &str) -> bool {
    field.starts_with("is") && field.chars().nth(2).is_some_and(|c| !c.is_lowercase())
}

fn lombok_getter_candidates(field: &str, ty: &str) -> Vec<String> {
    if ty != "boolean" {
        return vec![format!("get{}", capitalize_java(field))];
    }
    let mut bases = vec![field.to_owned()];
    if is_lombok_is_prefix(field) {
        bases.push(field[2..].to_owned());
    }
    let mut names = std::collections::BTreeSet::new();
    for base in bases {
        names.insert(format!("get{}", capitalize_java(&base)));
        names.insert(format!("is{}", capitalize_java(&base)));
    }
    names.into_iter().collect()
}

fn lombok_setter_candidates(field: &str, ty: &str) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    names.insert(format!("set{}", capitalize_java(field)));
    if ty == "boolean" && is_lombok_is_prefix(field) {
        names.insert(format!("set{}", capitalize_java(&field[2..])));
    }
    names.into_iter().collect()
}

fn has_explicit_constructor(node: &JavaSyntaxNode) -> bool {
    direct_field(node, &["body"]).is_some_and(|body| {
        body.children.iter().filter_map(element_node).any(|member| {
            matches!(
                member.kind.as_str(),
                "constructor_declaration" | "compact_constructor_declaration"
            )
        })
    })
}

fn class_has_constructor(node: &JavaSyntaxNode, expected_parameters: &[String]) -> bool {
    direct_field(node, &["body"]).is_some_and(|body| {
        body.children.iter().filter_map(element_node).any(|member| {
            matches!(
                member.kind.as_str(),
                "constructor_declaration" | "compact_constructor_declaration"
            ) && parameter_types(member) == expected_parameters
        })
    })
}

fn capitalize_java(name: &str) -> String {
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[derive(Debug)]
struct GeneratedDecl {
    kind: String,
    name: String,
    parameters: Vec<String>,
    owner: Vec<String>,
}

fn element_node(element: &crate::java_ir::JavaElement) -> Option<&JavaSyntaxNode> {
    match element {
        crate::java_ir::JavaElement::Declaration(n)
        | crate::java_ir::JavaElement::Type(n)
        | crate::java_ir::JavaElement::Expression(n)
        | crate::java_ir::JavaElement::Statement(n)
        | crate::java_ir::JavaElement::Annotation(n)
        | crate::java_ir::JavaElement::Import(n)
        | crate::java_ir::JavaElement::Other(n) => Some(n),
        crate::java_ir::JavaElement::Token(_) => None,
    }
}

fn element_node_mut(element: &mut crate::java_ir::JavaElement) -> Option<&mut JavaSyntaxNode> {
    match element {
        crate::java_ir::JavaElement::Declaration(n)
        | crate::java_ir::JavaElement::Type(n)
        | crate::java_ir::JavaElement::Expression(n)
        | crate::java_ir::JavaElement::Statement(n)
        | crate::java_ir::JavaElement::Annotation(n)
        | crate::java_ir::JavaElement::Import(n)
        | crate::java_ir::JavaElement::Other(n) => Some(n),
        crate::java_ir::JavaElement::Token(_) => None,
    }
}

fn collect_generated(node: &JavaSyntaxNode, owner: &[String], out: &mut Vec<GeneratedDecl>) {
    let is_type = matches!(
        node.kind.as_str(),
        "class_declaration"
            | "record_declaration"
            | "enum_declaration"
            | "interface_declaration"
            | "annotation_type_declaration"
    );
    let is_method = matches!(
        node.kind.as_str(),
        "method_declaration" | "constructor_declaration" | "compact_constructor_declaration"
    );
    let name = if is_type || is_method {
        declaration_name(node)
    } else {
        None
    };
    let mut nested_owner = owner.to_vec();
    if let Some(name) = name {
        if is_type {
            nested_owner.push(name.clone());
        }
        if is_type || is_method {
            out.push(GeneratedDecl {
                kind: node.kind.clone(),
                name,
                parameters: if is_method {
                    parameter_types(node)
                } else {
                    Vec::new()
                },
                owner: owner.to_vec(),
            });
        }
    }
    for child in &node.children {
        if let Some(child) = element_node(child) {
            collect_generated(child, &nested_owner, out);
        }
    }
}

fn collect_methods<'a>(
    node: &'a JavaSyntaxNode,
    owner: &[String],
    out: &mut Vec<(&'a JavaSyntaxNode, Vec<String>)>,
) {
    let is_type = matches!(
        node.kind.as_str(),
        "class_declaration"
            | "record_declaration"
            | "enum_declaration"
            | "interface_declaration"
            | "annotation_type_declaration"
    );
    let mut nested_owner = owner.to_vec();
    if is_type && let Some(name) = declaration_name(node) {
        nested_owner.push(name);
    }
    if matches!(
        node.kind.as_str(),
        "method_declaration" | "constructor_declaration"
    ) {
        out.push((node, owner.to_vec()));
    }
    for child in &node.children {
        if let Some(n) = element_node(child) {
            collect_methods(n, &nested_owner, out);
        }
    }
}

fn collect_resolved_references(node: &JavaSyntaxNode, out: &mut Vec<SymbolId>) {
    if matches!(node.kind.as_str(), "method_invocation" | "field_access")
        && let Some(target) = &node.origin_target
    {
        out.push(target.clone());
    }
    for child in &node.children {
        if let Some(n) = element_node(child) {
            collect_resolved_references(n, out);
        }
    }
}

fn declaration_name(node: &JavaSyntaxNode) -> Option<String> {
    fn find(node: &JavaSyntaxNode) -> Option<String> {
        for child in &node.children {
            if let Some(n) = element_node(child) {
                if n.field_name.as_deref() == Some("name") {
                    return last_identifier(n);
                }
            } else if let crate::java_ir::JavaElement::Token(t) = child
                && t.field_name.as_deref() == Some("name")
                && t.class == crate::java_ir::JavaTokenClass::Identifier
            {
                return Some(t.text.clone());
            }
        }
        None
    }
    find(node)
}

fn parameter_types(node: &JavaSyntaxNode) -> Vec<String> {
    fn collect(node: &JavaSyntaxNode, out: &mut Vec<String>) {
        for child in &node.children {
            if let Some(n) = element_node(child) {
                if n.kind == "formal_parameter" || n.kind == "spread_parameter" {
                    if let Some(ty) = find_field(n, &["type"]) {
                        let source = crate::java_ir::render(&crate::java_ir::JavaCompilationUnit {
                            root: ty.clone(),
                        });
                        out.push(normalize_java_type(&source));
                    }
                } else {
                    collect(n, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    if let Some(parameters) = direct_field(node, &["parameters"]) {
        collect(parameters, &mut out);
    }
    out
}

fn bridge_origin(
    decl: &GeneratedDecl,
    sources: &[SymbolId],
) -> Option<(SymbolId, String, &'static str)> {
    if matches!(
        decl.kind.as_str(),
        "constructor_declaration" | "compact_constructor_declaration"
    ) {
        let constructors = sources
            .iter()
            .filter(|symbol| symbol.kind == "constructor" && symbol.owner_path == decl.owner)
            .collect::<Vec<_>>();
        if constructors.iter().any(|symbol| {
            symbol.parameters.len() == decl.parameters.len()
                && decl
                    .parameters
                    .iter()
                    .zip(&symbol.parameters)
                    .all(|(java, kotlin)| compatible_parameter_type(java, kotlin))
        }) {
            return None;
        }
        if let Some(origin) = unique_origin(constructors.into_iter().filter(|symbol| {
            symbol.parameters.len() > decl.parameters.len()
                && decl
                    .parameters
                    .iter()
                    .zip(&symbol.parameters)
                    .all(|(java, kotlin)| compatible_parameter_type(java, kotlin))
        })) {
            return Some((
                origin,
                format!(
                    "constructor-overload:{}({})",
                    decl.name,
                    decl.parameters.join(",")
                ),
                "generated constructor overload",
            ));
        }
    }
    // A source-declared method is a translated declaration, never a generated
    // bridge merely because its name also resembles a property accessor.
    if sources.iter().any(|s| {
        s.kind == "function"
            && s.name == decl.name
            && same_owner(s, &decl.owner)
            && s.parameters.len() == decl.parameters.len()
            && decl
                .parameters
                .iter()
                .zip(&s.parameters)
                .all(|(java, kotlin)| compatible_parameter_type(java, kotlin))
    }) {
        return None;
    }
    if decl.kind == "class_declaration" && decl.name == "Companion" {
        return unique_origin(sources.iter().filter(|s| {
            s.kind == "object" && s.name == "Companion" && same_owner(s, &decl.owner)
        }))
        .map(|s| (s, "companion-class".into(), "generated Companion class"));
    }
    if decl.kind == "method_declaration" {
        if let Some(property_name) = bean_property_name(&decl.name) {
            let setter = decl.name.starts_with("set");
            let params_match = if setter {
                decl.parameters.len() == 1
            } else {
                decl.parameters.is_empty()
            };
            if params_match
                && let Some(source) = unique_origin(sources.iter().filter(|s| {
                    s.kind == "property" && s.name == property_name && same_owner(s, &decl.owner)
                }))
            {
                let signature = format!("{}({})", decl.name, decl.parameters.join(","));
                let kind = if setter { "setter" } else { "getter" };
                return Some((
                    source,
                    format!("{kind}:{signature}"),
                    "generated Java property accessor",
                ));
            }
        }
        if decl.name.starts_with("component")
            && decl.name[9..].parse::<usize>().is_ok()
            && let Some(source) = unique_origin(sources.iter().filter(|s| {
                s.kind == "class"
                    && decl.owner.last().is_some_and(|owner| owner == &s.name)
                    && same_file_owner(s, &decl.owner)
            }))
        {
            return Some((
                source,
                format!("component:{}", decl.name),
                "generated data class component",
            ));
        }
        if decl.name == "copy"
            && let Some(source) = unique_origin(sources.iter().filter(|s| {
                s.kind == "class"
                    && decl.owner.last().is_some_and(|owner| owner == &s.name)
                    && same_file_owner(s, &decl.owner)
            }))
        {
            return Some((
                source,
                format!("copy:{}", decl.parameters.join(",")),
                "generated data class copy method",
            ));
        }
        if decl.owner.iter().any(|o| o == "Companion")
            && let Some(source) = unique_origin(sources.iter().filter(|s| {
                s.kind == "function"
                    && s.name == decl.name
                    && s.owner_path.iter().any(|o| o == "Companion")
                    && same_file_owner(s, &decl.owner)
            }))
        {
            return Some((
                source,
                format!(
                    "companion-forwarder:{}({})",
                    decl.name,
                    decl.parameters.join(",")
                ),
                "generated companion method forwarder",
            ));
        }
        let candidates: Vec<_> = sources
            .iter()
            .filter(|s| {
                s.kind == "function"
                    && s.name == decl.name
                    && same_owner(s, &decl.owner)
                    && s.parameters.len() > decl.parameters.len()
                    && decl
                        .parameters
                        .iter()
                        .zip(&s.parameters)
                        .all(|(java, kotlin)| compatible_parameter_type(java, kotlin))
            })
            .cloned()
            .collect();
        if candidates.len() == 1 {
            let signature = format!("{}({})", decl.name, decl.parameters.join(","));
            return Some((
                candidates[0].clone(),
                format!("default-overload:{signature}"),
                "generated default-argument overload",
            ));
        }
    }
    None
}

fn unique_origin<'a>(mut candidates: impl Iterator<Item = &'a SymbolId>) -> Option<SymbolId> {
    let first = candidates.next()?.clone();
    candidates.next().is_none().then_some(first)
}

fn same_file_owner(source: &SymbolId, java_owner: &[String]) -> bool {
    java_owner.last().is_some_and(|name| {
        source.owner_path.last().is_some_and(|owner| owner == name) || source.name == *name
    })
}

fn same_owner(source: &SymbolId, java_owner: &[String]) -> bool {
    if source.owner_path.iter().any(|part| part == "Companion")
        || java_owner.iter().any(|part| part == "Companion")
    {
        return source.owner_path == java_owner;
    }
    source.owner_path == java_owner || (source.owner_path.is_empty() && java_owner.len() == 1)
}

fn normalize_java_type(ty: &str) -> String {
    ty.chars().filter(|c| !c.is_whitespace()).collect()
}

fn compatible_parameter_type(java: &str, kotlin: &str) -> bool {
    let kotlin = kotlin.trim();
    java == normalize_java_type(kotlin)
        || matches!(
            (java, kotlin),
            ("int", "Int")
                | ("long", "Long")
                | ("short", "Short")
                | ("byte", "Byte")
                | ("double", "Double")
                | ("float", "Float")
                | ("boolean", "Boolean")
                | ("char", "Char")
                | ("java.lang.String", "String")
        )
}

fn bean_property_name(method: &str) -> Option<String> {
    let suffix = method
        .strip_prefix("get")
        .or_else(|| method.strip_prefix("set"))
        .or_else(|| method.strip_prefix("is"))?;
    if suffix.is_empty() {
        return None;
    }
    let mut chars = suffix.chars();
    let first = chars.next()?;
    Some(first.to_lowercase().chain(chars).collect())
}

fn attach_resolved_targets(
    node: &mut JavaSyntaxNode,
    owner_path: &[String],
    provider: &dyn SemanticProvider,
    facade_targets: &[(String, SymbolId)],
) {
    let type_decl = matches!(
        node.kind.as_str(),
        "class_declaration"
            | "record_declaration"
            | "enum_declaration"
            | "interface_declaration"
            | "annotation_type_declaration"
    );
    let mut nested_owner = owner_path.to_vec();
    if type_decl && let Some(name) = declaration_name(node) {
        nested_owner.push(name);
    }
    if matches!(node.kind.as_str(), "method_invocation" | "field_access") {
        node.origin_target = resolve_target(node, &nested_owner, provider, facade_targets);
    }
    for child in &mut node.children {
        if let Some(n) = element_node_mut(child) {
            attach_resolved_targets(n, &nested_owner, provider, facade_targets);
        }
    }
}

fn resolve_target(
    node: &JavaSyntaxNode,
    owner_path: &[String],
    provider: &dyn SemanticProvider,
    facade_targets: &[(String, SymbolId)],
) -> Option<SymbolId> {
    let call = node.kind == "method_invocation";
    if !call && node.kind != "field_access" {
        return None;
    }
    let name_node = direct_field(
        node,
        if call {
            &["name"][..]
        } else {
            &["field", "name"][..]
        },
    );
    let name = name_node
        .and_then(last_identifier)
        .or_else(|| last_reference_identifier(node, call))?;
    let receiver = direct_field(node, &["object", "receiver"]);
    let target_owner = if let Some(receiver) = receiver {
        match receiver_name(receiver) {
            Receiver::Type(receiver_type) => {
                let classes: Vec<_> = provider
                    .symbols()
                    .iter()
                    .filter(|s| s.id.name == receiver_type && s.id.kind == "class")
                    .collect();
                if classes.is_empty() && call {
                    return unique_origin(
                        facade_targets
                            .iter()
                            .filter(|(facade, symbol)| {
                                facade == &receiver_type
                                    && symbol.name == name
                                    && symbol.parameters.len() == argument_count(node)
                            })
                            .map(|(_, symbol)| symbol),
                    );
                }
                if classes.len() != 1 {
                    return None;
                }
                let mut owner = classes[0].id.owner_path.clone();
                owner.push(classes[0].id.name.clone());
                owner
            }
            Receiver::This => owner_path.to_vec(),
            Receiver::Unknown => return None,
        }
    } else if owner_path.last().is_some_and(|name| name.ends_with("Kt")) {
        Vec::new()
    } else {
        owner_path.to_vec()
    };
    let kind = if call { "function" } else { "property" };
    let arity = if call {
        Some(argument_count(node))
    } else {
        None
    };
    let matches: Vec<_> = provider
        .symbols()
        .iter()
        .filter(|symbol| {
            symbol.id.name == name
                && symbol.id.kind == kind
                && symbol.id.owner_path == target_owner
                && arity.is_none_or(|count| symbol.id.parameters.len() == count)
        })
        .collect();
    (matches.len() == 1).then(|| matches[0].id.clone())
}

enum Receiver {
    Type(String),
    This,
    Unknown,
}

fn receiver_name(node: &JavaSyntaxNode) -> Receiver {
    if node.kind == "this" {
        return Receiver::This;
    }
    if let Some(name) =
        last_identifier(node).filter(|name| name.chars().next().is_some_and(char::is_uppercase))
    {
        Receiver::Type(name)
    } else {
        Receiver::Unknown
    }
}

fn argument_count(node: &JavaSyntaxNode) -> usize {
    let Some(args) = direct_field(node, &["arguments"]) else {
        return 0;
    };
    args.children
        .iter()
        .filter(|child| matches!(child, crate::java_ir::JavaElement::Expression(_)))
        .count()
}

fn find_field<'a>(node: &'a JavaSyntaxNode, names: &[&str]) -> Option<&'a JavaSyntaxNode> {
    for child in &node.children {
        let nested = match child {
            crate::java_ir::JavaElement::Declaration(n)
            | crate::java_ir::JavaElement::Type(n)
            | crate::java_ir::JavaElement::Expression(n)
            | crate::java_ir::JavaElement::Statement(n)
            | crate::java_ir::JavaElement::Annotation(n)
            | crate::java_ir::JavaElement::Import(n)
            | crate::java_ir::JavaElement::Other(n) => Some(n),
            crate::java_ir::JavaElement::Token(_) => None,
        };
        if let Some(n) = nested {
            if n.field_name
                .as_deref()
                .is_some_and(|field| names.contains(&field))
            {
                return Some(n);
            }
            if let Some(found) = find_field(n, names) {
                return Some(found);
            }
        }
    }
    None
}

fn direct_field<'a>(node: &'a JavaSyntaxNode, names: &[&str]) -> Option<&'a JavaSyntaxNode> {
    node.children.iter().filter_map(element_node).find(|child| {
        child
            .field_name
            .as_deref()
            .is_some_and(|field| names.contains(&field))
    })
}

fn last_identifier(node: &JavaSyntaxNode) -> Option<String> {
    last_reference_identifier(node, false)
}

fn last_reference_identifier(node: &JavaSyntaxNode, skip_arguments: bool) -> Option<String> {
    fn collect(node: &JavaSyntaxNode, found: &mut Option<String>, skip_arguments: bool) {
        for child in &node.children {
            match child {
                crate::java_ir::JavaElement::Token(token)
                    if token.class == crate::java_ir::JavaTokenClass::Identifier =>
                {
                    *found = Some(token.text.clone());
                }
                crate::java_ir::JavaElement::Declaration(n)
                | crate::java_ir::JavaElement::Type(n)
                | crate::java_ir::JavaElement::Expression(n)
                | crate::java_ir::JavaElement::Statement(n)
                | crate::java_ir::JavaElement::Annotation(n)
                | crate::java_ir::JavaElement::Import(n)
                | crate::java_ir::JavaElement::Other(n) => {
                    if !(skip_arguments && n.kind == "argument_list") {
                        collect(n, found, skip_arguments);
                    }
                }
                crate::java_ir::JavaElement::Token(_) => {}
            }
        }
    }
    let mut found = None;
    collect(node, &mut found, skip_arguments);
    found
}
