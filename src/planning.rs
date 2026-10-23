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
    pub owners: Vec<SymbolId>,
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

/// Pre-write invariants over the plan's signed call receipts.
///
/// `parse_java` proves syntax, not agreement: it cannot notice that a receipt
/// belongs to different source bytes, that a receipt sits outside every
/// declaration the plan owns (so nothing could ever consume it), or that a call
/// span carries two different contracts. Those are plan/emitter disagreements,
/// and the honest answer is a controlled error before any file is written.
///
/// Emission *consumption* (a receipt that planning required and emission never
/// read) is checked where it is observable — in `lower_with_plan`, against the
/// unit's translated spans — and reaches this function as a preparation
/// failure.
fn ensure_call_receipt_invariants(translation: &TranslationPlan) -> Result<(), AcceptedPlanError> {
    let fail = |message: String, start_byte: usize, end_byte: usize| AcceptedPlanError {
        path: "translation plan".to_string(),
        error: JavaIrError {
            message,
            start_byte,
            end_byte,
        },
    };
    // A recorder conflict is the first thing to refuse: the receipt it refused
    // could not be installed, and continuing would let the declaration fall back
    // to an implicit Legacy mode with no contract at all.
    if let Some(conflict) = translation.receipt_conflicts.first() {
        return Err(fail(
            conflict.message.clone(),
            conflict.start_byte,
            conflict.end_byte,
        ));
    }
    let mut spans = std::collections::BTreeSet::new();
    for receipt in &translation.call_receipts {
        if receipt.location.snapshot_hash != translation.source_hash
            || receipt.selector.snapshot_hash != translation.source_hash
        {
            return Err(fail(
                format!(
                    "call receipt for `{}` belongs to a different source snapshot than the plan",
                    receipt.callee.name
                ),
                receipt.location.start_byte,
                receipt.location.end_byte,
            ));
        }
        if !spans.insert((receipt.location.start_byte, receipt.location.end_byte)) {
            return Err(fail(
                format!(
                    "call span carries more than one lowering receipt for `{}`",
                    receipt.callee.name
                ),
                receipt.location.start_byte,
                receipt.location.end_byte,
            ));
        }
        // Selector and receiver are part of the same signed site: a receipt
        // whose selector or receiver lies outside its own call span cannot be
        // the contract for that call.
        let selector_inside = receipt.location.start_byte <= receipt.selector.start_byte
            && receipt.selector.end_byte <= receipt.location.end_byte;
        let receiver_inside = receipt.receiver.as_ref().is_none_or(|receiver| {
            receipt.location.start_byte <= receiver.start_byte
                && receiver.end_byte <= receipt.location.end_byte
                && receiver.file == receipt.location.file
                && receiver.snapshot_hash == translation.source_hash
        });
        if !selector_inside || !receiver_inside || receipt.selector.file != receipt.location.file {
            return Err(fail(
                format!(
                    "call receipt for `{}` has a selector or receiver outside its own call span",
                    receipt.callee.name
                ),
                receipt.location.start_byte,
                receipt.location.end_byte,
            ));
        }
        let owned = translation.declarations.iter().any(|decision| {
            decision.id.start_byte <= receipt.location.start_byte
                && receipt.location.end_byte <= decision.id.end_byte
                && decision.id.file == receipt.location.file
        });
        if !owned {
            return Err(fail(
                format!(
                    "call receipt for `{}` is outside every planned declaration, so no \
                     declaration contract can consume it",
                    receipt.callee.name
                ),
                receipt.location.start_byte,
                receipt.location.end_byte,
            ));
        }
        if receipt.callee.owner.is_empty()
            || receipt.callee.name.is_empty()
            || !receipt.callee.descriptor.starts_with('(')
        {
            return Err(fail(
                format!(
                    "call receipt for `{}` has no physical callee identity",
                    receipt.callee.name
                ),
                receipt.location.start_byte,
                receipt.location.end_byte,
            ));
        }
    }
    Ok(())
}

/// The exact source snapshot a plan is accepted against.
struct SourceEvidence<'a> {
    source: &'a str,
    tree: &'a tree_sitter::Tree,
    annots: crate::transpiler::types::AnnotationSet,
}

/// Re-validate every recorded receipt against the exact source tree.
///
/// This is the half the record-shape invariant cannot see: that the span still
/// denotes a call, that the selector text is the callee the receipt names, that
/// the receiver is the expression it proved, and that the argument count and
/// the physical descriptor agree with the recorded mapping and outcome. Any
/// disagreement is a controlled pre-write error — never a best-effort file.
fn ensure_receipts_match_source(
    translation: &TranslationPlan,
    source: &str,
    tree: &tree_sitter::Tree,
) -> Result<(), AcceptedPlanError> {
    let fail = |message: String, start_byte: usize, end_byte: usize| AcceptedPlanError {
        path: "translation plan".to_string(),
        error: JavaIrError {
            message,
            start_byte,
            end_byte,
        },
    };
    if *blake3::hash(source.as_bytes()).as_bytes() != translation.source_hash {
        return Err(fail(
            "the plan was built for different source bytes than the tree it is validated against"
                .to_string(),
            0,
            source.len(),
        ));
    }
    for receipt in &translation.call_receipts {
        let start = receipt.location.start_byte;
        let end = receipt.location.end_byte;
        let Some(call) = node_at(tree.root_node(), start, end) else {
            return Err(fail(
                format!(
                    "call receipt for `{}` does not denote any node in the planned source",
                    receipt.callee.name
                ),
                start,
                end,
            ));
        };
        if !matches!(call.kind(), "call_expression" | "method_invocation") {
            return Err(fail(
                format!(
                    "call receipt for `{}` denotes `{}`, not a call",
                    receipt.callee.name,
                    call.kind()
                ),
                start,
                end,
            ));
        }
        let Some(callee) = crate::semantics::call_callee_node(call) else {
            return Err(fail(
                format!(
                    "call receipt for `{}` denotes a call without a callee",
                    receipt.callee.name
                ),
                start,
                end,
            ));
        };
        let selector = source
            .get(callee.start_byte()..callee.end_byte())
            .unwrap_or("");
        if selector.trim() != receipt.callee.name
            || callee.start_byte() != receipt.selector.start_byte
            || callee.end_byte() != receipt.selector.end_byte
        {
            return Err(fail(
                format!(
                    "call receipt names `{}` but the source selector is `{}`",
                    receipt.callee.name,
                    selector.trim()
                ),
                start,
                end,
            ));
        }
        match &receipt.receiver {
            Some(receiver) => {
                let receiver_node =
                    node_at(tree.root_node(), receiver.start_byte, receiver.end_byte);
                let text = source
                    .get(receiver.start_byte..receiver.end_byte)
                    .unwrap_or("")
                    .trim();
                if text.is_empty()
                    || !matches!(
                        receiver_node.map(|node| node.kind()),
                        Some("identifier" | "simple_identifier")
                    )
                {
                    return Err(fail(
                        format!(
                            "call receipt for `{}` proved a receiver that is not a simple \
                             source binding",
                            receipt.callee.name
                        ),
                        start,
                        end,
                    ));
                }
            }
            None => {
                if crate::semantics::explicit_receiver_node(call, callee).is_some() {
                    return Err(fail(
                        format!(
                            "call receipt for `{}` has no receiver although the source call has one",
                            receipt.callee.name
                        ),
                        start,
                        end,
                    ));
                }
            }
        }
        let arguments = direct_argument_count(call);
        let crate::semantics::ArgumentMapping::Exact(mapped) = receipt.arguments;
        if arguments != mapped {
            return Err(fail(
                format!(
                    "call receipt for `{}` maps {mapped} arguments but the source call passes \
                     {arguments}",
                    receipt.callee.name
                ),
                start,
                end,
            ));
        }
        let Some(arity) = crate::semantics::descriptor_arity(&receipt.callee.descriptor) else {
            return Err(fail(
                format!(
                    "call receipt for `{}` has an unparseable descriptor `{}`",
                    receipt.callee.name, receipt.callee.descriptor
                ),
                start,
                end,
            ));
        };
        if arity != mapped {
            return Err(fail(
                format!(
                    "call receipt for `{}` maps {mapped} arguments but its descriptor takes \
                     {arity}",
                    receipt.callee.name
                ),
                start,
                end,
            ));
        }
        if !crate::semantics::descriptor_result_matches(
            &receipt.outcome,
            &receipt.callee.descriptor,
        ) {
            return Err(fail(
                format!(
                    "call receipt for `{}` records an outcome the descriptor `{}` does not \
                     produce",
                    receipt.callee.name, receipt.callee.descriptor
                ),
                start,
                end,
            ));
        }
    }
    Ok(())
}

/// Re-prove every persisted declaration contract from the source tree.
///
/// Acceptance cannot trust a plan that merely *looks* consistent: a stale
/// plan, a changed tree or an emitter that stopped writing one proved slot all
/// have to fail here, before the first byte reaches disk.
fn ensure_required_contracts(
    translation: &TranslationPlan,
    source: &str,
    tree: &tree_sitter::Tree,
    semantic_provider: Option<&dyn SemanticProvider>,
    annots: crate::transpiler::types::AnnotationSet,
) -> Result<(), AcceptedPlanError> {
    let fail = |message: String, start_byte: usize, end_byte: usize| AcceptedPlanError {
        path: "translation plan".to_string(),
        error: JavaIrError {
            message,
            start_byte,
            end_byte,
        },
    };
    let bindings = crate::semantics::TypeBindings::build(source, tree.root_node());
    for decision in &translation.declarations {
        if decision.final_owner != Some(crate::translation_plan::BackendOwner::Java) {
            continue;
        }
        if decision.contract_retained {
            return Err(fail(
                format!(
                    "declaration `{}` was refused by its lowering contract but is marked \
                     Java-owned",
                    decision.symbol_id.name
                ),
                decision.id.start_byte,
                decision.id.end_byte,
            ));
        }
        let Some(contract) = &decision.lowering_contract else {
            if decision.required_mode {
                return Err(fail(
                    format!(
                        "declaration `{}` is in Required mode with no persisted ABI contract",
                        decision.symbol_id.name
                    ),
                    decision.id.start_byte,
                    decision.id.end_byte,
                ));
            }
            continue;
        };
        let provider = semantic_provider.ok_or_else(|| {
            fail(
                format!(
                    "declaration `{}` has a persisted ABI contract that cannot be re-proved \
                     without a semantic provider",
                    decision.symbol_id.name
                ),
                decision.id.start_byte,
                decision.id.end_byte,
            )
        })?;
        let Some(unit) = node_at(
            tree.root_node(),
            decision.id.start_byte,
            decision.id.end_byte,
        ) else {
            return Err(fail(
                format!(
                    "declaration `{}` has no source node at its planned span",
                    decision.symbol_id.name
                ),
                decision.id.start_byte,
                decision.id.end_byte,
            ));
        };
        let abi = crate::semantics::declaration_abi(
            source,
            unit,
            &bindings,
            provider,
            annots,
            &translation.call_receipts,
        );
        let Some(replayed) = abi.contract() else {
            return Err(fail(
                format!(
                    "declaration `{}` no longer proves its persisted ABI contract (a slot or \
                     operation is now unproved)",
                    decision.symbol_id.name
                ),
                decision.id.start_byte,
                decision.id.end_byte,
            ));
        };
        if replayed.fingerprints() != contract.fingerprints() {
            return Err(fail(
                format!(
                    "declaration `{}` renders a different Java ABI than the plan proved",
                    decision.symbol_id.name
                ),
                decision.id.start_byte,
                decision.id.end_byte,
            ));
        }
    }
    Ok(())
}

/// The deepest node whose span is exactly `start..end`.
///
/// Several nodes can share one span (`values.sorted()` is both a `value_argument`
/// and its inner `call_expression`), so the search keeps descending and prefers
/// the innermost exact match.
fn node_at(root: tree_sitter::Node<'_>, start: usize, end: usize) -> Option<tree_sitter::Node<'_>> {
    let mut best = None;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.start_byte() == start && node.end_byte() == end {
            best = Some(node);
        }
        if node.start_byte() <= start && end <= node.end_byte() {
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
    }
    best
}

/// Positional argument count of a call, without named arguments or lambdas
/// (those forms are refused by the receipt builder, so a mismatched count here
/// is a real disagreement).
fn direct_argument_count(call: tree_sitter::Node<'_>) -> usize {
    let mut count = 0;
    let mut cursor = call.walk();
    for child in call.named_children(&mut cursor) {
        if matches!(child.kind(), "value_arguments" | "arguments") {
            let mut inner = child.walk();
            count += child
                .named_children(&mut inner)
                .filter(|argument| argument.kind() == "value_argument")
                .count();
        }
    }
    count
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
    emitted_owners: Vec<SymbolId>,
}

impl AcceptedTranslationPlan {
    /// Whether this plan carries any lowering-contract evidence at all.
    fn has_contract_evidence(translation: &TranslationPlan) -> bool {
        !translation.call_receipts.is_empty()
            || translation.declarations.iter().any(|decision| {
                decision.required_mode
                    || decision.lowering_contract.is_some()
                    || decision.contract_retained
            })
    }

    /// Accept a plan that carries **no** lowering-contract evidence.
    ///
    /// A receipt-bearing or Required-mode plan must be accepted through
    /// [`Self::accept_with_source`]: its receipts and ABI contracts have to be
    /// re-proved against the exact source tree before any write, and this
    /// entry point deliberately has nothing to prove them against.
    pub fn accept(
        translation: TranslationPlan,
        candidates: Vec<PreparedJavaFile>,
        semantic_provider: Option<&dyn SemanticProvider>,
    ) -> Result<Self, AcceptedPlanError> {
        // A recorder conflict is refused first: it is the evidence that a
        // receipt could not be installed, and it does not depend on the tree.
        ensure_call_receipt_invariants(&translation)?;
        if Self::has_contract_evidence(&translation) {
            return Err(AcceptedPlanError {
                path: "translation plan".to_string(),
                error: JavaIrError {
                    message: "a plan carrying declaration lowering contracts must be accepted \
                              against its exact source tree"
                        .to_string(),
                    start_byte: 0,
                    end_byte: 0,
                },
            });
        }
        Self::accept_inner(translation, candidates, semantic_provider, None)
    }

    /// Accept a plan against the exact source snapshot it was planned from.
    ///
    /// This is the production pre-write gate. Java syntax checking below is
    /// only syntax; it cannot tell whether the plan and the emitted text agree
    /// about a call. These invariants can: they re-read the source tree and
    /// re-prove every receipt and every Required declaration's persisted ABI
    /// contract before a single byte reaches disk.
    pub fn accept_with_source(
        translation: TranslationPlan,
        candidates: Vec<PreparedJavaFile>,
        semantic_provider: Option<&dyn SemanticProvider>,
        source: &str,
        tree: &tree_sitter::Tree,
        annots: crate::transpiler::types::AnnotationSet,
    ) -> Result<Self, AcceptedPlanError> {
        Self::accept_inner(
            translation,
            candidates,
            semantic_provider,
            Some(SourceEvidence {
                source,
                tree,
                annots,
            }),
        )
    }

    fn accept_inner(
        mut translation: TranslationPlan,
        candidates: Vec<PreparedJavaFile>,
        semantic_provider: Option<&dyn SemanticProvider>,
        evidence: Option<SourceEvidence<'_>>,
    ) -> Result<Self, AcceptedPlanError> {
        ensure_call_receipt_invariants(&translation)?;
        // Record shape is not production agreement. Re-read the exact source
        // snapshot the plan was built from and prove that every receipt still
        // denotes its call, and that every Required declaration's persisted ABI
        // contract still holds for its source node.
        if let Some(evidence) = evidence {
            ensure_receipts_match_source(&translation, evidence.source, evidence.tree)?;
            ensure_required_contracts(
                &translation,
                evidence.source,
                evidence.tree,
                semantic_provider,
                evidence.annots,
            )?;
        }
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
        let emitted_owners = candidates
            .iter()
            .flat_map(|candidate| candidate.owners.iter().cloned())
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
            crate::eager_collection_helpers::attach_projection_targets(
                &mut unit,
                &owners,
                &translation.eager_collection_calls,
                &translation.container_calls,
            )
            .map_err(|error| AcceptedPlanError {
                path: path.clone(),
                error,
            })?;
            let helpers = crate::eager_collection_helpers::inject(
                &mut unit,
                &path,
                &owners,
                &translation.eager_collection_helpers,
            )
            .map_err(|error| AcceptedPlanError {
                path: path.clone(),
                error,
            })?;
            for helper in helpers {
                let kind = match helper.operation {
                    crate::semantics::EagerCollectionOperation::Map => "eager-collection-map",
                    crate::semantics::EagerCollectionOperation::FilterNot => {
                        "eager-collection-filter-not"
                    }
                };
                let bridge = crate::translation_plan::PlannedBridge {
                    id: helper.id.clone(),
                    origin: helper.origin.clone(),
                    kind: kind.into(),
                };
                if !translation.bridges.contains(&bridge) {
                    translation.bridges.push(bridge);
                }
                let origin = crate::semantics::OriginMap {
                    generated: helper.id,
                    origin: helper.origin,
                    reason: kind.into(),
                };
                if !translation.provenance.contains(&origin) {
                    translation.provenance.push(origin);
                }
            }
            assignments.push((path.clone(), owners.clone(), snapshot_hash));
            files.push(AcceptedJavaFile { path, unit, owners });
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
        translation
            .eager_collection_helpers
            .retain(|helper| emitted_owners.contains(&helper.origin));
        record_generated_declarations(&mut translation, &files, semantic_provider);
        if let Some(provider) = semantic_provider {
            for file in &files {
                for getter in crate::lombok_getter_contracts::collect(file, provider) {
                    if !translation
                        .bridges
                        .iter()
                        .any(|bridge| bridge.id == getter.target)
                    {
                        translation.record_generated_bridge(
                            &getter.origin,
                            format!("lombok:getter:{}()", getter.method_name),
                            "Lombok-generated property getter",
                        );
                    }
                    translation.generated_getters.push(getter);
                }
            }
        }
        Ok(Self {
            translation,
            files,
            emitted_owners,
        })
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
    pub fn emitted_owners(&self) -> &[SymbolId] {
        &self.emitted_owners
    }
    pub fn translation(&self) -> &TranslationPlan {
        &self.translation
    }
    pub(crate) fn rejected(translation: TranslationPlan) -> Self {
        Self {
            translation,
            files: Vec::new(),
            emitted_owners: Vec::new(),
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
        for file in &source_files {
            for symbol in provider.symbols_in_file(file) {
                source_symbol_map.insert(symbol.id.stable_key(), symbol.id.clone());
            }
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
                    .symbols_named(&receiver_type)
                    .into_iter()
                    .filter(|s| s.id.kind == "class")
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
        .symbols_named(&name)
        .into_iter()
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

#[cfg(test)]
mod receipt_invariant_tests {
    use super::*;
    use crate::semantics::{
        ArgumentMapping, Behavior, CallReceipt, CalleeIdentity, CheckedEffects, Dispatch,
        JavaOutcome, JavaRepresentation, Nullability, ReceiptEvidence, SourceLocation,
    };
    use crate::translation_plan::{
        BackendOwner, DeclarationDecision, DeclarationId, DeclarationKind, TranslationPlan,
    };
    use std::path::PathBuf;

    const HASH: [u8; 32] = [3u8; 32];

    fn location(start: usize, end: usize, hash: [u8; 32]) -> SourceLocation {
        SourceLocation {
            file: PathBuf::from("Fixture.kt"),
            snapshot_hash: hash,
            start_byte: start,
            end_byte: end,
        }
    }

    fn receipt(hash: [u8; 32]) -> CallReceipt {
        CallReceipt {
            location: location(20, 40, hash),
            selector: location(27, 33, hash),
            receiver: Some(location(20, 26, hash)),
            callee: CalleeIdentity {
                owner: "java.util.stream.Stream".into(),
                name: "sorted".into(),
                descriptor: "()Ljava/util/stream/Stream;".into(),
            },
            dispatch: Dispatch::Instance,
            arguments: ArgumentMapping::Exact(0),
            outcome: JavaOutcome::Value(JavaRepresentation::erased_reference(
                "java.util.stream.Stream",
                Nullability::Unknown,
            )),
            effects: CheckedEffects::None,
            behavior: Behavior::Exact,
            evidence: ReceiptEvidence::PhysicalMember,
        }
    }

    fn plan(declarations: Vec<DeclarationDecision>) -> TranslationPlan {
        TranslationPlan {
            source_hash: HASH,
            declarations,
            ..TranslationPlan::default()
        }
    }

    fn declaration(start: usize, end: usize) -> DeclarationDecision {
        DeclarationDecision {
            id: DeclarationId {
                file: PathBuf::from("Fixture.kt"),
                source_hash: HASH,
                start_byte: start,
                end_byte: end,
            },
            symbol_id: SymbolId {
                module: "fixture".into(),
                package: "probe".into(),
                file: PathBuf::from("Fixture.kt"),
                owner_path: Vec::new(),
                kind: "function".into(),
                name: "sorted".into(),
                receiver: None,
                parameters: Vec::new(),
            },
            qualified_name: None,
            kind: DeclarationKind::Function,
            candidate_owner: BackendOwner::Java,
            retention_reasons: Vec::new(),
            preparation_outcome: None,
            final_owner: None,
            required_mode: false,
            lowering_contract: None,
            contract_retained: false,
        }
    }

    // ---------------------------------------------------------------------
    // Source-tree acceptance: record shape is not production agreement.
    // ---------------------------------------------------------------------

    use crate::transpiler::types::AnnotationSet;

    fn kotlin_tree(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    fn find_call<'tree>(root: tree_sitter::Node<'tree>) -> tree_sitter::Node<'tree> {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" {
                return node;
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        panic!("fixture has no call");
    }

    const CALL_SOURCE: &str = "package p\n\nfun link(s: String) {\n    s.hashCode()\n}\n";

    /// A real plan + tree for `CALL_SOURCE`, with one synthetic receipt bound
    /// to the real call span.
    fn call_plan() -> (TranslationPlan, tree_sitter::Tree) {
        let tree = kotlin_tree(CALL_SOURCE);
        let file = PathBuf::from("Fixture.kt");
        let mut plan = crate::translation_plan::analyze(CALL_SOURCE, tree.root_node(), &file);
        let call = find_call(tree.root_node());
        let callee = crate::semantics::call_callee_node(call).unwrap();
        let hash = plan.source_hash;
        let receipt = CallReceipt {
            location: SourceLocation {
                file: file.clone(),
                snapshot_hash: hash,
                start_byte: call.start_byte(),
                end_byte: call.end_byte(),
            },
            selector: SourceLocation {
                file: file.clone(),
                snapshot_hash: hash,
                start_byte: callee.start_byte(),
                end_byte: callee.end_byte(),
            },
            receiver: Some(SourceLocation {
                file,
                snapshot_hash: hash,
                start_byte: callee.start_byte() - 2,
                end_byte: callee.start_byte() - 1,
            }),
            callee: CalleeIdentity {
                owner: "java.lang.Object".into(),
                name: "hashCode".into(),
                descriptor: "()I".into(),
            },
            dispatch: Dispatch::Instance,
            arguments: ArgumentMapping::Exact(0),
            outcome: JavaOutcome::Value(JavaRepresentation::primitive("int")),
            effects: CheckedEffects::None,
            behavior: Behavior::Exact,
            evidence: ReceiptEvidence::PhysicalMember,
        };
        plan.record_call_receipt(receipt)
            .expect("the production recorder installs one receipt per span");
        (plan, tree)
    }

    fn source_failure(plan: TranslationPlan, tree: &tree_sitter::Tree) -> String {
        AcceptedTranslationPlan::accept_with_source(
            plan,
            Vec::new(),
            None,
            CALL_SOURCE,
            tree,
            AnnotationSet::None,
        )
        .expect_err("validation must refuse")
        .error
        .message
    }

    #[test]
    fn a_valid_receipt_is_accepted_against_its_source_tree() {
        let (plan, tree) = call_plan();
        AcceptedTranslationPlan::accept_with_source(
            plan,
            Vec::new(),
            None,
            CALL_SOURCE,
            &tree,
            AnnotationSet::None,
        )
        .expect("the receipt denotes its call");
    }

    #[test]
    fn a_receipt_whose_span_is_not_a_call_is_refused() {
        let (mut plan, tree) = call_plan();
        // A block that *contains* the selector and receiver, but is not a call.
        let literal = {
            let mut stack = vec![tree.root_node()];
            let mut found = None;
            while let Some(node) = stack.pop() {
                if node.kind() == "block" {
                    found = Some(node);
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
            found.unwrap()
        };
        let receipt = &mut plan.call_receipts[0];
        receipt.location.start_byte = literal.start_byte();
        receipt.location.end_byte = literal.end_byte();
        receipt.selector.start_byte = literal.start_byte();
        receipt.selector.end_byte = literal.end_byte();
        let message = source_failure(plan, &tree);
        assert!(message.contains("not a call"), "{message}");
    }

    #[test]
    fn a_receipt_naming_another_selector_is_refused() {
        let (mut plan, tree) = call_plan();
        plan.call_receipts[0].callee.name = "toString".into();
        let message = source_failure(plan, &tree);
        assert!(
            message.contains("source selector"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn a_receipt_with_a_wrong_argument_mapping_is_refused() {
        let (mut plan, tree) = call_plan();
        plan.call_receipts[0].arguments = ArgumentMapping::Exact(1);
        let message = source_failure(plan, &tree);
        assert!(
            message.contains("maps 1 arguments"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn a_receipt_whose_descriptor_takes_another_arity_is_refused() {
        let (mut plan, tree) = call_plan();
        plan.call_receipts[0].callee.descriptor = "(I)I".into();
        let message = source_failure(plan, &tree);
        assert!(
            message.contains("descriptor takes 1"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn a_receipt_whose_outcome_contradicts_its_descriptor_is_refused() {
        let (mut plan, tree) = call_plan();
        plan.call_receipts[0].outcome = JavaOutcome::Void;
        let message = source_failure(plan, &tree);
        assert!(
            message.contains("outcome the descriptor"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn a_stale_source_plan_is_refused() {
        let (plan, tree) = call_plan();
        let other = "package p\n\nfun link(s: String) {\n    s.hashCode() // edited\n}\n";
        let error = AcceptedTranslationPlan::accept_with_source(
            plan,
            Vec::new(),
            None,
            other,
            &tree,
            AnnotationSet::None,
        )
        .expect_err("a plan for other bytes must not validate");
        assert!(
            error.error.message.contains("different source bytes"),
            "{error}"
        );
    }

    #[test]
    fn a_required_java_declaration_without_a_contract_is_refused() {
        let (mut plan, tree) = call_plan();
        plan.declarations[0].final_owner = Some(BackendOwner::Java);
        plan.declarations[0].required_mode = true;
        let message = source_failure(plan, &tree);
        assert!(message.contains("no persisted ABI contract"), "{message}");
    }

    #[test]
    fn a_contract_retained_java_declaration_is_refused() {
        let (mut plan, tree) = call_plan();
        plan.declarations[0].final_owner = Some(BackendOwner::Java);
        plan.declarations[0].contract_retained = true;
        let message = source_failure(plan, &tree);
        assert!(
            message.contains("refused by its lowering contract"),
            "{message}"
        );
    }

    #[test]
    fn a_persisted_contract_that_no_longer_renders_its_slot_is_refused() {
        let (mut plan, tree) = call_plan();
        // Build the real contract, then change one rendered slot.
        let unit = {
            let mut stack = vec![tree.root_node()];
            let mut found = None;
            while let Some(node) = stack.pop() {
                if node.kind() == "function_declaration" {
                    found = Some(node);
                    break;
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
            found.unwrap()
        };
        let bindings = crate::semantics::TypeBindings::build(CALL_SOURCE, tree.root_node());
        let provider = crate::semantics::SyntaxSemanticProvider::new([(
            PathBuf::from("Fixture.kt"),
            CALL_SOURCE.to_string(),
        )]);
        let abi = crate::semantics::declaration_abi(
            CALL_SOURCE,
            unit,
            &bindings,
            &provider,
            AnnotationSet::None,
            &plan.call_receipts,
        );
        let mut contract = abi.contract().expect("the fixture proves");
        contract.slots[0].emitted = "Object".into();
        plan.declarations[0].final_owner = Some(BackendOwner::Java);
        plan.declarations[0].required_mode = true;
        plan.declarations[0].lowering_contract = Some(contract);
        let message = AcceptedTranslationPlan::accept_with_source(
            plan,
            Vec::new(),
            Some(&provider as &dyn SemanticProvider),
            CALL_SOURCE,
            &tree,
            AnnotationSet::None,
        )
        .expect_err("a changed slot must refuse")
        .error
        .message;
        assert!(
            message.contains("renders a different Java ABI"),
            "{message}"
        );
    }

    #[test]
    fn a_plan_with_contract_evidence_cannot_use_the_source_less_gate() {
        let (plan, _tree) = call_plan();
        let error = AcceptedTranslationPlan::accept(plan, Vec::new(), None)
            .expect_err("a receipt-bearing plan needs its source tree");
        assert!(
            error
                .error
                .message
                .contains("against its exact source tree"),
            "{error}"
        );
    }

    /// Legacy plans (no receipt) must pass untouched: this is the helper-off /
    /// old-jar floor.
    #[test]
    fn a_plan_without_receipts_passes() {
        assert!(ensure_call_receipt_invariants(&plan(vec![declaration(0, 60)])).is_ok());
    }

    #[test]
    fn a_receipt_inside_its_declaration_passes() {
        let mut plan = plan(vec![declaration(0, 60)]);
        plan.call_receipts.push(receipt(HASH));
        assert!(ensure_call_receipt_invariants(&plan).is_ok());
    }

    /// A receipt no declaration owns could never be consumed by a declaration
    /// contract: refuse before anything is written.
    #[test]
    fn a_receipt_outside_every_declaration_is_refused() {
        let mut plan = plan(vec![declaration(0, 10)]);
        plan.call_receipts.push(receipt(HASH));
        let error = ensure_call_receipt_invariants(&plan).expect_err("must refuse");
        assert!(
            error
                .error
                .message
                .contains("outside every planned declaration"),
            "{error}"
        );
    }

    #[test]
    fn a_receipt_from_another_snapshot_is_refused() {
        let mut plan = plan(vec![declaration(0, 60)]);
        plan.call_receipts.push(receipt([9u8; 32]));
        let error = ensure_call_receipt_invariants(&plan).expect_err("must refuse");
        assert!(
            error.error.message.contains("different source snapshot"),
            "{error}"
        );
    }

    #[test]
    fn two_receipts_for_one_call_span_are_refused() {
        let mut plan = plan(vec![declaration(0, 60)]);
        plan.call_receipts.push(receipt(HASH));
        plan.call_receipts.push(receipt(HASH));
        let error = ensure_call_receipt_invariants(&plan).expect_err("must refuse");
        assert!(
            error
                .error
                .message
                .contains("more than one lowering receipt"),
            "{error}"
        );
    }

    /// `record_call_receipt` is the only writer during planning. A second
    /// receipt for the same span is not a safe ambiguity to resolve by "first
    /// wins": it is a recorded conflict that the pre-write invariant refuses,
    /// and no second contract is installed.
    #[test]
    fn recording_the_same_span_twice_is_a_refused_conflict() {
        let mut plan = plan(vec![declaration(0, 60)]);
        assert!(plan.record_call_receipt(receipt(HASH)).is_ok());
        let conflict = plan
            .record_call_receipt(receipt(HASH))
            .expect_err("a duplicate span must be refused, not silently dropped");
        assert!(
            conflict.message.contains("more than one lowering receipt"),
            "{conflict:?}"
        );
        assert_eq!(
            plan.call_receipts.len(),
            1,
            "the conflict must not install a second contract"
        );
        let refused = AcceptedTranslationPlan::accept(plan, Vec::new(), None)
            .expect_err("a recorded conflict must reach the pre-write gate");
        assert!(
            refused
                .error
                .message
                .contains("more than one lowering receipt"),
            "{refused}"
        );
    }

    /// A receipt whose snapshot does not match the plan is a conflict, not a
    /// dropped observation: dropping it would quietly return the declaration to
    /// Legacy mode with no receipt and no refusal.
    #[test]
    fn recording_a_receipt_for_another_snapshot_is_a_refused_conflict() {
        let mut plan = plan(vec![declaration(0, 60)]);
        let conflict = plan
            .record_call_receipt(receipt([9u8; 32]))
            .expect_err("a foreign snapshot must be refused, not silently dropped");
        assert!(
            conflict.message.contains("different source snapshot"),
            "{conflict:?}"
        );
        assert!(
            plan.call_receipts.is_empty(),
            "a stale contract must never be recorded"
        );
        let refused = AcceptedTranslationPlan::accept(plan, Vec::new(), None)
            .expect_err("a recorded conflict must reach the pre-write gate");
        assert!(
            refused.error.message.contains("different source snapshot"),
            "{refused}"
        );
    }
}
