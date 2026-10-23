//! Declaration-wide Java lowering contracts.
//!
//! A *receipt* is the one shared object three seams used to decide
//! independently: resolution decided eligibility, emission re-decided the
//! spelling from a name table, and type inference guessed the result from the
//! member name (`infer_member_type`'s "unknown ⇒ `Object`"). Nothing forced the
//! value an expression denotes to be the value its consumer assumes, which is
//! how a `Stream.sorted()` proof could unlock a declaration that also emitted
//! `String.trim()` (Kotlin strips U+00A0, Java does not) or `InputStream.read()`
//! (checked `IOException`, unhandled in Java).
//!
//! This module owns:
//!
//! * [`CallReceipt`] — a snapshot-bound rendering/evaluation/result/effect
//!   contract for exactly one call site, keyed by [`SourceLocation`] and *not*
//!   by callee `SymbolId`. `SymbolId`/`TypeRef` stay source ABI; a
//!   [`JavaRepresentation`] is physical output. A receipt needs both, which is
//!   why it records the physical descriptor next to the source span.
//! * [`SiteRecord`] / [`record_receipts`] / [`plan_contracts`] — the
//!   whole-declaration inventory. The co-emitted unit of a Kotlin file is its
//!   top-level declaration (a class carries every member; a top-level function
//!   is its own unit), which is exactly the granularity
//!   `TranslationPlan::declarations` and the emitter's preflight already use.
//!   Proposed receipts are recorded **once**, before either inventory reads
//!   them, and [`declaration_abi`] then makes one closed walk over the unit
//!   that proves every emitted signature slot *and* enumerates every executable
//!   operation, using the grammar's own node kinds ([`kind_policy`]) and
//!   refusing anything it does not prove. A refusal retains the whole unit
//!   before the first write, with or without `--allow-approximations`, and the
//!   persisted [`RequiredContract`] is what acceptance re-proves from the
//!   source tree.
//!
//! Emission agreement is mechanical, not aspirational: a slot's `emitted` text
//! is produced by the emitter's own renderers (`java_parameter_type`,
//! `java_return_type`, the readonly-parameter rewrite) with the run's
//! annotation set, so it cannot drift into a second spelling table; acceptance
//! re-proves every slot from the exact source bytes and fails the write on any
//! difference. The closed shape is deliberately small: a top-level function or
//! a plain class whose members are plain functions, `name: Type` parameters,
//! no type parameters, receivers, defaults, varargs, annotations, constructors,
//! fields, properties, initializers or supertypes. Every other shape is
//! refused by name instead of being approximated.
//! * [`UseContext`] — where the call's value goes. Slice 1 admits a proved
//!   value only where its payload cannot be observed: a discarded statement, an
//!   exit through a *bound* nullable `java.lang.Object` ABI, or an exactly
//!   matching JVM primitive. Slice 2 narrows one more, structurally: a declared
//!   `String?` whose binding is the non-generic `java.lang.String` and whose
//!   receipt's physical descriptor result is `Ljava/lang/String;`. An opaque
//!   parameterized payload
//!   (`Stream.sorted()` erasing to `java.util.stream.Stream`) is *not*
//!   interchangeable with a `Stream<Int>` exit just because erasures match; a
//!   primitive is not a reference and Java `void` is not Kotlin
//!   `Unit`-in-value.
//!
//! A declared type is proved, never spelled: an `Any`/`Int`/… slot is the
//! builtin only when no import alias, file declaration, type parameter,
//! wildcard import, workspace typealias or same-package declaration binds that
//! name. `typealias Any = Stream<Int>` makes `Any?` a `Stream`, and an unused
//! `unused: Int` under `typealias Int = String` is a `String` parameter — both
//! change the emitted descriptor, which is why a spelling scan is not evidence.
//!
//! Slice-1 boundary, deliberate and reported rather than papered over: the only
//! receipt builder today is the bounded physical instance-member proof in
//! [`super::java_member_calls`] (JDK owner, zero arguments, instance dispatch,
//! `checked_exceptions == Some([])`). Every other operation inside a
//! receipt-bearing declaration is a generic missing-proof refusal, not a claim
//! that the operation is unsupported in principle.

use super::*;
use serde::{Deserialize, Serialize};

/// The one reference type a nullable exit may name structurally.
///
/// `java.lang.String` is final and non-generic, so its erased descriptor and
/// its complete representation are the same fact. Every other reference stays
/// a source-ABI question this slice does not answer.
pub(crate) const STRING_REFERENCE: &str = "java.lang.String";

/// A Java type as the emitter must write it. Never a K2 `TypeRef`: a source
/// type is evidence about the Kotlin program, this is evidence about the
/// emitted Java program, and the two differ exactly where the old design went
/// wrong (`Int` vs `int`, `Stream<Int>` vs erased `Stream`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaRepresentation {
    /// Exactly the text the emitter writes for this type.
    pub erased: String,
    pub kind: ReprKind,
    pub nullable: Nullability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReprKind {
    /// A JVM primitive. It has no Java members: `int.toString()` is not a
    /// lowering gap, it is a category error.
    Primitive,
    /// A reference type whose type arguments this receipt does not know.
    ErasedReference,
    /// A reference type with its complete instantiated arguments. No slice-1
    /// builder produces this — the variant exists so the admission rule names
    /// the evidence it would need instead of pretending erasure suffices.
    ParameterizedReference(Vec<JavaRepresentation>),
    /// A Java array. A distinct kind from a reference: `split(..)` yields
    /// `String[]`, and `.stream()` on it needs its own receipt.
    Array(Box<JavaRepresentation>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Nullability {
    NonNull,
    Nullable,
    /// The physical declaration carries no recognized nullness annotation.
    Unknown,
}

impl JavaRepresentation {
    pub fn primitive(erased: &str) -> Self {
        Self {
            erased: erased.to_string(),
            kind: ReprKind::Primitive,
            nullable: Nullability::NonNull,
        }
    }

    pub fn erased_reference(erased: impl Into<String>, nullable: Nullability) -> Self {
        Self {
            erased: erased.into(),
            kind: ReprKind::ErasedReference,
            nullable,
        }
    }

    /// A primitive is not a reference, so it can never exit through an
    /// `Object`-typed slot without a boxing receipt this slice does not have.
    pub fn is_reference(&self) -> bool {
        match &self.kind {
            ReprKind::Primitive => false,
            ReprKind::ErasedReference | ReprKind::ParameterizedReference(_) => true,
            ReprKind::Array(_) => true,
        }
    }

    /// Whether this value's *payload* (its type arguments) is known. An erased
    /// reference has an unknown payload, so any consumer that observes the
    /// payload must refuse; only contexts that cannot observe it may admit it.
    pub fn payload_complete(&self) -> bool {
        match &self.kind {
            ReprKind::Primitive => true,
            ReprKind::ErasedReference => false,
            ReprKind::ParameterizedReference(_) => true,
            ReprKind::Array(element) => element.payload_complete(),
        }
    }
}

/// What the call site produces. `Void` is a Java statement, not a Kotlin
/// `Unit` *value*: `UnitInValue` would have to name its own Java
/// representation, and no slice-1 builder can, so assigning a `void` call's
/// result is refused rather than approximated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JavaOutcome {
    Void,
    Value(JavaRepresentation),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dispatch {
    /// The only dispatch shape the slice-1 builder signs. A type-qualified
    /// receiver (static/object member) stays retained.
    Instance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArgumentMapping {
    /// Exactly `count` positional arguments, mapped one-to-one onto the
    /// physical formal parameters. Defaults, varargs, named arguments and
    /// trailing lambdas each need their own mapping and are refused.
    Exact(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckedEffects {
    /// Proved to declare no checked exception (`Some([])` on the wire).
    None,
    /// Proved to declare these checked exceptions.
    Checked(Vec<String>),
    /// Physical member identified, metadata absent (old helper/jar shape).
    /// Unknown is never "proved none".
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Behavior {
    /// The emitted Java call is the same member with the same evaluation
    /// semantics. Lowered behaviours (`String.trim`-class rewrites, container
    /// collection, …) need their own receipt builders and are not signed here.
    Exact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptEvidence {
    /// A hash-guarded Track B call site composed with a unique physical
    /// `ClassMembers` row whose reconstructed descriptor equals the K2 one.
    PhysicalMember,
}

/// The physical identity the emitter writes. `owner`/`name`/`descriptor` come
/// from the classpath row and the hash-guarded site, never from source
/// spelling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalleeIdentity {
    pub owner: String,
    pub name: String,
    pub descriptor: String,
}

/// One call site's signed lowering contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallReceipt {
    /// The whole call span; the emitter's lookup key and the plan's identity.
    pub location: SourceLocation,
    /// The callee selector's own span (the helper's call-site key).
    pub selector: SourceLocation,
    /// The receiver expression this receipt proved, when there is one.
    pub receiver: Option<SourceLocation>,
    pub callee: CalleeIdentity,
    pub dispatch: Dispatch,
    pub arguments: ArgumentMapping,
    pub outcome: JavaOutcome,
    pub effects: CheckedEffects,
    pub behavior: Behavior,
    pub evidence: ReceiptEvidence,
}

/// Where the call's value goes. Only these contexts can be admitted without
/// observing an erased payload, and each is proved structurally from the
/// source tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseContext {
    /// An expression statement in a block-bodied function. Its value is
    /// discarded, so nothing downstream can observe the payload. (Kotlin
    /// inserts no null check for a discarded platform-typed result, and neither
    /// does the emitted Java statement.)
    Discarded,
    /// The whole value is returned through a proven nullable
    /// `java.lang.Object` ABI, which cannot observe the payload either.
    NullableObjectExit,
    /// The whole value is returned through a declaration type structurally
    /// bound to the *non-generic* `java.lang.String`, and the receipt's own
    /// physical descriptor result is `Ljava/lang/String;`.
    ///
    /// This is deliberately not "any nullable reference": erasure equality
    /// cannot distinguish `Stream<Int>?` from `Stream<String>?`, so a
    /// generalized nullable-reference exit would admit a payload nobody
    /// proved. `java.lang.String` is final and carries no type arguments, so
    /// the erased representation *is* the complete representation.
    NullableStringExit,
    /// The whole value is returned through a declared JVM primitive, and the
    /// receipt's own representation is that same primitive. Both sides are
    /// physical primitives: no payload and no nullability question exists, so
    /// `boolean` → `boolean` is proved rather than assumed.
    PrimitiveExit(String),
    /// Anything else observes the value: an initializer (aliases), an
    /// argument, a receiver, a binary/Elvis operand, a parameterized return,
    /// a field store. The label is a stable worklist key.
    Observing(&'static str),
}

impl UseContext {
    pub fn label(&self) -> &str {
        match self {
            UseContext::Discarded => "discarded",
            UseContext::NullableObjectExit => "nullable-object exit",
            UseContext::NullableStringExit => "nullable-String exit",
            UseContext::PrimitiveExit(primitive) => primitive.as_str(),
            UseContext::Observing(label) => label,
        }
    }
}

/// Why a site could not be signed. Each variant names a distinct evidence gap
/// so the retention worklist stays actionable instead of becoming one opaque
/// "unsupported" bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No builder covers this operation at all.
    MissingReceipt,
    /// A value whose payload the use context can observe.
    ObservedPayload { use_context: String, erased: String },
    /// A value whose representation is complete, observed by an operation this
    /// slice has no builder for. Not a payload gap — a coverage gap.
    ObservedValue { use_context: String },
    /// The declared exit and the receipt's physical representation are
    /// different Java types. A source spelling is not evidence: the receipt's
    /// descriptor result is what the emitted Java method actually returns.
    WrongRepresentation { use_context: String, actual: String },
    /// A `void` result (or a primitive) in a value position.
    ValuePosition {
        use_context: String,
        detail: &'static str,
    },
}

impl Refusal {
    pub fn message(&self, name: &str) -> String {
        match self {
            Refusal::MissingReceipt => format!(
                "call `{name}` has no lowering contract; a declaration requiring Java lowering \
                 coverage must not emit an operation whose Java form is unproved"
            ),
            Refusal::ObservedPayload {
                use_context,
                erased,
            } => format!(
                "call `{name}` yields an erased `{erased}` with an unknown payload in {use_context}; \
                 a parameterized exit or consumer needs Java generic-expression typing, not erasure \
                 equality"
            ),
            Refusal::ObservedValue { use_context } => format!(
                "call `{name}` is observed in {use_context}, and no lowering contract signs that \
                 consumer yet"
            ),
            Refusal::WrongRepresentation {
                use_context,
                actual,
            } => format!(
                "call `{name}` yields `{actual}` in {use_context}, but the declared exit is \
                 `java.lang.String`; the receipt's physical descriptor result is what the \
                 emitted Java method returns"
            ),
            Refusal::ValuePosition {
                use_context,
                detail,
            } => format!("call `{name}` has no value representation in {use_context}: {detail}"),
        }
    }
}

/// One call site inside one top-level declaration's span, with whatever the
/// planners could sign for it.
pub struct SiteRecord {
    pub declaration: usize,
    pub name: String,
    pub location: SourceLocation,
    pub receipt: Option<CallReceipt>,
    pub use_context: UseContext,
    /// Effects evidence for a site with no receipt: the throws probe identified
    /// the physical member and it is not provably checked-free.
    pub effects: Option<CheckedEffects>,
}

/// The one receipt-outcome × use-context admission rule.
///
/// Every seam that decides "may this signed call be consumed here?" calls this
/// function: the declaration-wide [`SiteRecord`] inventory and the closed
/// operation walk's [`check_call`]. They used to hold separate whitelists, so a
/// context could be added to one and not the other; the match below is
/// exhaustive over [`UseContext`] (no wildcard arm), which makes a new exit a
/// compile error here instead of a half-added behaviour.
///
/// This rule answers only the *value* question. Where the value sits is a
/// separate structural question ([`permits_position`]): a discarded statement
/// and a whole-result exit are not interchangeable even when the same outcome
/// is admissible in both.
pub(crate) fn admit_use(outcome: &JavaOutcome, context: &UseContext) -> Result<(), Refusal> {
    match outcome {
        JavaOutcome::Void => match context {
            // A discarded `void` call is an ordinary Java statement.
            UseContext::Discarded => Ok(()),
            other => Err(Refusal::ValuePosition {
                use_context: other.label().to_string(),
                detail: "the callee returns Java `void`, which is not a Kotlin `Unit` value",
            }),
        },
        JavaOutcome::Value(value) => match context {
            // A discarded value is never observed, so any representation —
            // including an erased parameterized one — is safe there.
            UseContext::Discarded => Ok(()),
            UseContext::NullableObjectExit => {
                if value.is_reference() {
                    Ok(())
                } else {
                    Err(Refusal::ValuePosition {
                        use_context: context.label().to_string(),
                        detail: "a JVM primitive is not a reference and needs a boxing receipt",
                    })
                }
            }
            UseContext::NullableStringExit => {
                // The declaration side is already structurally bound to the
                // non-generic `java.lang.String`; this side is the receipt's
                // physical representation. Both must be the same final,
                // non-generic class, so erasure equality is complete identity
                // here specifically — never for a reference in general.
                if matches!(value.kind, ReprKind::ErasedReference)
                    && value.erased == STRING_REFERENCE
                {
                    Ok(())
                } else if matches!(value.kind, ReprKind::Primitive) {
                    Err(Refusal::ValuePosition {
                        use_context: context.label().to_string(),
                        detail: "a JVM primitive is not a reference and has no nullable \
                                 `java.lang.String` representation",
                    })
                } else {
                    Err(Refusal::WrongRepresentation {
                        use_context: context.label().to_string(),
                        actual: value.erased.clone(),
                    })
                }
            }
            UseContext::PrimitiveExit(declared) => match &value.kind {
                ReprKind::Primitive if value.erased == *declared => Ok(()),
                ReprKind::Primitive => Err(Refusal::ValuePosition {
                    use_context: context.label().to_string(),
                    detail: "the receipt's primitive differs from the declared one",
                }),
                _ => Err(Refusal::ObservedPayload {
                    use_context: "primitive exit".to_string(),
                    erased: value.erased.clone(),
                }),
            },
            // Any other observed use stays retained until the observing
            // operation is itself contracted. Even a complete payload is not
            // enough on its own: no builder signs an observed use, so this is
            // a coverage boundary, not only a payload one.
            UseContext::Observing(label) => {
                if value.payload_complete() {
                    Err(Refusal::ObservedValue {
                        use_context: label.to_string(),
                    })
                } else {
                    Err(Refusal::ObservedPayload {
                        use_context: label.to_string(),
                        erased: value.erased.clone(),
                    })
                }
            }
        },
    }
}

/// Where an admitted value may sit.
///
/// Deliberately separate from [`admit_use`]: a value admissible at a statement
/// position is not admissible as a whole result, so unifying the *outcome*
/// rule must not unify the *position* rule. A Java `void` result can therefore
/// never become a Kotlin `Unit` value through a shared whitelist.
pub(crate) fn permits_position(context: &UseContext, position: Position) -> bool {
    match position {
        Position::Statement => matches!(context, UseContext::Discarded),
        Position::WholeResult => matches!(
            context,
            UseContext::NullableObjectExit
                | UseContext::NullableStringExit
                | UseContext::PrimitiveExit(_)
        ),
    }
}

impl SiteRecord {
    /// The refusal this site contributes to a declaration that requires
    /// coverage, or `None` when the site is signed and admissible.
    pub fn refusal(&self) -> Option<Refusal> {
        let Some(receipt) = &self.receipt else {
            return Some(Refusal::MissingReceipt);
        };
        admit_use(&receipt.outcome, &self.use_context).err()
    }
}

/// Which declarations must cover their whole executable inventory.
///
/// Required mode is activated by the *existence* of a signed receipt in the
/// declaration's span, or by a [`RequiredObligations`] entry carried from the
/// snapshot that did sign receipts (a virtual re-plan, see
/// [`crate::translation_plan::TranslationPlan::carried_required`]). That fact is
/// computed once here and consumed by both the refusal pass and the operation
/// inventory, so "which declarations are in Required mode" cannot drift between
/// the two seams.
pub(crate) fn declarations_requiring_coverage(
    sites: &[SiteRecord],
    declarations: &[crate::translation_plan::DeclarationDecision],
    carried: &BTreeSet<SymbolId>,
) -> Vec<bool> {
    let mut required = vec![false; declarations.len()];
    for (index, decision) in declarations.iter().enumerate() {
        // A carried obligation is the same Required mode, one snapshot later:
        // the declaration had signed receipts on a snapshot the helper could
        // answer for, and this one cannot be answered for at all.
        if carried.contains(&decision.symbol_id) {
            required[index] = true;
        }
    }
    for site in sites {
        if site.receipt.is_some()
            && let Some(flag) = required.get_mut(site.declaration)
        {
            *flag = true;
        }
    }
    required
}

/// Apply the declaration-wide contract to one file's inventory.
///
/// A declaration is *Declared* (required mode) iff at least one receipt exists
/// inside its span. Otherwise it is Legacy and this function does nothing, so
/// the helper-off / old-jar / dead / stale floors stay byte-identical.
///
/// Refusals are `PreparationBlocker`s consumed by the preflight before the
/// first write, so no older Established/platform/approximation path can
/// override them and `--allow-approximations` cannot opt out.
///
/// `operations` is the non-call half of the inventory
/// ([`inventory_operations`]): every executable operation in the co-emitted
/// unit that is not a call site, plus every unproved declaration ABI slot. A
/// declaration in Required mode is refused when either half refuses, which is
/// what makes "call coverage" (the old bug) distinct from "declaration
/// coverage".
/// Record every proposed receipt exactly once, before either inventory runs.
///
/// This is the recording half of the contract: the operation inventory and the
/// declaration ABI walk both consume `plan.call_receipts`, so recording has to
/// happen first or they would see an empty slice and treat every signed site
/// as unsigned. `record_call_receipt` keeps genuine duplicate/foreign-snapshot
/// conflicts on the plan (never dropped, never first-wins); the pre-write
/// invariant turns the first one into a controlled error.
pub(crate) fn record_receipts(
    sites: &[SiteRecord],
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    for site in sites {
        if let Some(receipt) = &site.receipt {
            let _ = plan.record_call_receipt(receipt.clone());
        }
    }
}

pub(crate) fn plan_contracts(
    sites: &[SiteRecord],
    operations: &[OperationRefusal],
    required: &[bool],
    plan: &mut crate::translation_plan::TranslationPlan,
) {
    use crate::translation_plan::PreparationBlockerKind;

    for (index, required) in required.iter().enumerate() {
        if let Some(decision) = plan.declarations.get_mut(index) {
            decision.required_mode = *required;
        }
    }
    // Which declarations this snapshot actually re-established. A declaration
    // that is Required only because its obligation was carried has none: the
    // helper cannot answer for a virtual snapshot, so every site of it is
    // unsigned. That is one fact about the snapshot, not one checked-exception
    // question per call, so it is reported once per declaration.
    let mut signed = vec![false; plan.declarations.len()];
    for site in sites {
        if site.receipt.is_some()
            && let Some(flag) = signed.get_mut(site.declaration)
        {
            *flag = true;
        }
    }
    let carried = plan.carried_required.clone();
    let unproved_carried = plan
        .declarations
        .iter()
        .enumerate()
        .filter(|(index, decision)| {
            required.get(*index).copied().unwrap_or(false)
                && !signed.get(*index).copied().unwrap_or(false)
                && carried.contains(&decision.symbol_id)
        })
        .map(|(index, decision)| {
            (
                index,
                decision.symbol_id.name.clone(),
                decision.symbol_id.file.clone(),
                decision.id.start_byte,
                decision.id.end_byte,
            )
        })
        .collect::<Vec<_>>();
    for (index, name, file, start_byte, end_byte) in unproved_carried {
        let location = SourceLocation {
            file,
            snapshot_hash: plan.source_hash,
            start_byte,
            end_byte,
        };
        retain_contract(
            plan,
            index,
            format!(
                "declaration `{name}` was proved Required by signed receiver facts on an earlier \
                 snapshot of this file; this re-planned snapshot is not the text the helper \
                 answered for, so its receipts cannot be re-established and it stays Kotlin \
                 instead of reverting to the legacy path"
            ),
            location,
        );
    }
    for site in sites {
        if !required.get(site.declaration).copied().unwrap_or(false) {
            continue;
        }
        // Carried-and-unsigned declarations are already refused once, with the
        // reason that explains why no receipt exists at all.
        if !signed.get(site.declaration).copied().unwrap_or(false)
            && plan
                .declarations
                .get(site.declaration)
                .is_some_and(|decision| carried.contains(&decision.symbol_id))
        {
            continue;
        }
        let (kind, message) = match site.refusal() {
            None => continue,
            Some(Refusal::MissingReceipt) => match &site.effects {
                Some(CheckedEffects::Checked(exceptions)) => {
                    let plural = if exceptions.len() == 1 { "" } else { "s" };
                    (
                        PreparationBlockerKind::SemanticLoss,
                        format!(
                            "call `{}` declares checked exception{plural} {}; the emitted Java \
                             method has no `throws`/`catch` model",
                            site.name,
                            exceptions.join(", ")
                        ),
                    )
                }
                Some(CheckedEffects::Unknown) => (
                    PreparationBlockerKind::SemanticLoss,
                    format!(
                        "call `{}` has no checked-exception evidence; a declaration requiring \
                         Java lowering coverage must not assume a callee throws nothing",
                        site.name
                    ),
                ),
                _ => (
                    PreparationBlockerKind::SemanticLoss,
                    Refusal::MissingReceipt.message(&site.name),
                ),
            },
            Some(refusal) => (
                PreparationBlockerKind::SemanticLoss,
                refusal.message(&site.name),
            ),
        };
        retain_with(plan, site.declaration, kind, message, &site.location);
    }
    for operation in operations {
        if !required
            .get(operation.declaration)
            .copied()
            .unwrap_or(false)
        {
            continue;
        }
        retain_with(
            plan,
            operation.declaration,
            crate::translation_plan::PreparationBlockerKind::SemanticLoss,
            operation.message.clone(),
            &operation.location,
        );
    }
}

/// Record one authoritative refusal against a declaration.
fn retain_with(
    plan: &mut crate::translation_plan::TranslationPlan,
    declaration: usize,
    kind: crate::translation_plan::PreparationBlockerKind,
    message: String,
    location: &SourceLocation,
) {
    use crate::translation_plan::{BackendOwner, RetentionReason};

    let reason = RetentionReason::PreparationBlocker {
        kind,
        message: message.clone(),
    };
    if let Some(decision) = plan.declarations.get_mut(declaration) {
        if !decision.retention_reasons.contains(&reason) {
            decision.retention_reasons.push(reason.clone());
        }
        decision.candidate_owner = BackendOwner::Kotlin;
        // Authoritative, persisted evidence that this declaration was refused
        // by the lowering contract itself. Unlike the PreparationBlocker
        // reason (which is preparation state and is cleared by reconciliation)
        // this survives to acceptance, so a translated Java file for a
        // contract-blocked declaration is a controlled pre-write failure
        // rather than an invisible disagreement.
        decision.contract_retained = true;
    }
    if !plan
        .diagnostics
        .iter()
        .any(|existing| existing.message == message && existing.location == *location)
    {
        plan.diagnostics.push(SemanticDiagnostic {
            code: reason.code().into(),
            message: reason.message(),
            location: location.clone(),
        });
    }
}

/// Record one authoritative lowering-contract refusal against a declaration.
///
/// The declaration keeps a `PreparationBlocker` reason for reporting and the
/// persisted `contract_retained` flag for acceptance, so the refusal cannot be
/// erased between planning and the write.
pub(crate) fn retain_contract(
    plan: &mut crate::translation_plan::TranslationPlan,
    declaration: usize,
    message: String,
    location: SourceLocation,
) {
    retain_with(
        plan,
        declaration,
        crate::translation_plan::PreparationBlockerKind::SemanticLoss,
        message,
        &location,
    );
}

/// Receipts the plan signed inside a declaration that is being translated but
/// that emission never read.
///
/// This is the consumption half of the contract: production without
/// consumption would let the planner and the emitter disagree while both look
/// correct in isolation. The caller turns a non-empty result into a pre-write
/// failure (no Java is written), never a best-effort file.
pub(crate) fn unconsumed_receipts(
    receipts: &[CallReceipt],
    translated_spans: &[(usize, usize)],
    consumed: &std::collections::BTreeSet<(usize, usize)>,
) -> Vec<String> {
    receipts
        .iter()
        .filter(|receipt| {
            translated_spans.iter().any(|(start, end)| {
                *start <= receipt.location.start_byte && receipt.location.end_byte <= *end
            })
        })
        .filter(|receipt| {
            !consumed.contains(&(receipt.location.start_byte, receipt.location.end_byte))
        })
        .map(|receipt| format!("{}({})", receipt.callee.name, receipt.callee.owner))
        .collect()
}

// ---------------------------------------------------------------------------
// Declaration ABI identity
// ---------------------------------------------------------------------------

/// The declared return classification of every function in one co-emitted
/// unit, keyed by the declaring function's span.
pub(crate) type ReturnAbi = BTreeMap<(usize, usize), AbiProof>;

/// What a declared type slot physically is in the emitted Java.
///
/// `Unproved` is not "Object for now": it names the missing evidence and
/// retains the declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AbiProof {
    /// Kotlin `Unit`, or no declared type on a block body: Java `void`.
    Void,
    /// That exact JVM primitive.
    Primitive(String),
    /// Kotlin `Any?` — `java.lang.Object` with no intrinsic null check.
    NullableObject,
    /// Kotlin `String?` whose spelling is structurally bound to the
    /// non-generic `java.lang.String` (no import alias, file declaration, type
    /// parameter, workspace typealias or type argument). Distinct from
    /// [`AbiProof::Reference`] on purpose: that variant erases type arguments,
    /// so admitting every nullable reference would admit `Stream<Int>?` too.
    NullableString,
    /// A reference whose emitted Java spelling is bound by evidence.
    Reference(String),
    /// Not proved; the string names the missing evidence.
    Unproved(String),
}

impl AbiProof {
    fn unproved(detail: impl Into<String>) -> Self {
        AbiProof::Unproved(detail.into())
    }
}

/// The Kotlin builtins whose Java spelling a declaration ABI may rely on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Builtin {
    Void,
    Primitive(&'static str),
    Object,
    Reference(&'static str),
}

fn builtin_type(name: &str) -> Option<Builtin> {
    Some(match name {
        "Unit" => Builtin::Void,
        "Any" => Builtin::Object,
        "String" => Builtin::Reference("java.lang.String"),
        "Int" => Builtin::Primitive("int"),
        "Long" => Builtin::Primitive("long"),
        "Short" => Builtin::Primitive("short"),
        "Byte" => Builtin::Primitive("byte"),
        "Double" => Builtin::Primitive("double"),
        "Float" => Builtin::Primitive("float"),
        "Boolean" => Builtin::Primitive("boolean"),
        "Char" => Builtin::Primitive("char"),
        _ => return None,
    })
}

/// A simple name bound in this file, and what binds it.
///
/// Every variant means the name is *not* the Kotlin builtin of the same
/// spelling, so its physical Java ABI comes from the binding, which this slice
/// does not model.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
    FileType,
    TypeAlias,
    TypeParameter,
    ImportAlias(String),
}

impl Binding {
    fn describe(&self) -> String {
        match self {
            Binding::FileType => "a type declaration in this file".into(),
            Binding::TypeAlias => "a typealias in this file".into(),
            Binding::TypeParameter => "a type parameter in scope".into(),
            Binding::ImportAlias(path) => format!("the import alias for `{path}`"),
        }
    }
}

/// The name bindings of one source file, read structurally from its tree.
///
/// This is the evidence a declared type spelling is resolved through: an
/// `import` header (never line text, so a commented-out import binds nothing),
/// a declaration in the file, or a type parameter.
#[derive(Debug, Default)]
pub(crate) struct TypeBindings {
    bound: BTreeMap<String, Binding>,
    /// Simple name -> imported paths. More than one is an ambiguous binding.
    imports: BTreeMap<String, Vec<String>>,
    wildcard: Option<String>,
    package: String,
}

impl TypeBindings {
    pub(crate) fn build(source: &str, root: tree_sitter::Node<'_>) -> Self {
        let mut bindings = Self {
            package: package_name(source),
            ..Self::default()
        };
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() != "import" {
                continue;
            }
            let Some(header) = java_member_calls::import_header(source, child) else {
                continue;
            };
            match (header.alias, header.wildcard) {
                (Some(alias), false) => {
                    bindings
                        .bound
                        .entry(alias)
                        .or_insert(Binding::ImportAlias(header.path.clone()));
                }
                (None, false) => {
                    if let Some(simple) = header.path.rsplit('.').next() {
                        bindings
                            .imports
                            .entry(simple.to_string())
                            .or_default()
                            .push(header.path.clone());
                    }
                }
                (None, true) => {
                    bindings.wildcard.get_or_insert(header.path.clone());
                }
                (Some(_), true) => {}
            }
        }
        // Declarations anywhere in the file bind their name, including nested
        // and local ones: a shadowing declaration is never "the builtin".
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "class_declaration"
                | "interface_declaration"
                | "object_declaration"
                | "enum_class_declaration" => {
                    if let Some(name) = node_name(node, source) {
                        bindings.bound.entry(name).or_insert(Binding::FileType);
                    }
                }
                "type_alias" => {
                    let name = node
                        .child_by_field_name("type")
                        .or_else(|| node.child_by_field_name("name"))
                        .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                        .map(|text| text.trim().to_string());
                    if let Some(name) = name {
                        bindings.bound.entry(name).or_insert(Binding::TypeAlias);
                    }
                }
                "type_parameter" => {
                    let name = node
                        .child_by_field_name("name")
                        .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                        .or_else(|| {
                            let mut cursor = node.walk();
                            node.named_children(&mut cursor)
                                .find(|child| {
                                    matches!(child.kind(), "identifier" | "simple_identifier")
                                })
                                .and_then(|child| child.utf8_text(source.as_bytes()).ok())
                        })
                        .map(|text| text.trim().to_string());
                    if let Some(name) = name {
                        bindings.bound.entry(name).or_insert(Binding::TypeParameter);
                    }
                }
                _ => {}
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        bindings
    }

    /// Whether some import loads a declaration from `package`.
    fn imports_package(&self, package: &str) -> bool {
        self.imports.values().flatten().any(|path| {
            path.rsplit_once('.')
                .is_some_and(|(imported_package, _)| imported_package == package)
        })
    }
}

/// One declared ABI slot, with the *exact text* the plan requires emission to
/// write for it.
///
/// The text is produced by the emitter's own renderer
/// ([`crate::transpiler::kt::java_parameter_type`], `java_return_type` and the
/// readonly-parameter rewrite), so `emitted` is not a second spelling table:
/// it is the renderer's output for this exact node and annotation set,
/// persisted on the plan and re-verified from the source tree before any write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotProof {
    pub role: String,
    pub label: String,
    pub proof: AbiProof,
    pub emitted: String,
    start_byte: usize,
    end_byte: usize,
}

impl SlotProof {
    pub fn start_byte(&self) -> usize {
        self.start_byte
    }

    pub fn end_byte(&self) -> usize {
        self.end_byte
    }

    /// The refusal message for an unproved slot.
    pub fn message(&self) -> String {
        match &self.proof {
            AbiProof::Unproved(detail) => format!(
                "declaration ABI: the {} `{}` is not a proved Java type: {detail}",
                self.role, self.label
            ),
            _ => String::new(),
        }
    }

    /// The comparable identity of this slot: role, exact rendered text, the
    /// ABI classification that admitted it, and its span.
    ///
    /// The proof is part of the identity on purpose: a plan that proved
    /// `String?` as a nullable String ABI and a replay that proves it as a
    /// plain reference (or the reverse) are different contracts even when the
    /// rendered text is identical, and acceptance must refuse the delta.
    pub fn fingerprint(&self) -> (String, String, AbiProof, usize, usize) {
        (
            self.role.clone(),
            self.emitted.clone(),
            self.proof.clone(),
            self.start_byte,
            self.end_byte,
        )
    }
}

/// The persisted, snapshot-bound ABI contract of one Required declaration.
///
/// This is the shared object the question "may this declaration be emitted?"
/// is answered from: it is recorded while planning, stored on
/// [`crate::translation_plan::DeclarationDecision`], and re-proved from the
/// exact source tree at acceptance. A slot the plan proved cannot be replaced
/// by a source-spelling guess at emission time without failing that check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredContract {
    pub unit_kind: String,
    pub slots: Vec<SlotProof>,
}

impl RequiredContract {
    pub fn fingerprints(&self) -> Vec<(String, String, AbiProof, usize, usize)> {
        self.slots.iter().map(SlotProof::fingerprint).collect()
    }
}

/// The Required-mode obligations one file established on one snapshot.
///
/// This is the cross-snapshot certificate the workspace fixpoint needs. A
/// speculative round plans a *rewritten* text (`migrate::propose_speculative_migration`)
/// that the helper never saw, so [`crate::facts::runtime::FactsRuntime::call_site_at`]
/// refuses to answer and every receipt of that round is gone. Losing a receipt
/// is not evidence that the declaration is Legacy: it is evidence that this
/// snapshot cannot answer. Carrying the obligation (an identity set plus the
/// hash of the snapshot that signed it) keeps Required mode across the window
/// without ever reusing a receipt under a different hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredObligations {
    /// blake3 of the snapshot whose signed receipts established these
    /// obligations. Only a snapshot with the *same* bytes may re-establish
    /// them itself; anything else inherits the obligation instead.
    pub proved_hash: [u8; 32],
    /// Stable declaration identities (`SymbolId` carries no offsets and no
    /// snapshot hash, so it survives a re-plan of the same declaration header).
    pub declarations: BTreeSet<SymbolId>,
}

impl RequiredObligations {
    pub fn new(proved_hash: [u8; 32]) -> Self {
        Self {
            proved_hash,
            declarations: BTreeSet::new(),
        }
    }

    /// The identities a snapshot with `source_hash` may not silently lose.
    ///
    /// `None` means this snapshot is the very text the receipts were signed
    /// on, so it re-proves them itself and carries nothing. A hash mismatch is
    /// exactly the case where the helper refuses to answer: the obligation
    /// travels, the receipt does not.
    pub fn carried(&self, source_hash: &[u8; 32]) -> Option<&BTreeSet<SymbolId>> {
        (self.proved_hash != *source_hash).then_some(&self.declarations)
    }
}

/// One operation or shape inside a Required unit that no contract signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefusalRecord {
    pub message: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// The ABI proofs and operation refusals of one co-emitted top-level unit.
#[derive(Debug, Default)]
pub(crate) struct DeclarationAbi {
    unit_kind: String,
    returns: ReturnAbi,
    slots: Vec<SlotProof>,
    refusals: Vec<RefusalRecord>,
    /// Call spans the operation walk actually reached. Every receipt inside the
    /// unit must appear here: a receipt the walk never visited would mean the
    /// inventory and the recording disagree about what the site is.
    visited_calls: BTreeSet<(usize, usize)>,
}

impl DeclarationAbi {
    pub(crate) fn unproved(&self) -> impl Iterator<Item = &SlotProof> {
        self.slots
            .iter()
            .filter(|slot| matches!(slot.proof, AbiProof::Unproved(_)))
    }

    pub(crate) fn refusals(&self) -> &[RefusalRecord] {
        &self.refusals
    }

    /// The persisted contract, only when every slot proved and no operation or
    /// shape refused. A partial proof is never a contract.
    pub(crate) fn contract(&self) -> Option<RequiredContract> {
        if !self.refusals.is_empty() || self.unproved().next().is_some() {
            return None;
        }
        Some(RequiredContract {
            unit_kind: self.unit_kind.clone(),
            slots: self.slots.clone(),
        })
    }

    fn refuse(&mut self, message: String, node: tree_sitter::Node<'_>) {
        self.refusals.push(RefusalRecord {
            message,
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
        });
    }
}

/// Where a value sits: either a whole `return`/expression-body result, or a
/// statement whose value is discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Position {
    Statement,
    WholeResult,
}

/// The signed receipts of one file, keyed by the call span the plan and the
/// emitter use.
///
/// The operation walk reads the *receipt* here, not just its span: admission is
/// decided from the receipt's own outcome by the same [`admit_use`] rule the
/// declaration inventory uses. A duplicate span cannot silently win — the plan
/// recorder refuses duplicate-span receipts before any write.
type ReceiptIndex<'a> = BTreeMap<(usize, usize), &'a CallReceipt>;

/// The proof context for one file: the source bytes, the file's name bindings,
/// the semantic provider and the annotation set the emitter will use.
struct TypeCtx<'a, P: SemanticProvider + ?Sized> {
    source: &'a str,
    bindings: &'a TypeBindings,
    provider: &'a P,
    annots: crate::transpiler::types::AnnotationSet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Return,
    Parameter,
}

/// The maximum type nesting this proof descends. A deeper type is refused
/// rather than followed: the descent invariant already makes recursion
/// finite, and this is the second, independent bound.
const TYPE_PROOF_MAX_DEPTH: usize = 48;

/// A proved type: its classification and the exact emitted text.
struct ProvenType {
    proof: AbiProof,
    emitted: String,
}

/// Prove every declaration ABI slot and every executable operation of one
/// co-emitted unit.
///
/// The unit is a top-level declaration (a class carries every member; a
/// top-level function is its own unit). The walk is *closed*: every node kind
/// it meets is classified by [`kind_policy`], the only admitted shapes are the
/// ones this slice proves, and everything else is refused with its own
/// message. That is what makes "every emitted slot" true rather than
/// approximately true: a shape with a constructor parameter, a field, a
/// supertype, a type parameter, an extension receiver, a default/vararg
/// parameter, a literal statement or an unproved call is refused, not masked by
/// an unrelated taint.
pub(crate) fn declaration_abi<P: SemanticProvider + ?Sized>(
    source: &str,
    unit: tree_sitter::Node<'_>,
    bindings: &TypeBindings,
    provider: &P,
    annots: crate::transpiler::types::AnnotationSet,
    receipts: &[CallReceipt],
) -> DeclarationAbi {
    let ctx = TypeCtx {
        source,
        bindings,
        provider,
        annots,
    };
    let receipt_index: ReceiptIndex<'_> = receipts
        .iter()
        .map(|receipt| {
            (
                (receipt.location.start_byte, receipt.location.end_byte),
                receipt,
            )
        })
        .collect();
    let mut abi = DeclarationAbi {
        unit_kind: unit.kind().to_string(),
        ..DeclarationAbi::default()
    };
    match unit.kind() {
        "function_declaration" => prove_function(&ctx, unit, &receipt_index, &mut abi),
        // A class is proved only in the closed shape `prove_class` enumerates.
        // An interface's default methods additionally inherit variance/
        // covariant-getter ABI rewrites that this slice does not prove, so an
        // interface unit is refused outright rather than half-proved.
        "class_declaration" => prove_class(&ctx, unit, &receipt_index, &mut abi),
        "interface_declaration" => abi.refuse(
            "an interface declaration's default-method and variance ABI is not proved by this \
             slice"
                .to_string(),
            unit,
        ),
        other => abi.refuse(
            format!(
                "a top-level `{other}` declaration has no lowering contract; this slice proves \
                 top-level functions and classes with plain method members only"
            ),
            unit,
        ),
    }
    // Coverage: a receipt inside this unit that the operation walk never
    // reached is not evidence of coverage. It means the signed site sits in a
    // shape this walk refused (or never looked at), which must retain, not
    // silently pass.
    let (start, end) = (unit.start_byte(), unit.end_byte());
    for receipt in receipts {
        let span = (receipt.location.start_byte, receipt.location.end_byte);
        if start <= span.0 && span.1 <= end && !abi.visited_calls.contains(&span) {
            abi.refuse(
                format!(
                    "the signed lowering receipt for `{}` does not denote a call site in this \
                     declaration's admitted operation shapes",
                    receipt.callee.name
                ),
                unit,
            );
            break;
        }
    }
    abi
}

/// The declared return classification of every function in one unit.
///
/// This half does not depend on receipts, so the call-site walk can classify a
/// value's use context before any receipt is recorded.
pub(crate) fn declaration_returns<P: SemanticProvider + ?Sized>(
    source: &str,
    unit: tree_sitter::Node<'_>,
    bindings: &TypeBindings,
    provider: &P,
    annots: crate::transpiler::types::AnnotationSet,
) -> ReturnAbi {
    let ctx = TypeCtx {
        source,
        bindings,
        provider,
        annots,
    };
    let mut returns = ReturnAbi::new();
    let mut stack = vec![unit];
    while let Some(node) = stack.pop() {
        match node.kind() {
            // A local declaration or lambda is not part of the unit's ABI.
            "function_body" => continue,
            "function_declaration" => {
                returns.insert(
                    (node.start_byte(), node.end_byte()),
                    declared_return_proof(&ctx, node),
                );
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    returns
}

fn declared_return_proof<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    function: tree_sitter::Node<'_>,
) -> AbiProof {
    match declaration_type_node(function) {
        Some(node) => prove_slot(ctx, SlotKind::Return, node).proof,
        None if body_is_expression_node(function, ctx.source) => AbiProof::unproved(
            "an inferred expression-body return type has no declared Java spelling",
        ),
        None => AbiProof::Void,
    }
}

fn prove_function<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    function: tree_sitter::Node<'_>,
    receipts: &ReceiptIndex<'_>,
    abi: &mut DeclarationAbi,
) {
    if let Some(modifiers) = child_kind(function, "modifiers") {
        let reason = if has_annotation(modifiers) {
            "a declaration annotation has no proved Java lowering or retention slot in this contract"
        } else {
            "function modifiers can change JVM names, synthetic parameters or generated members, which this plain-function contract does not prove"
        };
        abi.refuse(reason.to_string(), modifiers);
    }
    if let Some(parameters) = child_kind(function, "type_parameters") {
        abi.refuse(
            "a type parameter's Java erasure (and its nullability bound) is \
             declaration-parameterized, which this slice does not model"
                .to_string(),
            parameters,
        );
    }
    if let Some(receiver) = extension_receiver(function) {
        abi.refuse(
            "an extension receiver is emitted as a synthetic first parameter, and this slice \
             does not prove that emitted slot"
                .to_string(),
            receiver,
        );
    }

    let return_node = declaration_type_node(function);
    match return_node {
        Some(node) => {
            let proven = prove_slot(ctx, SlotKind::Return, node);
            abi.returns.insert(
                (function.start_byte(), function.end_byte()),
                proven.proof.clone(),
            );
            abi.slots.push(SlotProof {
                role: "return type".to_string(),
                label: node_text(ctx.source, node),
                proof: proven.proof,
                emitted: proven.emitted,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            });
        }
        None => {
            let proof = declared_return_proof(ctx, function);
            abi.returns
                .insert((function.start_byte(), function.end_byte()), proof.clone());
            if let AbiProof::Unproved(detail) = proof {
                abi.refuse(detail, function);
            }
        }
    }

    if let Some(parameters) = child_kind(function, "function_value_parameters") {
        for parameter in code_children(parameters) {
            match parameter.kind() {
                "parameter" => {
                    prove_parameter(ctx, parameter, abi);
                }
                other => abi.refuse(
                    format!(
                        "`{other}` in a parameter list has no lowering contract; only a plain \
                         `name: Type` parameter is proved"
                    ),
                    parameter,
                ),
            }
        }
    }

    match child_kind(function, "function_body") {
        None => abi.refuse(
            "a bodyless declaration has no lowering contract; an abstract/native method needs \
             its own ABI proof"
                .to_string(),
            function,
        ),
        Some(body) => scan_body(ctx, body, receipts, abi),
    }
}

fn prove_parameter<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    parameter: tree_sitter::Node<'_>,
    abi: &mut DeclarationAbi,
) {
    let label = node_name(parameter, ctx.source).unwrap_or_else(|| "parameter".into());
    if let Some(modifiers) = child_kind(parameter, "parameter_modifiers") {
        abi.refuse(
            format!(
                "parameter `{label}` carries modifiers (a vararg/annotation parameter has no \
                 proved Java slot)"
            ),
            modifiers,
        );
        return;
    }
    if parameter_has_default(parameter) {
        abi.refuse(
            format!(
                "parameter `{label}` has a default value; the Kotlin `$default` bridge is a JVM \
                 ABI this emitter cannot reproduce"
            ),
            parameter,
        );
        return;
    }
    let Some(type_node) = parameter_type_node(parameter) else {
        abi.refuse(
            format!("parameter `{label}` declares no type, so its Java slot is unproved"),
            parameter,
        );
        return;
    };
    let proven = prove_slot(ctx, SlotKind::Parameter, type_node);
    let proof = if matches!(proven.proof, AbiProof::Void) {
        AbiProof::unproved("`Unit` is not a value parameter type")
    } else {
        proven.proof
    };
    abi.slots.push(SlotProof {
        role: "parameter type".to_string(),
        label: node_text(ctx.source, type_node),
        proof,
        emitted: proven.emitted,
        start_byte: type_node.start_byte(),
        end_byte: type_node.end_byte(),
    });
}

fn prove_class<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    class: tree_sitter::Node<'_>,
    receipts: &ReceiptIndex<'_>,
    abi: &mut DeclarationAbi,
) {
    // This grammar uses `class_declaration` for interfaces too. Only an
    // actual `class` keyword admits the plain-class ABI; Java default methods
    // do not provide Kotlin's DefaultImpls compatibility class.
    if !class
        .children(&mut class.walk())
        .any(|child| child.kind() == "class")
    {
        abi.refuse(
            "an interface declaration's default-method and variance ABI is not proved by this slice"
                .to_string(),
            class,
        );
        return;
    }
    if let Some(modifiers) = child_kind(class, "modifiers") {
        // `data`/`sealed`/`open`/annotations all change the emitted class shape
        // (generated members, permits, annotations). None of those slots has a
        // producer here, so the whole class is refused rather than emitted with
        // an unproved ABI.
        abi.refuse(
            "class modifiers or annotations change the emitted type shape (generated members, \
             permits, annotations), which this slice does not prove"
                .to_string(),
            modifiers,
        );
    }
    if let Some(parameters) = child_kind(class, "type_parameters") {
        abi.refuse(
            "a class type parameter is part of the emitted signature and its erasure is not \
             modelled here"
                .to_string(),
            parameters,
        );
    }
    for child in class.named_children(&mut class.walk()) {
        match child.kind() {
            "primary_constructor" => abi.refuse(
                "a primary constructor emits constructor/field slots (and `val` getters) that \
                 this slice does not prove"
                    .to_string(),
                child,
            ),
            "secondary_constructor" => abi.refuse(
                "a secondary constructor has no proved Java signature".to_string(),
                child,
            ),
            "delegation_specifiers" => abi.refuse(
                "a supertype is part of the emitted `extends`/`implements` ABI and is not proved \
                 by this slice"
                    .to_string(),
                child,
            ),
            "enum_class_body" => abi.refuse(
                "an enum body carries constant and synthetic member slots this slice does not \
                 prove"
                    .to_string(),
                child,
            ),
            _ => {}
        }
    }
    let Some(body) = child_kind(class, "class_body") else {
        abi.refuse(
            "a class without a body has no proved member inventory".to_string(),
            class,
        );
        return;
    };
    for member in code_children(body) {
        match member.kind() {
            "function_declaration" => prove_function(ctx, member, receipts, abi),
            other => abi.refuse(
                format!(
                    "class member `{other}` has no lowering contract; this slice proves plain \
                     member functions only (no fields, properties, initializers, nested types, \
                     companions or enum entries)"
                ),
                member,
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Type proofs
// ---------------------------------------------------------------------------

/// Prove one declared type slot and render the exact text the emitter writes.
fn prove_slot<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    kind: SlotKind,
    node: tree_sitter::Node<'_>,
) -> ProvenType {
    let emitted = render_slot(ctx, kind, node);
    match prove_type(ctx, node, 0) {
        Ok(proven) => ProvenType {
            proof: proven.proof,
            emitted,
        },
        Err(detail) => ProvenType {
            proof: AbiProof::unproved(detail),
            emitted,
        },
    }
}

/// The emitter's own rendering for a slot, with the run's annotation set.
///
/// A parameter is rendered exactly like `unit/function.rs` renders it
/// (annotations aside, `java_parameter_type` plus the readonly-parameter
/// rewrite); a return type exactly like `java_return_type` (which maps a
/// canonical `Unit` to `void`).
fn render_slot<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    kind: SlotKind,
    node: tree_sitter::Node<'_>,
) -> String {
    match kind {
        SlotKind::Return => crate::transpiler::kt::java_return_type(node, ctx.source, ctx.annots),
        SlotKind::Parameter => {
            let java = crate::transpiler::kt::java_parameter_type(node, ctx.source, ctx.annots);
            crate::transpiler::types::covariant_readonly_parameter(
                node.utf8_text(ctx.source.as_bytes()).unwrap_or(""),
                &java,
            )
        }
    }
}

/// Prove one type node. Every recursive step goes through [`prove_descend`],
/// which refuses a node that is not strictly inside its parent — so the
/// classifier is total: it either descends or refuses, and can never re-enter
/// the same node.
fn prove_type<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    node: tree_sitter::Node<'_>,
    depth: usize,
) -> Result<ProvenType, String> {
    if depth > TYPE_PROOF_MAX_DEPTH {
        return Err("the type nesting exceeds the depth this proof models".to_string());
    }
    match node.kind() {
        "user_type" => prove_user_type(ctx, node, depth),
        "nullable_type" => {
            let inner = one_named_child(node, "nullable type")?;
            let proven = prove_descend(ctx, node, inner, depth)?;
            match proven.proof {
                AbiProof::Primitive(primitive) => Err(format!(
                    "a nullable `{primitive}` is a boxed reference in Java and needs a boxing \
                     receipt"
                )),
                AbiProof::Void => Err(
                    "`Unit?` has no Java representation (Java `void` is not a value)".to_string(),
                ),
                AbiProof::Reference(java) if java == "java.lang.Object" => Ok(ProvenType {
                    proof: AbiProof::NullableObject,
                    emitted: proven.emitted,
                }),
                // `String?` is the one nullable reference this slice may
                // admit, and only when the *whole* inner type is argument-free.
                // The proof below is reachable only through the binding guards
                // in `prove_simple_name`/`prove_user_type`, so an alias,
                // shadow, type parameter or generic spelling never arrives
                // here as `java.lang.String`. The structural check is the
                // second, independent bound: `String<Int>?` (not valid Kotlin,
                // but parseable) must not borrow the builtin's erasure.
                AbiProof::Reference(java)
                    if java == STRING_REFERENCE && !has_type_arguments(inner) =>
                {
                    Ok(ProvenType {
                        proof: AbiProof::NullableString,
                        emitted: proven.emitted,
                    })
                }
                other => Ok(ProvenType {
                    proof: other,
                    emitted: proven.emitted,
                }),
            }
        }
        "type_projection" => prove_projection(ctx, node, depth),
        "parenthesized_type" => {
            let inner = one_named_child(node, "parenthesized type")?;
            prove_descend(ctx, node, inner, depth)
        }
        "function_type" => {
            Err("a Kotlin function type has no Java erasure this proof models".to_string())
        }
        other if other.ends_with("_type") || other == "type" => {
            Err(format!("`{other}` is not a type shape this proof models"))
        }
        other => Err(format!("`{other}` is not a type")),
    }
}

/// Prove `child` as a type, requiring that it strictly descends from `parent`.
fn prove_descend<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    parent: tree_sitter::Node<'_>,
    child: tree_sitter::Node<'_>,
    depth: usize,
) -> Result<ProvenType, String> {
    // A wrapper can share its child's span exactly (`type_projection` has no
    // punctuation of its own: `<String>` puts the angle brackets on
    // `type_arguments`), so descent is proved by node identity plus
    // containment, not by a strictly smaller span. The depth bound is the
    // independent backstop against any cycle a future grammar could introduce.
    if child.id() == parent.id()
        || child.start_byte() < parent.start_byte()
        || child.end_byte() > parent.end_byte()
    {
        return Err(format!(
            "`{}` does not descend from its parent type node",
            node_text(ctx.source, child)
        ));
    }
    prove_type(ctx, child, depth + 1)
}

/// A `type_projection`: an optional `out`/`in` modifier plus one type, or the
/// anonymous `*` token (a star projection, which has no Java type).
fn prove_projection<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    node: tree_sitter::Node<'_>,
    depth: usize,
) -> Result<ProvenType, String> {
    let mut projected = None;
    for child in code_children(node) {
        match child.kind() {
            "variance_modifier" => {}
            _ => {
                if projected.is_some() {
                    return Err(
                        "a type projection with more than one type has no proved Java form"
                            .to_string(),
                    );
                }
                projected = Some(child);
            }
        }
    }
    match projected {
        Some(inner) => prove_descend(ctx, node, inner, depth),
        // `*` is an anonymous token: no named child, and the projection text is
        // exactly `*`. Both facts are grammar facts, not spelling guesses.
        None => Err("a star projection has no Java type argument".to_string()),
    }
}

fn prove_user_type<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    node: tree_sitter::Node<'_>,
    depth: usize,
) -> Result<ProvenType, String> {
    let mut names: Vec<String> = Vec::new();
    let mut arguments: Vec<tree_sitter::Node<'_>> = Vec::new();
    for child in code_children(node) {
        match child.kind() {
            "identifier" | "simple_identifier" => names.push(
                child
                    .utf8_text(ctx.source.as_bytes())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            ),
            "type_arguments" => arguments.push(child),
            "type_modifiers" => {
                return Err(
                    "a type-use modifier or annotation has no proved Java type form".to_string(),
                );
            }
            other => {
                return Err(format!(
                    "`{other}` inside a type has no proved Java type form"
                ));
            }
        }
    }
    if names.is_empty() {
        return Err(format!("`{}` names no type", node_text(ctx.source, node)));
    }
    // Every type argument is its own ABI question (`Stream<Int>` vs
    // `Stream<Key>` with `typealias Key = …`), including star and function-type
    // arguments.
    for argument in &arguments {
        for projection in code_children(*argument) {
            prove_descend(ctx, *argument, projection, depth)?;
        }
    }
    let emitted = render_slot(ctx, SlotKind::Parameter, node);
    let base = emitted_base(&emitted).to_string();
    if names.len() == 1 {
        return prove_simple_name(ctx, &names[0], &base);
    }
    // A qualified spelling names its own package path. Every segment is a
    // source name, so the first one is resolved exactly like a simple name
    // would be: a declared, aliased or imported first segment makes the path
    // a source-relative name, not a physical Java class.
    let root = &names[0];
    if let Some(binding) = ctx.bindings.bound.get(root) {
        return Err(format!("`{root}` is bound by {}", binding.describe()));
    }
    if ctx.bindings.imports.contains_key(root) {
        return Err(format!(
            "`{root}` is an imported name, so `{}` is not a proved Java package path",
            names.join(".")
        ));
    }
    if workspace_binding(root, ctx.bindings, ctx.provider).is_some() {
        return Err(format!(
            "`{root}` is bound by a workspace declaration, so `{}` is not a proved Java path",
            names.join(".")
        ));
    }
    let path = names.join(".");
    if !is_provable_qualified_path(&path) {
        return Err(format!(
            "`{path}` is not a `java.`/`javax.` binary name this slice can prove"
        ));
    }
    // Physical evidence: the classpath helper must know this exact binary name
    // as a loadable class. A dotted spelling alone proves nothing.
    if !ctx.provider.has_classpath_facts() || ctx.provider.classpath_member_facts(&path).is_none() {
        return Err(format!(
            "`{path}` has no classpath evidence that it is a loadable Java class"
        ));
    }
    if base != path {
        return Err(format!(
            "the emitter writes `{emitted}` for `{path}`, which is not that class"
        ));
    }
    Ok(ProvenType {
        proof: AbiProof::Reference(path),
        emitted,
    })
}

fn prove_simple_name<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    name: &str,
    emitted_base: &str,
) -> Result<ProvenType, String> {
    if let Some(binding) = ctx.bindings.bound.get(name) {
        return Err(format!("`{name}` is bound by {}", binding.describe()));
    }
    // Kotlin's explicit imports displace a default (builtin) import, so the
    // import binding is consulted before the builtin spelling. An import of a
    // class whose simple name is `Int`/`String` is exactly the case a builtin
    // check would have read as the Kotlin type.
    if let Some(paths) = ctx.bindings.imports.get(name) {
        let [path] = paths.as_slice() else {
            return Err(format!(
                "`{name}` has {} conflicting import bindings",
                paths.len()
            ));
        };
        if workspace_binding(name, ctx.bindings, ctx.provider).is_some() {
            return Err(format!(
                "`{name}` is bound both by an import and by a workspace declaration"
            ));
        }
        if path.rsplit('.').next() != Some(name) {
            return Err(format!(
                "the import `{path}` does not bind the simple name `{name}`"
            ));
        }
        if emitted_base != name {
            return Err(format!(
                "the emitter writes `{emitted_base}` for the imported `{path}`, which is not \
                 that class"
            ));
        }
        return Ok(ProvenType {
            proof: AbiProof::Reference(path.clone()),
            emitted: String::new(),
        });
    }
    if let Some(alias) = workspace_binding(name, ctx.bindings, ctx.provider) {
        return Err(format!("`{name}` is bound by {alias}"));
    }
    let Some(builtin) = builtin_type(name) else {
        return Err(format!(
            "`{name}` has no import or declaration binding in this file"
        ));
    };
    if ctx.bindings.wildcard.is_some() {
        return Err(format!(
            "the wildcard import `{}` may supply `{name}`",
            ctx.bindings.wildcard.as_deref().unwrap_or("")
        ));
    }
    let proof = match builtin {
        Builtin::Void => {
            if emitted_base != "void" && emitted_base != "kotlin.Unit" {
                return Err(format!(
                    "the emitter writes `{emitted_base}` for Kotlin `Unit`"
                ));
            }
            AbiProof::Void
        }
        Builtin::Primitive(primitive) => {
            if emitted_base != primitive {
                return Err(format!(
                    "the emitter writes `{emitted_base}` for Kotlin `{name}`, not `{primitive}`"
                ));
            }
            AbiProof::Primitive(primitive.to_string())
        }
        Builtin::Object => {
            if emitted_base != "Object" && emitted_base != "java.lang.Object" {
                return Err(format!(
                    "the emitter writes `{emitted_base}` for Kotlin `Any`"
                ));
            }
            AbiProof::Reference("java.lang.Object".to_string())
        }
        Builtin::Reference(java) => {
            let simple = java.rsplit('.').next().unwrap_or(java);
            if emitted_base != java && emitted_base != simple {
                return Err(format!(
                    "the emitter writes `{emitted_base}` for Kotlin `{name}` (`{java}`)"
                ));
            }
            AbiProof::Reference(java.to_string())
        }
    };
    Ok(ProvenType {
        proof,
        emitted: String::new(),
    })
}

/// The emitted text without a leading annotation list and type arguments.
fn emitted_base(emitted: &str) -> &str {
    let mut rest = emitted.trim();
    while rest.starts_with('@') {
        match rest.find(char::is_whitespace) {
            Some(index) => rest = rest[index..].trim_start(),
            None => return "",
        }
    }
    rest.split('<').next().unwrap_or("").trim()
}

fn is_provable_qualified_path(path: &str) -> bool {
    (path.starts_with("java.") || path.starts_with("javax."))
        && path.split('.').all(|segment| {
            !segment.is_empty()
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
                && !segment.chars().next().is_some_and(|c| c.is_ascii_digit())
        })
}

fn one_named_child<'tree>(
    node: tree_sitter::Node<'tree>,
    what: &str,
) -> Result<tree_sitter::Node<'tree>, String> {
    let mut cursor = node.walk();
    let children: Vec<_> = node
        .named_children(&mut cursor)
        .filter(|child| kind_policy(child.kind()) != KindPolicy::Trivia)
        .collect();
    match children.as_slice() {
        [only] => Ok(*only),
        _ => Err(format!(
            "a {what} with {} type children has no proved Java form",
            children.len()
        )),
    }
}

/// Whether a type node or any of its descendants carries type arguments.
///
/// Used to keep a nullable exit's declaration type non-generic structurally,
/// not by reading the rendered text (a rendered `String` says nothing about a
/// source `String<…>` shape).
fn has_type_arguments(node: tree_sitter::Node<'_>) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "type_arguments" {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
    false
}

/// A workspace-level binding this file's own tree cannot see: a typealias in
/// the same package, a typealias imported from elsewhere, or a same-package
/// type declaration in another file.
fn workspace_binding<P: SemanticProvider + ?Sized>(
    name: &str,
    bindings: &TypeBindings,
    provider: &P,
) -> Option<String> {
    if provider.has_type_alias_in_package(&bindings.package, name) {
        return Some(format!("a typealias in package `{}`", bindings.package));
    }
    for ((package, alias), _) in provider.type_aliases() {
        if alias != *name {
            continue;
        }
        if package == bindings.package
            || bindings.imports_package(&package)
            || bindings.wildcard.is_some()
        {
            return Some(format!("the typealias `{name}` from package `{package}`"));
        }
    }
    if provider.symbols().iter().any(|symbol| {
        symbol.id.name == *name
            && symbol.id.package == bindings.package
            && is_type_symbol(&symbol.id)
    }) {
        return Some(format!("a declaration in package `{}`", bindings.package));
    }
    None
}

// ---------------------------------------------------------------------------
// Closed operation walk
// ---------------------------------------------------------------------------

/// What the closed walk does with one node kind.
///
/// This is an explicit per-kind decision, not a substring blacklist: an
/// unknown kind is [`KindPolicy::Refuse`], and `every_named_grammar_kind_is_classified`
/// checks the admitted set against the grammar's own `node-types.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KindPolicy {
    /// A function declaration whose slots and body are proved.
    Function,
    /// Class-like syntax whose keyword, ABI and members must still be proved.
    Class,
    /// A body: statements are walked.
    Body,
    /// A statement container.
    Block,
    /// A `return` statement (bare, or one whole value).
    Return,
    /// A call expression: admitted only with a receipt in an admitted position.
    Call,
    /// A transparent wrapper around a whole result.
    Transparent,
    /// Explicit grammar trivia, never an executable operation or type slot.
    Trivia,
    /// Anything else: refused with its kind named.
    Refuse,
}

/// The closed decision table for the vendored grammar.
pub(crate) fn kind_policy(kind: &str) -> KindPolicy {
    match kind {
        "function_declaration" => KindPolicy::Function,
        "class_declaration" | "interface_declaration" => KindPolicy::Class,
        "function_body" => KindPolicy::Body,
        "block" | "statements" => KindPolicy::Block,
        "return_expression" => KindPolicy::Return,
        "call_expression" => KindPolicy::Call,
        "parenthesized_expression" => KindPolicy::Transparent,
        "line_comment" | "block_comment" => KindPolicy::Trivia,
        _ => KindPolicy::Refuse,
    }
}

fn code_children<'tree>(
    node: tree_sitter::Node<'tree>,
) -> impl Iterator<Item = tree_sitter::Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| kind_policy(child.kind()) != KindPolicy::Trivia)
        .collect::<Vec<_>>()
        .into_iter()
}

fn scan_body<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    body: tree_sitter::Node<'_>,
    receipts: &ReceiptIndex<'_>,
    abi: &mut DeclarationAbi,
) {
    for child in code_children(body) {
        match kind_policy(child.kind()) {
            KindPolicy::Block => {
                for statement in code_children(child) {
                    scan_statement(ctx, statement, receipts, abi);
                }
            }
            KindPolicy::Call => {
                // An expression body: the call is the function's whole result.
                check_call(ctx, child, receipts, abi, Position::WholeResult);
            }
            KindPolicy::Transparent => {
                scan_value(ctx, child, receipts, abi, Position::WholeResult);
            }
            _ => refuse_operation(ctx, child, abi),
        }
    }
}

fn scan_statement<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    statement: tree_sitter::Node<'_>,
    receipts: &ReceiptIndex<'_>,
    abi: &mut DeclarationAbi,
) {
    match kind_policy(statement.kind()) {
        KindPolicy::Call => check_call(ctx, statement, receipts, abi, Position::Statement),
        KindPolicy::Return => {
            let values: Vec<_> = code_children(statement).collect();
            match values.as_slice() {
                [] => {}
                [value] => check_value(ctx, *value, receipts, abi),
                _ => refuse_operation(ctx, statement, abi),
            }
        }
        _ => refuse_operation(ctx, statement, abi),
    }
}

fn scan_value<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    value: tree_sitter::Node<'_>,
    receipts: &ReceiptIndex<'_>,
    abi: &mut DeclarationAbi,
    position: Position,
) {
    match kind_policy(value.kind()) {
        KindPolicy::Call => check_call(ctx, value, receipts, abi, position),
        KindPolicy::Transparent if position == Position::WholeResult => {
            let inner: Vec<_> = code_children(value).collect();
            match inner.as_slice() {
                [only] => scan_value(ctx, *only, receipts, abi, Position::WholeResult),
                _ => refuse_operation(ctx, value, abi),
            }
        }
        _ => refuse_operation(ctx, value, abi),
    }
}

/// A whole result (a `return` operand or an expression body) may be a call
/// whose receipt proves a whole-value exit, or a transparent wrapper around
/// one. Nothing else is a proved value: a literal, identifier, template,
/// operator or nested call has no contract here and is refused, never admitted
/// as a "pure leaf".
fn check_value<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    value: tree_sitter::Node<'_>,
    receipts: &ReceiptIndex<'_>,
    abi: &mut DeclarationAbi,
) {
    scan_value(ctx, value, receipts, abi, Position::WholeResult);
}

fn check_call<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    call: tree_sitter::Node<'_>,
    receipts: &ReceiptIndex<'_>,
    abi: &mut DeclarationAbi,
    position: Position,
) {
    abi.visited_calls
        .insert((call.start_byte(), call.end_byte()));
    let name = call_callee_node(call)
        .and_then(|callee| callee.utf8_text(ctx.source.as_bytes()).ok())
        .unwrap_or("")
        .trim()
        .to_string();
    let Some(receipt) = receipts.get(&(call.start_byte(), call.end_byte())) else {
        abi.refuse(
            format!(
                "call `{name}` has no lowering contract; a declaration requiring Java lowering \
                 coverage must not emit an operation whose Java form is unproved"
            ),
            call,
        );
        return;
    };
    let context = classify_use(ctx.source, call, &abi.returns);
    // One admission rule for both seams: the receipt's own outcome against the
    // structurally classified use context. The separate position proof below
    // answers "may a value sit here at all", so unifying the outcome rule
    // cannot make a discard context a return value or a Java `void` a Kotlin
    // `Unit` value.
    if let Err(refusal) = admit_use(&receipt.outcome, &context) {
        abi.refuse(refusal.message(&name), call);
        return;
    }
    if !permits_position(&context, position) {
        abi.refuse(
            format!(
                "call `{name}` is used as {} in {} position, but a proved value may only be a \
                 discarded statement or a whole value exit (a `void` result is never a value)",
                context.label(),
                match position {
                    Position::Statement => "statement",
                    Position::WholeResult => "whole-result",
                }
            ),
            call,
        );
    }
}

fn refuse_operation<P: SemanticProvider + ?Sized>(
    ctx: &TypeCtx<'_, P>,
    node: tree_sitter::Node<'_>,
    abi: &mut DeclarationAbi,
) {
    abi.refuse(
        format!(
            "`{}` (`{}`) has no lowering contract; a declaration requiring Java lowering \
             coverage must not emit an operation whose Java form is unproved",
            node_text(ctx.source, node),
            node.kind()
        ),
        node,
    );
}

// ---------------------------------------------------------------------------
// Declaration structural helpers
// ---------------------------------------------------------------------------

/// A short, whitespace-normalized source excerpt for diagnostics.
fn node_text(source: &str, node: tree_sitter::Node<'_>) -> String {
    let text = node.utf8_text(source.as_bytes()).unwrap_or("").trim();
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() > 60 {
        let truncated: String = normalized.chars().take(57).collect();
        format!("{truncated}…")
    } else {
        normalized
    }
}

fn child_kind<'tree>(
    node: tree_sitter::Node<'tree>,
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn has_annotation(modifiers: tree_sitter::Node<'_>) -> bool {
    let mut stack = vec![modifiers];
    while let Some(node) = stack.pop() {
        if node.kind().ends_with("annotation") {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

/// The declared type of a function, positionally after the parameter list
/// (the grammar has no `return_type` field; DEV.md).
fn declaration_type_node(function: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut after_parameters = false;
    let mut cursor = function.walk();
    for child in function.children(&mut cursor) {
        if kind_policy(child.kind()) == KindPolicy::Trivia {
            continue;
        }
        if child.kind() == "function_value_parameters" {
            after_parameters = true;
            continue;
        }
        if after_parameters && child.is_named() {
            return match child.kind() {
                "user_type" | "nullable_type" | "function_type" | "type" | "parenthesized_type" => {
                    Some(child)
                }
                _ => None,
            };
        }
    }
    None
}

/// The extension receiver: a fieldless type node strictly before the `name`
/// field (DEV.md).
fn extension_receiver(function: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut fields = Vec::new();
    let mut cursor = function.walk();
    if !cursor.goto_first_child() {
        return None;
    }
    loop {
        fields.push((cursor.field_name().map(str::to_string), cursor.node()));
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    let name_pos = fields
        .iter()
        .position(|(field, _)| field.as_deref() == Some("name"))?;
    fields
        .iter()
        .enumerate()
        .find_map(|(index, (field, node))| {
            let is_type = matches!(node.kind(), "user_type" | "nullable_type");
            (index < name_pos && is_type && field.is_none()).then_some(*node)
        })
}

fn parameter_type_node(parameter: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    if let Some(node) = parameter.child_by_field_name("type") {
        return Some(node);
    }
    let mut cursor = parameter.walk();
    parameter.named_children(&mut cursor).find(|child| {
        child.kind() == "user_type"
            || child.kind() == "nullable_type"
            || child.kind() == "function_type"
            || child.kind() == "parenthesized_type"
    })
}

/// `= default` is a sibling of `parameter` in this grammar (DEV.md).
fn parameter_has_default(parameter: tree_sitter::Node<'_>) -> bool {
    let mut sibling = parameter.next_sibling();
    while let Some(node) = sibling {
        match node.kind() {
            "=" => return true,
            "," | ")" => return false,
            kind if kind.ends_with("comment") => sibling = node.next_sibling(),
            _ => return false,
        }
    }
    false
}

fn body_is_expression_node(function: tree_sitter::Node<'_>, source: &str) -> bool {
    let mut cursor = function.walk();
    function
        .named_children(&mut cursor)
        .find(|child| child.kind() == "function_body")
        .is_some_and(|body| body_is_expression(source, body))
}

// ---------------------------------------------------------------------------
// Use contexts
// ---------------------------------------------------------------------------

/// One call site inside one top-level declaration's span, with whatever the
/// planners could sign for it.
pub(crate) struct OperationRefusal {
    pub declaration: usize,
    pub message: String,
    pub location: SourceLocation,
}

/// Classify where one call's value goes, from the source tree only.
///
/// Both admitted contexts are deliberately narrow and structural:
///
/// * [`UseContext::Discarded`] needs a block-bodied function whose proved ABI is
///   `void`. A lambda's last expression is its result, so a call inside a lambda
///   body is never discarded here.
/// * [`UseContext::NullableObjectExit`] needs the call to *be* the function's
///   whole result and the declared return type to be a *bound* `Any?`.
///   [`ReturnAbi`] supplies that proof; a spelling is never enough. A non-null
///   `Any` would make Kotlin insert an intrinsic null check for a
///   platform-typed value, so it is not admitted.
pub(crate) fn classify_use(
    source: &str,
    call: tree_sitter::Node<'_>,
    returns: &ReturnAbi,
) -> UseContext {
    let mut node = call;
    while let Some(parent) = node.parent()
        && parent.kind() == "parenthesized_expression"
    {
        node = parent;
    }
    let Some(parent) = node.parent() else {
        return UseContext::Observing("value position");
    };
    match parent.kind() {
        "return_expression" => {
            if code_children(parent).count() != 1 {
                return UseContext::Observing("computed return value");
            }
            match enclosing_function(parent) {
                Some(function) => exit_context(function, returns),
                None => UseContext::Observing("value position"),
            }
        }
        // An expression body is the function's result. A block body is not:
        // its statements discard their values (and a single-statement block
        // body arrives here without a `statements` wrapper).
        "function_body" => match enclosing_function(parent) {
            Some(function) if body_is_expression(source, parent) => exit_context(function, returns),
            Some(function) if returns_void(function, returns) => UseContext::Discarded,
            Some(_) => UseContext::Observing("return through a declared type"),
            None => UseContext::Observing("value position"),
        },
        // `block` is the single-statement body shape in this grammar; the
        // multi-statement shape arrives as `statements`.
        "statements" | "block" => {
            let Some(body) = parent
                .parent()
                .filter(|body| body.kind() == "function_body")
            else {
                // A lambda's last expression is the lambda's result, and an
                // `if`/`when` branch block feeds the enclosing expression.
                return UseContext::Observing("nested block");
            };
            if body_is_expression(source, body) {
                return UseContext::Observing("value position");
            }
            match enclosing_function(body) {
                // A block body is only discarded when the function cannot
                // return the value at all.
                Some(function) if returns_void(function, returns) => UseContext::Discarded,
                Some(_) => UseContext::Observing("return through a declared type"),
                None => UseContext::Observing("value position"),
            }
        }
        "value_argument" | "value_arguments" => UseContext::Observing("argument"),
        "assignment" | "property_declaration" | "variable_declaration" => {
            UseContext::Observing("initializer")
        }
        "binary_expression" | "infix_expression" => UseContext::Observing("binary operand"),
        "navigation_expression" => UseContext::Observing("receiver"),
        "call_expression" => UseContext::Observing("argument"),
        "if_expression" | "when_expression" | "when_entry" => {
            UseContext::Observing("conditional result")
        }
        "indexing_expression" => UseContext::Observing("index"),
        "elvis_expression" => UseContext::Observing("elvis operand"),
        _ => UseContext::Observing("value position"),
    }
}

fn enclosing_function(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut current = node.parent();
    while let Some(candidate) = current {
        match candidate.kind() {
            "function_declaration" => return Some(candidate),
            // A lambda or a nested declaration is a different value scope.
            "lambda_literal" | "class_declaration" | "object_declaration" => return None,
            _ => current = candidate.parent(),
        }
    }
    None
}

fn body_is_expression(source: &str, body: tree_sitter::Node<'_>) -> bool {
    body.children(&mut body.walk())
        .any(|child| child.kind() == "=")
        || body
            .utf8_text(source.as_bytes())
            .is_ok_and(|text| text.trim_start().starts_with('='))
}

fn returns_void(function: tree_sitter::Node<'_>, returns: &ReturnAbi) -> bool {
    matches!(
        returns.get(&(function.start_byte(), function.end_byte())),
        Some(AbiProof::Void)
    )
}

/// Classify a value that *is* the function's whole result.
///
/// Three exits are proved by [`ReturnAbi`]: a bound nullable `Object` ABI
/// (which cannot observe a payload), an exactly matching JVM primitive, and
/// nothing else. A parameterized reference result stays retained until Java
/// generic-expression typing is proved for it.
fn exit_context(function: tree_sitter::Node<'_>, returns: &ReturnAbi) -> UseContext {
    match returns.get(&(function.start_byte(), function.end_byte())) {
        Some(AbiProof::NullableObject) => UseContext::NullableObjectExit,
        Some(AbiProof::NullableString) => UseContext::NullableStringExit,
        Some(AbiProof::Primitive(primitive)) => UseContext::PrimitiveExit(primitive.clone()),
        _ => UseContext::Observing("return through a declared type"),
    }
}

/// Build the physical representation a member row declares.
///
/// `None` means this slice cannot represent the row's result at all, which is a
/// refusal, not an `Object` guess.
pub(crate) fn representation_from_row(
    result_type: &str,
    result_nullability: &str,
) -> Option<JavaOutcome> {
    if result_type == "<void>" {
        return Some(JavaOutcome::Void);
    }
    let nullable = match result_nullability {
        "nullable" => Nullability::Nullable,
        "non_null" => Nullability::NonNull,
        _ => Nullability::Unknown,
    };
    if let Some(primitive) = primitive_representation(result_type) {
        return Some(JavaOutcome::Value(primitive));
    }
    if let Some(rest) = result_type.strip_prefix('[') {
        let element = match rest {
            "I" => JavaRepresentation::primitive("int"),
            "J" => JavaRepresentation::primitive("long"),
            "D" => JavaRepresentation::primitive("double"),
            "F" => JavaRepresentation::primitive("float"),
            "Z" => JavaRepresentation::primitive("boolean"),
            "C" => JavaRepresentation::primitive("char"),
            "B" => JavaRepresentation::primitive("byte"),
            "S" => JavaRepresentation::primitive("short"),
            other => {
                let object = other.strip_prefix('L')?.strip_suffix(';')?;
                if object.is_empty() {
                    return None;
                }
                JavaRepresentation::erased_reference(object.replace('/', "."), Nullability::NonNull)
            }
        };
        return Some(JavaOutcome::Value(JavaRepresentation {
            erased: java_array_spelling(&element.erased),
            kind: ReprKind::Array(Box::new(element)),
            nullable,
        }));
    }
    if !result_type.contains('.') {
        return None;
    }
    Some(JavaOutcome::Value(JavaRepresentation::erased_reference(
        result_type,
        nullable,
    )))
}

fn primitive_representation(name: &str) -> Option<JavaRepresentation> {
    matches!(
        name,
        "int" | "long" | "double" | "float" | "boolean" | "char" | "byte" | "short"
    )
    .then(|| JavaRepresentation::primitive(name))
}

fn java_array_spelling(element: &str) -> String {
    match element {
        "int" | "long" | "double" | "float" | "boolean" | "char" | "byte" | "short" => {
            format!("{element}[]")
        }
        other => format!("{other}[]"),
    }
}

/// Number of parameter slots a JVM method descriptor declares.
///
/// Used by acceptance to check a receipt's `ArgumentMapping` against the
/// physical descriptor it recorded: a mapping and a descriptor that disagree
/// are an internal contradiction, not evidence.
pub(crate) fn descriptor_arity(descriptor: &str) -> Option<usize> {
    if !descriptor.starts_with('(') {
        return None;
    }
    let end = descriptor.find(')')?;
    let mut count = 0;
    let mut chars = descriptor[1..end].chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '[' => {}
            'L' => {
                let mut closed = false;
                for character in chars.by_ref() {
                    if character == ';' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return None;
                }
                count += 1;
            }
            'I' | 'J' | 'D' | 'F' | 'Z' | 'C' | 'B' | 'S' => count += 1,
            _ => return None,
        }
    }
    Some(count)
}

/// Whether a receipt's recorded outcome can be the value its own descriptor
/// returns.
pub(crate) fn descriptor_result_matches(outcome: &JavaOutcome, descriptor: &str) -> bool {
    let Some((_, result)) = descriptor.split_once(')') else {
        return false;
    };
    match outcome {
        JavaOutcome::Void => result == "V",
        JavaOutcome::Value(value) => match &value.kind {
            ReprKind::Primitive => {
                primitive_descriptor(&value.erased).is_some_and(|code| result == code)
            }
            ReprKind::Array(_) => result.starts_with('['),
            ReprKind::ErasedReference | ReprKind::ParameterizedReference(_) => {
                let Some(class) = result
                    .strip_prefix('L')
                    .and_then(|rest| rest.strip_suffix(';'))
                else {
                    return false;
                };
                class.replace('/', ".") == value.erased
            }
        },
    }
}

fn primitive_descriptor(name: &str) -> Option<&'static str> {
    Some(match name {
        "int" => "I",
        "long" => "J",
        "double" => "D",
        "float" => "F",
        "boolean" => "Z",
        "char" => "C",
        "byte" => "B",
        "short" => "S",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(start: usize, end: usize) -> SourceLocation {
        SourceLocation {
            file: std::path::PathBuf::from("Fixture.kt"),
            snapshot_hash: [7u8; 32],
            start_byte: start,
            end_byte: end,
        }
    }

    fn receipt(outcome: JavaOutcome) -> CallReceipt {
        CallReceipt {
            location: location(10, 30),
            selector: location(17, 23),
            receiver: Some(location(10, 16)),
            callee: CalleeIdentity {
                owner: "java.util.stream.Stream".into(),
                name: "sorted".into(),
                descriptor: "()Ljava/util/stream/Stream;".into(),
            },
            dispatch: Dispatch::Instance,
            arguments: ArgumentMapping::Exact(0),
            outcome,
            effects: CheckedEffects::None,
            behavior: Behavior::Exact,
            evidence: ReceiptEvidence::PhysicalMember,
        }
    }

    fn site(outcome: JavaOutcome, use_context: UseContext) -> SiteRecord {
        SiteRecord {
            declaration: 0,
            name: "sorted".into(),
            location: location(10, 30),
            receipt: Some(receipt(outcome)),
            use_context,
            effects: None,
        }
    }

    fn erased_stream() -> JavaOutcome {
        JavaOutcome::Value(JavaRepresentation::erased_reference(
            "java.util.stream.Stream",
            Nullability::Unknown,
        ))
    }

    #[test]
    fn an_erased_parameterized_value_is_admissible_only_where_it_is_unobserved() {
        assert_eq!(site(erased_stream(), UseContext::Discarded).refusal(), None);
        assert_eq!(
            site(erased_stream(), UseContext::NullableObjectExit).refusal(),
            None
        );
    }

    #[test]
    fn erasure_equality_does_not_prove_a_parameterized_exit() {
        assert!(matches!(
            site(
                erased_stream(),
                UseContext::Observing("return through a declared type")
            )
            .refusal(),
            Some(Refusal::ObservedPayload { .. })
        ));
    }

    #[test]
    fn java_void_is_not_a_kotlin_unit_value() {
        assert!(matches!(
            site(JavaOutcome::Void, UseContext::NullableObjectExit).refusal(),
            Some(Refusal::ValuePosition { .. })
        ));
        // …but a discarded `void` call is an ordinary Java statement.
        assert_eq!(
            site(JavaOutcome::Void, UseContext::Discarded).refusal(),
            None
        );
    }

    #[test]
    fn a_primitive_never_exits_through_an_object_slot() {
        let primitive = JavaOutcome::Value(JavaRepresentation::primitive("int"));
        assert!(matches!(
            site(primitive.clone(), UseContext::NullableObjectExit).refusal(),
            Some(Refusal::ValuePosition { .. })
        ));
        // …and an exactly declared primitive exit is proved on both sides.
        assert_eq!(
            site(
                primitive.clone(),
                UseContext::PrimitiveExit("int".to_string())
            )
            .refusal(),
            None
        );
        assert!(matches!(
            site(primitive, UseContext::PrimitiveExit("long".to_string())).refusal(),
            Some(Refusal::ValuePosition { .. })
        ));
    }

    fn string_outcome() -> JavaOutcome {
        JavaOutcome::Value(JavaRepresentation::erased_reference(
            STRING_REFERENCE,
            Nullability::Unknown,
        ))
    }

    fn erased(name: &str) -> JavaOutcome {
        JavaOutcome::Value(JavaRepresentation::erased_reference(
            name,
            Nullability::Unknown,
        ))
    }

    fn array_outcome() -> JavaOutcome {
        JavaOutcome::Value(JavaRepresentation {
            erased: "java.lang.String[]".into(),
            kind: ReprKind::Array(Box::new(JavaRepresentation::erased_reference(
                STRING_REFERENCE,
                Nullability::NonNull,
            ))),
            nullable: Nullability::Unknown,
        })
    }

    fn parameterized_outcome() -> JavaOutcome {
        JavaOutcome::Value(JavaRepresentation {
            erased: "java.util.stream.Stream".into(),
            kind: ReprKind::ParameterizedReference(vec![JavaRepresentation::erased_reference(
                STRING_REFERENCE,
                Nullability::NonNull,
            )]),
            nullable: Nullability::Unknown,
        })
    }

    /// Every use context this module can classify, with a payload so the
    /// matrix below is a real cross product rather than a label list.
    fn every_context() -> Vec<UseContext> {
        vec![
            UseContext::Discarded,
            UseContext::NullableObjectExit,
            UseContext::NullableStringExit,
            UseContext::PrimitiveExit("int".to_string()),
            UseContext::PrimitiveExit("long".to_string()),
            UseContext::Observing("initializer"),
            UseContext::Observing("return through a declared type"),
        ]
    }

    /// The admission matrix, exhaustively: no outcome/context pair may be left
    /// undecided, and the nullable String exit admits exactly the physical
    /// String representation.
    #[test]
    fn the_shared_admission_rule_covers_every_outcome_and_context() {
        let mut admitted = std::collections::BTreeSet::new();
        for (outcome, name) in [
            (JavaOutcome::Void, "void"),
            (
                JavaOutcome::Value(JavaRepresentation::primitive("int")),
                "int",
            ),
            (array_outcome(), "array"),
            (erased("java.util.stream.Stream"), "erased stream"),
            (parameterized_outcome(), "parameterized stream"),
            (string_outcome(), "String"),
        ] {
            for context in every_context() {
                let verdict = admit_use(&outcome, &context);
                let key = (name, context.label().to_string());
                if verdict.is_ok() {
                    admitted.insert(key);
                } else {
                    // A refusal must name the gap; `Ok`/`Err` is the whole
                    // contract, so an empty message would be a silent drop.
                    let refusal = verdict.unwrap_err();
                    assert!(
                        !refusal.message("probe").is_empty(),
                        "a refusal with no message is not evidence"
                    );
                }
            }
        }
        let expected: std::collections::BTreeSet<(&str, String)> = [
            ("void", "discarded"),
            ("int", "discarded"),
            ("array", "discarded"),
            ("erased stream", "discarded"),
            ("parameterized stream", "discarded"),
            ("String", "discarded"),
            ("array", "nullable-object exit"),
            ("erased stream", "nullable-object exit"),
            ("parameterized stream", "nullable-object exit"),
            ("String", "nullable-object exit"),
            ("String", "nullable-String exit"),
            ("int", "int"),
        ]
        .into_iter()
        .map(|(outcome, context)| (outcome, context.to_string()))
        .collect();
        assert_eq!(admitted, expected, "the admission matrix changed");
    }

    /// The nullable String exit is exactly one representation, never a family.
    #[test]
    fn the_nullable_string_exit_admits_only_a_physical_string() {
        for refused in [
            JavaOutcome::Void,
            JavaOutcome::Value(JavaRepresentation::primitive("int")),
            array_outcome(),
            erased("java.lang.Object"),
            erased("java.lang.CharSequence"),
            erased("java.util.stream.Stream"),
            parameterized_outcome(),
        ] {
            assert!(
                admit_use(&refused, &UseContext::NullableStringExit).is_err(),
                "{refused:?} must not satisfy a nullable String exit"
            );
        }
        assert_eq!(
            admit_use(&string_outcome(), &UseContext::NullableStringExit),
            Ok(())
        );
    }

    /// Position is a separate proof: unifying admission must not let a discard
    /// context become a return value, or a `void` statement become a value.
    #[test]
    fn position_admission_is_not_the_outcome_admission() {
        for context in every_context() {
            let statement = permits_position(&context, Position::Statement);
            let whole_result = permits_position(&context, Position::WholeResult);
            match &context {
                UseContext::Discarded => {
                    assert!(statement, "a discarded value is a Java statement");
                    assert!(!whole_result, "a return cannot discard its value");
                }
                UseContext::NullableObjectExit
                | UseContext::NullableStringExit
                | UseContext::PrimitiveExit(_) => {
                    assert!(whole_result, "{context:?} is a whole-value exit");
                    assert!(!statement, "{context:?} is not a discardable statement");
                }
                UseContext::Observing(label) => {
                    assert!(!statement && !whole_result, "{label} is admitted nowhere");
                }
            }
        }
        // A void call is admissible only as a statement, so no position can
        // turn it into a Kotlin `Unit` value.
        assert_eq!(
            admit_use(&JavaOutcome::Void, &UseContext::Discarded),
            Ok(())
        );
        assert!(!permits_position(
            &UseContext::Discarded,
            Position::WholeResult
        ));
    }

    /// The declaration slot proof is structural: the builtin spelling alone is
    /// not enough, and a generic reference never becomes a nullable String.
    #[test]
    fn a_nullable_string_slot_is_structurally_bound() {
        fn return_proofs(source: &str) -> Vec<AbiProof> {
            abi(source)
                .slots
                .iter()
                .filter(|slot| slot.role == "return type")
                .map(|slot| slot.proof.clone())
                .collect()
        }

        assert_eq!(
            return_proofs(
                "package p\n\nimport java.io.File\n\nfun nullableName(f: File): String? = f.getName()\n"
            ),
            vec![AbiProof::NullableString]
        );
        // A non-null `String` is a plain reference, not the nullable exit.
        assert_eq!(
            return_proofs(
                "package p\n\nimport java.io.File\n\nfun strictName(f: File): String = f.getName()\n"
            ),
            vec![AbiProof::Reference(STRING_REFERENCE.to_string())]
        );
        // A nullable parameterized reference keeps its erased-reference proof.
        assert_eq!(
            return_proofs(
                "package p\n\nimport java.util.stream.Stream\n\nfun s(v: Stream<Int>): Stream<Int>? = v.sorted()\n"
            ),
            vec![AbiProof::Reference("java.util.stream.Stream".to_string())]
        );
        // Bindings that displace the builtin never prove the exit at all.
        for (label, source) in [
            (
                "typealias",
                "package p\n\nimport java.io.File\n\ntypealias String = Any\n\nfun n(f: File): String? = f.getName()\n",
            ),
            (
                "import alias",
                "package p\n\nimport java.io.File\nimport java.util.stream.Stream as String\n\nfun n(f: File): String? = f.getName()\n",
            ),
            (
                "file declaration",
                "package p\n\nimport java.io.File\n\nclass String\n\nfun n(f: File): String? = f.getName()\n",
            ),
            (
                "type parameter",
                "package p\n\nimport java.io.File\n\nfun <String> n(f: File): String? = f.getName()\n",
            ),
        ] {
            let proofs = return_proofs(source);
            assert!(
                proofs.iter().all(|proof| !matches!(
                    proof,
                    AbiProof::NullableString | AbiProof::Reference(_)
                )),
                "{label}: a bound `String` spelling must not prove the builtin: {proofs:?}"
            );
        }
    }

    /// Build the receipt the operation walk would consume for the fixture's
    /// first call, with an arbitrary physical outcome.
    fn call_receipt(source: &str, outcome: JavaOutcome) -> CallReceipt {
        let tree = parse(source);
        let mut stack = vec![tree.root_node()];
        let mut call = None;
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" && call.is_none() {
                call = Some(node);
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        let call = call.expect("fixture has a call");
        let mut receipt = receipt(outcome);
        receipt.location.start_byte = call.start_byte();
        receipt.location.end_byte = call.end_byte();
        receipt
    }

    /// The declaration-wide inventory and the closed operation walk must reach
    /// the *same* verdict. They consume one `admit_use` rule now; this test
    /// drives both seams from the same fabricated receipt so an exit added to
    /// one side only cannot pass.
    #[test]
    fn both_seams_agree_on_every_admitted_and_refused_exit() {
        fn agreement(source: &str, outcome: JavaOutcome) -> (bool, bool) {
            let tree = parse(source);
            let root = tree.root_node();
            let unit = root
                .named_children(&mut root.walk())
                .find(|node| node.kind() == "function_declaration")
                .expect("fixture has a top-level function");
            let bindings = TypeBindings::build(source, root);
            let provider = fixture_provider(source);
            let receipt = call_receipt(source, outcome.clone());
            let returns = declaration_returns(
                source,
                unit,
                &bindings,
                &provider,
                crate::transpiler::types::AnnotationSet::None,
            );
            let mut stack = vec![tree.root_node()];
            let mut call = None;
            while let Some(node) = stack.pop() {
                if node.kind() == "call_expression" && call.is_none() {
                    call = Some(node);
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
            let context = classify_use(source, call.unwrap(), &returns);
            let site = SiteRecord {
                declaration: 0,
                name: "member".into(),
                location: receipt.location.clone(),
                receipt: Some(receipt.clone()),
                use_context: context,
                effects: None,
            };
            let abi = declaration_abi(
                source,
                unit,
                &bindings,
                &provider,
                crate::transpiler::types::AnnotationSet::None,
                std::slice::from_ref(&receipt),
            );
            (site.refusal().is_none(), abi.contract().is_some())
        }

        let stream_discard = "package p\n\nimport java.util.stream.Stream\n\nfun link(v: Stream<Int>) {\n    v.sorted()\n}\n";
        let object_exit = "package p\n\nimport java.util.stream.Stream\n\nfun opaque(v: Stream<Int>): Any? = v.sorted()\n";
        let string_exit = "package p\n\nimport java.io.File\n\nfun nullableName(f: File): String? = f.getName()\n";
        let strict_exit =
            "package p\n\nimport java.io.File\n\nfun strictName(f: File): String = f.getName()\n";
        let stream_exit = "package p\n\nimport java.util.stream.Stream\n\nfun nullableSorted(v: Stream<Int>): Stream<Int>? = v.sorted()\n";

        for (label, source, outcome, admitted) in [
            (
                "discarded stream",
                stream_discard,
                erased("java.util.stream.Stream"),
                true,
            ),
            (
                "nullable object",
                object_exit,
                erased("java.util.stream.Stream"),
                true,
            ),
            ("nullable string", string_exit, string_outcome(), true),
            ("non-null string", strict_exit, string_outcome(), false),
            (
                "nullable string/Object",
                string_exit,
                erased("java.lang.Object"),
                false,
            ),
            ("nullable string/array", string_exit, array_outcome(), false),
            (
                "nullable Stream exit",
                stream_exit,
                erased("java.util.stream.Stream"),
                false,
            ),
        ] {
            let (site_admits, walk_admits) = agreement(source, outcome);
            assert_eq!(
                site_admits, admitted,
                "{label}: the declaration inventory verdict changed"
            );
            assert_eq!(
                walk_admits, admitted,
                "{label}: the operation walk disagrees with the inventory"
            );
        }
    }

    #[test]
    fn member_rows_map_to_physical_representations() {
        assert_eq!(
            representation_from_row("<void>", "unknown"),
            Some(JavaOutcome::Void)
        );
        assert_eq!(
            representation_from_row("int", "unknown"),
            Some(JavaOutcome::Value(JavaRepresentation::primitive("int")))
        );
        assert!(matches!(
            representation_from_row("java.util.stream.Stream", "unknown"),
            Some(JavaOutcome::Value(JavaRepresentation {
                kind: ReprKind::ErasedReference,
                nullable: Nullability::Unknown,
                ..
            }))
        ));
        assert!(matches!(
            representation_from_row("[B", "unknown"),
            Some(JavaOutcome::Value(JavaRepresentation {
                erased,
                kind: ReprKind::Array(_),
                ..
            })) if erased == "byte[]"
        ));
        // No package separator ⇒ not a loadable reference spelling.
        assert_eq!(representation_from_row("Stream", "unknown"), None);
    }

    #[test]
    fn a_receipt_in_a_translated_span_must_be_consumed() {
        let receipt = receipt(erased_stream());
        let spans = [(0usize, 40usize)];
        let key = (receipt.location.start_byte, receipt.location.end_byte);

        let missing = unconsumed_receipts(
            std::slice::from_ref(&receipt),
            &spans,
            &std::collections::BTreeSet::new(),
        );
        assert_eq!(missing, vec!["sorted(java.util.stream.Stream)".to_string()]);

        let consumed = std::collections::BTreeSet::from([key]);
        assert!(unconsumed_receipts(std::slice::from_ref(&receipt), &spans, &consumed).is_empty());
    }

    /// A receipt outside every translated span belongs to a retained
    /// declaration: emission was not supposed to read it, so its absence from
    /// the consumed set is not a disagreement.
    #[test]
    fn a_receipt_in_a_retained_declaration_is_not_a_consumption_gap() {
        let receipt = receipt(erased_stream());
        let retained_spans = [(100usize, 140usize)];
        assert!(
            unconsumed_receipts(
                std::slice::from_ref(&receipt),
                &retained_spans,
                &std::collections::BTreeSet::new(),
            )
            .is_empty()
        );
    }

    // ---------------------------------------------------------------------
    // Totality, closed shape and resolution regressions.
    // ---------------------------------------------------------------------

    use crate::semantics::SyntaxSemanticProvider;

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    fn fixture_provider(source: &str) -> SyntaxSemanticProvider {
        SyntaxSemanticProvider::new([(std::path::PathBuf::from("Fixture.kt"), source.to_owned())])
    }

    fn abi(source: &str) -> DeclarationAbi {
        let tree = parse(source);
        let root = tree.root_node();
        let unit = root
            .named_children(&mut root.walk())
            .find(|node| {
                matches!(
                    node.kind(),
                    "function_declaration"
                        | "class_declaration"
                        | "interface_declaration"
                        | "property_declaration"
                )
            })
            .expect("fixture has a top-level declaration");
        let bindings = TypeBindings::build(source, root);
        let provider = fixture_provider(source);
        declaration_abi(
            source,
            unit,
            &bindings,
            &provider,
            crate::transpiler::types::AnnotationSet::None,
            &[],
        )
    }

    fn slot_detail(abi: &DeclarationAbi) -> Vec<String> {
        abi.unproved()
            .map(|slot| match &slot.proof {
                AbiProof::Unproved(detail) => detail.clone(),
                other => format!("proved {other:?}"),
            })
            .chain(abi.refusals().iter().map(|refusal| refusal.message.clone()))
            .collect()
    }

    fn assert_unproved(source: &str, needle: &str) {
        let abi = abi(source);
        assert!(
            abi.contract().is_none(),
            "a refused declaration must not produce a contract"
        );
        let details = slot_detail(&abi);
        assert!(
            details.iter().any(|detail| detail.contains(needle)),
            "expected a refusal containing {needle:?}, got {details:#?}"
        );
    }

    #[test]
    fn a_star_projection_is_unproved_and_never_recurses() {
        assert_unproved(
            "package p\n\nimport java.util.stream.Stream\n\nfun link(values: Stream<*>) {\n}\n",
            "star projection",
        );
    }

    #[test]
    fn a_function_type_argument_is_unproved() {
        // `Stream<() -> Unit>` used to recurse on the projection's own node.
        assert_unproved(
            "package p\n\nimport java.util.stream.Stream\n\nfun link(values: Stream<() -> Unit>) {\n}\n",
            "function type",
        );
    }

    #[test]
    fn a_variance_projection_still_proves_its_type() {
        let abi = abi(
            "package p\n\nimport java.util.stream.Stream\n\nfun link(values: Stream<out String>) {\n}\n",
        );
        assert!(
            abi.unproved().next().is_none(),
            "variance is a rendering detail of a proved argument: {:#?}",
            slot_detail(&abi)
        );
    }

    #[test]
    fn an_explicit_import_of_a_builtin_spelling_is_not_the_builtin() {
        // Kotlin resolves `Int` through the explicit import, so the emitted
        // `int` is a different descriptor. The emitter writes `int`, the proof
        // would be `foreign.Int`, and that disagreement must refuse.
        assert_unproved(
            "package p\n\nimport java.util.stream.Stream\nimport foreign.Int\n\nfun link(values: Stream<String>, unused: Int) {\n}\n",
            "emitter writes",
        );
    }

    #[test]
    fn a_qualified_typealias_path_is_not_a_java_class() {
        assert_unproved(
            "package p\n\nimport java.util.stream.Stream\n\nfun link(values: Stream<String>, unused: aliases.Int) {\n}\n",
            "java.`/`javax.",
        );
    }

    #[test]
    fn a_file_typealias_binds_its_name_away_from_the_builtin() {
        assert_unproved(
            "package p\n\ntypealias Any = String\n\nfun link(): Any? = null\n",
            "bound by a typealias",
        );
    }

    #[test]
    fn a_qualified_java_path_without_classpath_evidence_is_refused() {
        // The syntax provider has no classpath facts: a dotted spelling alone
        // is not proof that the class exists.
        assert_unproved(
            "package p\n\nfun link(unused: java.util.stream.Stream<String>) {\n}\n",
            "classpath evidence",
        );
    }

    #[test]
    fn a_literal_statement_has_no_lowering_contract() {
        assert_unproved(
            "package p\n\nfun link(values: java.util.stream.Stream<String>) {\n    1\n}\n",
            "no lowering contract",
        );
    }

    #[test]
    fn an_identifier_statement_has_no_lowering_contract() {
        assert_unproved(
            "package p\n\nfun link(values: java.util.stream.Stream<String>) {\n    values\n}\n",
            "no lowering contract",
        );
    }

    #[test]
    fn a_parenthesized_call_statement_has_no_lowering_contract() {
        // Java `(call);` is not a StatementExpression even though Kotlin
        // accepts a parenthesized expression as a statement.
        assert_unproved(
            "package p\n\nimport java.util.stream.Stream\n\nfun link(values: Stream<String>) {\n    (values.sorted())\n}\n",
            "no lowering contract",
        );
    }

    #[test]
    fn a_string_template_is_not_a_leaf() {
        // `$name` parses as string_content siblings, `${expr}` as
        // interpolation; neither is a call, so both are refused.
        for return_value in ["\"probe: $values\"", "\"probe: ${values}\""] {
            let source = format!(
                "package p\n\nimport java.util.stream.Stream\n\nfun link(values: Stream<Int>): Any? = {return_value}\n"
            );
            assert_unproved(&source, "no lowering contract");
        }
    }

    #[test]
    fn an_interface_parsed_as_a_class_cannot_acquire_a_contract() {
        let source = "package p\n\ninterface Endpoint {\n    fun touch() {\n    }\n}\n";
        let tree = parse(source);
        let root = tree.root_node();
        let unit = root.named_child(1).expect("interface declaration");
        assert_eq!(unit.kind(), "class_declaration");
        assert!(
            unit.children(&mut unit.walk())
                .any(|child| child.kind() == "interface")
        );
        assert_unproved(source, "interface");
    }

    #[test]
    fn comment_trivia_does_not_change_a_plain_declaration_contract() {
        for source in [
            "package p\n\nfun touch() {\n    // before\n    /* after */\n}\n",
            "package p\n\nclass Box {\n    // member\n    fun touch(\n        /* parameter */ unused: Int\n    ) {\n        // body\n    }\n}\n",
        ] {
            let proof = abi(source);
            assert!(
                proof.contract().is_some(),
                "comment trivia was treated as an operation: {:#?}",
                slot_detail(&proof)
            );
        }
    }

    #[test]
    fn function_modifiers_cannot_rely_on_unrelated_legacy_guards() {
        for modifier in ["suspend", "inline", "internal"] {
            let source = format!("package p\n\n{modifier} fun touch() {{\n}}\n");
            assert_unproved(&source, "function modifiers");
        }
    }

    #[test]
    fn a_class_with_a_primary_constructor_is_refused() {
        assert_unproved(
            "package p\n\nimport java.util.stream.Stream\n\nclass Box(val values: Stream<Int>) {\n    fun touch() {\n    }\n}\n",
            "primary constructor",
        );
    }

    #[test]
    fn a_class_with_a_property_initializer_is_refused() {
        assert_unproved(
            "package p\n\nclass Box {\n    fun touch() {\n        val sum = 1 + 2\n    }\n}\n",
            "no lowering contract",
        );
    }

    #[test]
    fn a_class_with_a_supertype_is_refused() {
        assert_unproved(
            "package p\n\nclass Box : Comparable<Box> {\n    override fun compareTo(other: Box): Int = 0\n}\n",
            "supertype",
        );
    }

    #[test]
    fn a_top_level_property_unit_is_refused() {
        assert_unproved("package p\n\nval answer: Int = 42\n", "top-level");
    }

    #[test]
    fn an_unsigned_call_in_a_required_declaration_has_no_contract() {
        let source = "package p\n\nimport java.util.stream.Stream\n\nfun link(values: Stream<String>) {\n    values.sorted()\n    values.distinct()\n}\n";
        let tree = parse(source);
        let root = tree.root_node();
        let unit = root
            .named_children(&mut root.walk())
            .find(|node| node.kind() == "function_declaration")
            .unwrap();
        let mut calls = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" {
                calls.push(node);
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        calls.sort_by_key(|call| call.start_byte());
        assert_eq!(calls.len(), 2, "fixture must contain both calls");
        let mut receipt = receipt(erased_stream());
        receipt.location.start_byte = calls[0].start_byte();
        receipt.location.end_byte = calls[0].end_byte();
        let bindings = TypeBindings::build(source, root);
        let provider = fixture_provider(source);
        let abi = declaration_abi(
            source,
            unit,
            &bindings,
            &provider,
            crate::transpiler::types::AnnotationSet::None,
            std::slice::from_ref(&receipt),
        );
        assert!(
            abi.refusals().iter().any(|refusal| {
                refusal.message.contains("`distinct`")
                    && refusal.message.contains("no lowering contract")
            }),
            "an unsigned sibling call must refuse the declaration: {:#?}",
            abi.refusals()
        );
    }

    #[test]
    fn an_extension_receiver_is_refused() {
        assert_unproved(
            "package p\n\nimport java.util.stream.Stream\n\nfun Stream<Int>.sorted2() {\n}\n",
            "extension receiver",
        );
    }

    #[test]
    fn a_defaulted_parameter_is_refused() {
        assert_unproved(
            "package p\n\nfun link(unused: Int = 1) {\n}\n",
            "default value",
        );
    }

    #[test]
    fn a_vararg_parameter_is_refused() {
        assert_unproved(
            "package p\n\nfun link(vararg names: String) {\n}\n",
            "modifiers",
        );
    }

    #[test]
    fn every_named_grammar_kind_is_classified_explicitly() {
        // The grammar's own node-types.json is the source of truth: the
        // admitted set must be exactly the kinds this slice decided, and every
        // admitted kind must still exist there. No substring test may silently
        // admit a future `…type…` or `…modifier…` kind.
        let node_types = tree_sitter_kotlin_ng::NODE_TYPES;
        let mut named = std::collections::BTreeSet::new();
        for block in node_types.split("\"type\": \"").skip(1) {
            let Some(end) = block.find('"') else { continue };
            let kind = &block[..end];
            let rest = &block[end..];
            let named_here = rest
                .split("\"named\": ")
                .nth(1)
                .is_some_and(|flag| flag.starts_with("true"));
            if named_here {
                named.insert(kind.to_string());
            }
        }
        assert!(
            named.len() > 100,
            "the grammar dump did not parse ({} kinds)",
            named.len()
        );
        let admitted: std::collections::BTreeSet<&str> = named
            .iter()
            .map(String::as_str)
            .filter(|kind| kind_policy(kind) != KindPolicy::Refuse)
            .collect();
        // `statements`/`interface_declaration` are policy decisions for shapes
        // this grammar version does not expose as named kinds; the comparison
        // is against the grammar's own set so a new kind cannot slip in.
        let expected: std::collections::BTreeSet<&str> = [
            "function_declaration",
            "class_declaration",
            "interface_declaration",
            "function_body",
            "block",
            "statements",
            "return_expression",
            "call_expression",
            "parenthesized_expression",
            "line_comment",
            "block_comment",
        ]
        .into_iter()
        .filter(|kind| named.contains(*kind))
        .collect();
        assert_eq!(
            admitted, expected,
            "the admitted kind set changed; classify the new kind explicitly"
        );
        for kind in [
            "string_literal",
            "number_literal",
            "identifier",
            "binary_expression",
            "navigation_expression",
            "if_expression",
            "when_expression",
            "lambda_literal",
            "assignment",
            "property_declaration",
            "type_projection",
            "annotation",
            "type_parameter",
        ] {
            assert_eq!(
                kind_policy(kind),
                KindPolicy::Refuse,
                "`{kind}` must be refused explicitly"
            );
        }
    }

    #[test]
    fn descriptors_are_checked_against_the_recorded_outcome() {
        assert_eq!(descriptor_arity("()Ljava/util/stream/Stream;"), Some(0));
        assert_eq!(descriptor_arity("(ILjava/lang/String;[B)V"), Some(3));
        assert_eq!(descriptor_arity("(Lfoo/Bar;)V"), Some(1));
        assert_eq!(descriptor_arity("not-a-descriptor"), None);

        let stream = JavaOutcome::Value(JavaRepresentation::erased_reference(
            "java.util.stream.Stream",
            Nullability::Unknown,
        ));
        assert!(descriptor_result_matches(
            &stream,
            "()Ljava/util/stream/Stream;"
        ));
        assert!(!descriptor_result_matches(&stream, "()Ljava/lang/String;"));
        assert!(descriptor_result_matches(&JavaOutcome::Void, "()V"));
        assert!(descriptor_result_matches(
            &JavaOutcome::Value(JavaRepresentation::primitive("int")),
            "()I"
        ));
        assert!(!descriptor_result_matches(
            &JavaOutcome::Value(JavaRepresentation::primitive("int")),
            "()J"
        ));
    }

    #[test]
    fn a_receipt_the_walk_never_visits_is_a_coverage_gap() {
        let source = "package p\n\nimport java.util.stream.Stream\n\nfun link(values: Stream<String>) {\n    values.sorted()\n}\n";
        let tree = parse(source);
        let root = tree.root_node();
        let unit = root
            .named_children(&mut root.walk())
            .find(|node| node.kind() == "function_declaration")
            .unwrap();
        // A receipt recorded for a span the operation walk never visits (here
        // the parameter type node) is not coverage: it must retain rather than
        // pass.
        let parameter_type = {
            let mut stack = vec![root];
            let mut found = None;
            while let Some(node) = stack.pop() {
                if node.kind() == "user_type" {
                    found = Some(node);
                }
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
            found.unwrap()
        };
        let mut receipt = receipt(erased_stream());
        receipt.location.start_byte = parameter_type.start_byte();
        receipt.location.end_byte = parameter_type.end_byte();
        let bindings = TypeBindings::build(source, root);
        let provider = fixture_provider(source);
        let abi = declaration_abi(
            source,
            unit,
            &bindings,
            &provider,
            crate::transpiler::types::AnnotationSet::None,
            std::slice::from_ref(&receipt),
        );
        assert!(
            abi.refusals()
                .iter()
                .any(|refusal| refusal.message.contains("does not denote")),
            "unexpected refusals: {:#?}",
            abi.refusals()
        );
    }
}
