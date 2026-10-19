//! Source-backed inherited call lookup when a nearer overload needs arguments.
//!
//! Name-only lookup remains useful for shadowing proofs. Calls need a separate
//! search: a required-argument overload does not hide a farther no-argument one.

use super::{
    CallableShape, FactStatus, KotlinType, ReferenceReturnNullability, SemanticProvider, SymbolId,
    SyntaxSemanticProvider, TypeRef,
};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn outside_constructor_header(call: tree_sitter::Node<'_>) -> bool {
    let mut ancestor = call.parent();
    while let Some(node) = ancestor {
        match node.kind() {
            "primary_constructor" | "constructor_delegation_call" | "delegation_specifiers" => {
                return false;
            }
            "function_value_parameters"
                if node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "secondary_constructor") =>
            {
                return false;
            }
            "class_declaration" | "object_declaration" => return true,
            _ => {}
        }
        ancestor = node.parent();
    }
    false
}

pub(super) fn resolve(
    provider: &SyntaxSemanticProvider,
    owner: &SymbolId,
    name: &str,
) -> FactStatus<SymbolId> {
    if !provider.has_complete_inheritance(owner) {
        return FactStatus::Unknown;
    }
    let named = provider.symbols_named(name);
    let mut pending = vec![(owner.clone(), false)];
    let mut active = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut snapshots = BTreeMap::new();
    let mut identities = BTreeSet::new();
    let mut candidates = Vec::new();
    while let Some((current, exiting)) = pending.pop() {
        if exiting {
            active.remove(&current);
            visited.insert(current);
            continue;
        }
        if active.contains(&current) {
            return FactStatus::Unknown;
        }
        if visited.contains(&current) {
            continue;
        }
        // An ambiguous classifier cannot provide a unique member scope, even
        // if its duplicate declarations happen to contain different methods.
        if provider
            .symbols_named(&current.name)
            .iter()
            .filter(|symbol| symbol.id == current)
            .count()
            != 1
        {
            return FactStatus::Unknown;
        }
        active.insert(current.clone());
        pending.push((current.clone(), true));
        pending.extend(
            provider
                .parents
                .get(&current)
                .into_iter()
                .flatten()
                .cloned()
                .map(|parent| (parent, false)),
        );
        let mut path = current.owner_path.clone();
        path.push(current.name.clone());
        for member in named.iter().filter(|symbol| {
            symbol.id.file == current.file
                && symbol.id.module == current.module
                && symbol.id.package == current.package
                && symbol.id.owner_path == path
        }) {
            if current == *owner
                || !matches!(member.id.kind.as_str(), "function" | "method")
                || member.id.receiver.is_some()
                || !identities.insert(member.id.clone())
            {
                return FactStatus::Unknown;
            }
            let Some(source) = provider.source_for_file(&member.id.file) else {
                return FactStatus::Unknown;
            };
            let snapshot = snapshots
                .entry(&member.id.file)
                .or_insert_with(|| *blake3::hash(source.as_bytes()).as_bytes());
            if *snapshot != member.location.snapshot_hash {
                return FactStatus::Unknown;
            }
            let Some(CallableShape {
                arity: Some(arity),
                declares_throws,
            }) = provider
                .callable_shapes
                .get(&(member.id.file.clone(), member.location.start_byte))
            else {
                return FactStatus::Unknown;
            };
            if !arity.accepts_arity(0) {
                continue;
            }
            // A default or empty vararg is a competing source call, but its
            // Java lowering and overload priority need a separate proof.
            if arity.fixed != 0
                || arity.vararg
                || *declares_throws
                || !member.id.parameters.is_empty()
                || !provider.has_stable_noarg_jvm_signature(&member.id)
                || !provider.is_java_callable_member(&member.id)
                || matches!(
                    member.ty,
                    FactStatus::Established(TypeRef {
                        ty: KotlinType::TypeParameter(_),
                        ..
                    })
                )
            {
                return FactStatus::Unknown;
            }
            candidates.push(*member);
        }
    }
    // Only identical explicit return contracts with proven owner ancestry
    // suppress an ancestor. Unproven covariance stays ambiguous.
    let effective = provider.discard_overridden_inherited_methods(candidates);
    if effective.iter().any(|member| {
        member
            .id
            .file
            .extension()
            .is_some_and(|extension| extension == "java")
            && !primitive_return(&member.ty)
            && provider.reference_return_nullability(&member.id)
                != Some(ReferenceReturnNullability::NonNull)
    }) {
        // A newly resolved Java platform return could require a Kotlin
        // null check at the call site. Do not manufacture a non-null fact.
        return FactStatus::Unknown;
    }
    super::fact_from_candidates(
        effective
            .into_iter()
            .map(|symbol| symbol.id.clone())
            .collect(),
    )
}

fn primitive_return(ty: &FactStatus<TypeRef>) -> bool {
    matches!(
        ty,
        FactStatus::Established(TypeRef {
            ty: KotlinType::Primitive(_),
            nullable: false
        })
    ) || matches!(ty,
        FactStatus::Established(TypeRef {
            ty: KotlinType::Named { name, arguments }, nullable: false
        }) if arguments.is_empty() && matches!(name.as_str(),
            "byte" | "short" | "int" | "long" | "float" | "double" | "boolean" | "char" | "void")
    )
}
