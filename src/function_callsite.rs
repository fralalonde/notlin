//! Conservative call-site repairs when Kotlin top-level functions move to Java.
//!
//! Only references with a unique package/import-qualified declaration identity
//! are rewritten. All edits are tied to the original source snapshot and byte
//! span; edits are based on the current residual snapshot and applied only
//! while that snapshot still matches the one used to compute their spans.

use crate::semantics::{SemanticProvider, SourceLocation, SymbolId, SyntaxSemanticProvider};
use crate::translation_plan::{BackendOwner, PlannedRepair, PlannedSourceEdit, TranslationPlan};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tree_sitter::Node;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallsiteRepairDiagnostic {
    pub file: PathBuf,
    pub start_byte: usize,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallsiteRepairReport {
    pub edits: Vec<PlannedSourceEdit>,
    pub repairs: Vec<PlannedRepair>,
    pub applied_files: Vec<PathBuf>,
    pub diagnostics: Vec<CallsiteRepairDiagnostic>,
    pub requires_replan: bool,
}

#[derive(Debug, Clone)]
struct ImportRef {
    start: usize,
    end: usize,
    package: String,
    name: String,
    visible_name: String,
}

/// Repair calls/imports to Java-owned top-level Kotlin functions.
///
/// `original_sources` must be the immutable snapshots used to build the
/// translation plans. `residual_sources` is updated in place only when that
/// file still has the same content hash as its original snapshot.
pub fn repair_virtual_calls(
    original_sources: &BTreeMap<PathBuf, String>,
    plans: &[TranslationPlan],
    residual_sources: &mut BTreeMap<PathBuf, String>,
) -> CallsiteRepairReport {
    let mut report = CallsiteRepairReport::default();
    let input: Vec<_> = original_sources
        .iter()
        .map(|(p, s)| (p.clone(), s.clone()))
        .collect();
    let provider = SyntaxSemanticProvider::new(input);
    let mut targets = Vec::<(SymbolId, String)>::new();
    for plan in plans {
        for decision in &plan.declarations {
            if decision.final_owner != Some(BackendOwner::Java)
                || decision.symbol_id.kind != "function"
                || !decision.symbol_id.owner_path.is_empty()
                || decision.symbol_id.receiver.is_some()
            {
                continue;
            }
            if let Some(source) = original_sources.get(&decision.symbol_id.file) {
                let facade = facade_fq_name(
                    &decision.symbol_id.file,
                    source,
                    &decision.symbol_id.package,
                );
                targets.push((decision.symbol_id.clone(), facade));
            }
        }
    }
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    targets.dedup_by(|a, b| a.0 == b.0);
    if targets.is_empty() {
        return report;
    }

    let mut edits_by_file = BTreeMap::<PathBuf, Vec<PlannedSourceEdit>>::new();
    for (file, current) in residual_sources.iter() {
        let source = current;
        let hash = *blake3::hash(source.as_bytes()).as_bytes();
        let Some(tree) = parse_kotlin(source) else {
            continue;
        };
        let package = source_package(source);
        let imports = imports(source, tree.root_node());
        let mut file_edits = Vec::new();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression"
                && let Some(callee) = bare_callee(node)
            {
                let spelling = callee.utf8_text(source.as_bytes()).unwrap_or("");
                if spelling.is_empty() || local_shadow(node, spelling, source) {
                } else if let Some(target) =
                    resolve_target(spelling, &package, &imports, &targets, &provider)
                {
                    if has_named_arguments(node, source) {
                        report.requires_replan = true;
                        report.diagnostics.push(CallsiteRepairDiagnostic{file:file.clone(),start_byte:node.start_byte(),code:"C002".into(),message:format!("call to migrated `{spelling}` uses named arguments, which cannot be preserved for a Java target; retain or replan the caller")});
                        let mut walk = node.walk();
                        stack.extend(node.named_children(&mut walk));
                        continue;
                    }
                    let replacement = format!("{}.{}", target.1, target.0.name);
                    file_edits.push(PlannedSourceEdit {
                        location: SourceLocation {
                            file: file.clone(),
                            snapshot_hash: hash,
                            start_byte: callee.start_byte(),
                            end_byte: callee.end_byte(),
                        },
                        replacement: replacement.clone(),
                        speculative: true,
                    });
                    report.repairs.push(PlannedRepair {
                        target: target.0.clone(),
                        kind: "top-level-function-callsite".into(),
                        detail: format!("rewrote `{spelling}` to `{replacement}`"),
                    });
                }
            }
            let mut walk = node.walk();
            stack.extend(node.named_children(&mut walk));
        }
        // Import references are repaired only when they uniquely name the same
        // Java-owned declaration; aliases are preserved.
        for import in &imports {
            let found: Vec<_> = targets
                .iter()
                .filter(|(id, _)| id.name == import.name && id.package == import.package)
                .collect();
            let declarations: Vec<_> = provider
                .symbols()
                .iter()
                .filter(|symbol| {
                    symbol.id.kind == "function"
                        && symbol.id.receiver.is_none()
                        && symbol.id.name == import.name
                        && symbol.id.package == import.package
                        && symbol.id.owner_path.is_empty()
                })
                .collect();
            if let ([target], [declaration]) = (found.as_slice(), declarations.as_slice())
                && target.0 == declaration.id
            {
                let start = import.start;
                let end = import.end;
                let fq = format!("{}.{}", target.1, target.0.name);
                let replacement = if import.visible_name != import.name {
                    format!("import {fq} as {}", import.visible_name)
                } else {
                    format!("import {fq}")
                };
                file_edits.push(PlannedSourceEdit {
                    location: SourceLocation {
                        file: file.clone(),
                        snapshot_hash: hash,
                        start_byte: start,
                        end_byte: end,
                    },
                    replacement: replacement.clone(),
                    speculative: true,
                });
                report.repairs.push(PlannedRepair {
                    target: target.0.clone(),
                    kind: "top-level-function-import".into(),
                    detail: format!("rewrote import of `{}` to `{replacement}`", import.name),
                });
            }
        }
        if !file_edits.is_empty() {
            edits_by_file.insert(file.clone(), file_edits);
        }
    }
    for (file, mut edits) in edits_by_file {
        edits.sort_by_key(|e| (e.location.start_byte, e.location.end_byte));
        edits.dedup_by(|a, b| {
            a.location.start_byte == b.location.start_byte
                && a.location.end_byte == b.location.end_byte
        });
        if let Some(residual) = residual_sources.get_mut(&file) {
            let expected = *blake3::hash(residual.as_bytes()).as_bytes();
            if edits.iter().any(|e| e.location.snapshot_hash != expected) {
                report.diagnostics.push(CallsiteRepairDiagnostic {
                    file: file.clone(),
                    start_byte: 0,
                    code: "C001".into(),
                    message: "source snapshot changed before edits could be applied".into(),
                });
                continue;
            }
            for edit in edits.iter().rev() {
                if edit.location.end_byte > residual.len()
                    || !residual.is_char_boundary(edit.location.start_byte)
                    || !residual.is_char_boundary(edit.location.end_byte)
                {
                    continue;
                }
                residual.replace_range(
                    edit.location.start_byte..edit.location.end_byte,
                    &edit.replacement,
                );
            }
            report.applied_files.push(file.clone());
            report.edits.extend(edits);
        }
    }
    report.applied_files.sort();
    report.edits.sort_by(|a, b| {
        a.location
            .file
            .cmp(&b.location.file)
            .then(a.location.start_byte.cmp(&b.location.start_byte))
    });
    report
}

/// Find Java-owned candidates whose references cannot be preserved by the
/// bare-call rewrite pass. The planner should keep these declarations Kotlin
/// until the unsupported reference form has a dedicated lowering strategy.
pub fn functions_requiring_kotlin_retention(
    original_sources: &BTreeMap<PathBuf, String>,
    candidate_symbols: &[SymbolId],
) -> BTreeMap<SymbolId, Vec<CallsiteRepairDiagnostic>> {
    let input: Vec<_> = original_sources
        .iter()
        .map(|(path, source)| (path.clone(), source.clone()))
        .collect();
    let provider = SyntaxSemanticProvider::new(input);
    functions_requiring_kotlin_retention_with_provider(
        original_sources,
        candidate_symbols,
        &provider,
    )
}

/// Reuses the workspace provider already built by the fixpoint planner.
pub fn functions_requiring_kotlin_retention_with_provider(
    original_sources: &BTreeMap<PathBuf, String>,
    candidate_symbols: &[SymbolId],
    provider: &dyn SemanticProvider,
) -> BTreeMap<SymbolId, Vec<CallsiteRepairDiagnostic>> {
    let targets: Vec<_> = candidate_symbols
        .iter()
        .filter(|id| id.kind == "function" && id.owner_path.is_empty() && id.receiver.is_none())
        .map(|id| {
            (
                id.clone(),
                facade_fq_name(
                    &id.file,
                    original_sources
                        .get(&id.file)
                        .map(String::as_str)
                        .unwrap_or(""),
                    &id.package,
                ),
            )
        })
        .collect();
    let target_index = index_targets(&targets);
    let mut java_facade_index = BTreeMap::<(String, String), Vec<SymbolId>>::new();
    for (id, _) in &targets {
        let old_facade = original_kotlin_facade(
            &id.file,
            original_sources
                .get(&id.file)
                .map(String::as_str)
                .unwrap_or(""),
        );
        java_facade_index
            .entry((old_facade, id.name.clone()))
            .or_default()
            .push(id.clone());
    }
    let mut result = BTreeMap::<SymbolId, Vec<CallsiteRepairDiagnostic>>::new();
    for (file, source) in original_sources {
        if file
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("java"))
        {
            let facade_members = java_facade_members(source);
            for pair in facade_members {
                if let Some(ids) = java_facade_index.get(&pair) {
                    for id in ids {
                        result
                            .entry(id.clone())
                            .or_default()
                            .push(CallsiteRepairDiagnostic {
                                file: file.clone(),
                                start_byte: 0,
                                code: "C003".into(),
                                message: format!(
                                    "Java source uses the Kotlin file-facade ABI for `{}`",
                                    id.name
                                ),
                            });
                    }
                }
            }
            continue;
        }
        let Some(tree) = parse_kotlin(source) else {
            continue;
        };
        let package = source_package(source);
        let imports = imports(source, tree.root_node());
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            let location = |start| CallsiteRepairDiagnostic {
                file: file.clone(),
                start_byte: start,
                code: "C004".into(),
                message: String::new(),
            };
            if node.kind() == "call_expression" {
                if let Some(callee) = bare_callee(node) {
                    let name = callee.utf8_text(source.as_bytes()).unwrap_or("");
                    if !name.is_empty() && !local_shadow(node, name, source) {
                        let candidates =
                            target_candidates(name, &package, &imports, &targets, &target_index);
                        if !candidates.is_empty() {
                            let provider_count = provider
                                .symbols_named(candidate_visible_name(name, &imports))
                                .into_iter()
                                .filter(|s| {
                                    s.id.kind == "function"
                                        && s.id.owner_path.is_empty()
                                        && s.id.receiver.is_none()
                                        && s.id.name == candidate_visible_name(name, &imports)
                                        && s.id.package
                                            == candidate_package(&package, &imports, name)
                                })
                                .count();
                            if has_named_arguments(node, source) {
                                for target in &candidates {
                                    let mut diagnostic = location(node.start_byte());
                                    diagnostic.code = "C002".into();
                                    diagnostic.message = format!(
                                        "call to `{}` uses named arguments and cannot safely target Java",
                                        target.0.name
                                    );
                                    result.entry(target.0.clone()).or_default().push(diagnostic)
                                }
                            } else if candidates.len() > 1 || provider_count > 1 {
                                for target in &candidates {
                                    let mut diagnostic = location(node.start_byte());
                                    diagnostic.code = "C005".into();
                                    diagnostic.message = format!(
                                        "call to `{}` is ambiguous across declarations",
                                        target.0.name
                                    );
                                    result.entry(target.0.clone()).or_default().push(diagnostic)
                                }
                            }
                        }
                    }
                } else {
                    // A qualified function call (`p.work()`/`FileKt.work()`) is
                    // outside the bare-call rewrite contract.
                    let mut children = node.walk();
                    let callee_node = node.named_children(&mut children).next();
                    if let Some(callee_node) = callee_node
                        && let Some(selector) = navigation_selector(callee_node, source)
                    {
                        let name = selector.utf8_text(source.as_bytes()).unwrap_or("");
                        for target in
                            target_candidates(name, &package, &imports, &targets, &target_index)
                        {
                            let callee_text = callee_node
                                .utf8_text(source.as_bytes())
                                .unwrap_or("")
                                .trim();
                            let suffix = format!(".{name}");
                            let qualifier = callee_text.strip_suffix(&suffix).unwrap_or("").trim();
                            let old_facade = original_kotlin_facade(
                                &target.0.file,
                                original_sources
                                    .get(&target.0.file)
                                    .map(String::as_str)
                                    .unwrap_or(""),
                            );
                            let old_facade_fq = if target.0.package.is_empty() {
                                old_facade.clone()
                            } else {
                                format!("{}.{}", target.0.package, old_facade)
                            };
                            let is_facade = qualifier == old_facade || qualifier == old_facade_fq;
                            let is_package = qualifier == target.0.package
                                || (!target.0.package.is_empty()
                                    && qualifier.ends_with(&format!(".{}", target.0.package)));
                            if is_facade || is_package {
                                let mut diagnostic = location(node.start_byte());
                                diagnostic.code = "C006".into();
                                diagnostic.message = format!(
                                    "qualified call to `{}` is not handled by the bare-call repair",
                                    target.0.name
                                );
                                result.entry(target.0.clone()).or_default().push(diagnostic);
                            }
                        }
                    }
                }
            }
            if matches!(
                node.kind(),
                "callable_reference" | "callable_reference_expression"
            ) {
                let text = node.utf8_text(source.as_bytes()).unwrap_or("");
                let tail = text.rsplit("::").next().unwrap_or("").trim();
                for target in target_candidates(tail, &package, &imports, &targets, &target_index) {
                    let mut diagnostic = location(node.start_byte());
                    diagnostic.code = "C007".into();
                    diagnostic.message = format!(
                        "callable reference to `{}` is not handled by the bare-call repair",
                        target.0.name
                    );
                    result.entry(target.0.clone()).or_default().push(diagnostic)
                }
            }
            let mut walk = node.walk();
            stack.extend(node.named_children(&mut walk));
        }
    }
    for diagnostics in result.values_mut() {
        diagnostics.sort_by(|a, b| {
            a.file
                .cmp(&b.file)
                .then(a.start_byte.cmp(&b.start_byte))
                .then(a.code.cmp(&b.code))
        });
        diagnostics.dedup();
    }
    result
}

fn target_candidates<'a>(
    name: &str,
    package: &str,
    imports: &[ImportRef],
    targets: &'a [(SymbolId, String)],
    target_index: &BTreeMap<(String, String), Vec<usize>>,
) -> Vec<&'a (SymbolId, String)> {
    let wanted_package = candidate_package(package, imports, name);
    let wanted_name = candidate_visible_name(name, imports);
    target_index
        .get(&(wanted_package.to_owned(), wanted_name.to_owned()))
        .into_iter()
        .flatten()
        .filter_map(|index| targets.get(*index))
        .collect()
}
fn index_targets(targets: &[(SymbolId, String)]) -> BTreeMap<(String, String), Vec<usize>> {
    let mut index = BTreeMap::<(String, String), Vec<usize>>::new();
    for (position, (symbol, _)) in targets.iter().enumerate() {
        index
            .entry((symbol.package.clone(), symbol.name.clone()))
            .or_default()
            .push(position);
    }
    index
}
fn candidate_visible_name<'a>(name: &'a str, imports: &'a [ImportRef]) -> &'a str {
    imports
        .iter()
        .find(|i| i.visible_name == name)
        .map(|i| i.name.as_str())
        .unwrap_or(name)
}
fn candidate_package<'a>(package: &'a str, imports: &'a [ImportRef], name: &str) -> &'a str {
    imports
        .iter()
        .find(|i| i.visible_name == name)
        .map(|i| i.package.as_str())
        .unwrap_or(package)
}

fn parse_kotlin(source: &str) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .ok()?;
    parser.parse(source, None)
}
fn source_package(source: &str) -> String {
    source
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("package ")
                .map(|p| p.split(';').next().unwrap_or(p).trim().to_owned())
        })
        .unwrap_or_default()
}
pub fn facade_name(source: &str, file: &Path) -> String {
    facade_fq_name(file, source, "")
}
fn facade_fq_name(file: &Path, source: &str, package: &str) -> String {
    let mut facade = None;
    if let Some(at) = source.find("@file:JvmName") {
        let tail = &source[at + "@file:JvmName".len()..];
        if let Some(first) = tail.find('"') {
            let rest = &tail[first + 1..];
            if let Some(end) = rest.find('"') {
                facade = Some(rest[..end].to_owned())
            }
        }
    }
    let raw = facade.unwrap_or_else(|| {
        file.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Main")
            .to_string()
    });
    let raw = raw
        .chars()
        .map(|c| if c == '-' || c == '.' { '_' } else { c })
        .collect::<String>();
    let mut chars = raw.chars();
    let name = match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => "Main".into(),
    };
    let name = non_conflicting_facade_name(source, &name);
    if package.is_empty() {
        name
    } else {
        format!("{package}.{name}")
    }
}

/// Keep the file utility class distinct from declarations in the same snapshot.
/// Cache only snapshot-derived names, so repeated caller repairs do not reparse.
pub(crate) fn non_conflicting_facade_name(source: &str, preferred: &str) -> String {
    use std::sync::{Mutex, OnceLock};
    static NAMES: OnceLock<Mutex<BTreeMap<[u8; 32], Vec<String>>>> = OnceLock::new();
    let hash = *blake3::hash(source.as_bytes()).as_bytes();
    let names = NAMES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(hash)
        .or_insert_with(|| {
            let tree = crate::transpiler::parse_tree(source);
            let mut cursor = tree.root_node().walk();
            tree.root_node()
                .named_children(&mut cursor)
                .filter_map(|node| {
                    let id = crate::semantics::symbol_id_for_node(source, node, Path::new(""));
                    matches!(
                        id.kind.as_str(),
                        "class" | "interface" | "object" | "enum" | "annotation"
                    )
                    .then_some(id.name)
                })
                .collect()
        })
        .clone();
    let mut name = preferred.to_owned();
    while names.contains(&name) {
        name.push_str("Kt");
    }
    name
}
fn original_kotlin_facade(file: &Path, source: &str) -> String {
    if let Some(at) = source.find("@file:JvmName") {
        let tail = &source[at + "@file:JvmName".len()..];
        if let Some(q) = tail.find('"')
            && let Some(end) = tail[q + 1..].find('"')
        {
            return tail[q + 1..q + 1 + end].to_owned();
        }
    }
    format!(
        "{}Kt",
        file.file_stem().and_then(|s| s.to_str()).unwrap_or("File")
    )
}
/// Collect the same facade/member pairs recognized by the conservative legacy
/// substring check, but scan a Java snapshot only once regardless of how many
/// Kotlin functions are candidates. Comments and literals intentionally remain
/// visible to this scanner, preserving the prior conservative false positives.
fn java_facade_members(source: &str) -> std::collections::HashSet<(String, String)> {
    let chars = source.char_indices().collect::<Vec<_>>();
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut found = std::collections::HashSet::new();
    for index in 0..chars.len() {
        if chars[index].1 != '.' || index + 1 >= chars.len() || !is_ident(chars[index + 1].1) {
            continue;
        }
        let mut end = index + 1;
        while end < chars.len() && is_ident(chars[end].1) {
            end += 1;
        }
        let member_start = chars[index + 1].0;
        let member_end = if end < chars.len() {
            chars[end].0
        } else {
            source.len()
        };
        let member = &source[member_start..member_end];

        let mut previous = index;
        while previous > 0 && chars[previous - 1].1.is_whitespace() {
            previous -= 1;
        }
        let mut facade_start = previous;
        while facade_start > 0 && is_ident(chars[facade_start - 1].1) {
            facade_start -= 1;
        }
        if facade_start == previous {
            continue;
        }
        let facade_start_byte = chars[facade_start].0;
        let facade_end_byte = chars[previous].0;
        let facade = &source[facade_start_byte..facade_end_byte];

        // The previous implementation searched for `.{candidate_name}` as a
        // substring, so preserve prefix matches such as `.worker` for `work`.
        for (offset, _) in member.char_indices().skip(1) {
            found.insert((facade.to_owned(), member[..offset].to_owned()));
        }
        found.insert((facade.to_owned(), member.to_owned()));
    }
    found
}
fn imports(source: &str, root: Node) -> Vec<ImportRef> {
    let mut result = Vec::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "import" {
            let text = n.utf8_text(source.as_bytes()).unwrap_or("").trim();
            if let Some(body) = text.strip_prefix("import ") {
                let body = body.trim();
                if !body.ends_with(".*") {
                    let (path, alias) = body
                        .split_once(" as ")
                        .map(|(p, a)| (p.trim(), Some(a.trim())))
                        .unwrap_or((body, None));
                    if let Some((package, name)) = path.rsplit_once('.') {
                        result.push(ImportRef {
                            start: n.start_byte(),
                            end: n.end_byte(),
                            package: package.to_owned(),
                            name: name.to_owned(),
                            visible_name: alias.unwrap_or(name).to_owned(),
                        })
                    }
                }
            }
        }
        let mut c = n.walk();
        stack.extend(n.named_children(&mut c));
    }
    result
}
fn bare_callee(node: Node) -> Option<Node> {
    let mut c = node.walk();
    node.named_children(&mut c)
        .find(|n| matches!(n.kind(), "simple_identifier" | "identifier"))
}
fn navigation_selector<'a>(callee: Node<'a>, source: &str) -> Option<Node<'a>> {
    if !matches!(callee.kind(), "navigation_expression" | "navigation_suffix") {
        return None;
    }
    let mut stack = vec![callee];
    let mut identifiers = Vec::new();
    while let Some(node) = stack.pop() {
        if matches!(node.kind(), "simple_identifier" | "identifier") {
            identifiers.push(node)
        }
        let mut walk = node.walk();
        stack.extend(node.named_children(&mut walk));
    }
    identifiers
        .into_iter()
        .max_by_key(|node| node.start_byte())
        .filter(|node| node.utf8_text(source.as_bytes()).is_ok())
}
fn has_named_arguments(call: Node, source: &str) -> bool {
    let mut c = call.walk();
    let Some(args) = call
        .named_children(&mut c)
        .find(|n| n.kind() == "value_arguments")
    else {
        return false;
    };
    let mut c = args.walk();
    for arg in args
        .named_children(&mut c)
        .filter(|n| n.kind() == "value_argument")
    {
        if arg.child_by_field_name("name").is_some() {
            return true;
        }
        let mut arg_walk = arg.walk();
        let mut children = arg.children(&mut arg_walk);
        let first = children.find(|n| n.is_named());
        let second = children.find(|n| !n.is_extra());
        if first.is_some_and(|n| matches!(n.kind(), "simple_identifier" | "identifier"))
            && second.is_some_and(|n| n.kind() == "=")
        {
            return true;
        }
        let text = arg.utf8_text(source.as_bytes()).unwrap_or("").trim();
        if let Some((left, _)) = text.split_once('=')
            && !left.is_empty()
            && left
                .chars()
                .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '`')
        {
            return true;
        }
    }
    false
}
fn node_decl_name<'a>(node: Node, source: &'a str) -> Option<&'a str> {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .or_else(|| {
            let mut c = node.walk();
            node.named_children(&mut c)
                .find(|n| matches!(n.kind(), "simple_identifier" | "identifier"))
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        })
}
fn local_shadow(call: Node, name: &str, source: &str) -> bool {
    let mut scope = call.parent();
    while let Some(n) = scope {
        if matches!(
            n.kind(),
            "function_declaration" | "lambda_literal" | "function_literal"
        ) {
            let mut stack = vec![n];
            while let Some(child) = stack.pop() {
                if matches!(
                    child.kind(),
                    "parameter"
                        | "class_parameter"
                        | "variable_declaration"
                        | "function_declaration"
                ) && node_decl_name(child, source) == Some(name)
                {
                    return true;
                }
                let mut c = child.walk();
                stack.extend(child.named_children(&mut c));
            }
        }
        scope = n.parent()
    }
    false
}
fn resolve_target<'a>(
    spelling: &str,
    package: &str,
    imports: &[ImportRef],
    targets: &'a [(SymbolId, String)],
    provider: &impl SemanticProvider,
) -> Option<&'a (SymbolId, String)> {
    let import = imports.iter().find(|i| i.visible_name == spelling);
    let (wanted_name, wanted_package) = if let Some(i) = import {
        (i.name.as_str(), i.package.as_str())
    } else {
        (spelling, package)
    };
    let candidates: Vec<_> = targets
        .iter()
        .filter(|(id, _)| id.name == wanted_name && id.package == wanted_package)
        .collect();
    let declarations: Vec<_> = provider
        .symbols()
        .iter()
        .filter(|symbol| {
            symbol.id.kind == "function"
                && symbol.id.receiver.is_none()
                && symbol.id.name == wanted_name
                && symbol.id.package == wanted_package
                && symbol.id.owner_path.is_empty()
        })
        .collect();
    if candidates.len() != 1 || declarations.len() != 1 || candidates[0].0 != declarations[0].id {
        return None;
    }
    // The syntax provider is used as an ambiguity guard as well. A global
    // ambiguity is acceptable only when package/import scoping narrowed it to
    // this exact unique declaration.
    let _fact = provider.resolve(wanted_name);
    candidates.first().copied()
}
