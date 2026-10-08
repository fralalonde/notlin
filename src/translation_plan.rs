//! Snapshot-scoped, syntax-only planning for Kotlin declaration translation.
//!
//! The planner records source identity and provisional ownership. It does not
//! perform Kotlin name resolution or make claims about compiler semantics.

use crate::diagnostics::FileCoverage;
use crate::semantics::{SymbolId, symbol_id_for_node};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tree_sitter::Node;

/// Identity of one declaration in one parsed source snapshot. Byte spans make
/// same-name declarations and overloads distinct without relying on names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeclarationId {
    pub file: PathBuf,
    /// Content identity prevents equal byte spans in different revisions from
    /// being mistaken for the same declaration snapshot.
    pub source_hash: [u8; 32],
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeclarationKind {
    Class,
    Object,
    Function,
    Property,
    TypeAlias,
    Other,
}

/// The backend that owns a declaration at a given planning stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendOwner {
    Java,
    Kotlin,
}

/// A structured, source-local reason that a declaration must stay in Kotlin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetentionReason {
    SuspendConstruct {
        start_byte: usize,
        end_byte: usize,
    },
    ParseError {
        start_byte: usize,
        end_byte: usize,
    },
    PreparationBlocker {
        kind: PreparationBlockerKind,
        message: String,
    },
    OpaqueBoundary {
        start_byte: usize,
        end_byte: usize,
    },
    UnknownRequiredFact {
        name: String,
    },
    AmbiguousRequiredFact {
        name: String,
        candidates: usize,
    },
    OwnershipCycle {
        period: usize,
    },
}

impl RetentionReason {
    /// Stable category code suitable for diagnostics or reports.
    pub fn code(&self) -> &'static str {
        match self {
            Self::SuspendConstruct { .. } => "T001",
            Self::ParseError { .. } => "T002",
            Self::PreparationBlocker {
                kind: PreparationBlockerKind::SemanticLoss,
                ..
            } => "S003",
            Self::PreparationBlocker {
                kind: PreparationBlockerKind::UnresolvedAssumption,
                ..
            } => "S004",
            Self::PreparationBlocker {
                kind: PreparationBlockerKind::CompatibilityRule,
                ..
            } => "P001",
            Self::OpaqueBoundary { .. } => "T004",
            Self::UnknownRequiredFact { .. } => "S001",
            Self::AmbiguousRequiredFact { .. } => "S002",
            Self::OwnershipCycle { .. } => "P002",
        }
    }

    /// Human-readable explanation for this retention decision.
    pub fn message(&self) -> String {
        match self {
            Self::SuspendConstruct { .. } => {
                "contains a Kotlin suspend construct whose coroutine semantics are not represented in Java".to_string()
            }
            Self::ParseError { .. } => {
                "contains syntax errors and cannot be safely planned for translation".to_string()
            }
            Self::PreparationBlocker { message, .. } => message.clone(),
            Self::OpaqueBoundary { .. } => {
                "opaque annotated declaration boundary is retained conservatively".to_string()
            }
            Self::UnknownRequiredFact { name } => format!("required symbol `{name}` could not be resolved"),
            Self::AmbiguousRequiredFact { name, candidates } => format!("required symbol `{name}` has {candidates} possible declarations"),
            Self::OwnershipCycle { period } => format!(
                "workspace ownership remained cyclic across {period} probe states; retained conservatively in Kotlin"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreparationBlockerKind {
    SemanticLoss,
    UnresolvedAssumption,
    CompatibilityRule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreparationOutcome {
    /// The preparation phase produced the declaration in Java.
    Prepared,
    /// The declaration remains owned by Kotlin.
    Retained,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationDecision {
    pub id: DeclarationId,
    /// Stable semantic identity for this declaration header, independent of its body.
    pub symbol_id: SymbolId,
    /// Informational only; never used as identity.
    pub qualified_name: Option<String>,
    pub kind: DeclarationKind,
    /// Provisional preflight ownership before legacy lowering runs.
    pub candidate_owner: BackendOwner,
    pub retention_reasons: Vec<RetentionReason>,
    /// Set only after reconciling with legacy lowering coverage.
    pub preparation_outcome: Option<PreparationOutcome>,
    pub final_owner: Option<BackendOwner>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationPlan {
    pub source_hash: [u8; 32],
    pub declarations: Vec<DeclarationDecision>,
    pub bridges: Vec<PlannedBridge>,
    pub repairs: Vec<PlannedRepair>,
    pub outputs: Vec<PlannedOutput>,
    pub source_edits: Vec<PlannedSourceEdit>,
    pub provenance: Vec<crate::semantics::OriginMap>,
    pub dependencies: Vec<SymbolDependency>,
    pub type_facts: Vec<(
        crate::semantics::SourceLocation,
        crate::semantics::FactStatus<crate::semantics::TypeRef>,
    )>,
    pub diagnostics: Vec<crate::semantics::SemanticDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolDependency {
    pub from: SymbolId,
    pub spelling: String,
    pub resolution: crate::semantics::FactStatus<SymbolId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedBridge {
    pub id: SymbolId,
    pub origin: SymbolId,
    pub kind: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedRepair {
    pub target: SymbolId,
    pub kind: String,
    pub detail: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedOutput {
    pub path: PathBuf,
    pub owner: BackendOwner,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedSourceEdit {
    pub location: crate::semantics::SourceLocation,
    pub replacement: String,
    pub speculative: bool,
}

impl TranslationPlan {
    /// Losses known independently of Kotlin resolution belong to eligibility,
    /// before any candidate Java is prepared.
    pub fn plan_type_losses(&mut self, source: &str, root: Node, allow_approximations: bool) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let text = node.utf8_text(source.as_bytes()).unwrap_or("");
            let loss = if matches!(node.kind(), "user_type" | "type_identifier") {
                let base = text.split(['<', '?', '.']).next().unwrap_or(text).trim();
                match base {
                    "UInt" | "ULong" | "UShort" | "UByte" => Some(
                        "unsigned Kotlin types have signed Java arithmetic and a different JVM ABI",
                    ),
                    "KClass" => Some(
                        "KClass cannot be replaced by java.lang.Class without changing its reflection contract",
                    ),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(loss) = loss
                && let Some(decision) = self.declarations.iter_mut().find(|decision| {
                    node.start_byte() >= decision.id.start_byte
                        && node.end_byte() <= decision.id.end_byte
                })
            {
                let message = format!("semantic loss: {loss}");
                if allow_approximations {
                    self.diagnostics.push(crate::semantics::SemanticDiagnostic {
                        code: "A001".into(),
                        message,
                        location: crate::semantics::SourceLocation {
                            file: decision.id.file.clone(),
                            snapshot_hash: self.source_hash,
                            start_byte: node.start_byte(),
                            end_byte: node.end_byte(),
                        },
                    });
                } else {
                    let reason = RetentionReason::PreparationBlocker {
                        kind: PreparationBlockerKind::SemanticLoss,
                        message,
                    };
                    if !decision.retention_reasons.contains(&reason) {
                        decision.retention_reasons.push(reason);
                    }
                    decision.candidate_owner = BackendOwner::Kotlin;
                }
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }
    pub fn record_type_fact(
        &mut self,
        location: crate::semantics::SourceLocation,
        fact: crate::semantics::FactStatus<crate::semantics::TypeRef>,
    ) {
        self.type_facts.push((location, fact));
    }
    /// Resolve explicitly identified dependencies through a provider and make
    /// every non-established required fact a retention reason.
    pub fn require_symbols<P: crate::semantics::SemanticProvider>(
        &mut self,
        declaration: usize,
        provider: &P,
        names: impl IntoIterator<Item = String>,
    ) -> Vec<bool> {
        names
            .into_iter()
            .map(|name| {
                let fact = provider.resolve(&name);
                self.require_symbol(declaration, name, fact)
            })
            .collect()
    }

    /// Record a symbol reference that must resolve for Java ownership. Anything
    /// except an established binding conservatively retains the source owner.
    pub fn require_symbol(
        &mut self,
        declaration: usize,
        spelling: impl Into<String>,
        resolution: crate::semantics::FactStatus<SymbolId>,
    ) -> bool {
        let Some(decision) = self.declarations.get_mut(declaration) else {
            return false;
        };
        let spelling = spelling.into();
        self.dependencies.push(SymbolDependency {
            from: decision.symbol_id.clone(),
            spelling: spelling.clone(),
            resolution: resolution.clone(),
        });
        match resolution {
            crate::semantics::FactStatus::Established(_) => true,
            crate::semantics::FactStatus::Ambiguous(candidates) => {
                decision
                    .retention_reasons
                    .push(RetentionReason::AmbiguousRequiredFact {
                        name: spelling,
                        candidates: candidates.len(),
                    });
                decision.candidate_owner = BackendOwner::Kotlin;
                false
            }
            crate::semantics::FactStatus::Unknown | crate::semantics::FactStatus::Inferred(_) => {
                decision
                    .retention_reasons
                    .push(RetentionReason::UnknownRequiredFact { name: spelling });
                decision.candidate_owner = BackendOwner::Kotlin;
                false
            }
        }
    }

    /// Add a generated bridge and the explicit mapping back to its declaration.
    pub fn record_generated_bridge(
        &mut self,
        origin: &SymbolId,
        kind: impl Into<String>,
        reason: impl Into<String>,
    ) -> SymbolId {
        let kind = kind.into();
        let reason = reason.into();
        let id = SymbolId::generated(origin, &kind);
        self.bridges.push(PlannedBridge {
            id: id.clone(),
            origin: origin.clone(),
            kind,
        });
        self.provenance.push(crate::semantics::OriginMap {
            generated: id.clone(),
            origin: origin.clone(),
            reason,
        });
        id
    }

    /// Reasons for the top-level declaration whose node begins at this byte.
    pub fn retain_reasons_at(&self, start_byte: usize) -> Option<&[RetentionReason]> {
        self.declarations
            .iter()
            .find(|decision| decision.id.start_byte == start_byte)
            .map(|decision| decision.retention_reasons.as_slice())
    }

    /// Record prepared Java output coverage using exact declaration spans.
    pub fn reconcile(&mut self, source: &str, coverage: &FileCoverage) -> Result<(), &'static str> {
        if *blake3::hash(source.as_bytes()).as_bytes() != self.source_hash {
            return Err("translation plan belongs to a different source snapshot");
        }
        for decision in &mut self.declarations {
            let semantic_block = decision.retention_reasons.iter().any(|reason| {
                matches!(
                    reason,
                    RetentionReason::UnknownRequiredFact { .. }
                        | RetentionReason::AmbiguousRequiredFact { .. }
                        | RetentionReason::OwnershipCycle { .. }
                )
            });
            let translated = !semantic_block
                && coverage
                    .translated_spans
                    .contains(&(decision.id.start_byte, decision.id.end_byte));
            let outcome = if translated {
                PreparationOutcome::Prepared
            } else {
                PreparationOutcome::Retained
            };
            decision.preparation_outcome = Some(outcome);
            decision
                .retention_reasons
                .retain(|reason| !matches!(reason, RetentionReason::PreparationBlocker { .. }));
            decision.final_owner = Some(if translated {
                BackendOwner::Java
            } else {
                if decision.retention_reasons.is_empty() {
                    let blockers: Vec<_> = coverage
                        .blockers
                        .iter()
                        .filter(|(offset, _)| {
                            *offset >= decision.id.start_byte && *offset <= decision.id.end_byte
                        })
                        .map(|(_, message)| message.as_str())
                        .collect();
                    let message = if blockers.is_empty() {
                        "preparation retained this declaration without a classified blocker"
                            .to_string()
                    } else {
                        format!(
                            "preparation retained this declaration: {}",
                            blockers.join("; ")
                        )
                    };
                    let lower = message.to_ascii_lowercase();
                    let kind = if [
                        "unresolved",
                        "unknown",
                        "ambiguous",
                        "not found",
                        "cannot resolve",
                        "missing",
                    ]
                    .iter()
                    .any(|needle| lower.contains(needle))
                    {
                        PreparationBlockerKind::UnresolvedAssumption
                    } else if [
                        "unsupported",
                        "cannot preserve",
                        "semantic loss",
                        "incompatible",
                        "coroutine",
                    ]
                    .iter()
                    .any(|needle| lower.contains(needle))
                    {
                        PreparationBlockerKind::SemanticLoss
                    } else {
                        PreparationBlockerKind::CompatibilityRule
                    };
                    decision
                        .retention_reasons
                        .push(RetentionReason::PreparationBlocker { kind, message });
                }
                BackendOwner::Kotlin
            });
        }
        Ok(())
    }
}

/// Build a syntax-only plan for top-level declaration boundaries.
pub fn analyze(source: &str, root: Node, file: &Path) -> TranslationPlan {
    let source_hash = *blake3::hash(source.as_bytes()).as_bytes();
    let mut plan = TranslationPlan {
        source_hash,
        declarations: Vec::new(),
        bridges: Vec::new(),
        repairs: Vec::new(),
        outputs: Vec::new(),
        source_edits: Vec::new(),
        provenance: Vec::new(),
        dependencies: Vec::new(),
        type_facts: Vec::new(),
        diagnostics: Vec::new(),
    };
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        if !is_declaration_boundary(node.kind()) && !is_opaque_annotated_interface(node, source) {
            continue;
        }
        let mut reasons = Vec::new();
        if has_unclassified_parse_error(node, source) {
            reasons.push(RetentionReason::ParseError {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            });
        }
        if is_opaque_annotated_interface(node, source) {
            reasons.push(RetentionReason::OpaqueBoundary {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            });
        }
        if let Some((start_byte, end_byte)) = suspend_token(node, source) {
            reasons.push(RetentionReason::SuspendConstruct {
                start_byte,
                end_byte,
            });
        }
        let candidate_owner = if reasons.is_empty() {
            BackendOwner::Java
        } else {
            BackendOwner::Kotlin
        };
        plan.declarations.push(DeclarationDecision {
            id: DeclarationId {
                file: file.to_path_buf(),
                source_hash,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            },
            symbol_id: symbol_id(root, node, source, file),
            qualified_name: qualified_name(root, node, source),
            kind: declaration_kind(node.kind()),
            candidate_owner,
            retention_reasons: reasons,
            preparation_outcome: None,
            final_owner: None,
        });
    }
    let mut identities = std::collections::BTreeMap::<SymbolId, Vec<usize>>::new();
    for (i, decision) in plan.declarations.iter().enumerate() {
        identities
            .entry(decision.symbol_id.clone())
            .or_default()
            .push(i);
    }
    for (id, indices) in identities.into_iter().filter(|(_, v)| v.len() > 1) {
        let count = indices.len();
        for i in indices {
            let decision = &mut plan.declarations[i];
            decision.candidate_owner = BackendOwner::Kotlin;
            decision
                .retention_reasons
                .push(RetentionReason::AmbiguousRequiredFact {
                    name: id.name.clone(),
                    candidates: count,
                });
        }
    }
    plan
}

fn symbol_id(_root: Node, node: Node, source: &str, file: &Path) -> SymbolId {
    symbol_id_for_node(source, node, file)
}

fn is_declaration_boundary(kind: &str) -> bool {
    matches!(
        kind,
        "class_declaration"
            | "object_declaration"
            | "function_declaration"
            | "property_declaration"
            | "type_alias"
            | "ERROR"
    )
}

/// Tree-sitter's Kotlin grammar can insert this separator after a valid
/// expression-bodied member at the end of a class. It is recovery metadata,
/// not evidence that the declaration is unsafe. Other ERROR or missing nodes
/// remain blockers.
fn has_unclassified_parse_error(node: Node, source: &str) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.is_error() {
            if !is_interface_property_recovery(current, node, source) {
                return true;
            }
            continue;
        }
        if current.is_missing() && current.kind() != "_class_member_semi" {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.children(&mut cursor));
    }
    false
}

/// This grammar version represents a valid abstract interface property as an
/// ERROR when the member's nullable generic type follows a type parameter
/// (for example `interface Has<T> { val item: T? }`). Accept only that narrow
/// shape; malformed properties and errors in other declarations still block.
fn is_interface_property_recovery(error: Node, declaration: Node, source: &str) -> bool {
    if declaration.kind() != "class_declaration"
        || !declaration
            .utf8_text(source.as_bytes())
            .ok()
            .is_some_and(|text| text.trim_start().starts_with("interface "))
    {
        return false;
    }
    let Some(body) = error.parent() else {
        return false;
    };
    let Some(owner) = body.parent() else {
        return false;
    };
    if !matches!(body.kind(), "class_body" | "enum_class_body")
        || owner.start_byte() != declaration.start_byte()
        || owner.end_byte() != declaration.end_byte()
    {
        return false;
    }
    let Ok(text) = error.utf8_text(source.as_bytes()) else {
        return false;
    };
    let text = text.trim();
    let Some(rest) = text
        .strip_prefix("val ")
        .or_else(|| text.strip_prefix("var "))
    else {
        return false;
    };
    let Some((name, ty)) = rest.split_once(':') else {
        return false;
    };
    let name = name.trim();
    let ty = ty.trim();
    let mut type_parameters = Vec::new();
    let mut stack = vec![declaration];
    while let Some(current) = stack.pop() {
        if current.kind() == "type_parameter" {
            let mut cursor = current.walk();
            if let Some(identifier) = current
                .named_children(&mut cursor)
                .find(|child| child.kind() == "identifier")
                && let Ok(name) = identifier.utf8_text(source.as_bytes())
            {
                type_parameters.push(name.to_string());
            }
            continue;
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
    let nullable_parameter = ty.strip_suffix('?').unwrap_or_default();
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && name
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && !nullable_parameter.is_empty()
        && nullable_parameter
            .chars()
            .next()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && nullable_parameter
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && type_parameters
            .iter()
            .any(|parameter| parameter == nullable_parameter)
}

/// A parser-recovery `annotated_expression` is a declaration boundary only
/// when it actually owns an interface token. Ordinary annotation wrappers do
/// not become independent translation units.
fn is_opaque_annotated_interface(node: Node, source: &str) -> bool {
    if node.kind() != "annotated_expression" {
        return false;
    }
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind().contains("comment") || current.kind().contains("string") {
            continue;
        }
        if current.child_count() == 0
            && current.utf8_text(source.as_bytes()).ok() == Some("interface")
        {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.children(&mut cursor));
    }
    false
}

fn declaration_kind(kind: &str) -> DeclarationKind {
    match kind {
        "class_declaration" => DeclarationKind::Class,
        "object_declaration" => DeclarationKind::Object,
        "function_declaration" => DeclarationKind::Function,
        "property_declaration" => DeclarationKind::Property,
        "type_alias" => DeclarationKind::TypeAlias,
        _ => DeclarationKind::Other,
    }
}

fn declaration_name(node: Node, source: &str) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        return name.utf8_text(source.as_bytes()).ok().map(str::to_string);
    }
    if node.kind() == "property_declaration" {
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            if current.kind() == "variable_declaration" {
                if let Some(name) = current.child_by_field_name("name") {
                    return name.utf8_text(source.as_bytes()).ok().map(str::to_string);
                }
                let mut cursor = current.walk();
                if let Some(name) = current
                    .named_children(&mut cursor)
                    .find(|child| matches!(child.kind(), "simple_identifier" | "identifier"))
                {
                    return name.utf8_text(source.as_bytes()).ok().map(str::to_string);
                }
            }
            let mut cursor = current.walk();
            stack.extend(current.named_children(&mut cursor));
        }
    }
    None
}

fn qualified_name(root: Node, declaration: Node, source: &str) -> Option<String> {
    let name = declaration_name(declaration, source)?;
    let mut cursor = root.walk();
    let package = root
        .named_children(&mut cursor)
        .find(|child| child.kind() == "package_header")
        .and_then(|node| node.utf8_text(source.as_bytes()).ok())
        .map(|text| {
            text.trim()
                .strip_prefix("package")
                .unwrap_or(text.trim())
                .trim()
        })
        .filter(|text| !text.is_empty());
    Some(match package {
        Some(package) => format!("{package}.{name}"),
        None => name,
    })
}

/// Locate the `suspend` keyword token. Literal string text is ignored, while
/// executable interpolation expressions remain eligible for inspection.
fn suspend_token(node: Node, source: &str) -> Option<(usize, usize)> {
    let mut stack = vec![(node, false)];
    while let Some((current, inside_interpolation)) = stack.pop() {
        let kind = current.kind();
        if kind.contains("comment") || kind.contains("character") {
            continue;
        }
        let in_interpolation = inside_interpolation
            || kind.contains("interpolation")
            || kind.contains("template_expression")
            || kind.contains("template_entry");
        if (kind == "suspend" || kind == "function_modifier")
            && current.child_count() == 0
            && current.utf8_text(source.as_bytes()).ok() == Some("suspend")
        {
            return Some((current.start_byte(), current.end_byte()));
        }
        if kind.contains("string") && !in_interpolation {
            let mut cursor = current.walk();
            for child in current.children(&mut cursor) {
                let child_kind = child.kind();
                if child_kind.contains("interpolation")
                    || child_kind.contains("template_expression")
                    || child_kind.contains("template_entry")
                {
                    stack.push((child, true));
                }
            }
            continue;
        }
        let mut cursor = current.walk();
        stack.extend(
            current
                .children(&mut cursor)
                .map(|child| (child, in_interpolation)),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Tree;

    fn parse(source: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .expect("Kotlin grammar loads");
        parser.parse(source, None).expect("parse succeeds")
    }

    #[test]
    fn overload_and_same_name_identities_use_spans() {
        let source = "fun convert(x: Int) = x\nfun convert(x: String) = x\nclass convert\n";
        let tree = parse(source);
        let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        assert_eq!(plan.declarations.len(), 3);
        assert_eq!(
            plan.declarations[0].qualified_name.as_deref(),
            Some("convert")
        );
        assert_eq!(
            plan.declarations[1].qualified_name.as_deref(),
            Some("convert")
        );
        assert_ne!(plan.declarations[0].id, plan.declarations[1].id);
        assert_ne!(plan.declarations[0].id, plan.declarations[2].id);
    }

    #[test]
    fn suspend_classifier_ignores_strings_and_comments() {
        let source = "// suspend fun nope()\nfun ordinary() = \"suspend\"\n";
        let tree = parse(source);
        let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        assert!(
            plan.declarations
                .iter()
                .all(|d| d.retention_reasons.is_empty())
        );
    }

    #[test]
    fn suspend_function_type_parameter_retains_function() {
        let source = "fun register(block: suspend () -> Unit) {}\n";
        let tree = parse(source);
        let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        assert_eq!(plan.declarations[0].candidate_owner, BackendOwner::Kotlin);
        assert!(matches!(
            plan.declarations[0].retention_reasons.first(),
            Some(RetentionReason::SuspendConstruct { .. })
        ));
    }

    #[test]
    fn suspend_lambda_retains_top_level_property() {
        let source = "val work: suspend () -> Int = { 42 }\n";
        let tree = parse(source);
        let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        assert_eq!(plan.declarations.len(), 1);
        assert_eq!(plan.declarations[0].kind, DeclarationKind::Property);
        assert_eq!(plan.declarations[0].candidate_owner, BackendOwner::Kotlin);
        assert!(matches!(
            plan.declarations[0].retention_reasons.first(),
            Some(RetentionReason::SuspendConstruct { .. })
        ));
    }

    #[test]
    fn executable_string_interpolation_is_scanned() {
        let source = "fun work(value: Any) = \"value: ${value is suspend () -> Unit}\"\n";
        let tree = parse(source);
        let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        assert_eq!(plan.declarations[0].candidate_owner, BackendOwner::Kotlin);
        assert!(matches!(
            plan.declarations[0].retention_reasons.first(),
            Some(RetentionReason::SuspendConstruct { .. })
        ));
    }

    #[test]
    fn suspend_member_retains_containing_class() {
        let source = "class Worker {\n suspend fun run() {}\n}\n";
        let tree = parse(source);
        let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        assert_eq!(plan.declarations.len(), 1);
        assert_eq!(plan.declarations[0].kind, DeclarationKind::Class);
        assert_eq!(plan.declarations[0].candidate_owner, BackendOwner::Kotlin);
        assert!(matches!(
            plan.declarations[0].retention_reasons.as_slice(),
            [RetentionReason::SuspendConstruct { .. }]
        ));
    }

    #[test]
    fn ordinary_syntax_is_a_java_candidate() {
        let source = "class Widget {\n fun value() = 1\n}\nfun top() = 2\n";
        let tree = parse(source);
        let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        assert_eq!(plan.declarations.len(), 2);
        assert!(plan.declarations.iter().all(|d| {
            d.candidate_owner == BackendOwner::Java && d.retention_reasons.is_empty()
        }));
    }

    #[test]
    fn regression_inline_expression_body_in_data_class_is_eligible() {
        let source = "data class Session(val authenticated: Boolean) { fun isAuthenticated(): Boolean = authenticated }\n";
        let tree = parse(source);
        let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        assert_eq!(plan.declarations[0].candidate_owner, BackendOwner::Java);
    }

    #[test]
    fn nullable_generic_property_boundary_is_eligible() {
        let source = "package selected.nullablegeneric\nclass Item\ninterface Has<T> { val item: T? }\nclass Impl(override val item: Item) : Has<Item?>\n";
        let tree = parse(source);
        let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        assert!(
            plan.declarations
                .iter()
                .all(|decision| decision.candidate_owner == BackendOwner::Java)
        );
    }

    #[test]
    fn interface_property_recovery_does_not_hide_unrelated_errors() {
        for source in [
            "interface Has<T> { val item: T?? }\n",
            "interface Has<T> { val item T? }\n",
            "interface Has<T> { val item: = }\n",
        ] {
            let tree = parse(source);
            let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
            assert!(
                plan.declarations
                    .iter()
                    .all(|decision| decision.candidate_owner == BackendOwner::Kotlin),
                "malformed interface property should remain a blocker: {source}"
            );
        }
    }

    #[test]
    fn parse_error_retains_only_its_top_level_boundary() {
        let source = "fun broken() { val x = }\nfun fine() = 2\n";
        let tree = parse(source);
        let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        assert_eq!(plan.declarations.len(), 2);
        assert_eq!(plan.declarations[0].candidate_owner, BackendOwner::Kotlin);
        assert!(matches!(
            plan.declarations[0].retention_reasons.as_slice(),
            [RetentionReason::ParseError { .. }]
        ));
        assert_eq!(plan.declarations[1].candidate_owner, BackendOwner::Java);
    }

    #[test]
    fn package_and_property_names_are_informational() {
        let source = "package sample.pkg\nval answer = 42\n";
        let tree = parse(source);
        let plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        assert_eq!(
            plan.declarations[0].qualified_name.as_deref(),
            Some("sample.pkg.answer")
        );
    }

    #[test]
    fn reconcile_overloads_by_span_and_is_idempotent() {
        let source = "fun convert(x: Int) = x\nfun convert(x: String) = x\n";
        let tree = parse(source);
        let mut plan = analyze(source, tree.root_node(), Path::new("sample.kt"));
        let spans = plan
            .declarations
            .iter()
            .map(|d| (d.id.start_byte, d.id.end_byte))
            .collect();
        let coverage = FileCoverage {
            translated_spans: spans,
            ..FileCoverage::default()
        };
        plan.reconcile(source, &coverage).unwrap();
        let once = plan.clone();
        plan.reconcile(source, &coverage).unwrap();
        assert_eq!(plan, once);
        assert!(plan.declarations.iter().all(|d| {
            d.final_owner == Some(BackendOwner::Java)
                && d.preparation_outcome == Some(PreparationOutcome::Prepared)
        }));
    }

    #[test]
    fn equal_spans_in_different_revisions_have_different_identity() {
        let first = "fun value() = 1\n";
        let second = "fun value() = 2\n";
        let first_tree = parse(first);
        let second_tree = parse(second);
        let file = Path::new("revision.kt");
        let mut first_plan = analyze(first, first_tree.root_node(), file);
        let second_plan = analyze(second, second_tree.root_node(), file);
        assert_ne!(
            first_plan.declarations[0].id,
            second_plan.declarations[0].id
        );
        let before = first_plan.clone();
        assert!(
            first_plan
                .reconcile(second, &FileCoverage::default())
                .is_err()
        );
        assert_eq!(
            first_plan, before,
            "mismatch must leave ownership unchanged"
        );
    }
}
