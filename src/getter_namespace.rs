//! Preserve independent Kotlin property and function getter descriptors.
//!
//! Run before property reads become calls: an original `getX()` call and a
//! generated call replacing `.x` must never be confused during this repair.

use crate::property_abi::PlannedAccessorRepairs;
use crate::semantics::{self, FactStatus, OriginMap, SymbolId, SyntaxSemanticProvider};
use crate::smart_cast::Edit;
use crate::translation_plan::PlannedRepair;
use crate::workspace::{Declaration, DeclarationKind, MemberKind, SourceIndex, SourceLanguage};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use tree_sitter::Node;

#[derive(Default)]
pub(crate) struct NameSplits {
    edits: HashMap<PathBuf, Vec<Edit>>,
    source_hashes: HashMap<PathBuf, [u8; 32]>,
    pub report: PlannedAccessorRepairs,
}

impl NameSplits {
    pub fn proposal(
        &self,
        index: &SourceIndex,
        path: &std::path::Path,
        source: &str,
        coverage: &crate::diagnostics::FileCoverage,
    ) -> Result<crate::migrate::MigrationProposal, String> {
        let path = index
            .source_file(path)
            .map(|file| file.path.as_path())
            .unwrap_or(path);
        let Some(edits) = self.edits.get(path) else {
            return Ok(crate::migrate::propose_speculative_migration(
                source, coverage,
            ));
        };
        if self.source_hashes.get(path) != Some(blake3::hash(source.as_bytes()).as_bytes()) {
            return Err("getter namespace source changed after planning".into());
        }
        let shift = |position: usize| -> usize {
            let delta = edits
                .iter()
                .filter(|edit| edit.end <= position)
                .map(|edit| edit.text.len() as isize - (edit.end - edit.start) as isize)
                .sum::<isize>();
            position
                .checked_add_signed(delta)
                .expect("retained edits cannot move a span before the source")
        };
        let mut adjusted = crate::diagnostics::FileCoverage {
            translated: coverage.translated.clone(),
            untranslated: coverage.untranslated.clone(),
            ..Default::default()
        };
        for &(start, end) in &coverage.translated_spans {
            if edits.iter().any(|edit| {
                edit.start < end && edit.end > start
                    || edit.start == edit.end && start < edit.start && edit.start < end
            }) {
                return Err("getter namespace edit overlaps a translated declaration".into());
            }
            adjusted.translated_spans.push((shift(start), shift(end)));
        }
        let changed = crate::smart_cast::apply_edits(
            source,
            edits
                .iter()
                .map(|edit| Edit {
                    start: edit.start,
                    end: edit.end,
                    text: edit.text.clone(),
                })
                .collect(),
        );
        match crate::migrate::propose_speculative_migration(&changed, &adjusted) {
            crate::migrate::MigrationProposal::Untouched => {
                Ok(crate::migrate::MigrationProposal::Rewrite(changed))
            }
            crate::migrate::MigrationProposal::Delete => {
                Err("getter namespace edit belongs to a deleted Kotlin owner".into())
            }
            proposal => Ok(proposal),
        }
    }
}

fn retained_context(
    index: &SourceIndex,
    mut node: Node<'_>,
    source: &str,
    path: &std::path::Path,
    retained: &HashSet<SymbolId>,
) -> bool {
    while let Some(parent) = node.parent().filter(|parent| parent.parent().is_some()) {
        node = parent;
    }
    index.symbol_retained(&semantics::symbol_id_for_node(source, node, path), retained)
}

fn getter_names(
    index: &SourceIndex,
    owner: &Declaration,
    properties_only: bool,
) -> BTreeSet<String> {
    let mut pending = vec![owner];
    let mut seen = HashSet::new();
    let mut names = BTreeSet::new();
    while let Some(owner) = pending.pop() {
        let Some(file) = index.declaration_source_file(owner) else {
            continue;
        };
        if !seen.insert((file.path.clone(), owner.name.clone())) {
            continue;
        }
        for member in &owner.members {
            if member.is_static || member.visibility.as_deref() == Some("private") {
                continue;
            }
            if owner.language == SourceLanguage::Kotlin
                && member.kind == MemberKind::Property
                && !member.is_jvm_field
                && !member.setter_name_renamed
            {
                names.insert(crate::workspace::property_accessor_name(&member.name));
            } else if !properties_only
                && member.kind == MemberKind::Method
                && member.parameter_types.is_empty()
            {
                names.insert(member.name.clone());
            }
        }
        for parent in &owner.supertypes {
            if let Some(parent) =
                crate::workspace::property_receiver_type_binding(index, file, parent)
                    .and_then(|binding| binding.declaration)
            {
                pending.push(parent);
            }
        }
    }
    names
}

/// javac adds a covariant bridge even to an otherwise empty subclass of a
/// Kotlin class with independent wide/narrow getter descriptors. That bridge
/// redirects wide property reads to the narrow method, potentially recursing.
/// Keep descendants in Kotlin until both physical names and Java callers can
/// be migrated together. Reuse cached syntax and scoped ancestry; no file I/O.
pub(crate) fn inherits_independent_getters(
    index: &SourceIndex,
    path: &std::path::Path,
    name: &str,
) -> bool {
    let Some(file) = index.source_file(path) else {
        return false;
    };
    let owners = file
        .declarations
        .iter()
        .filter(|owner| owner.name == name)
        .collect::<Vec<_>>();
    let [owner] = owners.as_slice() else {
        return false;
    };
    let mut pending = owner
        .supertypes
        .iter()
        .filter_map(|parent| {
            crate::workspace::property_receiver_type_binding(index, file, parent)
                .and_then(|binding| binding.declaration)
        })
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    while let Some(parent) = pending.pop() {
        let Some(file) = index.declaration_source_file(parent) else {
            continue;
        };
        if !seen.insert((file.path.clone(), parent.name.clone())) {
            continue;
        }
        pending.extend(parent.supertypes.iter().filter_map(|ancestor| {
            crate::workspace::property_receiver_type_binding(index, file, ancestor)
                .and_then(|binding| binding.declaration)
        }));
        if parent.language != SourceLanguage::Kotlin || parent.kind != DeclarationKind::Class {
            continue;
        }
        let Some(tree) = index.source_tree(file) else {
            continue;
        };
        let source = file.source_text();
        let properties = getter_names(index, parent, true);
        let mut nodes = vec![tree.root_node()];
        while let Some(node) = nodes.pop() {
            if node.kind() != "class_declaration"
                || node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                    != Some(parent.name.as_str())
            {
                nodes.extend(node.named_children(&mut node.walk()));
                continue;
            }
            let Some(body) = crate::transpiler::kt::child(node, "class_body") else {
                continue;
            };
            for function in body
                .named_children(&mut body.walk())
                .filter(|node| node.kind() == "function_declaration")
            {
                let id = semantics::symbol_id_for_node(source, function, &file.path);
                if !id.parameters.is_empty() || id.receiver.is_some() {
                    continue;
                }
                if has_physical_getter_collision(index, &file.path, function, source) {
                    return true;
                }
                if properties.contains(&id.name)
                    && !crate::transpiler::kt::child(function, "modifiers").is_some_and(
                        |modifiers| {
                            modifiers
                                .named_children(&mut modifiers.walk())
                                .any(|modifier| {
                                    matches!(
                                        modifier.utf8_text(source.as_bytes()).unwrap_or("").trim(),
                                        "override" | "private"
                                    )
                                })
                        },
                    )
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Generated name splits remain Kotlin while two independent JVM descriptors
/// still share a Java method name. Emitting a covariant Java override would
/// synthesize a bridge that changes calls to the original wider getter.
pub(crate) fn has_physical_getter_collision(
    index: &SourceIndex,
    file: &std::path::Path,
    function: Node<'_>,
    source: &str,
) -> bool {
    let signature = semantics::symbol_id_for_node(source, function, file);
    if !signature.parameters.is_empty() || signature.receiver.is_some() {
        return false;
    }
    let Some(modifiers) = crate::transpiler::kt::child(function, "modifiers") else {
        return false;
    };
    let physical = modifiers
        .named_children(&mut modifiers.walk())
        .find_map(|node| {
            let text = node.utf8_text(source.as_bytes()).ok()?.trim();
            text.strip_prefix("@kotlin.jvm.JvmName(\"")?
                .strip_suffix("\")")
                .map(str::to_owned)
        });
    let Some(physical) = physical else {
        return false;
    };
    if function
        .child_by_field_name("name")
        .and_then(|node| node.utf8_text(source.as_bytes()).ok())
        == Some(physical.as_str())
    {
        return false;
    }
    let mut parent = function.parent();
    while let Some(node) = parent {
        if node.kind() == "class_declaration" {
            let name = node
                .child_by_field_name("name")
                .and_then(|node| node.utf8_text(source.as_bytes()).ok());
            return index
                .source_file(file)
                .and_then(|file| {
                    file.declarations
                        .iter()
                        .find(|owner| Some(owner.name.as_str()) == name)
                })
                .is_some_and(|owner| getter_names(index, owner, false).contains(&physical));
        }
        parent = node.parent();
    }
    false
}

struct Split {
    original: SymbolId,
    alias: String,
    declaration_start: usize,
    name_start: usize,
    name_end: usize,
    add_suppression: bool,
    suppression_edit: Option<Edit>,
}

/// Kotlin rejects @JvmName on plugin-opened methods even though the JVM supports
/// the independent descriptors. Preserve virtual dispatch and scope the opt-out
/// to that diagnostic; merge an existing annotation instead of repeating it.
fn name_suppression(
    index: &SourceIndex,
    file: &crate::workspace::SourceFile,
    function: Node<'_>,
    source: &str,
) -> Result<(bool, Option<Edit>), String> {
    let Some(modifiers) = crate::transpiler::kt::child(function, "modifiers") else {
        return Ok((true, None));
    };
    for annotation in modifiers.named_children(&mut modifiers.walk()) {
        match crate::transpiler::unit::kotlin_builtin_annotation_binding(
            annotation,
            "kotlin.Suppress",
            source,
            Some(index),
            Some(file),
        ) {
            Some(false) => continue,
            None => {
                return Err(
                    "getter namespace repair cannot bind the existing Suppress annotation".into(),
                );
            }
            Some(true) => {}
        }
        let mut pending = vec![annotation];
        let mut arguments = None;
        while let Some(node) = pending.pop() {
            if node.kind() == "string_literal"
                && node.utf8_text(source.as_bytes()).ok() == Some("\"INAPPLICABLE_JVM_NAME\"")
            {
                return Ok((false, None));
            }
            if node.kind() == "value_arguments" && arguments.is_none() {
                arguments = Some(node);
            }
            pending.extend(
                node.named_children(&mut node.walk())
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev(),
            );
        }
        let Some(arguments) = arguments else {
            return Ok((
                false,
                Some(Edit {
                    start: annotation.end_byte(),
                    end: annotation.end_byte(),
                    text: "(\"INAPPLICABLE_JVM_NAME\")".into(),
                }),
            ));
        };
        let values = arguments
            .named_children(&mut arguments.walk())
            .filter(|node| !node.is_extra())
            .collect::<Vec<_>>();
        let mut container = arguments;
        if let [value] = values.as_slice()
            && value
                .children(&mut value.walk())
                .any(|node| node.kind() == "=" || node.kind() == "*")
        {
            let mut pending = value.named_children(&mut value.walk()).collect::<Vec<_>>();
            container = loop {
                let Some(node) = pending.pop() else {
                    return Err(
                        "getter namespace repair cannot append to the existing suppression array"
                            .into(),
                    );
                };
                if matches!(node.kind(), "collection_literal" | "value_arguments") {
                    break node;
                }
                pending.extend(node.named_children(&mut node.walk()));
            };
        }
        let last = container
            .named_children(&mut container.walk())
            .filter(|node| !node.is_extra())
            .last();
        let position = last.map_or(container.start_byte() + 1, |node| node.end_byte());
        return Ok((
            false,
            Some(Edit {
                start: position,
                end: position,
                text: format!(
                    "{}\"INAPPLICABLE_JVM_NAME\"",
                    if last.is_some() { ", " } else { "" }
                ),
            }),
        ));
    }
    Ok((true, None))
}

/// Only residual Kotlin is changed. Already accepted Java callers keep the
/// original physical name, which `@JvmName` preserves on the retained method.
pub(crate) fn plan(
    index: &SourceIndex,
    sources: &[(PathBuf, String)],
    retained: &HashSet<SymbolId>,
) -> Result<NameSplits, String> {
    let selected = sources
        .iter()
        .filter_map(|(path, source)| {
            index
                .source_file(path)
                .map(|file| (file.path.clone(), source.as_str()))
        })
        .collect::<HashMap<_, _>>();
    let mut names = index
        .files
        .iter()
        .flat_map(|file| file.declarations.iter())
        .flat_map(|owner| owner.members.iter().map(|member| member.name.clone()))
        .collect::<HashSet<_>>();
    let mut splits = BTreeMap::<SymbolId, Split>::new();
    for (path, source) in sources.iter() {
        let Some(file) = index.source_file(path) else {
            continue;
        };
        let tree = crate::transpiler::parse_tree(source);
        let mut pending = vec![tree.root_node()];
        while let Some(node) = pending.pop() {
            pending.extend(node.named_children(&mut node.walk()));
            if node.kind() != "class_declaration" {
                continue;
            }
            if !retained_context(index, node, source, &file.path, retained) {
                continue;
            }
            let Some(name) = node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source.as_bytes()).ok())
            else {
                continue;
            };
            let Some(owner) = file.declarations.iter().find(|owner| {
                owner.name == name
                    && matches!(owner.kind, DeclarationKind::Class | DeclarationKind::Enum)
            }) else {
                continue;
            };
            let getters = getter_names(index, owner, true);
            if getters.is_empty() {
                continue;
            }
            let Some(body) = crate::transpiler::kt::child(node, "class_body")
                .or_else(|| crate::transpiler::kt::child(node, "enum_class_body"))
            else {
                continue;
            };
            for function in body
                .named_children(&mut body.walk())
                .filter(|node| node.kind() == "function_declaration")
            {
                let id = semantics::symbol_id_for_node(source, function, &file.path);
                if !id.parameters.is_empty() || id.receiver.is_some() || !getters.contains(&id.name)
                {
                    continue;
                }
                let modifiers = crate::transpiler::kt::child(function, "modifiers");
                if modifiers.is_some_and(|modifiers| {
                    modifiers
                        .named_children(&mut modifiers.walk())
                        .any(|modifier| {
                            matches!(
                                modifier.utf8_text(source.as_bytes()).unwrap_or("").trim(),
                                "override"
                                    | "abstract"
                                    | "external"
                                    | "expect"
                                    | "actual"
                                    | "suspend"
                                    | "inline"
                                    | "open"
                            )
                        })
                }) {
                    continue;
                }
                if crate::transpiler::kt::child(function, "type_parameters").is_some()
                    || function
                        .utf8_text(source.as_bytes())
                        .unwrap_or("")
                        .split("fun")
                        .next()
                        .unwrap_or("")
                        .contains("JvmName")
                {
                    continue;
                }
                let owner_name = owner.package.as_ref().map_or_else(
                    || owner.name.clone(),
                    |package| format!("{package}.{}", owner.name),
                );
                if index
                    .kotlin_files()
                    .flat_map(|file| &file.declarations)
                    .any(|child| {
                        !std::ptr::eq(child, owner)
                            && child.members.iter().any(|member| {
                                member.kind == MemberKind::Method
                                    && member.name == id.name
                                    && member.parameter_types.is_empty()
                                    && !member.is_static
                            })
                            && index.declaration_source_file(child).is_some_and(|file| {
                                index.supertype_closure_contains(file, child, &owner_name)
                            })
                    })
                {
                    return Err(format!(
                        "getter namespace repair needs the override family of `{}` in {}",
                        id.name,
                        file.path.display()
                    ));
                }
                let mut alias = format!("notlin{}{}", id.name[..1].to_uppercase(), &id.name[1..]);
                let stem = alias.clone();
                let mut suffix = 2;
                while names.contains(&alias) {
                    alias = format!("{stem}{suffix}");
                    suffix += 1;
                }
                names.insert(alias.clone());
                let name = function.child_by_field_name("name").unwrap();
                let (add_suppression, suppression_edit) =
                    name_suppression(index, file, function, source)?;
                splits.insert(
                    id.clone(),
                    Split {
                        original: id,
                        alias,
                        declaration_start: function.start_byte(),
                        name_start: name.start_byte(),
                        name_end: name.end_byte(),
                        add_suppression,
                        suppression_edit,
                    },
                );
            }
        }
    }
    if splits.is_empty() {
        return Ok(NameSplits::default());
    }
    let method_names = splits
        .keys()
        .map(|id| id.name.as_str())
        .collect::<HashSet<_>>();
    let mut provider = SyntaxSemanticProvider::new(
        index
            .files
            .iter()
            .map(|file| {
                (
                    file.path.clone(),
                    selected
                        .get(&file.path)
                        .copied()
                        .unwrap_or(file.source_text())
                        .to_owned(),
                )
            })
            .collect::<Vec<_>>(),
    );
    provider.enrich_workspace_type_facts(index);
    let mut edits = HashMap::<PathBuf, Vec<Edit>>::new();
    for file in index.kotlin_files() {
        let source = selected
            .get(&file.path)
            .copied()
            .unwrap_or(file.source_text());
        if !method_names.iter().any(|name| source.contains(name)) {
            continue;
        }
        let tree = crate::transpiler::parse_tree(source);
        let mut pending = vec![tree.root_node()];
        while let Some(node) = pending.pop() {
            pending.extend(node.named_children(&mut node.walk()));
            if selected.contains_key(&file.path)
                && !retained_context(index, node, source, &file.path, retained)
            {
                continue;
            }
            if node.kind() == "callable_reference"
                && node.utf8_text(source.as_bytes()).ok().is_some_and(|text| {
                    method_names
                        .iter()
                        .any(|name| text.trim_end().ends_with(name))
                })
            {
                return Err(format!(
                    "getter namespace repair needs a bound callable-reference plan in {}",
                    file.path.display()
                ));
            }
            if node.kind() != "call_expression" || semantics::call_argument_count(node) != 0 {
                continue;
            }
            let Some(callee) = semantics::call_callee_node(node) else {
                continue;
            };
            let Ok(name) = callee.utf8_text(source.as_bytes()) else {
                continue;
            };
            if !method_names.contains(name) {
                continue;
            }
            if semantics::explicit_receiver_node(node, callee).is_none()
                && crate::function_callsite::local_shadow(node, name, source)
            {
                continue;
            }
            match semantics::resolve_required_call(
                source, &file.path, node, callee, name, &provider,
            ) {
                FactStatus::Established(id) => {
                    if let Some(split) = splits.get(&id) {
                        if !selected.contains_key(&file.path) {
                            return Err(format!(
                                "getter namespace caller is outside selected sources: {}",
                                file.path.display()
                            ));
                        }
                        edits.entry(file.path.clone()).or_default().push(Edit {
                            start: callee.start_byte(),
                            end: callee.end_byte(),
                            text: split.alias.clone(),
                        });
                    }
                }
                _ => {
                    return Err(format!(
                        "getter namespace repair cannot establish `{name}` at {}:{}",
                        file.path.display(),
                        node.start_position().row + 1
                    ));
                }
            }
        }
    }
    let mut report = PlannedAccessorRepairs::default();
    for split in splits.values() {
        let source = selected[&split.original.file];
        let indent = source[..split.declaration_start]
            .rsplit('\n')
            .next()
            .filter(|line| line.chars().all(char::is_whitespace))
            .unwrap_or("");
        let file_edits = edits.entry(split.original.file.clone()).or_default();
        if let Some(edit) = &split.suppression_edit {
            file_edits.push(Edit {
                start: edit.start,
                end: edit.end,
                text: edit.text.clone(),
            });
        }
        file_edits.push(Edit {
            start: split.declaration_start,
            end: split.declaration_start,
            text: format!(
                "{}@kotlin.jvm.JvmName(\"{}\")\n{indent}",
                if split.add_suppression {
                    format!("@kotlin.Suppress(\"INAPPLICABLE_JVM_NAME\")\n{indent}")
                } else {
                    String::new()
                },
                split.original.name
            ),
        });
        file_edits.push(Edit {
            start: split.name_start,
            end: split.name_end,
            text: split.alias.clone(),
        });
        report.repairs.push(PlannedRepair{target:split.original.clone(),kind:"getter-namespace".into(),detail:format!("Kotlin `{}` renamed to `{}`; its original JVM name and independent getter descriptor are preserved",split.original.name,split.alias)});
        let mut renamed = split.original.clone();
        renamed.name = split.alias.clone();
        report.provenance.push(OriginMap {
            generated: renamed,
            origin: split.original.clone(),
            reason: "Kotlin function name split preserves the existing JVM getter descriptor"
                .into(),
        });
    }
    for changes in edits.values_mut() {
        changes.sort_by(|a, b| (a.start, a.end, &a.text).cmp(&(b.start, b.end, &b.text)));
        changes.dedup_by(|a, b| a.start == b.start && a.end == b.end && a.text == b.text);
        if changes
            .windows(2)
            .any(|pair| pair[0].end > pair[1].start || pair[0].start == pair[1].start)
        {
            return Err("overlapping getter namespace repairs".into());
        }
    }
    report.count = edits.values().map(Vec::len).sum();
    let source_hashes = edits
        .keys()
        .map(|path| {
            (
                path.clone(),
                *blake3::hash(selected[path].as_bytes()).as_bytes(),
            )
        })
        .collect();
    Ok(NameSplits {
        edits,
        source_hashes,
        report,
    })
}

#[cfg(test)]
mod tests;
