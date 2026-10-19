//! Source-bound retention for Kotlin's canonical `Nothing` return type.
//!
//! Java's `Void` is not a bottom type. Keeping a declaration that promises
//! `Nothing` in Kotlin also means callers cannot be emitted as value-producing
//! Java calls until the backend can preserve that contract.

use super::{
    FactStatus, KotlinType, SemanticProvider, SemanticSymbol, SourceLocation, SymbolId, TypeRef,
    declaration_type_node, package_name, resolve_required_call,
};
use crate::translation_plan::{
    BackendOwner, PreparationBlockerKind, RetentionReason, TranslationPlan,
};
use std::collections::BTreeMap;
use std::path::Path;
use tree_sitter::{Node, Tree};

pub(super) fn plan(
    source: &str,
    tree: &Tree,
    file: &Path,
    provider: &(impl SemanticProvider + ?Sized),
    allow_approximations: bool,
    plan: &mut TranslationPlan,
) {
    let source_hash = *blake3::hash(source.as_bytes()).as_bytes();
    assert_eq!(plan.source_hash, source_hash);
    if provider.source_for_file(file) != Some(source) {
        return;
    }

    // Restrict declaration inspection to the current source. Cross-file
    // targets are classified lazily after ordinary call resolution gives us
    // their exact SymbolId.
    let current_symbols = provider
        .symbols_in_file(file)
        .into_iter()
        .filter(|symbol| {
            matches!(symbol.id.kind.as_str(), "function" | "method" | "property")
                && symbol.location.snapshot_hash == source_hash
                && potential_nothing_return(symbol)
        })
        .collect::<Vec<_>>();
    let mut return_facts = current_symbols
        .iter()
        .map(|symbol| (symbol.id.clone(), nothing_return_status(symbol, provider)))
        .collect::<BTreeMap<_, _>>();
    let owners = OwnerIndex::new(plan);

    for symbol in &current_symbols {
        let Some(index) = owners.owner_span(symbol.location.start_byte, symbol.location.end_byte)
        else {
            continue;
        };
        match return_facts
            .get(&symbol.id)
            .cloned()
            .unwrap_or(FactStatus::Unknown)
        {
            FactStatus::Established(true) | FactStatus::Inferred(true) => retain(
                plan,
                index,
                symbol.location.clone(),
                format!(
                    "semantic loss: `{}` has a Kotlin `Nothing` or `Nothing?` return contract, which Java `Void` cannot preserve",
                    symbol.id.name
                ),
                PreparationBlockerKind::SemanticLoss,
                allow_approximations,
            ),
            FactStatus::Unknown | FactStatus::Ambiguous(_) => {
                if has_explicit_nothing_spelling(source, symbol) {
                    retain(
                        plan,
                        index,
                        symbol.location.clone(),
                        format!(
                            "cannot establish whether `{}` denotes canonical Kotlin `Nothing` because its return type binding is unknown or ambiguous",
                            symbol.id.name
                        ),
                        PreparationBlockerKind::UnresolvedAssumption,
                        allow_approximations,
                    );
                }
            }
            FactStatus::Established(false) | FactStatus::Inferred(false) => {}
        }
    }

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression"
            && let Some(index) = owners.owner(node)
            && !is_planned_todo(node, source, file, source_hash, plan)
            && let Some((callee, name)) = call_callee(source, node)
            && provider
                .symbols_named(&name)
                .iter()
                .any(|symbol| potential_nothing_return(symbol))
        {
            match resolve_required_call(source, file, node, callee, &name, provider) {
                FactStatus::Established(id) => {
                    let status = return_facts.entry(id.clone()).or_insert_with(|| {
                        unique_symbol(provider, &id)
                            .map(|symbol| nothing_return_status(symbol, provider))
                            .unwrap_or(FactStatus::Unknown)
                    });
                    match status {
                        FactStatus::Established(true) | FactStatus::Inferred(true) => retain(
                            plan,
                            index,
                            location(source, file, source_hash, node),
                            format!(
                                "semantic loss: call `{name}` has a Kotlin `Nothing` or `Nothing?` return contract, which Java `Void` cannot preserve as an expression"
                            ),
                            PreparationBlockerKind::SemanticLoss,
                            allow_approximations,
                        ),
                        FactStatus::Unknown | FactStatus::Ambiguous(_) => retain(
                            plan,
                            index,
                            location(source, file, source_hash, node),
                            format!(
                                "cannot establish whether call `{name}` has Kotlin `Nothing` semantics because its return type binding is unknown or ambiguous"
                            ),
                            PreparationBlockerKind::UnresolvedAssumption,
                            allow_approximations,
                        ),
                        FactStatus::Established(false) | FactStatus::Inferred(false) => {}
                    }
                }
                FactStatus::Ambiguous(candidates) => {
                    let may_be_bottom = candidates.iter().any(|candidate| {
                        let status = return_facts.entry(candidate.clone()).or_insert_with(|| {
                            unique_symbol(provider, candidate)
                                .map(|symbol| nothing_return_status(symbol, provider))
                                .unwrap_or(FactStatus::Unknown)
                        });
                        !matches!(
                            status,
                            FactStatus::Established(false) | FactStatus::Inferred(false)
                        )
                    });
                    if may_be_bottom {
                        retain(
                            plan,
                            index,
                            location(source, file, source_hash, node),
                            format!(
                                "cannot establish whether call `{name}` has Kotlin `Nothing` semantics because its source callable binding is ambiguous"
                            ),
                            PreparationBlockerKind::UnresolvedAssumption,
                            allow_approximations,
                        );
                    }
                }
                FactStatus::Inferred(_) | FactStatus::Unknown => {}
            }
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
}

fn nothing_return_status(
    symbol: &SemanticSymbol,
    provider: &(impl SemanticProvider + ?Sized),
) -> FactStatus<bool> {
    if !potential_nothing_return(symbol) {
        return FactStatus::Established(false);
    }
    if !symbol
        .id
        .file
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("kt"))
    {
        return FactStatus::Established(false);
    }
    let type_fact = match &symbol.ty {
        FactStatus::Established(value) | FactStatus::Inferred(value) => value,
        FactStatus::Unknown | FactStatus::Ambiguous(_) => return FactStatus::Unknown,
    };
    let TypeRef {
        ty: KotlinType::Named { name, arguments },
        ..
    } = type_fact
    else {
        return FactStatus::Established(false);
    };
    if !arguments.is_empty() || !matches!(name.as_str(), "Nothing" | "kotlin.Nothing") {
        return FactStatus::Established(false);
    }
    let Some(decl_source) = provider.source_for_file(&symbol.id.file) else {
        return FactStatus::Unknown;
    };
    if *blake3::hash(decl_source.as_bytes()).as_bytes() != symbol.location.snapshot_hash {
        return FactStatus::Unknown;
    }
    let Some(spelling) = declared_return_spelling(
        decl_source,
        symbol.location.start_byte,
        symbol.location.end_byte,
    ) else {
        return FactStatus::Unknown;
    };
    let spelling = spelling.trim().trim_end_matches('?').trim();
    if !matches!(spelling, "Nothing" | "kotlin.Nothing") {
        return FactStatus::Established(false);
    }
    let package = package_name(decl_source);
    if provider.has_type_alias_in_package(&package, "Nothing") {
        return FactStatus::Unknown;
    }
    if spelling == "kotlin.Nothing" {
        if provider
            .symbols_named("Nothing")
            .iter()
            .any(|candidate| candidate.id.package == "kotlin")
            || provider
                .symbols_named("kotlin")
                .iter()
                .any(|candidate| is_classifier(&candidate.id) && candidate.id.package == package)
        {
            return FactStatus::Unknown;
        }
        return FactStatus::Established(true);
    }
    match provider.resolve_type_in_file(&symbol.id.file, &package, "Nothing") {
        FactStatus::Established(owner) => {
            let top_level = owner.owner_path.is_empty() && owner.package == package;
            let lexical = owner.file == symbol.id.file
                && !owner.owner_path.is_empty()
                && symbol.id.owner_path.starts_with(&owner.owner_path);
            let imported = super::kotlin_import_lines(decl_source)
                .any(|import| import.trim() == super::qualified_source_type_name(&owner));
            return if is_classifier(&owner)
                && owner.kind != "typealias"
                && (top_level || lexical || imported)
            {
                FactStatus::Established(false)
            } else {
                FactStatus::Unknown
            };
        }
        FactStatus::Ambiguous(_) | FactStatus::Inferred(_) => return FactStatus::Unknown,
        FactStatus::Unknown => {}
    }

    // A simple `Nothing` spelling is the builtin only if no import or source
    // package can bind it elsewhere. Unknown stars are not namespace evidence.
    for import in decl_source.lines().filter_map(|line| {
        line.trim()
            .strip_prefix("import ")
            .map(|value| value.trim().trim_end_matches(';'))
    }) {
        let (path, alias) = import
            .split_once(" as ")
            .map(|(path, alias)| (path.trim(), Some(alias.trim())))
            .unwrap_or((import, None));
        if alias == Some("Nothing") {
            if path == "kotlin.Nothing" {
                continue;
            }
            return FactStatus::Unknown;
        }
        if path.ends_with(".*") {
            if !matches!(path, "kotlin.*" | "java.lang.*" | "java.util.*") {
                return FactStatus::Unknown;
            }
            continue;
        }
        if path.rsplit('.').next() == Some("Nothing") && path != "kotlin.Nothing" {
            return FactStatus::Unknown;
        }
    }
    FactStatus::Established(true)
}

fn potential_nothing_return(symbol: &SemanticSymbol) -> bool {
    matches!(&symbol.ty,
        FactStatus::Established(TypeRef { ty: KotlinType::Named { name, .. }, .. })
        | FactStatus::Inferred(TypeRef { ty: KotlinType::Named { name, .. }, .. })
        if matches!(name.as_str(), "Nothing" | "kotlin.Nothing"))
}

fn has_explicit_nothing_spelling(source: &str, symbol: &SemanticSymbol) -> bool {
    declared_return_spelling(source, symbol.location.start_byte, symbol.location.end_byte)
        .is_some_and(|spelling| {
            matches!(
                spelling.trim().trim_end_matches('?').trim(),
                "Nothing" | "kotlin.Nothing"
            )
        })
}

fn declared_return_spelling(source: &str, start: usize, end: usize) -> Option<String> {
    // This helper is only called after provider/source snapshot validation.
    // The tree is parsed here from the exact declaration source for cross-file
    // symbols; the caller's tree is intentionally not reused for other files.
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.start_byte() <= start
            && node.end_byte() >= end
            && matches!(node.kind(), "function_declaration" | "property_declaration")
        {
            let type_node = declaration_type_node(node)?;
            return Some(type_node.utf8_text(source.as_bytes()).ok()?.to_owned());
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    None
}

fn is_classifier(id: &SymbolId) -> bool {
    matches!(
        id.kind.as_str(),
        "class" | "interface" | "enum" | "typealias"
    )
}

fn unique_symbol<'a>(
    provider: &'a (impl SemanticProvider + ?Sized),
    id: &SymbolId,
) -> Option<&'a SemanticSymbol> {
    let mut found = provider
        .symbols_named(&id.name)
        .into_iter()
        .filter(|symbol| symbol.id == *id);
    let symbol = found.next()?;
    found.next().is_none().then_some(symbol)
}

fn call_callee<'tree>(source: &str, call: Node<'tree>) -> Option<(Node<'tree>, String)> {
    let callee = call.child_by_field_name("callee").or_else(|| {
        let mut cursor = call.walk();
        call.named_children(&mut cursor).find(|child| {
            matches!(
                child.kind(),
                "identifier" | "simple_identifier" | "navigation_expression"
            )
        })
    })?;
    let name_node = if callee.kind() == "navigation_expression" {
        let mut cursor = callee.walk();
        callee.named_children(&mut cursor).last()?
    } else {
        callee
    };
    let name = name_node.utf8_text(source.as_bytes()).ok()?.to_owned();
    Some((name_node, name))
}

fn is_planned_todo(
    node: Node<'_>,
    source: &str,
    file: &Path,
    hash: [u8; 32],
    plan: &TranslationPlan,
) -> bool {
    let location = location(source, file, hash, node);
    plan.todo_lowerings
        .iter()
        .any(|todo| todo.location == location)
}

fn location(source: &str, file: &Path, hash: [u8; 32], node: Node<'_>) -> SourceLocation {
    let _ = source;
    SourceLocation {
        file: file.to_path_buf(),
        snapshot_hash: hash,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    }
}

fn retain(
    plan: &mut TranslationPlan,
    index: usize,
    location: SourceLocation,
    message: String,
    kind: PreparationBlockerKind,
    allow_approximations: bool,
) {
    if allow_approximations {
        if !plan.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "A001"
                && diagnostic.location == location
                && diagnostic.message == message
        }) {
            plan.diagnostics.push(super::SemanticDiagnostic {
                code: "A001".into(),
                message,
                location,
            });
        }
        return;
    }
    let Some(decision) = plan.declarations.get_mut(index) else {
        return;
    };
    let reason = RetentionReason::PreparationBlocker { kind, message };
    if !decision.retention_reasons.contains(&reason) {
        decision.retention_reasons.push(reason);
    }
    decision.candidate_owner = BackendOwner::Kotlin;
}

struct OwnerIndex {
    spans: Vec<(usize, usize, usize)>,
}

impl OwnerIndex {
    fn new(plan: &TranslationPlan) -> Self {
        let mut spans = plan
            .declarations
            .iter()
            .enumerate()
            .map(|(index, decision)| (decision.id.start_byte, decision.id.end_byte, index))
            .collect::<Vec<_>>();
        spans.sort_unstable();
        Self { spans }
    }

    fn owner(&self, node: Node<'_>) -> Option<usize> {
        self.owner_span(node.start_byte(), node.end_byte())
    }

    fn owner_span(&self, start: usize, end: usize) -> Option<usize> {
        self.spans
            .iter()
            .filter(|(decl_start, decl_end, _)| *decl_start <= start && *decl_end >= end)
            .min_by_key(|(decl_start, decl_end, _)| decl_end - decl_start)
            .map(|(_, _, index)| *index)
    }
}
