//! Repair Kotlin references to top-level properties whose plain backing fields
//! are emitted on a generated Java file facade.
//!
//! The pass is intentionally narrower than Kotlin property resolution: only
//! unique, top-level properties with an ordinary field ABI are rewritten. Any
//! reference that cannot be represented as a static Java field pins that
//! property to Kotlin during planning.

use crate::function_callsite::CallsiteRepairDiagnostic;
use crate::semantics::{SemanticProvider, SourceLocation, SymbolId};
use crate::translation_plan::{BackendOwner, PlannedRepair, PlannedSourceEdit, TranslationPlan};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use tree_sitter::Node;

pub type RetentionMap = BTreeMap<SymbolId, Vec<CallsiteRepairDiagnostic>>;

#[derive(Debug, Default)]
pub struct PropertyCallsiteReport {
    pub edits: Vec<PlannedSourceEdit>,
    pub repairs: Vec<PlannedRepair>,
    pub diagnostics: Vec<CallsiteRepairDiagnostic>,
    pub applied_files: Vec<PathBuf>,
}

#[derive(Default)]
struct TargetIndex<'a> {
    by_package_name: HashMap<(String, String), Vec<&'a Target>>,
    by_name: HashMap<String, Vec<&'a Target>>,
}

impl<'a> TargetIndex<'a> {
    fn new(targets: &'a [Target]) -> Self {
        let mut index = Self::default();
        for target in targets {
            index
                .by_package_name
                .entry((target.id.package.clone(), target.id.name.clone()))
                .or_default()
                .push(target);
            index
                .by_name
                .entry(target.id.name.clone())
                .or_default()
                .push(target);
        }
        index
    }

    fn qualified(&self, package: &str, name: &str) -> Vec<&'a Target> {
        self.by_package_name
            .get(&(package.to_owned(), name.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    fn visible(&self, name: &str, package: &str, imports: &[Import]) -> Vec<&'a Target> {
        let exact = imports
            .iter()
            .filter(|i| !i.wildcard && i.visible == name)
            .collect::<Vec<_>>();
        if !exact.is_empty() {
            return exact
                .iter()
                .flat_map(|i| self.qualified(&i.package, &i.name))
                .collect();
        }
        let wildcard = imports
            .iter()
            .filter(|i| i.wildcard)
            .map(|i| i.package.as_str())
            .collect::<Vec<_>>();
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter(|t| t.id.package == package || wildcard.contains(&t.id.package.as_str()))
            .copied()
            .collect()
    }
}

#[derive(Clone)]
struct Target {
    id: SymbolId,
    facade: String,
    kotlin_facade: String,
}

#[derive(Clone)]
struct Import {
    start: usize,
    end: usize,
    package: String,
    name: String,
    visible: String,
    wildcard: bool,
}

struct PendingEdit {
    edit: PlannedSourceEdit,
    target: SymbolId,
}

/// Properties for which an observed Kotlin use cannot be safely changed to a
/// static field access. The map is computed once from the immutable workspace
/// snapshot and consumed by the ownership fixpoint.
pub fn properties_requiring_kotlin_retention(
    sources: &BTreeMap<PathBuf, String>,
    candidates: &[SymbolId],
    provider: &dyn SemanticProvider,
) -> RetentionMap {
    properties_requiring_kotlin_retention_in_selection(
        sources,
        candidates,
        provider,
        &sources.keys().cloned().collect(),
    )
}

pub(crate) fn properties_requiring_kotlin_retention_in_selection(
    sources: &BTreeMap<PathBuf, String>,
    candidates: &[SymbolId],
    provider: &dyn SemanticProvider,
    editable: &HashSet<PathBuf>,
) -> RetentionMap {
    let candidates = candidates
        .iter()
        .filter(|id| editable.contains(&id.file))
        .cloned()
        .collect::<Vec<_>>();
    let targets = targets_for(sources, &candidates, &[]);
    let mut result = RetentionMap::new();
    if targets.is_empty() {
        return result;
    }
    let mut safe_targets = HashMap::new();
    let mut by_file = BTreeMap::<PathBuf, Vec<&Target>>::new();
    for target in &targets {
        by_file
            .entry(target.id.file.clone())
            .or_default()
            .push(target);
    }
    for (file, file_targets) in by_file {
        let Some(source) = provider.source_for_file(&file) else {
            continue;
        };
        safe_targets.extend(property_safety_map(source, &file, &file_targets));
    }
    let target_index = TargetIndex::new(&targets);

    for (path, source) in sources {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
        {
            for target in &targets {
                // This pass repairs Kotlin field references only. A Java use
                // may be an accessor, a const field, or a static import;
                // conservatively preserve the original facade in all cases.
                if source.contains(&target.kotlin_facade) {
                    result.entry(target.id.clone()).or_default().push(diag(
                        path,
                        0,
                        "P006",
                        format!(
                            "Java source references the original file-facade accessor for `{}`",
                            target.id.name
                        ),
                    ));
                }
            }
            continue;
        }
        let Some(tree) = parse(source) else { continue };
        let package = package(source, tree.root_node());
        let imports = imports(source, tree.root_node());
        if !editable.contains(path) {
            for import in imports.iter().filter(|import| !import.wildcard) {
                let candidates = target_index.qualified(&import.package, &import.name);
                pin(
                    &mut result,
                    &candidates,
                    path,
                    import.start,
                    "P008",
                    &import.name,
                );
            }
        }
        for target in targets.iter().filter(|target| target.id.file == *path) {
            if !safe_targets.get(&target.id).copied().unwrap_or(false) {
                result.entry(target.id.clone()).or_default().push(diag(
                    path,
                    0,
                    "P001",
                    format!(
                        "top-level property `{}` requires a plain field ABI and an unambiguous generated file facade",
                        target.id.name
                    ),
                ));
            }
        }

        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if matches!(node.kind(), "string_literal" | "multiline_string_literal") {
                for name in short_template_names(node.utf8_text(source.as_bytes()).unwrap_or("")) {
                    let candidates = target_index.visible(&name, &package, &imports);
                    pin(
                        &mut result,
                        &candidates,
                        path,
                        node.start_byte(),
                        "P003",
                        &name,
                    );
                }
            }
            if is_identifier(node) && !is_declaration_name(node, source) && !in_import(node) {
                let spelling = node.utf8_text(source.as_bytes()).unwrap_or("");
                let (qualified, _) = qualified_property_reference(node, source);
                let candidates = if let Some((qualifier, _)) = &qualified {
                    target_index.qualified(qualifier, spelling)
                } else {
                    target_index.visible(spelling, &package, &imports)
                };
                if !candidates.is_empty() {
                    let mut unsafe_use = false;
                    if let Some((qualifier, whole)) = qualified {
                        if qualifier == candidates[0].id.package {
                            unsafe_use = candidates.len() != 1
                                || !editable.contains(path)
                                || local_shadow(
                                    node,
                                    candidates[0].id.package.split('.').next().unwrap_or(""),
                                    source,
                                )
                                || inside_class_or_object(node)
                                || implicit_receiver_scope(node, source)
                                || is_invoked_as_callable(whole)
                                || is_callable_reference(whole)
                                || imports.iter().any(|import| {
                                    import.wildcard
                                        || import.visible
                                            == qualifier.split('.').next().unwrap_or("")
                                })
                                || provider
                                    .symbols_named(qualifier.split('.').next().unwrap_or(""))
                                    .iter()
                                    .any(|symbol| {
                                        symbol.id.owner_path.is_empty()
                                            && symbol.id.package == package
                                    })
                                || !safe_targets
                                    .get(&candidates[0].id)
                                    .copied()
                                    .unwrap_or(false);
                            if unsafe_use {
                                pin(
                                    &mut result,
                                    &candidates,
                                    path,
                                    node.start_byte(),
                                    "P002",
                                    spelling,
                                );
                            }
                        }
                    } else if is_bare_reference(node, source) {
                        if candidates.len() != 1
                            || !editable.contains(path)
                            || local_shadow(node, spelling, source)
                            || inside_class_or_object(node)
                            || is_invoked_as_callable(node)
                            || is_callable_reference(node)
                            || is_short_string_template(node)
                            || implicit_receiver_scope(node, source)
                            || is_navigation_receiver(node)
                            || (candidates[0].id.package != package
                                && !imports
                                    .iter()
                                    .any(|import| !import.wildcard && import.visible == spelling))
                            || inside_annotation(node)
                            || !safe_targets
                                .get(&candidates[0].id)
                                .copied()
                                .unwrap_or(false)
                        {
                            unsafe_use = true;
                        }
                        if unsafe_use {
                            pin(
                                &mut result,
                                &candidates,
                                path,
                                node.start_byte(),
                                "P002",
                                spelling,
                            );
                        }
                    }
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }

        // A callback/property reference is not a field read. Pin the exact
        // candidate when syntax points at it, including `::name`.
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if matches!(
                node.kind(),
                "callable_reference" | "callable_reference_expression"
            ) {
                let text = node.utf8_text(source.as_bytes()).unwrap_or("");
                let name = text
                    .rsplit("::")
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('`');
                let refs = target_index.visible(name, &package, &imports);
                if !refs.is_empty() {
                    pin(&mut result, &refs, path, node.start_byte(), "P003", name);
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }
    for diagnostics in result.values_mut() {
        diagnostics.sort_by(|a, b| a.file.cmp(&b.file).then(a.start_byte.cmp(&b.start_byte)));
        diagnostics.dedup();
    }
    result
}

/// Rewrite references to properties that are present in accepted Java facade
/// output. Edits are anchored to the exact current residual snapshot.
pub fn repair_emitted_property_references(
    original: &BTreeMap<PathBuf, String>,
    plans: &[TranslationPlan],
    residual: &mut BTreeMap<PathBuf, String>,
    emitted_owners: &[SymbolId],
) -> PropertyCallsiteReport {
    let mut report = PropertyCallsiteReport::default();
    let mut candidate_ids = Vec::new();
    for plan in plans {
        for decision in &plan.declarations {
            let id = &decision.symbol_id;
            let snapshot_matches = original.get(&id.file).is_some_and(|source| {
                *blake3::hash(source.as_bytes()).as_bytes() == plan.source_hash
            });
            if decision.final_owner == Some(BackendOwner::Java)
                && id.kind == "property"
                && id.owner_path.is_empty()
                && emitted_owners.contains(id)
            {
                if !snapshot_matches {
                    report.diagnostics.push(diag(
                        &id.file,
                        0,
                        "P005",
                        "accepted property ownership belongs to a stale source snapshot".into(),
                    ));
                    continue;
                }
                candidate_ids.push(id.clone());
            }
        }
    }
    if !report.diagnostics.is_empty() {
        return report;
    }
    let targets = targets_for(original, &candidate_ids, emitted_owners);
    if targets.is_empty() {
        return report;
    }
    let target_index = TargetIndex::new(&targets);
    let mut edits_by_file = BTreeMap::<PathBuf, Vec<PendingEdit>>::new();

    for (path, source) in residual.iter() {
        let Some(tree) = parse(source) else { continue };
        let package = package(source, tree.root_node());
        let imports = imports(source, tree.root_node());
        let hash = *blake3::hash(source.as_bytes()).as_bytes();
        let mut edits = Vec::new();

        // Imports are redirected to Java static fields; aliases remain intact.
        for import in imports.iter().filter(|i| !i.wildcard) {
            let matches = targets
                .iter()
                .filter(|t| t.id.package == import.package && t.id.name == import.name)
                .collect::<Vec<_>>();
            if let [target] = matches.as_slice() {
                let fq = format!("{}.{}", target.facade, target.id.name);
                let replacement = if import.visible != import.name {
                    format!("import {fq} as {}", import.visible)
                } else {
                    format!("import {fq}")
                };
                edits.push(PendingEdit {
                    edit: edit(path, hash, import.start, import.end, replacement),
                    target: target.id.clone(),
                });
            }
        }

        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if is_identifier(node) && !is_declaration_name(node, source) && !in_import(node) {
                let spelling = node.utf8_text(source.as_bytes()).unwrap_or("");
                let qualified = qualified_property_reference(node, source).0;
                let candidates = if let Some((qualifier, _)) = &qualified {
                    target_index.qualified(qualifier, spelling)
                } else {
                    target_index.visible(spelling, &package, &imports)
                };
                if !candidates.is_empty() {
                    if let Some((qualifier, whole)) = qualified {
                        if qualifier == candidates[0].id.package
                            && candidates.len() == 1
                            && !local_shadow(
                                node,
                                qualifier.split('.').next().unwrap_or(""),
                                source,
                            )
                            && !inside_class_or_object(node)
                            && !implicit_receiver_scope(node, source)
                            && !is_invoked_as_callable(whole)
                            && !is_callable_reference(whole)
                        {
                            let target = candidates[0];
                            edits.push(PendingEdit {
                                edit: edit(
                                    path,
                                    hash,
                                    whole.start_byte(),
                                    whole.end_byte(),
                                    format!("{}.{}", target.facade, target.id.name),
                                ),
                                target: target.id.clone(),
                            });
                        }
                    } else if is_bare_reference(node, source)
                        && candidates.len() == 1
                        && !local_shadow(node, spelling, source)
                        && !inside_class_or_object(node)
                        && !is_invoked_as_callable(node)
                        && !is_callable_reference(node)
                        && !is_short_string_template(node)
                        && !implicit_receiver_scope(node, source)
                        && !is_navigation_receiver(node)
                        && (candidates[0].id.package == package
                            || imports
                                .iter()
                                .any(|import| !import.wildcard && import.visible == spelling))
                        && !inside_annotation(node)
                    {
                        // Imported aliases are preserved by the rewritten
                        // import and must keep their lexical spelling.
                        if imports.iter().any(|i| {
                            !i.wildcard
                                && i.visible == spelling
                                && i.name == candidates[0].id.name
                                && i.package == candidates[0].id.package
                        }) {
                            // Its import edit is sufficient.
                        } else {
                            let target = candidates[0];
                            edits.push(PendingEdit {
                                edit: edit(
                                    path,
                                    hash,
                                    node.start_byte(),
                                    node.end_byte(),
                                    format!("{}.{}", target.facade, target.id.name),
                                ),
                                target: target.id.clone(),
                            });
                        }
                    }
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        if !edits.is_empty() {
            edits_by_file.insert(path.clone(), edits);
        }
    }

    for (path, mut pending) in edits_by_file {
        pending.sort_by_key(|e| (e.edit.location.start_byte, e.edit.location.end_byte));
        let mut overlap = false;
        for pair in pending.windows(2) {
            if pair[0].edit.location.end_byte > pair[1].edit.location.start_byte {
                overlap = true;
                break;
            }
        }
        if overlap {
            report.diagnostics.push(diag(
                &path,
                0,
                "P004",
                "overlapping property call-site edits were rejected".into(),
            ));
            continue;
        }
        let Some(source) = residual.get_mut(&path) else {
            continue;
        };
        let expected = *blake3::hash(source.as_bytes()).as_bytes();
        if pending
            .iter()
            .any(|e| e.edit.location.snapshot_hash != expected)
        {
            report.diagnostics.push(diag(
                &path,
                0,
                "P005",
                "source snapshot changed before property call-site edits could be applied".into(),
            ));
            continue;
        }
        if pending.iter().any(|e| {
            e.edit.location.end_byte > source.len()
                || !source.is_char_boundary(e.edit.location.start_byte)
                || !source.is_char_boundary(e.edit.location.end_byte)
        }) {
            report.diagnostics.push(diag(
                &path,
                0,
                "P007",
                "property call-site edit no longer matches a UTF-8 source boundary".into(),
            ));
            continue;
        }
        for e in pending.iter().rev() {
            let e = &e.edit;
            source.replace_range(e.location.start_byte..e.location.end_byte, &e.replacement);
        }
        report.applied_files.push(path.clone());
        for pending in pending {
            report.repairs.push(PlannedRepair {
                target: pending.target.clone(),
                kind: "top-level-property-callsite".into(),
                detail: format!(
                    "rewrote a Kotlin reference to `{}` as a Java facade field",
                    pending.target.name
                ),
            });
            report.edits.push(pending.edit);
        }
    }
    report.applied_files.sort();
    report
}

fn targets_for(
    sources: &BTreeMap<PathBuf, String>,
    candidates: &[SymbolId],
    emitted: &[SymbolId],
) -> Vec<Target> {
    let mut facades = HashMap::<PathBuf, (String, String)>::new();
    let mut targets = candidates
        .iter()
        .filter(|id| {
            id.kind == "property"
                && id.owner_path.is_empty()
                && (emitted.is_empty() || emitted.contains(id))
        })
        .filter_map(|id| {
            let source = sources.get(&id.file)?;
            let (facade, kotlin_facade) = facades.entry(id.file.clone()).or_insert_with(|| {
                (
                    facade_fq_name(&id.file, source, &id.package),
                    original_facade(&id.file, source),
                )
            });
            Some(Target {
                id: id.clone(),
                facade: facade.clone(),
                kotlin_facade: kotlin_facade.clone(),
            })
        })
        .collect::<Vec<_>>();
    targets.sort_by(|a, b| a.id.cmp(&b.id));
    targets.dedup_by(|a, b| a.id == b.id);
    targets
}

fn property_safety_map(source: &str, file: &Path, targets: &[&Target]) -> HashMap<SymbolId, bool> {
    let Some(tree) = parse(source) else {
        return HashMap::new();
    };
    let mut properties = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "property_declaration"
            && node.parent().is_some_and(|p| p.kind() == "source_file")
        {
            properties.push((
                crate::semantics::symbol_id_for_node(source, node, file),
                node,
            ));
        }
        let mut c = node.walk();
        stack.extend(node.named_children(&mut c));
    }
    let mut cursor = tree.root_node().walk();
    // File annotations can rename or combine JVM facades. A generated name
    // that equals the Kotlin facade also collides with a retained file part.
    let facade_safe = !tree.root_node().has_error()
        && !tree
            .root_node()
            .named_children(&mut cursor)
            .any(|node| node.kind() == "file_annotation")
        && targets
            .iter()
            .all(|target| target.facade.rsplit('.').next() != Some(target.kotlin_facade.as_str()));
    targets
        .iter()
        .map(|target| {
            let matching = properties
                .iter()
                .filter(|(id, _)| id == &target.id)
                .collect::<Vec<_>>();
            let safe =
                facade_safe && matching.len() == 1 && plain_property_shape(source, matching[0].1);
            (target.id.clone(), safe)
        })
        .collect()
}

pub(crate) fn has_effectful_top_level_initializer(root: Node, source: &str) -> bool {
    root.named_children(&mut root.walk())
        .filter(|node| node.kind() == "property_declaration")
        .filter_map(property_initializer)
        .any(|expression| {
            let text = expression.utf8_text(source.as_bytes()).unwrap_or("");
            !matches!(
                expression.kind(),
                "number_literal" | "float_literal" | "character_literal"
            ) && !(expression.kind() == "string_literal" && !text.contains('$'))
                && !(expression.kind() == "identifier" && matches!(text, "true" | "false" | "null"))
        })
}

fn property_initializer(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let mut after_equals = false;
    for child in node.children(&mut cursor) {
        if after_equals && child.is_named() {
            return Some(child);
        }
        after_equals |= child.kind() == "=";
    }
    None
}

fn plain_property_shape(source: &str, node: Node) -> bool {
    let modifiers = crate::transpiler::kt::child(node, "modifiers")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .unwrap_or("");
    if crate::transpiler::kt::child(node, "getter").is_some()
        || crate::transpiler::kt::child(node, "setter").is_some()
        || crate::transpiler::kt::child(node, "property_delegate").is_some()
        || modifiers.contains("lateinit")
        || modifiers.contains("private")
        || modifiers.contains("internal")
        || modifiers.contains('@')
    {
        return false;
    }
    let id = crate::semantics::symbol_id_for_node(source, node, Path::new(""));
    id.name
        .chars()
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && id
            .name
            .chars()
            .all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}
fn pin(
    result: &mut RetentionMap,
    targets: &[&Target],
    file: &Path,
    at: usize,
    code: &str,
    name: &str,
) {
    for target in targets {
        result.entry(target.id.clone()).or_default().push(diag(
            file,
            at,
            code,
            format!("reference to `{name}` cannot be represented as a Java facade field"),
        ));
    }
}
fn diag(file: &Path, at: usize, code: &str, message: String) -> CallsiteRepairDiagnostic {
    CallsiteRepairDiagnostic {
        file: file.to_path_buf(),
        start_byte: at,
        code: code.into(),
        message,
    }
}
fn edit(
    file: &Path,
    hash: [u8; 32],
    start: usize,
    end: usize,
    replacement: String,
) -> PlannedSourceEdit {
    PlannedSourceEdit {
        location: SourceLocation {
            file: file.to_path_buf(),
            snapshot_hash: hash,
            start_byte: start,
            end_byte: end,
        },
        replacement,
        speculative: true,
    }
}
fn parse(source: &str) -> Option<tree_sitter::Tree> {
    let mut p = tree_sitter::Parser::new();
    p.set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .ok()?;
    p.parse(source, None)
}
fn package(source: &str, root: Node) -> String {
    root.named_children(&mut root.walk())
        .find(|node| node.kind() == "package_header")
        .and_then(|header| {
            header
                .named_children(&mut header.walk())
                .find(|node| node.kind() == "qualified_identifier")
        })
        .and_then(|name| name.utf8_text(source.as_bytes()).ok())
        .map(|name| name.trim().to_owned())
        .unwrap_or_default()
}

fn short_template_names(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '$' {
            continue;
        }
        let mut name = String::new();
        while chars
            .peek()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        {
            name.push(chars.next().unwrap());
        }
        if !name.is_empty() {
            names.push(name);
        }
    }
    names
}
fn imports(source: &str, root: Node) -> Vec<Import> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "import" {
            let s = n
                .utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim()
                .strip_prefix("import ")
                .unwrap_or("")
                .trim();
            let (path, alias) = s
                .split_once(" as ")
                .map(|(p, a)| (p.trim(), Some(a.trim())))
                .unwrap_or((s, None));
            let wildcard = path.ends_with(".*");
            let path = path.strip_suffix(".*").unwrap_or(path);
            if wildcard {
                out.push(Import {
                    start: n.start_byte(),
                    end: n.end_byte(),
                    package: path.into(),
                    name: "*".into(),
                    visible: "*".into(),
                    wildcard,
                });
            } else if let Some((pkg, name)) = path.rsplit_once('.') {
                out.push(Import {
                    start: n.start_byte(),
                    end: n.end_byte(),
                    package: pkg.into(),
                    name: name.into(),
                    visible: alias.unwrap_or(name).into(),
                    wildcard,
                });
            }
        }
        let mut c = n.walk();
        stack.extend(n.named_children(&mut c));
    }
    out
}
fn file_jvm_name(source: &str) -> Option<String> {
    let tree = parse(source)?;
    let mut cursor = tree.root_node().walk();
    tree.root_node()
        .named_children(&mut cursor)
        .filter(|n| n.kind() == "file_annotation")
        .find_map(|node| {
            node.utf8_text(source.as_bytes())
                .ok()?
                .trim()
                .strip_prefix("@file:JvmName(\"")?
                .split_once("\")")
                .map(|(name, _)| name.to_owned())
                .filter(|name| !name.is_empty())
        })
}
fn facade_fq_name(file: &Path, source: &str, package: &str) -> String {
    let mut name = file_jvm_name(source).unwrap_or_else(|| {
        file.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Main")
            .to_owned()
    });
    name = name
        .chars()
        .map(|c| if c == '-' || c == '.' { '_' } else { c })
        .collect();
    let mut chars = name.chars();
    if let Some(first) = chars.next() {
        name = first.to_uppercase().collect::<String>() + chars.as_str();
    }
    name = crate::function_callsite::non_conflicting_facade_name(source, &name);
    if package.is_empty() {
        name
    } else {
        format!("{package}.{name}")
    }
}
fn original_facade(file: &Path, source: &str) -> String {
    file_jvm_name(source).unwrap_or_else(|| {
        let raw = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("File")
            .replace(['-', '.'], "_");
        let mut chars = raw.chars();
        let prefix = chars
            .next()
            .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
            .unwrap_or_else(|| "File".into());
        format!("{prefix}Kt")
    })
}
fn is_identifier(n: Node) -> bool {
    matches!(n.kind(), "simple_identifier" | "identifier")
}
fn in_import(n: Node) -> bool {
    let mut p = n.parent();
    while let Some(x) = p {
        if x.kind() == "import" {
            return true;
        }
        if !matches!(x.kind(), "navigation_expression" | "navigation_suffix") {
            break;
        }
        p = x.parent()
    }
    false
}
fn is_declaration_name(n: Node, source: &str) -> bool {
    if n.parent().is_some_and(|p| {
        matches!(
            p.kind(),
            "variable_declaration" | "multi_variable_declaration"
        )
    }) {
        return true;
    }
    let mut p = n.parent();
    while let Some(x) = p {
        if matches!(
            x.kind(),
            "property_declaration"
                | "function_declaration"
                | "class_declaration"
                | "object_declaration"
                | "parameter"
                | "class_parameter"
                | "type_alias"
        ) && x
            .child_by_field_name("name")
            .is_some_and(|name| name.start_byte() == n.start_byte())
        {
            return true;
        }
        if !matches!(
            x.kind(),
            "variable_declaration" | "multi_variable_declaration"
        ) {
            break;
        }
        p = x.parent()
    }
    let _ = source;
    false
}
fn is_bare_reference(n: Node, source: &str) -> bool {
    let mut p = n.parent();
    while let Some(node) = p {
        if node.kind() == "navigation_suffix" {
            return false;
        }
        if node.kind() == "navigation_expression" {
            return node
                .named_child(0)
                .is_some_and(|first| first.id() == n.id());
        }
        if matches!(
            node.kind(),
            "user_type" | "type_identifier" | "annotation" | "annotation_entry"
        ) {
            return false;
        }
        if matches!(
            node.kind(),
            "source_file" | "function_declaration" | "property_declaration"
        ) {
            break;
        }
        p = node.parent();
    }
    let _ = source;
    true
}
fn is_navigation_receiver(node: Node) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "navigation_expression"
            && parent
                .named_child(0)
                .is_some_and(|first| first.id() == node.id())
    })
}
fn is_invoked_as_callable(n: Node) -> bool {
    n.parent().is_some_and(|p| {
        p.kind() == "call_expression"
            && p.named_children(&mut p.walk())
                .next()
                .is_some_and(|c| c.start_byte() == n.start_byte())
    })
}
fn is_callable_reference(n: Node) -> bool {
    let mut p = n.parent();
    while let Some(x) = p {
        if matches!(
            x.kind(),
            "callable_reference" | "callable_reference_expression"
        ) {
            return true;
        }
        if !matches!(x.kind(), "navigation_expression" | "navigation_suffix") {
            break;
        }
        p = x.parent()
    }
    false
}
fn inside_class_or_object(n: Node) -> bool {
    let mut p = n.parent();
    while let Some(x) = p {
        if matches!(
            x.kind(),
            "class_declaration" | "object_declaration" | "class_body"
        ) {
            return true;
        }
        if x.kind() == "source_file" {
            break;
        }
        p = x.parent()
    }
    false
}
fn is_short_string_template(n: Node) -> bool {
    let mut p = n.parent();
    while let Some(x) = p {
        if x.kind() == "simple_name_string_template_entry" {
            return true;
        }
        if matches!(x.kind(), "string_literal" | "source_file") {
            break;
        }
        p = x.parent()
    }
    false
}
fn inside_annotation(n: Node) -> bool {
    let mut p = n.parent();
    while let Some(x) = p {
        if matches!(x.kind(), "annotation" | "annotation_entry") {
            return true;
        }
        if matches!(
            x.kind(),
            "source_file" | "function_declaration" | "property_declaration"
        ) {
            break;
        }
        p = x.parent()
    }
    false
}
fn implicit_receiver_scope(n: Node, source: &str) -> bool {
    let mut p = n.parent();
    while let Some(node) = p {
        if node.kind() == "function_declaration" {
            let name = node
                .child_by_field_name("name")
                .map(|name| name.start_byte())
                .unwrap_or(node.end_byte());
            let prefix = source.get(node.start_byte()..name).unwrap_or("");
            if prefix
                .rsplit("fun")
                .next()
                .is_some_and(|signature| signature.contains('.'))
            {
                return true;
            }
        }
        // An arbitrary user function may accept a receiver lambda. Its name
        // alone cannot establish whether a bare identifier binds to `this`.
        if node.kind() == "lambda_literal" {
            return true;
        }
        if node.kind() == "source_file" {
            break;
        }
        p = node.parent();
    }
    false
}
fn qualified_property_reference<'a>(
    n: Node<'a>,
    source: &str,
) -> (Option<(String, Node<'a>)>, Option<Node<'a>>) {
    let last = n
        .utf8_text(source.as_bytes())
        .unwrap_or("")
        .trim_matches('`');
    let mut expression = None;
    let mut parent = n.parent();
    while let Some(node) = parent {
        if !matches!(node.kind(), "navigation_expression" | "navigation_suffix") {
            break;
        }
        let text = node
            .utf8_text(source.as_bytes())
            .unwrap_or("")
            .replace('`', "");
        if text.ends_with(&format!(".{last}")) {
            expression = Some(node);
        }
        parent = node.parent();
    }
    let Some(whole) = expression else {
        return (None, None);
    };
    let text = whole
        .utf8_text(source.as_bytes())
        .unwrap_or("")
        .replace('`', "");
    let Some(prefix) = text.strip_suffix(&format!(".{last}")) else {
        return (None, Some(whole));
    };
    (Some((prefix.to_owned(), whole)), Some(whole))
}
fn local_shadow(node: Node, name: &str, source: &str) -> bool {
    crate::function_callsite::local_shadow(node, name, source)
}
