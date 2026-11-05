//! Argument-taking super calls search the complete inherited overload family.
//! A nearer overload hides only the same bound parameter contract, never every
//! method with the same name. Ordinary source-call emission proves the ABI.

use super::{FactStatus, KotlinType, SemanticProvider, SymbolId, TypeRef};
use std::collections::BTreeSet;

pub(super) fn with_arguments<P: SemanticProvider + ?Sized>(
    source: &str,
    file: &std::path::Path,
    call: tree_sitter::Node<'_>,
    parents: Vec<SymbolId>,
    name: &str,
    provider: &P,
) -> FactStatus<SymbolId> {
    if provider.source_for_file(file) != Some(source)
        || super::overloads::positional_arguments(call).is_none()
    {
        return FactStatus::Unknown;
    }
    let arity = super::call_argument_count(call);
    let mut pending = parents;
    let mut seen = BTreeSet::new();
    let mut candidates = Vec::new();
    while let Some(owner) = pending.pop() {
        if !seen.insert(owner.clone()) {
            continue;
        }
        let FactStatus::Established(parents) = provider.instance_contract_parents(&owner) else {
            return FactStatus::Unknown;
        };
        pending.extend(parents);
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        for member in provider
            .symbols_named(name)
            .into_iter()
            .filter(|member| member.id.file == owner.file && member.id.owner_path == path)
        {
            if !matches!(member.id.kind.as_str(), "function" | "method") {
                return FactStatus::Unknown;
            }
            let Some(fixed) = provider.plain_fixed_callable_arity(&member.id) else {
                // Defaults and varargs can compete at a different source arity.
                return FactStatus::Unknown;
            };
            if fixed != arity {
                continue;
            }
            if !provider.is_public_instance_callable(&member.id)
                || !provider.has_plain_callable_parameters(&member.id)
            {
                return FactStatus::Unknown;
            }
            candidates.push((owner.clone(), member.id.clone()));
        }
    }
    let effective = candidates
        .iter()
        .filter(|(owner, member)| {
            !candidates.iter().any(|(other_owner, other)| {
                other_owner != owner
                    && provider.inherits_type(other_owner, owner)
                    && parameter_contract(member, provider)
                        .zip(parameter_contract(other, provider))
                        .is_some_and(|(left, right)| left == right)
            })
        })
        .map(|(_, member)| member.clone())
        .collect::<Vec<_>>();
    let selected = super::resolved_call_candidates(source, file, call, effective, provider);
    if let FactStatus::Established(target) = &selected
        && super::source_calls::ordinary_call(source, file, call, target, provider).is_none()
    {
        return FactStatus::Unknown;
    }
    selected
}

#[derive(PartialEq, Eq)]
enum Parameter {
    Builtin(String),
    Nominal(SymbolId),
}

fn parameter_contract<P: SemanticProvider + ?Sized>(
    member: &SymbolId,
    provider: &P,
) -> Option<Vec<Parameter>> {
    let source = provider.source_for_file(&member.file)?;
    member
        .parameters
        .iter()
        .map(|spelling| {
            let TypeRef {
                ty,
                nullable: false,
            } = TypeRef::parse(spelling)
            else {
                return None;
            };
            if let Some(descriptor) = provider.builtin_jvm_type_descriptor(&member.file, spelling) {
                return Some(Parameter::Builtin(descriptor.into()));
            }
            match ty {
                KotlinType::Primitive(_) => {
                    // Both declarations in the same file have identical bindings.
                    // Cross-file primitive signatures need the workspace proof.
                    None
                }
                KotlinType::Named { name, arguments } if arguments.is_empty() => {
                    if let Some(canonical) =
                        super::standard_external_type(source, provider, &name, false)
                    {
                        return Some(Parameter::Builtin(canonical));
                    }
                    let FactStatus::Established(owner) =
                        provider.resolve_type_in_file(&member.file, &member.package, &name)
                    else {
                        return None;
                    };
                    matches!(
                        provider.is_ordinary_jvm_reference(&owner),
                        FactStatus::Established(true)
                    )
                    .then_some(Parameter::Nominal(owner))
                }
                _ => None,
            }
        })
        .collect()
}
