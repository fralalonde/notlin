#[cfg(test)]
use super::SourceOverlay;
use super::{
    Declaration, DeclarationKind, Member, MemberKind, SourceFile, SourceIndex, SourceLanguage,
    getter_is_public, is_physical_zero_arg_getter, property_accessor_name,
};
use crate::semantics::{self, KotlinType, SymbolId, TypeArgument, TypeRef, Variance};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use tree_sitter::Node;

type BridgeOutcome = Result<Vec<CovariantDefaultGetterBridge>, String>;

#[derive(Debug, Clone, Default)]
struct BridgeDependencies {
    files: HashSet<String>,
    names: HashSet<String>,
}

thread_local! {
    static ACTIVE_DEPENDENCIES: RefCell<Vec<Rc<RefCell<BridgeDependencies>>>> = const { RefCell::new(Vec::new()) };
}

struct DependencyRecorder {
    dependencies: Rc<RefCell<BridgeDependencies>>,
}

impl DependencyRecorder {
    fn start() -> Self {
        let dependencies = Rc::new(RefCell::new(BridgeDependencies::default()));
        ACTIVE_DEPENDENCIES.with(|active| active.borrow_mut().push(dependencies.clone()));
        Self { dependencies }
    }

    fn finish(self) -> BridgeDependencies {
        self.dependencies.borrow().clone()
    }
}

impl Drop for DependencyRecorder {
    fn drop(&mut self) {
        ACTIVE_DEPENDENCIES.with(|active| {
            let mut active = active.borrow_mut();
            if active
                .last()
                .is_some_and(|current| Rc::ptr_eq(current, &self.dependencies))
            {
                active.pop();
            } else {
                active.retain(|current| !Rc::ptr_eq(current, &self.dependencies));
            }
        });
    }
}

fn record_file(path: &Path) {
    if !recording_active() {
        return;
    }
    let path = super::normalized_path(path);
    ACTIVE_DEPENDENCIES.with(|active| {
        for dependencies in active.borrow().iter() {
            dependencies.borrow_mut().files.insert(path.clone());
        }
    });
}

fn record_name(name: &str) {
    if !recording_active() {
        return;
    }
    let mut token_start = None;
    let flush = |start: usize, end: usize| {
        let value = name[start..end].trim_matches('.');
        if value.is_empty() {
            return;
        }
        let simple = value.rsplit('.').next().unwrap_or(value);
        ACTIVE_DEPENDENCIES.with(|active| {
            for dependencies in active.borrow().iter() {
                let mut dependencies = dependencies.borrow_mut();
                if !dependencies.names.contains(value) {
                    dependencies.names.insert(value.to_owned());
                }
                if !dependencies.names.contains(simple) {
                    dependencies.names.insert(simple.to_owned());
                }
            }
        });
    };
    for (index, character) in name.char_indices() {
        if character.is_alphanumeric() || character == '_' || character == '$' || character == '.' {
            token_start.get_or_insert(index);
        } else if let Some(start) = token_start.take() {
            flush(start, index);
        }
    }
    if let Some(start) = token_start {
        flush(start, name.len());
    }
}

fn merge_dependencies(dependencies: &BridgeDependencies) {
    if !recording_active() {
        return;
    }
    ACTIVE_DEPENDENCIES.with(|active| {
        for active in active.borrow().iter() {
            let mut active = active.borrow_mut();
            active.files.extend(dependencies.files.iter().cloned());
            active.names.extend(dependencies.names.iter().cloned());
        }
    });
}

fn recording_active() -> bool {
    ACTIVE_DEPENDENCIES.with(|active| !active.borrow().is_empty())
}

pub(super) fn record_source_path(path: &Path) {
    record_file(path);
}

pub(super) fn record_type_resolution(index: &SourceIndex, file: &SourceFile, type_name: &str) {
    if !recording_active() {
        return;
    }
    record_file(&file.path);
    record_name(type_name);
    let base = type_name
        .trim()
        .trim_end_matches('?')
        .split('<')
        .next()
        .unwrap_or("")
        .split('(')
        .next()
        .unwrap_or("")
        .trim();
    let simple = base.rsplit('.').next().unwrap_or(base);
    record_name(base);
    for (file_index, _) in index.declaration_names.get(simple).into_iter().flatten() {
        if let Some(candidate) = index.files.get(*file_index) {
            record_file(&candidate.path);
        }
    }
    for file_index in index.alias_names.get(simple).into_iter().flatten() {
        if let Some(candidate) = index.files.get(*file_index) {
            record_file(&candidate.path);
        }
    }
    for (file_index, _) in index.qualified_declarations.get(base).into_iter().flatten() {
        if let Some(candidate) = index.files.get(*file_index) {
            record_file(&candidate.path);
        }
    }
    if let Some(package) = &file.package {
        let qualified = format!("{package}.{simple}");
        for (file_index, _) in index
            .qualified_declarations
            .get(&qualified)
            .into_iter()
            .flatten()
        {
            if let Some(candidate) = index.files.get(*file_index) {
                record_file(&candidate.path);
            }
        }
    }
    for import in &file.imports {
        if let Some(package) = import.strip_suffix(".*") {
            let qualified = format!("{package}.{simple}");
            for (file_index, _) in index
                .qualified_declarations
                .get(&qualified)
                .into_iter()
                .flatten()
            {
                if let Some(candidate) = index.files.get(*file_index) {
                    record_file(&candidate.path);
                }
            }
        } else if import.ends_with(&format!(".{simple}")) {
            for (file_index, _) in index
                .qualified_declarations
                .get(import)
                .into_iter()
                .flatten()
            {
                if let Some(candidate) = index.files.get(*file_index) {
                    record_file(&candidate.path);
                }
            }
        }
    }
}

pub(super) fn record_alias_lookup(index: &SourceIndex, name: &str) {
    if !recording_active() {
        return;
    }
    record_name(name);
    let simple = name.rsplit('.').next().unwrap_or(name);
    for candidate_name in std::iter::once(name).chain((simple != name).then_some(simple)) {
        for (file_index, _, _) in index.member_names.get(candidate_name).into_iter().flatten() {
            if let Some(file) = index.files.get(*file_index) {
                record_file(&file.path);
            }
        }
        for (file_index, _) in index
            .declaration_names
            .get(candidate_name)
            .into_iter()
            .flatten()
        {
            if let Some(file) = index.files.get(*file_index) {
                record_file(&file.path);
            }
        }
        for file_index in index.alias_names.get(candidate_name).into_iter().flatten() {
            if let Some(file) = index.files.get(*file_index) {
                record_file(&file.path);
            }
        }
    }
}

pub(super) fn record_declaration_lookup(declaration: &Declaration) {
    record_name(&declaration.name);
    record_name(&super::declaration_key(declaration));
}

#[derive(Debug, Default)]
struct BridgeCacheEntry {
    cell: OnceLock<BridgeOutcome>,
    dependencies: OnceLock<BridgeDependencies>,
}

type SharedBridgeEntry = Arc<BridgeCacheEntry>;

#[derive(Debug, Default)]
struct InferredPropertyEntry {
    cell: OnceLock<Option<(String, PathBuf)>>,
    dependencies: OnceLock<BridgeDependencies>,
}

#[derive(Debug, Default)]
pub(super) struct BridgeCache {
    entries: Mutex<std::collections::HashMap<SymbolId, SharedBridgeEntry>>,
    // Inference is snapshot-local. Its dependencies are still propagated to
    // bridge entries, which may survive a compatible overlay.
    inferred_properties:
        Mutex<std::collections::HashMap<(SymbolId, String), Arc<InferredPropertyEntry>>>,
}

impl BridgeCache {
    fn entry(&self, owner: SymbolId) -> SharedBridgeEntry {
        self.entries
            .lock()
            .expect("getter bridge cache poisoned")
            .entry(owner)
            .or_default()
            .clone()
    }

    pub(super) fn carry_compatible_entries(
        &self,
        previous: &Self,
        new_index: &SourceIndex,
        changed_files: &HashSet<String>,
        changed_names: &HashSet<String>,
    ) -> (usize, usize) {
        let previous = previous
            .entries
            .lock()
            .expect("getter bridge cache poisoned");
        let mut entries = self.entries.lock().expect("getter bridge cache poisoned");
        let mut reused = 0;
        let mut invalidated = 0;
        for (owner, entry) in previous.iter() {
            let compatible = entry.dependencies.get().is_some_and(|dependencies| {
                dependencies.files.is_disjoint(changed_files)
                    && dependencies.names.is_disjoint(changed_names)
            }) && unique_class_by_id(new_index, owner).is_some();
            if compatible {
                entries.insert(owner.clone(), entry.clone());
                reused += 1;
            } else {
                invalidated += 1;
            }
        }
        (reused, invalidated)
    }
}

fn unique_class_by_id<'a>(index: &'a SourceIndex, owner: &SymbolId) -> Option<&'a Declaration> {
    let file = index.source_file(Path::new(&owner.file))?;
    let mut declarations = file.declarations.iter().filter(|declaration| {
        declaration.kind == DeclarationKind::Class
            && semantics::workspace_symbol(index, declaration) == *owner
    });
    let declaration = declarations.next()?;
    declarations.next().is_none().then_some(declaration)
}

fn unique_parent<'a>(
    index: &'a SourceIndex,
    file: &SourceFile,
    supertype: &str,
) -> Result<Option<&'a Declaration>, String> {
    let Some(parent) = index.resolve_type(file, supertype) else {
        return Ok(None);
    };
    let qualified = super::declaration_key(parent);
    if semantics::workspace_symbol(index, parent)
        .kind
        .starts_with("ambiguous:")
        || index
            .qualified_declarations
            .get(&qualified)
            .is_some_and(|candidates| candidates.len() != 1)
    {
        return Err(format!(
            "inherited getter type `{qualified}` has ambiguous declarations"
        ));
    }
    Ok(Some(parent))
}

/// A concrete implementation needed to preserve a covariant inherited default
/// getter when Java resolves a wider abstract declaration from another branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CovariantDefaultGetterBridge {
    pub owner: SymbolId,
    /// The direct interface qualifier used by `Provider.super.getX()`.
    pub provider_owner: SymbolId,
    /// The interface that actually declares the selected effective default.
    pub target: SymbolId,
    /// Every unrelated inherited interface declaration participating in the
    /// JVM contract. Consumers can use this to distinguish a bridge that
    /// resolves interface inheritance from one that would override a class.
    pub contract_owners: Vec<SymbolId>,
    /// Inherited property declarations among the contracts. A retained Kotlin
    /// property cannot be overridden by a source method bridge.
    pub property_contract_owners: Vec<SymbolId>,
    pub method_name: String,
    /// The exact source FQName of the selected default's reference return.
    pub return_type: String,
    pub return_type_ref: TypeRef,
    /// The selected default's own body when it was proven to be a chain of
    /// public zero-argument accessors rooted at implicit `this`. `None` means
    /// this bridge can only work by inheriting Java method semantics from
    /// `target`.
    pub body: Option<ForwardingGetterBody>,
}

/// One accessor call of a source-forwarding default getter body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForwardingGetterStep {
    /// Java method name called on the receiver of this step (`getEnvelope`).
    pub accessor: String,
}

/// The whole body of a Kotlin default getter proven to be a chain of public
/// zero-argument accessors rooted at implicit `this`.
///
/// Re-creating that chain in a Java child evaluates the same accessors on the
/// same receivers in the same order, so dynamic dispatch, evaluation order and
/// getter side effects are preserved without depending on how the retained
/// Kotlin provider was compiled (`-jvm-default` mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForwardingGetterBody {
    /// Accessor calls in evaluation order, starting from `this`.
    pub steps: Vec<ForwardingGetterStep>,
}

#[derive(Clone)]
struct Getter<'a> {
    owner: &'a Declaration,
    member: &'a Member,
    return_type: Cow<'a, str>,
}

#[derive(Clone)]
struct GetterContract<'a> {
    owner: &'a Declaration,
    member: &'a Member,
    canonical_return: Option<super::CanonicalGetterType>,
    erased_return: Option<super::CanonicalGetterType>,
}

#[derive(Clone)]
struct InstantiatedOwner<'a> {
    declaration: &'a Declaration,
    bindings: BTreeMap<String, TypeRef>,
}

fn inherited_raw_bindings(
    index: &SourceIndex,
    parent: &Declaration,
    supertype: &str,
    edge_owner: &Declaration,
    current: &BTreeMap<String, TypeRef>,
) -> Option<BTreeMap<String, TypeRef>> {
    if parent.type_params.is_empty() {
        return Some(BTreeMap::new());
    }
    let (_, arguments) = super::split_generic(supertype.trim().trim_end_matches("()"));
    let arguments = arguments?;
    if arguments.len() != parent.type_params.len() {
        return None;
    }
    parent
        .type_params
        .iter()
        .zip(arguments)
        .map(|(parameter, argument)| {
            let argument = TypeRef::parse_with_type_parameters(argument, &edge_owner.type_params)
                .substitute(current);
            let file = index.declaration_source_file(edge_owner)?;
            let aliases = file
                .type_aliases
                .iter()
                .map(|(name, target)| {
                    (
                        name.clone(),
                        TypeRef::parse_with_type_parameters(target, &edge_owner.type_params),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let argument = argument.expand_aliases(&aliases);
            Some((
                parameter.clone(),
                normalize_type_ref_names(index, edge_owner, argument)?,
            ))
        })
        .collect::<Option<BTreeMap<_, _>>>()
}

fn scoped_type_ref_bindings(index: &SourceIndex, owner: &Declaration) -> BTreeMap<String, TypeRef> {
    let scope = semantics::workspace_symbol(index, owner).stable_key();
    owner
        .type_params
        .iter()
        .map(|parameter| {
            (
                parameter.clone(),
                TypeRef {
                    ty: KotlinType::TypeParameter(format!("{scope}#{parameter}")),
                    nullable: false,
                },
            )
        })
        .collect()
}

fn normalize_type_ref_names(
    index: &SourceIndex,
    owner: &Declaration,
    source: TypeRef,
) -> Option<TypeRef> {
    fn normalize(index: &SourceIndex, owner: &Declaration, ty: KotlinType) -> Option<KotlinType> {
        Some(match ty {
            KotlinType::Named { name, arguments } => {
                let file = index.declaration_source_file(owner)?;
                let identity = name
                    .strip_prefix("$canonical$")
                    .map(str::to_owned)
                    .or_else(|| super::canonical_getter_named_identity(index, file, &name))?;
                KotlinType::Named {
                    name: format!("$canonical${identity}"),
                    arguments: arguments
                        .into_iter()
                        .map(|argument| {
                            Some(TypeArgument {
                                variance: argument.variance,
                                ty: match argument.ty {
                                    Some(ty) => Some(normalize(index, owner, ty)?),
                                    None => None,
                                },
                            })
                        })
                        .collect::<Option<Vec<_>>>()?,
                }
            }
            KotlinType::Primitive(name) => {
                let file = index.declaration_source_file(owner)?;
                let identity = super::canonical_getter_named_identity(index, file, &name)?;
                KotlinType::Named {
                    name: format!("$canonical${identity}"),
                    arguments: Vec::new(),
                }
            }
            KotlinType::Nullable(inner) => {
                KotlinType::Nullable(Box::new(normalize(index, owner, *inner)?))
            }
            KotlinType::Array(inner) => {
                KotlinType::Array(Box::new(normalize(index, owner, *inner)?))
            }
            KotlinType::Function { .. } | KotlinType::Unknown(_) => return None,
            other => other,
        })
    }
    Some(TypeRef {
        ty: normalize(index, owner, source.ty)?,
        nullable: source.nullable,
    })
}

fn canonical_return(
    index: &SourceIndex,
    owner: &Declaration,
    source_type: &str,
    bindings: &BTreeMap<String, TypeRef>,
) -> Option<super::CanonicalGetterType> {
    let file = index.declaration_source_file(owner)?;
    let aliases = file
        .type_aliases
        .iter()
        .map(|(name, target)| {
            (
                name.clone(),
                TypeRef::parse_with_type_parameters(target, &owner.type_params),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let source = TypeRef::parse_with_type_parameters(source_type, &owner.type_params)
        .substitute(bindings)
        .expand_aliases(&aliases);
    let source = normalize_type_ref_names(index, owner, source)?;
    canonical_type_ref(index, owner, &source)
}

fn erased_return(
    index: &SourceIndex,
    owner: &Declaration,
    member: &Member,
) -> Option<super::CanonicalGetterType> {
    use super::CanonicalGetterType as C;
    let return_type = getter_return_type(index, owner, member)?;
    let raw = return_type.trim();
    let base = raw.trim_end_matches('?').trim();
    let canonical = if owner.type_params.iter().any(|parameter| parameter == base) {
        let owner_file = index.declaration_source_file(owner)?;
        let bound = match crate::property_abi::inline_type_parameter_upper_bound(index, owner, base)
        {
            Some(bound) => bound,
            // The bound helper returns None for unbounded parameters and for
            // `where` clauses. Only the former is known to erase to Any.
            None if owner.language == SourceLanguage::Kotlin
                && !owner_file.source_text().contains("where") =>
            {
                "Any".to_owned()
            }
            None => return None,
        };
        canonical_return(index, owner, &bound, &BTreeMap::new())?
    } else {
        canonical_return(index, owner, raw, &BTreeMap::new())?
    };
    match canonical {
        C::Named(identity, _) => Some(C::Named(identity, Vec::new())),
        C::Nullable(inner) => match *inner {
            C::Named(identity, _) => Some(C::Named(identity, Vec::new())),
            _ => None,
        },
        _ => None,
    }
}

fn canonical_type_ref(
    index: &SourceIndex,
    owner: &Declaration,
    source: &TypeRef,
) -> Option<super::CanonicalGetterType> {
    fn type_node(
        index: &SourceIndex,
        owner: &Declaration,
        ty: &KotlinType,
    ) -> Option<super::CanonicalGetterType> {
        use super::CanonicalGetterType as C;
        match ty {
            KotlinType::TypeParameter(parameter) => {
                if parameter.contains('#') {
                    Some(C::ScopedParameter(parameter.clone()))
                } else {
                    Some(C::ScopedParameter(format!(
                        "{}#{parameter}",
                        semantics::workspace_symbol(index, owner).stable_key()
                    )))
                }
            }
            KotlinType::Primitive(name) => {
                let file = index.declaration_source_file(owner)?;
                Some(C::Named(
                    super::canonical_getter_named_identity(index, file, name)?,
                    Vec::new(),
                ))
            }
            KotlinType::Named { name, arguments } => {
                let file = index.declaration_source_file(owner)?;
                let identity = name
                    .strip_prefix("$canonical$")
                    .map(str::to_owned)
                    .or_else(|| super::canonical_getter_named_identity(index, file, name))?;
                let arguments = arguments
                    .iter()
                    .map(|argument| {
                        let value = type_node(index, owner, argument.ty.as_ref()?)?;
                        Some(match argument.variance {
                            Variance::Invariant => value,
                            Variance::In => C::Projection("in".into(), Box::new(value)),
                            Variance::Out => C::Projection("out".into(), Box::new(value)),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(C::Named(identity, arguments))
            }
            KotlinType::Nullable(inner) => {
                Some(C::Nullable(Box::new(type_node(index, owner, inner)?)))
            }
            KotlinType::Array(_) | KotlinType::Function { .. } | KotlinType::Unknown(_) => None,
        }
    }
    let canonical = type_node(index, owner, &source.ty)?;
    Some(if source.nullable {
        super::CanonicalGetterType::Nullable(Box::new(canonical))
    } else {
        canonical
    })
}

fn instantiated_routes<'a>(
    index: &'a SourceIndex,
    start: InstantiatedOwner<'a>,
    fallback_file: &'a SourceFile,
    profile: &mut super::profile::GetterTraversal,
) -> (Vec<InstantiatedOwner<'a>>, bool) {
    let mut pending = vec![start];
    let mut routes = Vec::new();
    let mut visited = HashSet::<(SymbolId, String)>::new();
    let mut incomplete = false;
    let mut explored = 0usize;
    while let Some(state) = pending.pop() {
        profile.visit();
        explored += 1;
        if explored > 256 {
            return (routes, true);
        }
        let identity = semantics::workspace_symbol(index, state.declaration);
        let binding_key = format!("{:?}", state.bindings);
        if !visited.insert((identity, binding_key)) {
            continue;
        }
        let declaration_file = index
            .declaration_source_file(state.declaration)
            .unwrap_or(fallback_file);
        for supertype in &state.declaration.supertypes {
            if external_root(supertype) {
                continue;
            }
            let Some(parent) = index.resolve_type(declaration_file, supertype) else {
                if !known_external_without_getters(index, declaration_file, supertype) {
                    incomplete = true;
                }
                continue;
            };
            let Some(bindings) = inherited_raw_bindings(
                index,
                parent,
                supertype,
                state.declaration,
                &state.bindings,
            ) else {
                incomplete = true;
                continue;
            };
            pending.push(InstantiatedOwner {
                declaration: parent,
                bindings,
            });
        }
        routes.push(state);
    }
    (routes, incomplete)
}

fn canonical_reference_subtype(
    index: &SourceIndex,
    implementation: &super::CanonicalGetterType,
    contract: &super::CanonicalGetterType,
) -> bool {
    let super::CanonicalGetterType::Named(implementation, implementation_args) = implementation
    else {
        return false;
    };
    let contract = match contract {
        super::CanonicalGetterType::Nullable(inner) => inner.as_ref(),
        other => other,
    };
    let super::CanonicalGetterType::Named(contract, contract_args) = contract else {
        return false;
    };
    if !implementation_args.is_empty() || !contract_args.is_empty() {
        return false;
    }
    if contract == "kotlin.Any" || contract == "external:java.lang.Object" {
        return true;
    }
    let (Some(implementation), Some(contract)) = (
        source_declaration_for_identity(index, implementation),
        source_declaration_for_identity(index, contract),
    ) else {
        return false;
    };
    let Some(fallback) = index.files.first() else {
        return false;
    };
    super::proven_reference_subtype(
        index,
        index
            .declaration_source_file(implementation)
            .unwrap_or(fallback),
        &super::declaration_key(implementation),
        index.declaration_source_file(contract).unwrap_or(fallback),
        &super::declaration_key(contract),
    )
}

fn source_reference_declaration<'a>(
    index: &'a SourceIndex,
    ty: &super::CanonicalGetterType,
) -> Option<&'a Declaration> {
    let super::CanonicalGetterType::Named(identity, arguments) = ty else {
        return None;
    };
    if !arguments.is_empty() {
        return None;
    }
    source_declaration_for_identity(index, identity)
}

fn source_declaration_for_identity<'a>(
    index: &'a SourceIndex,
    identity: &str,
) -> Option<&'a Declaration> {
    let symbol = serde_json::from_str::<SymbolId>(identity).ok()?;
    let file = index.source_file(&symbol.file)?;
    let mut matching = file
        .declarations
        .iter()
        .filter(|declaration| semantics::workspace_symbol(index, declaration) == symbol);
    let declaration = matching.next()?;
    matching.next().is_none().then_some(declaration)
}

fn getter_return_type<'a>(
    index: &SourceIndex,
    owner: &Declaration,
    member: &'a Member,
) -> Option<Cow<'a, str>> {
    member.type_name.as_deref().map(Cow::Borrowed).or_else(|| {
        index
            .inferred_simple_property_type(owner, member)
            .map(|(ty, _)| Cow::Owned(ty))
    })
}

fn source_getter<'a>(
    index: &SourceIndex,
    owner: &'a Declaration,
    member: &'a Member,
) -> Option<Getter<'a>> {
    if member.is_static
        || member.visibility.as_deref() == Some("private")
        || !getter_is_public(member, owner)
    {
        return None;
    }
    let method_name = if member.kind == MemberKind::Property {
        property_accessor_name(&member.name)
    } else {
        member.name.clone()
    };
    let return_type = getter_return_type(index, owner, member)?;
    match member.kind {
        MemberKind::Property => {
            let ty = return_type.trim();
            if ty.is_empty() || matches!(ty, "Unit" | "kotlin.Unit" | "void") {
                return None;
            }
        }
        MemberKind::Method if !is_physical_zero_arg_getter(member, &method_name) => return None,
        MemberKind::Method => {}
        _ => return None,
    }
    Some(Getter {
        owner,
        member,
        return_type,
    })
}

/// Prove that a Kotlin default getter body is exactly a chain of public
/// zero-argument property accessors rooted at implicit `this`.
///
/// This is the narrowest shape a Java child can re-create without knowing how
/// the retained Kotlin provider was compiled: every step is a virtual accessor
/// call, so the dispatched methods, their order and their side effects are the
/// ones the Kotlin default already performs. Everything else stays unproven:
/// annotations (including accessor renames), non-public or static members,
/// mutable or nullable steps, safe calls, `!!`, calls, casts, `super`/outer or
/// labelled receivers, and anything generic.
fn proven_forwarding_getter_body(
    index: &SourceIndex,
    target: &Declaration,
    member: &Member,
    expected_return: &super::CanonicalGetterType,
) -> Option<ForwardingGetterBody> {
    if target.language != SourceLanguage::Kotlin || !target.type_params.is_empty() {
        return None;
    }
    let file = index.declaration_source_file(target)?;
    // The proof reads this snapshot's text, so a later edit must invalidate the
    // cached bridge instead of reusing a body that no longer exists.
    record_file(&file.path);
    let symbol = semantics::workspace_symbol(index, target);
    if symbol.kind.starts_with("ambiguous:") {
        return None;
    }
    let source = file.source_text();
    let tree = crate::transpiler::parse_tree(source);
    let declaration = exact_declaration_node(tree.root_node(), source, &file.path, &symbol)?;
    let member_node = exact_member_node(declaration, source, member)?;
    if member_has_annotations(member_node) {
        return None;
    }
    let names = accessor_chain(expression_body(member_node, source)?, source)?;
    let mut owner = target;
    let mut steps = Vec::with_capacity(names.len());
    for (position, name) in names.iter().enumerate() {
        let accessor = property_accessor_name(name);
        let candidates = owner
            .members
            .iter()
            .filter(|candidate| match candidate.kind {
                MemberKind::Property => candidate.name == *name,
                // A property with a block accessor body is indexed as its
                // physical getter method. Both shapes perform the same JVM
                // call, so a step is resolvable through either.
                MemberKind::Method => {
                    candidate.name == accessor && is_physical_zero_arg_getter(candidate, &accessor)
                }
                _ => false,
            })
            .collect::<Vec<_>>();
        // Inherited, shadowed or duplicated: the accessor the Kotlin default
        // resolves is not this declaration's, so refuse to guess.
        let [property] = candidates.as_slice() else {
            return None;
        };
        let property = *property;
        if position == 0 && property.name == member.name {
            // The default would read itself through the bridge.
            return None;
        }
        if step_declaration_has_annotations(index, owner, property) {
            // An annotation may rename the accessor (`@get:JvmName`) or carry a
            // framework contract a re-created body would silently drop.
            return None;
        }
        if property.is_static
            || property.is_mutable
            || property.is_jvm_field
            || property.is_nullable
            || !getter_is_public(property, owner)
            || !plain_accessor_name(&accessor)
        {
            return None;
        }
        let declared = getter_return_type(index, owner, property)?;
        let declared = declared.trim();
        // Generic and nullable spellings need projection or null handling this
        // slice cannot prove; decline instead of manufacturing a cast.
        if declared.is_empty() || declared.ends_with('?') || declared.contains('<') {
            return None;
        }
        let canonical = canonical_return(index, owner, declared, &BTreeMap::new())?;
        steps.push(ForwardingGetterStep { accessor });
        if position + 1 == names.len() {
            if canonical != *expected_return {
                return None;
            }
        } else {
            owner = source_reference_declaration(index, &canonical)?;
        }
    }
    Some(ForwardingGetterBody { steps })
}

/// The one class-like syntax node whose exact symbol identity is `symbol`.
fn exact_declaration_node<'t>(
    root: Node<'t>,
    source: &str,
    file: &Path,
    symbol: &SymbolId,
) -> Option<Node<'t>> {
    let mut pending = vec![root];
    let mut matches = Vec::new();
    while let Some(node) = pending.pop() {
        pending.extend(node.named_children(&mut node.walk()));
        if !matches!(
            node.kind(),
            "class_declaration"
                | "interface_declaration"
                | "object_declaration"
                | "enum_declaration"
        ) {
            continue;
        }
        if semantics::symbol_id_for_node(source, node, file) == *symbol {
            matches.push(node);
        }
    }
    let [declaration] = matches.as_slice() else {
        return None;
    };
    Some(*declaration)
}

/// The one member declaration node inside `declaration` that is exactly
/// `member`. Nested types own their own members and are never searched.
fn exact_member_node<'t>(declaration: Node<'t>, source: &str, member: &Member) -> Option<Node<'t>> {
    let body = declaration
        .named_children(&mut declaration.walk())
        .find(|child| child.kind() == "class_body")?;
    let mut pending = vec![body];
    let mut matches = Vec::new();
    while let Some(node) = pending.pop() {
        if node.id() != body.id()
            && matches!(
                node.kind(),
                "class_declaration" | "interface_declaration" | "object_declaration"
            )
        {
            continue;
        }
        let matching = match (member.kind, node.kind()) {
            (MemberKind::Property, "property_declaration") => {
                property_name(node, source) == Some(member.name.as_str())
            }
            (MemberKind::Method, "function_declaration") => {
                member_function_name(node, source) == Some(member.name.as_str())
                    && function_parameter_count(node) == 0
            }
            _ => false,
        };
        if matching {
            matches.push(node);
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    let [node] = matches.as_slice() else {
        return None;
    };
    Some(*node)
}

fn property_name<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    let declaration = node
        .named_children(&mut node.walk())
        .find(|child| child.kind() == "variable_declaration")?;
    let identifier = declaration
        .named_children(&mut declaration.walk())
        .find(|child| child.kind() == "identifier")?;
    identifier.utf8_text(source.as_bytes()).ok()
}

fn member_function_name<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    node.child_by_field_name("name")?
        .utf8_text(source.as_bytes())
        .ok()
}

fn function_parameter_count(node: Node<'_>) -> usize {
    node.named_children(&mut node.walk())
        .find(|child| child.kind() == "function_value_parameters")
        .map_or(0, |parameters| {
            parameters
                .named_children(&mut parameters.walk())
                .filter(|child| matches!(child.kind(), "parameter" | "spread_parameter"))
                .count()
        })
}

/// Any annotation on the declaring member keeps the body unproven: accessor
/// renames (`@get:JvmName`), `@JvmField` storage and framework contracts that
/// expect the annotated method must not be re-created without it.
fn member_has_annotations(member: Node<'_>) -> bool {
    let mut cursor = member.walk();
    member
        .children(&mut cursor)
        .take_while(|child| !matches!(child.kind(), "getter" | "function_body" | "class_body"))
        .any(|child| {
            matches!(child.kind(), "annotation" | "annotations")
                || (child.kind() == "modifiers" && {
                    let mut modifiers = child.walk();
                    child
                        .named_children(&mut modifiers)
                        .any(|modifier| matches!(modifier.kind(), "annotation" | "annotations"))
                })
        })
}

/// Whether the exact source declaration of one forwarding step carries any
/// annotation. An unresolvable declaration counts as annotated: the proof
/// never falls back to a same-named declaration elsewhere.
fn step_declaration_has_annotations(
    index: &SourceIndex,
    owner: &Declaration,
    property: &Member,
) -> bool {
    let Some(file) = index.declaration_source_file(owner) else {
        return true;
    };
    record_file(&file.path);
    let symbol = semantics::workspace_symbol(index, owner);
    if symbol.kind.starts_with("ambiguous:") {
        return true;
    }
    let source = file.source_text();
    let tree = crate::transpiler::parse_tree(source);
    let Some(declaration) = exact_declaration_node(tree.root_node(), source, &file.path, &symbol)
    else {
        return true;
    };
    let Some(node) = exact_member_node(declaration, source, property) else {
        return true;
    };
    member_has_annotations(node)
}

/// The single expression a getter body `= expr` evaluates. Block bodies and
/// non-value bodies are unproven: they may contain arbitrary statements.
fn expression_body<'t>(member: Node<'t>, _source: &str) -> Option<Node<'t>> {
    let holder = if member.kind() == "property_declaration" {
        member
            .named_children(&mut member.walk())
            .find(|child| child.kind() == "getter")?
    } else {
        member
    };
    let body = holder
        .named_children(&mut holder.walk())
        .find(|child| child.kind() == "function_body")?;
    let mut cursor = body.walk();
    let mut expressions = body.named_children(&mut cursor);
    let expression = expressions.next()?;
    if expressions.next().is_some() || matches!(expression.kind(), "block") {
        return None;
    }
    Some(expression)
}

/// Flatten `a.b.c`, `this.a.b` into its identifier steps. Anything that is not
/// plain `.` member access (calls, safe calls, `!!`, casts, labels, `super`,
/// outer receivers, indexing) is unproven.
fn accessor_chain(node: Node<'_>, source: &str) -> Option<Vec<String>> {
    match node.kind() {
        "identifier" => {
            let name = node.utf8_text(source.as_bytes()).ok()?;
            plain_identifier(name).then(|| vec![name.to_owned()])
        }
        "this_expression" => {
            let text = node.utf8_text(source.as_bytes()).ok()?;
            (text.trim() == "this" && node.named_child_count() == 0).then(Vec::new)
        }
        "navigation_expression" => {
            let mut receiver = None;
            let mut member = None;
            for index in 0..node.child_count() {
                let child = node.child(index)?;
                match child.kind() {
                    "." => {}
                    "identifier" if receiver.is_some() && member.is_none() => member = Some(child),
                    "identifier" | "this_expression" | "navigation_expression"
                        if receiver.is_none() =>
                    {
                        receiver = Some(child);
                    }
                    // `?.`, `!!`, call suffixes and every other token shape.
                    _ => return None,
                }
            }
            let name = member?.utf8_text(source.as_bytes()).ok()?;
            if !plain_identifier(name) {
                return None;
            }
            let mut chain = accessor_chain(receiver?, source)?;
            chain.push(name.to_owned());
            Some(chain)
        }
        _ => None,
    }
}

fn plain_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_alphabetic())
        && characters.all(|character| character == '_' || character.is_alphanumeric())
}

/// A plain JavaBean accessor spelling. Kotlin renames an accessor through
/// `@get:JvmName`, which the annotation guard already rejects; this keeps a
/// mangled or non-getter name from reaching the emitter. The spelling must be
/// Java's ASCII identifier alphabet as well: the Kotlin proof may read a
/// Unicode property name, but the Java emitter cannot re-create that accessor,
/// so it has to be declined here rather than asserted at emission time.
fn plain_accessor_name(accessor: &str) -> bool {
    let mut characters = accessor.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
        && accessor
            .strip_prefix("get")
            .or_else(|| accessor.strip_prefix("is"))
            .and_then(|suffix| suffix.chars().next())
            .is_some_and(|first| first.is_ascii_uppercase())
}

fn external_root(name: &str) -> bool {
    matches!(
        name.trim().trim_end_matches("()"),
        "Any" | "kotlin.Any" | "Object" | "java.lang.Object"
    )
}

pub(super) fn known_external_without_getters(
    index: &SourceIndex,
    file: &SourceFile,
    source_type: &str,
) -> bool {
    let source_type = source_type.trim().trim_end_matches("()").trim();
    let (base, arguments) = super::split_generic(source_type);
    // Unknown ancestors are already tracked by the type lookup. Only consult
    // the general identity resolver for the standard contracts we can prove;
    // otherwise its member lookups add unrelated cache dependencies.
    let standard_name = matches!(
        base.rsplit('.').next(),
        Some("Serializable" | "Cloneable" | "RandomAccess" | "Comparable")
    );
    let imported_alias = imported_marker_alias(index, file, base);
    if !standard_name && imported_alias.is_none() {
        return false;
    }
    let identity = super::canonical_getter_named_identity(index, file, base).or(imported_alias);
    let Some(identity) = identity else {
        return false;
    };
    match identity.as_str() {
        "external:java.io.Serializable"
        | "external:java.lang.Cloneable"
        | "external:java.util.RandomAccess" => arguments.is_none(),
        "external:java.lang.Comparable" => arguments
            .as_ref()
            .is_some_and(|arguments| arguments.len() == 1 && !arguments[0].is_empty()),
        _ => false,
    }
}

fn imported_marker_alias(index: &SourceIndex, file: &SourceFile, base: &str) -> Option<String> {
    const MARKERS: [&str; 5] = [
        "java.io.Serializable",
        "java.lang.Cloneable",
        "java.util.RandomAccess",
        "java.lang.Comparable",
        "kotlin.Comparable",
    ];
    // The canonical resolver already handles visible declarations, aliases,
    // and ordinary imports. This fallback is only for a unique explicit
    // marker import alias, which the general resolver intentionally leaves
    // unresolved as source aliases.
    let bindings = file
        .imports
        .iter()
        .filter_map(|import| {
            let mut parts = import.split_whitespace();
            let path = parts.next()?;
            let alias = match (parts.next(), parts.next(), parts.next()) {
                (Some("as"), Some(alias), None) => Some(alias),
                (None, None, None) => None,
                _ => return None,
            };
            let binds_base =
                alias == Some(base) || (alias.is_none() && path.rsplit('.').next() == Some(base));
            binds_base.then_some((path, alias))
        })
        .collect::<Vec<_>>();
    let [(path, Some(_))] = bindings.as_slice() else {
        return None;
    };
    if !MARKERS.contains(path)
        || index.alias_names.contains_key(base)
        || file.type_aliases.contains_key(base)
        || index.declarations_named(base).next().is_some()
    {
        return None;
    }
    super::canonical_getter_named_identity(index, file, path)
}

impl SourceIndex {
    /// Proves a supertype spelling denotes one of the canonical JVM marker
    /// interfaces that declare no methods at all, so an ancestor outside the
    /// workspace index cannot own a callable member name.
    ///
    /// Only the methodless markers qualify. `Comparable` is deliberately
    /// excluded even though `known_external_without_getters` accepts it: it
    /// declares `compareTo`, so "no getter" is not "no members", and an
    /// unresolved `Comparable` ancestor is not a closed hierarchy. The spelling
    /// still goes through the shared canonical identity resolver, so aliases,
    /// source collisions and duplicate imports stay unproved.
    pub(crate) fn canonical_methodless_external_marker(
        &self,
        file: &SourceFile,
        type_name: &str,
    ) -> bool {
        let source_type = type_name.trim().trim_end_matches("()").trim();
        let (base, arguments) = super::split_generic(source_type);
        // The markers take no type arguments: a parameterised spelling is a
        // different contract and stays unknown.
        if arguments.is_some() {
            return false;
        }
        let marker_name = matches!(
            base.rsplit('.').next().map(|name| name.trim_matches('`')),
            Some("Serializable" | "Cloneable" | "RandomAccess")
        );
        let imported_alias = imported_marker_alias(self, file, base);
        if !marker_name && imported_alias.is_none() {
            return false;
        }
        let identity = super::canonical_getter_named_identity(self, file, base).or(imported_alias);
        matches!(
            identity.as_deref(),
            Some(
                "external:java.io.Serializable"
                    | "external:java.lang.Cloneable"
                    | "external:java.util.RandomAccess"
            )
        )
    }

    pub(crate) fn inferred_simple_property_type(
        &self,
        owner: &Declaration,
        member: &Member,
    ) -> Option<(String, PathBuf)> {
        // The proof requires an explicit delegating getter; ordinary
        // initializers cannot establish it and need no source-tree scan.
        if member.kind != MemberKind::Property
            || member.type_name.is_some()
            || !member.has_custom_accessor
        {
            return None;
        }
        let owner_id = semantics::workspace_symbol(self, owner);
        if owner_id.kind.starts_with("ambiguous:")
            || owner
                .members
                .iter()
                .filter(|candidate| {
                    candidate.kind == MemberKind::Property && candidate.name == member.name
                })
                .count()
                != 1
        {
            return None;
        }
        let entry = self
            .getter_bridge_cache
            .inferred_properties
            .lock()
            .expect("inferred property cache poisoned")
            .entry((owner_id, member.name.clone()))
            .or_default()
            .clone();
        let outcome = entry
            .cell
            .get_or_init(|| {
                let recorder = DependencyRecorder::start();
                let outcome =
                    crate::property_abi::inferred_simple_property_type(self, owner, member);
                let _ = entry.dependencies.set(recorder.finish());
                outcome
            })
            .clone();
        if let Some(dependencies) = entry.dependencies.get() {
            merge_dependencies(dependencies);
        }
        outcome
    }

    /// Find concrete covariant getter bridges for a source class. Unknown
    /// relevant inheritance returns `Err`, allowing planning to retain the
    /// class rather than emit a guessed JVM contract.
    pub(crate) fn covariant_default_getter_bridges(
        &self,
        declaring: &Path,
        name: &str,
    ) -> Result<Vec<CovariantDefaultGetterBridge>, String> {
        let file = self
            .source_file(declaring)
            .ok_or_else(|| "declaring source file is not indexed".to_string())?;
        let mut matching = file
            .declarations
            .iter()
            .filter(|declaration| declaration.name == name);
        let owner = matching.next().ok_or_else(|| {
            let indexed = file
                .declarations
                .iter()
                .map(|declaration| declaration.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("declaration `{name}` is not indexed in this file; indexed: {indexed}")
        })?;
        if matching.next().is_some() {
            return Err(format!("declaration `{name}` is ambiguous in this file"));
        }
        if owner.kind != DeclarationKind::Class {
            return Ok(Vec::new());
        }
        let owner_id = semantics::workspace_symbol(self, owner);
        if owner_id.kind.starts_with("ambiguous:") {
            return Err(format!(
                "declaration `{name}` has an ambiguous source identity"
            ));
        }
        let entry = self.getter_bridge_cache.entry(owner_id);
        let initialized_here = Cell::new(false);
        let outcome = entry
            .cell
            .get_or_init(|| {
                initialized_here.set(true);
                let recorder = DependencyRecorder::start();
                record_file(&file.path);
                record_name(&owner.name);
                let outcome = self.compute_covariant_default_getter_bridges(file, owner);
                let dependencies = recorder.finish();
                let _ = entry.dependencies.set(dependencies);
                outcome
            })
            .clone();
        if initialized_here.get() {
            super::profile::getter_cache_miss();
        } else {
            super::profile::getter_cache_hit();
        }
        if let Some(dependencies) = entry.dependencies.get() {
            merge_dependencies(dependencies);
        }
        outcome
    }

    fn compute_covariant_default_getter_bridges(
        &self,
        file: &SourceFile,
        owner: &Declaration,
    ) -> Result<Vec<CovariantDefaultGetterBridge>, String> {
        let mut profile = super::profile::GetterTraversal::start();

        let mut hierarchy = Vec::<InstantiatedOwner<'_>>::new();
        let mut pending = Vec::<InstantiatedOwner<'_>>::new();
        let mut visited = HashSet::<(SymbolId, String)>::new();
        let mut incomplete_hierarchy = false;
        let owner_bindings = scoped_type_ref_bindings(self, owner);
        for supertype in &owner.supertypes {
            if external_root(supertype) {
                continue;
            }
            let Some(parent) = unique_parent(self, file, supertype)? else {
                if known_external_without_getters(self, file, supertype) {
                    continue;
                }
                incomplete_hierarchy = true;
                continue;
            };
            let Some(bindings) =
                inherited_raw_bindings(self, parent, supertype, owner, &owner_bindings)
            else {
                incomplete_hierarchy = true;
                continue;
            };
            pending.push(InstantiatedOwner {
                declaration: parent,
                bindings,
            });
        }
        let mut explored = 0usize;
        while let Some(state) = pending.pop() {
            profile.visit();
            explored += 1;
            if explored > 256 {
                incomplete_hierarchy = true;
                break;
            }
            let identity = semantics::workspace_symbol(self, state.declaration);
            let binding_key = format!("{:?}", state.bindings);
            if !visited.insert((identity, binding_key)) {
                continue;
            }
            let declaration_file = self
                .declaration_source_file(state.declaration)
                .unwrap_or(file);
            for supertype in &state.declaration.supertypes {
                if external_root(supertype) {
                    continue;
                }
                let Some(parent) = unique_parent(self, declaration_file, supertype)? else {
                    if !known_external_without_getters(self, declaration_file, supertype) {
                        incomplete_hierarchy = true;
                    }
                    continue;
                };
                let Some(bindings) = inherited_raw_bindings(
                    self,
                    parent,
                    supertype,
                    state.declaration,
                    &state.bindings,
                ) else {
                    incomplete_hierarchy = true;
                    continue;
                };
                pending.push(InstantiatedOwner {
                    declaration: parent,
                    bindings,
                });
            }
            hierarchy.push(state);
        }

        // A declaration-level getter already decides the concrete class ABI;
        // this pass only fills the inherited-default gap.
        let own_getters = owner
            .members
            .iter()
            .filter_map(|member| source_getter(self, owner, member))
            .map(|getter| {
                if getter.member.kind == MemberKind::Property {
                    property_accessor_name(&getter.member.name)
                } else {
                    getter.member.name.clone()
                }
            })
            .collect::<HashSet<_>>();

        let mut names = BTreeMap::<String, Vec<GetterContract<'_>>>::new();
        for ancestor in &hierarchy {
            for member in &ancestor.declaration.members {
                if let Some(getter) = source_getter(self, ancestor.declaration, member) {
                    let method_name = if member.kind == MemberKind::Property {
                        property_accessor_name(&member.name)
                    } else {
                        member.name.clone()
                    };
                    if !own_getters.contains(&method_name) {
                        let canonical_return = if member.kind == MemberKind::Method
                            && self
                                .declaration_source_file(ancestor.declaration)
                                .is_some_and(|source_file| {
                                    !super::getter_has_no_method_type_parameters(
                                        source_file,
                                        ancestor.declaration,
                                        &method_name,
                                    )
                                }) {
                            None
                        } else {
                            canonical_return(
                                self,
                                ancestor.declaration,
                                &getter.return_type,
                                &ancestor.bindings,
                            )
                        };
                        names.entry(method_name).or_default().push(GetterContract {
                            owner: getter.owner,
                            member: getter.member,
                            canonical_return,
                            erased_return: erased_return(self, ancestor.declaration, getter.member),
                        });
                    }
                }
            }
        }

        let mut bridges = Vec::new();
        for (method_name, contracts) in names {
            if contracts.len() < 2
                || !contracts.iter().any(|contract| {
                    contract.owner.kind == DeclarationKind::Interface && contract.member.has_body
                })
                || own_getters.contains(&method_name)
            {
                continue;
            }
            if contracts.iter().any(|contract| contract.member.is_mutable) {
                return Err(format!(
                    "mutable getter contract `{method_name}` cannot be repaired with a getter bridge"
                ));
            }
            let direct_interfaces = owner
                .supertypes
                .iter()
                .filter_map(|supertype| {
                    let provider = self.resolve_type(file, supertype)?;
                    if provider.kind != DeclarationKind::Interface {
                        return None;
                    }
                    let raw_bindings =
                        inherited_raw_bindings(self, provider, supertype, owner, &owner_bindings)?;
                    Some((
                        supertype.as_str(),
                        provider,
                        InstantiatedOwner {
                            declaration: provider,
                            bindings: raw_bindings,
                        },
                    ))
                })
                .filter(|(_, provider, _)| {
                    !owner
                        .supertypes
                        .iter()
                        .filter_map(|supertype| {
                            let other = self.resolve_type(file, supertype)?;
                            (other.kind == DeclarationKind::Interface).then_some(other)
                        })
                        .any(|other| {
                            semantics::workspace_symbol(self, other)
                                != semantics::workspace_symbol(self, provider)
                                && super::proven_reference_subtype(
                                    self,
                                    self.declaration_source_file(other).unwrap_or(file),
                                    &super::declaration_key(other),
                                    self.declaration_source_file(provider).unwrap_or(file),
                                    &super::declaration_key(provider),
                                )
                        })
                })
                .collect::<Vec<_>>();
            let mut defaults = Vec::new();
            let mut blocked_default = false;
            for (direct_type, provider, provider_state) in direct_interfaces {
                let Some((target, member)) =
                    self.effective_default_getter(provider, file, &method_name)
                else {
                    blocked_default |= provider.members.iter().any(|member| {
                        source_getter(self, provider, member).is_some_and(|getter| {
                            if getter.member.kind == MemberKind::Property {
                                property_accessor_name(&getter.member.name) == method_name
                            } else {
                                getter.member.name == method_name
                            }
                        })
                    });
                    continue;
                };
                if member.is_nullable || !member.parameter_types.is_empty() {
                    blocked_default = true;
                    continue;
                }
                if member.kind == MemberKind::Method
                    && self
                        .declaration_source_file(target)
                        .is_some_and(|source_file| {
                            !super::getter_has_no_method_type_parameters(
                                source_file,
                                target,
                                &method_name,
                            )
                        })
                {
                    blocked_default = true;
                    continue;
                }
                let Some(default_erased_return) = erased_return(self, target, member) else {
                    blocked_default = true;
                    continue;
                };
                let Some(raw_return_type) = getter_return_type(self, target, member) else {
                    blocked_default = true;
                    continue;
                };
                let (routes, route_incomplete) =
                    instantiated_routes(self, provider_state, file, &mut profile);
                let target_routes = routes
                    .iter()
                    .filter(|route| {
                        semantics::workspace_symbol(self, route.declaration)
                            == semantics::workspace_symbol(self, target)
                    })
                    .filter_map(|route| {
                        canonical_return(self, target, raw_return_type.trim(), &route.bindings)
                    })
                    .collect::<Vec<_>>();
                let Some(canonical_default_return) = target_routes.first().cloned() else {
                    blocked_default = true;
                    continue;
                };
                if route_incomplete
                    || target_routes
                        .iter()
                        .any(|canonical| canonical != &canonical_default_return)
                {
                    blocked_default = true;
                    continue;
                }
                let Some(return_decl) =
                    source_reference_declaration(self, &canonical_default_return)
                else {
                    blocked_default = true;
                    continue;
                };
                if return_decl.is_jvm_value_class || !return_decl.type_params.is_empty() {
                    blocked_default = true;
                    continue;
                }
                let body =
                    proven_forwarding_getter_body(self, target, member, &canonical_default_return);
                defaults.push((
                    direct_type,
                    provider,
                    target,
                    member,
                    return_decl,
                    canonical_default_return,
                    default_erased_return,
                    body,
                ));
            }
            if defaults.is_empty() {
                let distinct_returns = contracts
                    .iter()
                    .filter_map(|contract| contract.canonical_return.as_ref())
                    .fold(Vec::new(), |mut unique, current| {
                        if !unique.contains(&current) {
                            unique.push(current);
                        }
                        unique
                    });
                if blocked_default
                    && (distinct_returns.len() > 1
                        || contracts
                            .iter()
                            .any(|contract| contract.canonical_return.is_none()))
                {
                    return Err(format!(
                        "cannot prove inherited default getter `{method_name}`"
                    ));
                }
                continue;
            }

            // Collapse repeated routes to one inherited default, then require
            // a single most-specific provider across all direct branches.
            defaults.sort_by_key(|(_, provider, target, member, _, _, _, _)| {
                (
                    provider.language != SourceLanguage::Java
                        || target.language != SourceLanguage::Java,
                    semantics::workspace_symbol(self, target),
                    member.name.clone(),
                )
            });
            defaults.dedup_by(|left, right| {
                semantics::workspace_symbol(self, left.2)
                    == semantics::workspace_symbol(self, right.2)
                    && left.3.name == right.3.name
                    && left.5 == right.5
                    && left.6 == right.6
            });
            let most_specific = defaults
                .iter()
                .enumerate()
                .filter(|(_, (_, _, target, _, _, _, _, _))| {
                    !defaults.iter().any(|(_, _, other, _, _, _, _, _)| {
                        semantics::workspace_symbol(self, other)
                            != semantics::workspace_symbol(self, target)
                            && super::proven_reference_subtype(
                                self,
                                self.declaration_source_file(other).unwrap_or(file),
                                &super::declaration_key(other),
                                self.declaration_source_file(target).unwrap_or(file),
                                &super::declaration_key(target),
                            )
                    })
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if most_specific.len() != 1 {
                if !contracts.is_empty() {
                    return Err(format!(
                        "ambiguous inherited default getter `{method_name}`"
                    ));
                }
                continue;
            }
            let (
                _direct_type,
                provider,
                target,
                _member,
                return_decl,
                default_canonical_return,
                default_erased_return,
                body,
            ) = defaults[most_specific[0]].clone();

            let mut has_wider_contract = false;
            let mut compatible = true;
            for contract in &contracts {
                if semantics::workspace_symbol(self, contract.owner)
                    == semantics::workspace_symbol(self, target)
                {
                    continue;
                }
                // A class member outranks interface defaults in Java method
                // resolution. Do not synthesize an interface-super bridge
                // that would override an inherited superclass method.
                if contract.owner.kind != DeclarationKind::Interface {
                    return Err(format!(
                        "superclass getter `{method_name}` takes precedence over interface defaults"
                    ));
                }
                let same_logical_return =
                    contract.canonical_return.as_ref() == Some(&default_canonical_return);
                let same_return = same_logical_return
                    && contract.erased_return.as_ref() == Some(&default_erased_return);
                if same_return {
                    continue;
                }
                has_wider_contract = true;
                if !same_logical_return
                    && !contract
                        .canonical_return
                        .as_ref()
                        .is_some_and(|contract_return| {
                            canonical_reference_subtype(
                                self,
                                &default_canonical_return,
                                contract_return,
                            )
                        })
                {
                    compatible = false;
                    break;
                }
            }
            if !has_wider_contract {
                continue;
            }
            if !compatible {
                return Err(format!(
                    "cannot prove covariant inherited getter `{method_name}`"
                ));
            }
            if incomplete_hierarchy {
                return Err(format!(
                    "incomplete hierarchy prevents proving getter `{method_name}`"
                ));
            }

            let mut contract_owners = contracts
                .iter()
                .filter(|contract| {
                    semantics::workspace_symbol(self, contract.owner)
                        != semantics::workspace_symbol(self, target)
                })
                .map(|contract| semantics::workspace_symbol(self, contract.owner))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            contract_owners.sort_by_key(SymbolId::stable_key);
            let mut property_contract_owners = contracts
                .iter()
                .filter(|contract| contract.member.kind == MemberKind::Property)
                .map(|contract| semantics::workspace_symbol(self, contract.owner))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            property_contract_owners.sort_by_key(SymbolId::stable_key);
            let return_type = super::declaration_key(return_decl);
            bridges.push(CovariantDefaultGetterBridge {
                owner: semantics::workspace_symbol(self, owner),
                provider_owner: semantics::workspace_symbol(self, provider),
                target: semantics::workspace_symbol(self, target),
                contract_owners,
                property_contract_owners,
                method_name,
                return_type_ref: TypeRef::parse(&return_type),
                return_type,
                body,
            });
        }
        Ok(bridges)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct FixtureRoot(PathBuf);

    impl std::ops::Deref for FixtureRoot {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for FixtureRoot {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FixtureRoot {
        fn drop(&mut self) {
            assert!(
                self.0.starts_with(
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-temp")
                )
            );
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn abstract_covariant_contracts_do_not_require_a_default_bridge() {
        let (_root, path, index) = index_for(
            "package sample\nclass Code {}\ninterface Wide {\n fun getCode(): Any\n}\ninterface Narrow : Wide {\n override fun getCode(): Code\n}\nabstract class Child : Narrow, Wide {}\n",
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()
                .is_empty()
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()
                .is_empty()
        );
        assert_eq!(index.getter_bridge_cache.entries.lock().unwrap().len(), 1);
    }

    #[test]
    fn inferred_property_cache_replays_dependencies_and_recomputes_after_overlay() {
        let bounds = "package sample\ninterface Bound {\n val value: String?\n}\n";
        let source = "package sample\n\
            interface Parent {\n val value: String?\n}\n\
            interface Child<T : Bound> : Parent {\n\
                val payload: T\n\
                override val value get() = payload.value\n\
            }\n";
        let (root, _, index) = index_for_sources(&[("Bound.kt", bounds), ("Child.kt", source)]);
        let query = |index: &SourceIndex| {
            let owner = index
                .declarations()
                .find(|owner| owner.name == "Child")
                .unwrap();
            let member = owner
                .members
                .iter()
                .find(|member| member.name == "value")
                .unwrap();
            let inferred = index.inferred_simple_property_type(owner, member);
            assert!(
                member.type_name.is_none(),
                "fixture getter unexpectedly has an explicit type: {member:?}"
            );
            inferred
        };
        let cold = DependencyRecorder::start();
        assert_eq!(
            query(&index).expect("bounded nullable property proof").0,
            "String?"
        );
        let cold = cold.finish();
        let warm = DependencyRecorder::start();
        assert_eq!(query(&index).unwrap().0, "String?");
        let warm = warm.finish();
        assert_eq!(cold.files, warm.files);
        assert_eq!(cold.names, warm.names);
        assert!(
            warm.files
                .contains(&super::super::normalized_path(&root.join("Bound.kt")))
        );
        assert_eq!(
            index
                .getter_bridge_cache
                .inferred_properties
                .lock()
                .unwrap()
                .len(),
            1
        );

        let changed = index
            .with_overlays(&[SourceOverlay::Replace {
                path: root.join("Bound.kt"),
                language: SourceLanguage::Kotlin,
                source: bounds.replace("String?", "Int"),
            }])
            .unwrap();
        assert!(query(&changed).is_none());
        assert!(query(&changed).is_none());
        assert_eq!(
            changed
                .getter_bridge_cache
                .inferred_properties
                .lock()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn overlay_cache_reuses_unrelated_files_and_invalidates_alias_dependencies() {
        let source = r#"
            package sample
            class Code {}
            class Other {}
            typealias CodeAlias = Code
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): CodeAlias = Code()
            }
            class Child : Narrow, Wide {
            }
        "#;
        let unrelated_source = "package sample\nclass Unrelated {}\n";
        let (_root, path, index) =
            index_for_sources(&[("Unrelated.kt", unrelated_source), ("Model.kt", source)]);
        assert_clean_fixture(&index, &path, &["Code", "Other", "Wide", "Narrow", "Child"]);
        let bridges = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(bridges[0].return_type, "sample.Code");
        let owner = bridges[0].owner.clone();
        let cached = index
            .getter_bridge_cache
            .entries
            .lock()
            .unwrap()
            .get(&owner)
            .unwrap()
            .clone();

        let unrelated_path = _root.join("Unrelated.kt");
        let unrelated = unrelated_source.replace(
            "class Unrelated {}",
            "class Unrelated { val value: Int = 1 }",
        );
        let unrelated_overlay = index
            .with_overlays(&[super::SourceOverlay::Replace {
                path: unrelated_path,
                language: SourceLanguage::Kotlin,
                source: unrelated.clone(),
            }])
            .unwrap();
        let reused = unrelated_overlay
            .getter_bridge_cache
            .entries
            .lock()
            .unwrap()
            .get(&owner)
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(&cached, &reused));
        assert_eq!(
            unrelated_overlay
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()[0]
                .return_type,
            "sample.Code"
        );

        let changed_alias = source.replace("CodeAlias = Code", "CodeAlias = Other");
        let alias_overlay = unrelated_overlay
            .with_overlays(&[super::SourceOverlay::Replace {
                path: path.clone(),
                language: SourceLanguage::Kotlin,
                source: changed_alias,
            }])
            .unwrap();
        assert!(
            !alias_overlay
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert_eq!(
            alias_overlay
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()[0]
                .return_type,
            "sample.Other"
        );
    }

    #[test]
    fn seeded_overlay_cache_uses_original_snapshot_and_invalidates_edits_and_reverts() {
        let source = r#"
            package sample
            class Code {
            }
            class Other {
            }
            typealias CodeAlias = Code
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): CodeAlias = Code()
            }
            class Child : Narrow, Wide {
            }
        "#;
        let unrelated_source = "package sample\nclass Unrelated {\n}\n";
        let (_root, path, base) =
            index_for_sources(&[("Unrelated.kt", unrelated_source), ("Model.kt", source)]);
        let warm = base.with_overlays(&[]).unwrap();
        let initial = warm
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(initial[0].return_type, "sample.Code");
        let owner = initial[0].owner.clone();
        let initial_entry = warm
            .getter_bridge_cache
            .entries
            .lock()
            .unwrap()
            .get(&owner)
            .unwrap()
            .clone();

        let unrelated_overlays = [super::SourceOverlay::Replace {
            path: _root.join("Unrelated.kt"),
            language: SourceLanguage::Kotlin,
            source: "package sample\nclass Unrelated {\n    val value: Int = 1\n}\n".into(),
        }];
        let carried_unrelated = base
            .with_overlays_using_cache_from(&unrelated_overlays, &warm)
            .unwrap();
        let plain_unrelated = base.with_overlays(&unrelated_overlays).unwrap();
        assert_eq!(carried_unrelated.files, plain_unrelated.files);
        let carried_entry = carried_unrelated
            .getter_bridge_cache
            .entries
            .lock()
            .unwrap()
            .get(&owner)
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(&initial_entry, &carried_entry));

        let changed_alias = source.replace("CodeAlias = Code", "CodeAlias = Other");
        let changed_overlays = [super::SourceOverlay::Replace {
            path: path.clone(),
            language: SourceLanguage::Kotlin,
            source: changed_alias,
        }];
        let changed = base
            .with_overlays_using_cache_from(&changed_overlays, &carried_unrelated)
            .unwrap();
        let plain_changed = base.with_overlays(&changed_overlays).unwrap();
        assert_eq!(changed.files, plain_changed.files);
        assert!(
            !changed
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert_eq!(
            changed
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()[0]
                .return_type,
            "sample.Other"
        );

        let reverted = base.with_overlays_using_cache_from(&[], &changed).unwrap();
        let plain_reverted = base.with_overlays(&[]).unwrap();
        assert_eq!(reverted.files, plain_reverted.files);
        assert!(
            !reverted
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert_eq!(
            reverted
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()[0]
                .return_type,
            "sample.Code"
        );
    }

    #[test]
    fn qualified_alias_canonicalization_tracks_alias_target_file() {
        let model = r#"
            package sample
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): types.CodeAlias = TODO()
            }
            class Child : Narrow, Wide {
            }
        "#;
        let aliases = r#"
            package types
            class Code {
            }
            class OtherCode {
            }
            typealias CodeAlias = Code
        "#;
        let (_root, path, index) = index_for_sources(&[("Types.kt", aliases), ("Model.kt", model)]);
        assert_clean_fixture(&index, &path, &["Wide", "Narrow", "Child"]);
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap_err()
                .contains("cannot prove inherited default getter")
        );
        let owner = semantics::workspace_symbol(
            &index,
            index
                .source_file(&path)
                .unwrap()
                .declarations
                .iter()
                .find(|declaration| declaration.name == "Child")
                .unwrap(),
        );
        let entry = index
            .getter_bridge_cache
            .entries
            .lock()
            .unwrap()
            .get(&owner)
            .unwrap()
            .clone();
        let alias_file = super::super::normalized_path(&_root.join("Types.kt"));
        assert!(
            entry
                .dependencies
                .get()
                .unwrap()
                .files
                .contains(&alias_file)
        );

        let changed_alias = aliases.replace("CodeAlias = Code", "CodeAlias = OtherCode");
        let changed = index
            .with_overlays(&[super::SourceOverlay::Replace {
                path: _root.join("Types.kt"),
                language: SourceLanguage::Kotlin,
                source: changed_alias,
            }])
            .unwrap();
        assert!(
            !changed
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert!(
            changed
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap_err()
                .contains("cannot prove inherited default getter")
        );

        let duplicate_aliases = changed
            .with_overlays(&[super::SourceOverlay::Replace {
                path: _root.join("OtherAlias.kt"),
                language: SourceLanguage::Kotlin,
                source: "package types\nclass ThirdCode {\n}\ntypealias CodeAlias = ThirdCode\n"
                    .into(),
            }])
            .unwrap();
        assert!(
            !duplicate_aliases
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert!(
            duplicate_aliases
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap_err()
                .contains("cannot prove inherited default getter")
        );
    }

    #[test]
    fn same_name_member_addition_invalidates_and_recomputes_bridge() {
        let model = r#"
            package sample
            class Code {
            }
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): Code = Code()
            }
            class Child : Narrow, Wide {
            }
        "#;
        let (_root, path, index) = index_for(model);
        assert_clean_fixture(&index, &path, &["Code", "Wide", "Narrow", "Child"]);
        let initial = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(initial.len(), 1);
        let owner = initial[0].owner.clone();
        let overload = model.replace(
            "override fun getCode(): Code = Code()",
            "override fun getCode(): Code = Code()\n    fun getCode(flag: Boolean): Code = Code()",
        );
        let changed = index
            .with_overlays(&[super::SourceOverlay::Replace {
                path: path.clone(),
                language: SourceLanguage::Kotlin,
                source: overload,
            }])
            .unwrap();
        assert!(
            !changed
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert_eq!(
            changed
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn overlay_cache_invalidates_negative_type_probes_when_a_declaration_is_added() {
        let source = r#"
            package sample
            class Code {}
            interface Wide {
                fun getCode(): Any
            }
            class Child : Missing, Wide {
            }
        "#;
        let unrelated_source =
            "package sample\nclass Noise {\n    fun Missing(): Any = TODO()\n}\n";
        let (_root, path, index) =
            index_for_sources(&[("Unrelated.kt", unrelated_source), ("Model.kt", source)]);
        assert_clean_fixture(&index, &path, &["Code", "Wide", "Child"]);
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()
                .is_empty()
        );
        let owner = semantics::workspace_symbol(
            &index,
            index
                .source_file(&path)
                .unwrap()
                .declarations
                .iter()
                .find(|declaration| declaration.name == "Child")
                .unwrap(),
        );
        let initial_entry = index
            .getter_bridge_cache
            .entries
            .lock()
            .unwrap()
            .get(&owner)
            .unwrap()
            .clone();
        let noise_file = super::super::normalized_path(&_root.join("Unrelated.kt"));
        assert!(
            !initial_entry
                .dependencies
                .get()
                .unwrap()
                .files
                .contains(&noise_file),
            "unresolved type lookup must not claim the file containing only a same-named member"
        );
        let unrelated_overlay = index
            .with_overlays(&[super::SourceOverlay::Replace {
                path: _root.join("Unrelated.kt"),
                language: SourceLanguage::Kotlin,
                source: unrelated_source.replace("= TODO()", "= 2"),
            }])
            .unwrap();
        let reused_entry = unrelated_overlay
            .getter_bridge_cache
            .entries
            .lock()
            .unwrap()
            .get(&owner)
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(&initial_entry, &reused_entry));
        assert!(
            unrelated_overlay
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()
                .is_empty()
        );

        let added_path = _root.join("Unrelated.kt");
        let repaired = unrelated_overlay
            .with_overlays(&[super::SourceOverlay::Replace {
                path: added_path,
                language: SourceLanguage::Kotlin,
                source: "package sample\nclass Noise {\n}\ninterface Missing : Wide {\n    override fun getCode(): Code = Code()\n}\n".into(),
            }])
            .unwrap();
        assert!(
            !repaired
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert_eq!(
            repaired
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()
                .len(),
            1
        );

        let same_file_duplicate = repaired
            .with_overlays(&[super::SourceOverlay::Replace {
                path: _root.join("Unrelated.kt"),
                language: SourceLanguage::Kotlin,
                source: format!(
                    "{}\ninterface Missing {{}}\n",
                    repaired
                        .source_file(&_root.join("Unrelated.kt"))
                        .unwrap()
                        .source_text()
                ),
            }])
            .unwrap();
        assert!(
            same_file_duplicate
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap_err()
                .contains("ambiguous declarations")
        );

        let duplicate = repaired
            .with_overlays(&[super::SourceOverlay::Replace {
                path: _root.join("OtherMissing.kt"),
                language: SourceLanguage::Kotlin,
                source: "package sample\ninterface Missing {\n}\n".into(),
            }])
            .unwrap();
        assert!(
            !duplicate
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert!(
            duplicate
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap_err()
                .contains("ambiguous declarations")
        );
    }

    #[test]
    fn overlay_cache_tracks_import_resolution_changes() {
        let model = r#"
            package sample
            import types.Code
            import types.OtherCode
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): Code = Code()
            }
            class Child : Narrow, Wide {
            }
        "#;
        let (_root, path, index) = index_for_sources(&[
            (
                "Code.kt",
                "package types\nclass Code {}\nclass OtherCode {}\n",
            ),
            ("Model.kt", model),
        ]);
        assert_clean_fixture(&index, &path, &["Wide", "Narrow", "Child"]);
        let before = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(before[0].return_type, "types.Code");
        let owner = before[0].owner.clone();
        let changed_model = model
            .replace("import types.Code", "import types.OtherCode")
            .replace("Code = Code()", "OtherCode = OtherCode()");
        let changed = index
            .with_overlays(&[super::SourceOverlay::Replace {
                path: path.clone(),
                language: SourceLanguage::Kotlin,
                source: changed_model,
            }])
            .unwrap();
        assert!(
            !changed
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert_eq!(
            changed
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()[0]
                .return_type,
            "types.OtherCode"
        );
    }

    #[test]
    fn overlay_cache_tracks_generic_argument_ancestor_reads() {
        let model = r#"
            package sample
            class Base {
            }
            interface Wide {
                fun getCode(): Base
            }
            interface Root<T> : Wide {
                override fun getCode(): T = TODO()
            }
            class Child : Root<Actual>, Wide {
            }
        "#;
        let types = "package sample\nclass Mid : Base {\n}\nclass Actual : Mid {\n}\n";
        let (_root, path, index) = index_for_sources(&[("Types.kt", types), ("Model.kt", model)]);
        assert_clean_fixture(&index, &path, &["Base", "Wide", "Root", "Child"]);
        let before = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(before.len(), 1);
        let owner = before[0].owner.clone();
        let types_path = _root.join("Types.kt");
        let changed_types = types.replace("class Mid : Base {\n}", "class Mid {\n}");
        let changed = index
            .with_overlays(&[super::SourceOverlay::Replace {
                path: types_path,
                language: SourceLanguage::Kotlin,
                source: changed_types,
            }])
            .unwrap();
        assert!(
            !changed
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert!(
            changed
                .covariant_default_getter_bridges(&path, "Child")
                .is_err()
        );
    }

    #[test]
    fn overlay_cache_tracks_generic_upper_bound_and_bound_type_ancestry() {
        let model = r#"
            package sample
            interface Wide {
                fun getCode(): Base
            }
            interface Root<T : Marker> : Wide {
                override fun getCode(): T = TODO()
            }
            class Child : Root<Marker>, Wide {
            }
        "#;
        let types =
            "package sample\nclass Base {\n}\nclass Marker : Base {\n}\nclass Other : Base {\n}\n";
        let (_root, path, index) = index_for_sources(&[("Types.kt", types), ("Model.kt", model)]);
        assert_clean_fixture(&index, &path, &["Wide", "Root", "Child"]);
        let before = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(before.len(), 1);
        let owner = before[0].owner.clone();
        let changed_bound = model.replace("T : Marker", "T : Other");
        let bound_overlay = index
            .with_overlays(&[super::SourceOverlay::Replace {
                path: path.clone(),
                language: SourceLanguage::Kotlin,
                source: changed_bound,
            }])
            .unwrap();
        assert!(
            !bound_overlay
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );

        let changed_ancestry = types.replace("class Marker : Base {\n}", "class Marker {\n}");
        let ancestry_overlay = index
            .with_overlays(&[super::SourceOverlay::Replace {
                path: _root.join("Types.kt"),
                language: SourceLanguage::Kotlin,
                source: changed_ancestry,
            }])
            .unwrap();
        assert!(
            !ancestry_overlay
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert!(
            ancestry_overlay
                .covariant_default_getter_bridges(&path, "Child")
                .is_err()
        );
    }

    #[test]
    fn overlay_cache_tracks_source_defined_builtin_shadows() {
        let model = r#"
            package sample
            class Code {
            }
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): Code = Code()
            }
            class Child : Narrow, Wide {
            }
        "#;
        let (_root, path, index) = index_for(model);
        assert_clean_fixture(&index, &path, &["Code", "Wide", "Narrow", "Child"]);
        let before = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(before.len(), 1);
        let owner = before[0].owner.clone();
        let changed = index
            .with_overlays(&[super::SourceOverlay::Replace {
                path: _root.join("Any.kt"),
                language: SourceLanguage::Kotlin,
                source: "package sample\nclass Any {}\n".into(),
            }])
            .unwrap();
        assert!(
            !changed
                .getter_bridge_cache
                .entries
                .lock()
                .unwrap()
                .contains_key(&owner)
        );
        assert!(
            changed
                .covariant_default_getter_bridges(&path, "Child")
                .is_err()
        );
    }

    #[test]
    fn ambiguous_same_file_owner_is_rejected_before_cache_lookup() {
        let (_root, path, index) =
            index_for("package sample\nclass Child {}\nclass Parent { class Child {} }\n");
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap_err()
                .contains("ambiguous")
        );
        assert!(index.getter_bridge_cache.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn error_outcomes_are_cached_within_the_snapshot() {
        let source = r#"
            package sample
            class Code {
            }
            interface Wide {
                val code: Any
            }
            interface Narrow : Wide {
                override val code: Code get() = Code()
            }
            class Child : Narrow, Wide, Missing {
            }
        "#;
        let (_root, path, index) = index_for(source);
        assert_clean_fixture(&index, &path, &["Code", "Wide", "Narrow", "Child"]);
        for _ in 0..2 {
            assert!(
                index
                    .covariant_default_getter_bridges(&path, "Child")
                    .unwrap_err()
                    .contains("incomplete hierarchy")
            );
        }
        assert_eq!(index.getter_bridge_cache.entries.lock().unwrap().len(), 1);
    }

    fn index_for(source: &str) -> (FixtureRoot, PathBuf, SourceIndex) {
        index_for_sources(&[("Model.kt", source)])
    }

    fn index_for_sources(sources: &[(&str, &str)]) -> (FixtureRoot, PathBuf, SourceIndex) {
        let _root = FixtureRoot(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("test-temp")
                .join(format!(
                    "covariant-default-getter-multi-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                )),
        );
        fs::create_dir_all(&_root).unwrap();
        for (relative, source) in sources {
            let path = _root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        let path = _root.join(sources.last().unwrap().0);
        let index = SourceIndex::discover(&_root).unwrap();
        (_root, path, index)
    }

    fn assert_clean_fixture(index: &SourceIndex, path: &Path, declarations: &[&str]) {
        let source = fs::read_to_string(path).unwrap();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(&source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "fixture has Kotlin parse errors: {source}"
        );
        let file = index.source_file(path).unwrap();
        for name in declarations {
            assert!(
                file.declarations
                    .iter()
                    .any(|declaration| declaration.name == *name),
                "fixture declaration {name} was not indexed: {:?}",
                file.declarations
                    .iter()
                    .map(|declaration| &declaration.name)
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            file.declarations
                .iter()
                .any(|declaration| declaration.name == "Child")
        );
    }

    #[test]
    fn finds_one_covariant_default_across_a_diamond() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            interface Wide {
                fun getCode(): Any
            }
            interface Root {
                fun getCode(): Code = Code()
            }
            interface Left : Root {}
            interface Right : Root {}
            class Child : Left, Right, Wide {}
            "#,
        );
        let bridges = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(bridges.len(), 1);
        assert_eq!(bridges[0].method_name, "getCode");
        assert_eq!(bridges[0].return_type, "sample.Code");
        assert_eq!(bridges[0].target.name, "Root");
    }

    #[test]
    fn same_return_and_explicit_class_override_need_no_bridge() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            interface Wide {
                fun getCode(): Code
            }
            interface Narrow : Wide {
                override fun getCode(): Code = Code()
            }
            class Inherited : Narrow, Wide {}
            class Explicit : Narrow, Wide {
                override fun getCode(): Code = Code()
            }
            "#,
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Inherited")
                .unwrap()
                .is_empty()
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Explicit")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn instantiated_generic_and_aliased_default_returns_are_canonicalized() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            typealias CodeAlias = Code
            interface Wide {
                fun getCode(): Any
            }
            interface Generic<T> : Wide {
                override fun getCode(): T = TODO()
            }
            interface Aliased : Wide {
                override fun getCode(): CodeAlias = Code()
            }
            class GenericChild : Generic<Code>, Wide {}
            class AliasChild : Aliased, Wide {}
            "#,
        );
        let generic = index
            .covariant_default_getter_bridges(&path, "GenericChild")
            .unwrap();
        assert_eq!(generic.len(), 1);
        assert_eq!(generic[0].return_type, "sample.Code");
        let aliased = index
            .covariant_default_getter_bridges(&path, "AliasChild")
            .unwrap();
        assert_eq!(aliased.len(), 1);
        assert_eq!(aliased[0].return_type, "sample.Code");
    }

    #[test]
    fn multihop_generic_default_and_equal_instantiated_diamond_share_one_bridge() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            class Identifier {}
            interface Wide { fun getAggregateIdentifier(): Any }
            interface Root<T> {
                fun getAggregateIdentifier(): T = TODO()
            }
            interface Mid<K> : Root<K> {}
            interface Left : Mid<Identifier> {}
            interface Right : Mid<Identifier> {}
            class Child : Left, Right, Wide {}
            class NullableChild : Root<Identifier?>, Wide {}
            "#,
        );
        let bridges = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(bridges.len(), 1);
        assert_eq!(bridges[0].method_name, "getAggregateIdentifier");
        assert_eq!(bridges[0].return_type, "sample.Identifier");
        assert_eq!(bridges[0].contract_owners.len(), 1);
        assert!(
            index
                .covariant_default_getter_bridges(&path, "NullableChild")
                .is_err()
        );
    }

    #[test]
    fn equal_instantiated_returns_still_bridge_when_jvm_erasure_differs() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            class Identifier {}
            interface Generic<T> {
                fun getId(): T = TODO()
            }
            interface Concrete {
                fun getId(): Identifier
            }
            class Child : Generic<Identifier>, Concrete {}
            "#,
        );
        let bridges = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(bridges.len(), 1);
        assert_eq!(bridges[0].return_type, "sample.Identifier");
    }

    #[test]
    fn conflicting_generic_diamond_instantiations_remain_unproven() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            class LeftId {}
            class RightId {}
            interface Wide { fun getId(): Any }
            interface Root<T> { fun getId(): T = TODO() }
            interface Left : Root<LeftId> {}
            interface Right : Root<RightId> {}
            class Child : Left, Right, Wide {}
            "#,
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .is_err()
        );
    }

    #[test]
    fn same_simple_name_from_distinct_packages_stays_distinct_after_substitution() {
        let (_root, path, index) = index_for_sources(&[
            ("left/Key.kt", "package left\nclass Key {}\n"),
            ("right/Key.kt", "package right\nclass Key {}\n"),
            (
                "sample/Model.kt",
                "package sample\ninterface Wide { fun getId(): Any }\n\
                 interface Root<T> { fun getId(): T = TODO() }\n\
                 interface Left : Root<left.Key> {}\n\
                 interface Right : Root<right.Key> {}\n\
                 class Child : Left, Right, Wide {}\n",
            ),
        ]);
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .is_err()
        );
    }

    #[test]
    fn primitive_spelling_is_resolved_in_the_edge_source_scope() {
        let (_root, path, index) = index_for_sources(&[
            ("domain/Int.kt", "package domain\nclass Int {}\n"),
            (
                "provider/Root.kt",
                "package provider\ninterface Root<T> {\n fun getValue(): T = TODO()\n}\n",
            ),
            (
                "sample/Wide.kt",
                "package sample\ninterface Wide {\n fun getValue(): kotlin.Int\n}\n",
            ),
            (
                "sample/Child.kt",
                "package sample\nimport domain.Int\nimport provider.Root\nclass Child : Root<Int>, Wide {}\n",
            ),
        ]);
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .is_err()
        );
    }

    #[test]
    fn incomplete_hierarchy_stays_unproven_only_when_a_bridge_is_needed() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): Code = Code()
            }
            class Child : Narrow, Wide, MissingContract {}
            class NoGetter : MissingContract {}
            "#,
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .is_err()
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "NoGetter")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn inherited_class_getter_prevents_interface_default_bridge() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            open class Base {
                fun getCode(): Code = Code()
            }
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): Code = Code()
            }
            class Child : Base(), Narrow, Wide {}
            "#,
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .is_err()
        );
    }

    #[test]
    fn direct_child_interface_is_used_instead_of_redundant_root_super() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            interface Wide {
                fun getCode(): Any
            }
            interface Root {
                fun getCode(): Code = Code()
            }
            interface ChildRoot : Root {}
            class Child : Root, ChildRoot, Wide {}
            "#,
        );
        let bridges = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(bridges.len(), 1);
        assert_eq!(bridges[0].provider_owner.name, "ChildRoot");
        assert_eq!(bridges[0].target.name, "Root");
    }

    #[test]
    fn imported_methodless_marker_is_skipped_but_unknown_import_is_not() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            import java.io.Serializable
            import java.lang.Cloneable as PlatformCloneable
            import external.contract.UnknownBase

            class ModelId {}
            interface Root : Serializable, PlatformCloneable {
                fun getIdentifier(): Any
            }
            interface Narrow : Root {
                override fun getIdentifier(): ModelId = ModelId()
            }
            interface Wide {
                fun getIdentifier(): Any
            }
            class MarkerChild : Narrow, Wide {}
            class UnknownChild : Narrow, Wide, UnknownBase {}
            "#,
        );
        let bridges = index
            .covariant_default_getter_bridges(&path, "MarkerChild")
            .unwrap();
        assert_eq!(bridges.len(), 1);
        assert_eq!(bridges[0].return_type, "sample.ModelId");
        assert!(
            index
                .covariant_default_getter_bridges(&path, "UnknownChild")
                .is_err()
        );
    }

    #[test]
    fn comparable_is_a_known_getter_free_contract_only_with_one_argument() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            import java.lang.Comparable as PlatformComparable

            class ModelId {}
            interface Root {
                fun getIdentifier(): Any
            }
            interface Narrow : Root {
                override fun getIdentifier(): ModelId = ModelId()
            }
            interface Wide {
                fun getIdentifier(): Any
            }
            class ComparableChild : Narrow, Wide, Comparable<ComparableChild> {
                override fun compareTo(other: ComparableChild): Int = 0
            }
            class AliasComparableChild : Narrow, Wide, PlatformComparable<AliasComparableChild> {
                override fun compareTo(other: AliasComparableChild): Int = 0
            }
            class RawComparableChild : Narrow, Wide, Comparable {
                override fun compareTo(other: ComparableChild): Int = 0
            }
            "#,
        );

        for class_name in ["ComparableChild", "AliasComparableChild"] {
            let bridges = index
                .covariant_default_getter_bridges(&path, class_name)
                .unwrap();
            assert_eq!(bridges.len(), 1, "{class_name}");
            assert_eq!(bridges[0].return_type, "sample.ModelId");
        }
        assert!(
            index
                .covariant_default_getter_bridges(&path, "RawComparableChild")
                .is_err()
        );
    }

    #[test]
    fn custom_or_foreign_comparable_is_not_treated_as_getter_free() {
        let (_root, path, index) = index_for(
            r#"
            package sample
            import external.contract.Comparable

            class ModelId {}
            interface Root {
                fun getIdentifier(): Any
            }
            interface Narrow : Root {
                override fun getIdentifier(): ModelId = ModelId()
            }
            interface Wide {
                fun getIdentifier(): Any
            }
            class ForeignComparableChild : Narrow, Wide, Comparable<ForeignComparableChild> {
                override fun compareTo(other: ForeignComparableChild): Int = 0
            }
            "#,
        );
        let file = index.source_file(&path).expect("indexed test source");
        assert!(!known_external_without_getters(
            &index,
            file,
            "Comparable<ForeignComparableChild>"
        ));

        let (_root, path, index) = index_for(
            r#"
            package sample
            interface Comparable<T> {}
            "#,
        );
        let file = index.source_file(&path).expect("indexed test source");
        assert!(!known_external_without_getters(
            &index,
            file,
            "Comparable<String>"
        ));
        assert!(!known_external_without_getters(&index, file, "Comparable"));
    }

    #[test]
    fn marker_aliases_with_source_collisions_or_multiple_imports_stay_unknown() {
        for source in [
            "package sample\nimport java.lang.Comparable as PlatformComparable\ninterface PlatformComparable<T> {}",
            "package sample\nimport java.lang.Comparable as PlatformComparable\nimport external.contract.Comparable as PlatformComparable",
            "package java.lang\nimport java.lang.Comparable as PlatformComparable\ninterface Comparable<T> {}",
        ] {
            let (_root, path, index) = index_for(source);
            let file = index.source_file(&path).expect("indexed test source");
            assert!(
                !known_external_without_getters(&index, file, "PlatformComparable<String>"),
                "{source}"
            );
        }
    }

    #[test]
    fn methodless_marker_proof_excludes_comparable_and_unproved_bindings() {
        for (source, type_name, expected) in [
            (
                "package sample\nimport java.io.Serializable\ninterface Model : Serializable",
                "Serializable",
                true,
            ),
            (
                "package sample\nimport java.lang.Cloneable as PlatformCloneable\ninterface Model : PlatformCloneable",
                "PlatformCloneable",
                true,
            ),
            (
                "package sample\ninterface Model : java.util.RandomAccess",
                "java.util.RandomAccess",
                true,
            ),
            // `Comparable` declares `compareTo`: no getter is not no members.
            (
                "package sample\nimport java.lang.Comparable as PlatformComparable\ninterface Model : PlatformComparable",
                "PlatformComparable",
                false,
            ),
            (
                "package sample\ninterface Model : Comparable<String>",
                "Comparable<String>",
                false,
            ),
            // A source classifier of the same name shadows the JDK marker.
            (
                "package sample\nimport java.lang.Comparable as PlatformComparable\ninterface PlatformComparable<T> {}",
                "PlatformComparable",
                false,
            ),
            (
                "package java.lang\nimport java.lang.Cloneable as PlatformCloneable\ninterface Cloneable {}",
                "PlatformCloneable",
                false,
            ),
            // Two bindings are ambiguous, exactly like Kotlin's own conflict.
            (
                "package sample\nimport java.lang.Cloneable as PlatformCloneable\nimport external.contract.Cloneable as PlatformCloneable",
                "PlatformCloneable",
                false,
            ),
            ("package sample\n", "Serializable", false),
        ] {
            let (_root, path, index) = index_for(source);
            let file = index.source_file(&path).expect("indexed test source");
            assert_eq!(
                index.canonical_methodless_external_marker(file, type_name),
                expected,
                "{source} / {type_name}"
            );
        }
    }

    const FORWARDING_PROVIDERS: &str = r#"
        package sample
        interface View { val text: String }
        class Marker(override val text: String) : View
        class Envelope(private val stored: Marker) {
            val marker: Marker get() = stored
        }
        interface Wide { fun getMarker(): View }
        interface Provider : Wide {
            val envelope: Envelope
            override fun getMarker(): Marker = envelope.marker
            suspend fun retainedOnly() {}
        }
    "#;

    const FORWARDING_CHILD: &str = r#"
        package sample
        class Child(private val storage: Envelope) : Provider {
            override val envelope: Envelope get() = storage
        }
    "#;

    #[test]
    fn forwarding_body_is_proven_for_an_exact_this_rooted_accessor_chain() {
        let (_root, path, index) = index_for_sources(&[
            ("Providers.kt", FORWARDING_PROVIDERS),
            ("Child.kt", FORWARDING_CHILD),
        ]);
        let bridges = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(bridges.len(), 1, "{bridges:?}");
        let body = bridges[0].body.as_ref().expect("forwarding body");
        let accessors = body
            .steps
            .iter()
            .map(|step| step.accessor.as_str())
            .collect::<Vec<_>>();
        assert_eq!(accessors, ["getEnvelope", "getMarker"]);
    }

    #[test]
    fn forwarding_body_is_proven_across_separate_source_files() {
        let view = "package sample\n\ninterface MarkerView {\n    val text: String\n}\n";
        let marker = "package sample\n\nclass Marker(override val text: String) : MarkerView\n";
        let envelope = "package sample\n\nclass Envelope(private val stored: Marker) {\n    val marker: Marker\n        get() {\n            Reads.markerRead()\n            return stored\n        }\n}\n";
        let reads = "package sample;\n\npublic final class Reads {\n    private static int markerReads;\n    public static int markerRead() { return ++markerReads; }\n}\n";
        let wide = "package sample\n\ninterface WideSource {\n    fun getMarker(): MarkerView\n}\n";
        let provider = "package sample\n\ninterface DefaultSource : WideSource {\n    val envelope: Envelope\n\n    override fun getMarker(): Marker = envelope.marker\n\n    suspend fun retainedMarker() {}\n}\n";
        let child = "package sample\n\nclass Subject(private val storage: Envelope) : DefaultSource {\n    override val envelope: Envelope\n        get() {\n            Reads.envelopeRead()\n            return storage\n        }\n}\n";
        let (_root, path, index) = index_for_sources(&[
            ("MarkerView.kt", view),
            ("Marker.kt", marker),
            ("Envelope.kt", envelope),
            ("Reads.java", reads),
            ("WideSource.kt", wide),
            ("DefaultSource.kt", provider),
            ("Subject.kt", child),
        ]);
        let bridges = index
            .covariant_default_getter_bridges(&path, "Subject")
            .unwrap();
        assert_eq!(bridges.len(), 1, "{bridges:?}");
        let body = bridges[0].body.as_ref().expect("forwarding body");
        let accessors = body
            .steps
            .iter()
            .map(|step| step.accessor.as_str())
            .collect::<Vec<_>>();
        assert_eq!(accessors, ["getEnvelope", "getMarker"]);
    }

    const FORWARDING_CHILD_BODY: &str = "package sample\n\
         class Child(private val storage: Envelope) : Provider {\n\
             override val envelope: Envelope get() = storage\n\
         }\n";

    /// A provider whose selected default getter body is `body`, plus a child
    /// that inherits it. The child file is always part of the fixture so a
    /// missing-source error can never stand in for a declined proof.
    fn forwarding_bridges(providers: &str) -> Result<Vec<CovariantDefaultGetterBridge>, String> {
        let (_root, path, index) = index_for_sources(&[
            ("Providers.kt", providers),
            ("Child.kt", FORWARDING_CHILD_BODY),
        ]);
        index.covariant_default_getter_bridges(&path, "Child")
    }

    fn provider_with(body: &str) -> String {
        format!(
            "package sample\n\
             interface View {{ val text: String }}\n\
             class Marker(override val text: String) : View\n\
             class Envelope(private val stored: Marker) {{\n\
                 val marker: Marker get() = stored\n\
                 fun pick(): Marker = stored\n\
             }}\n\
             interface Wide {{ fun getMarker(): View }}\n\
             interface Provider : Wide {{\n\
                 val envelope: Envelope\n\
                 {body}\n\
             }}\n"
        )
    }

    /// The bridge is still selected, so only the forwarding proof can keep the
    /// child in Kotlin.
    fn assert_bridge_without_forwarding_body(providers: &str) {
        let bridges = forwarding_bridges(providers)
            .unwrap_or_else(|error| panic!("bridge analysis must still succeed: {error}"));
        assert!(!bridges.is_empty(), "expected a selected default getter");
        assert!(
            bridges.iter().all(|bridge| bridge.body.is_none()),
            "expected no proven forwarding body: {bridges:?}"
        );
    }

    #[test]
    fn annotated_default_getter_body_stays_unproven() {
        // Renaming the accessor changes the JVM method the Kotlin default
        // reads, so the chain cannot be re-created by its property spelling.
        let renamed = "package sample\n\
             interface View { val text: String }\n\
             class Marker(override val text: String) : View\n\
             class Envelope(private val stored: Marker) {\n\
                 @get:JvmName(\"named\")\n\
                 val marker: Marker get() = stored\n\
             }\n\
             interface Wide { fun getMarker(): View }\n\
             interface Provider : Wide {\n\
                 val envelope: Envelope\n\
                 override fun getMarker(): Marker = envelope.marker\n\
             }\n";
        assert_bridge_without_forwarding_body(renamed);

        // An annotated target default keeps its annotation on the Kotlin
        // provider; a body re-created without it would silently drop the
        // contract, so this slice refuses the whole default.
        let annotated = provider_with(
            "@Deprecated(\"old\")\n    override fun getMarker(): Marker = envelope.marker",
        );
        assert_bridge_without_forwarding_body(&annotated);
    }

    #[test]
    fn a_default_that_reads_its_own_getter_stays_unproven() {
        // The re-created body must never call the bridge it belongs to.
        let providers = "package sample\n\
             interface View { val text: String }\n\
             class Marker(override val text: String) : View\n\
             interface Wide { fun getMarker(): View }\n\
             interface Provider : Wide {\n\
                 override val marker: Marker get() = marker\n\
             }\n";
        if let Ok(bridges) = forwarding_bridges(providers) {
            assert!(
                bridges.iter().all(|bridge| bridge.body.is_none()),
                "a self-reading default must stay unproven: {bridges:?}"
            );
        }
    }

    #[test]
    fn calls_casts_labelled_receivers_and_block_bodies_stay_unproven() {
        for body in [
            "override fun getMarker(): Marker = envelope.pick()",
            "override fun getMarker(): Marker = envelope as Marker",
            "override fun getMarker(): Marker = this@Provider.envelope.marker",
            "override fun getMarker(): Marker = Outer(this).envelope.marker",
            "override fun getMarker(): Marker { return envelope.marker }",
        ] {
            assert_bridge_without_forwarding_body(&provider_with(body));
        }
    }

    #[test]
    fn non_public_or_mutable_steps_stay_unproven() {
        for (label, marker) in [
            ("private", "private val marker: Marker get() = stored"),
            ("mutable", "var marker: Marker get() = stored"),
        ] {
            let providers = format!(
                "package sample\n\
                 interface View {{ val text: String }}\n\
                 class Marker(override val text: String) : View\n\
                 class Envelope(private val stored: Marker) {{\n\
                     {marker}\n\
                 }}\n\
                 interface Wide {{ fun getMarker(): View }}\n\
                 interface Provider : Wide {{\n\
                     val envelope: Envelope\n\
                     override fun getMarker(): Marker = envelope.marker\n\
                 }}\n"
            );
            // A private step has no readable accessor at all, so the chain is
            // unresolvable; a mutable step is refused by the proof itself.
            let bridges =
                forwarding_bridges(&providers).unwrap_or_else(|error| panic!("{label}: {error}"));
            assert!(
                bridges.iter().all(|bridge| bridge.body.is_none()),
                "{label} step must stay unproven: {bridges:?}"
            );
        }
    }

    #[test]
    fn nullable_and_generic_default_returns_stay_unproven() {
        let nullable = provider_with("override fun getMarker(): Marker? = envelope.marker");
        assert!(
            forwarding_bridges(&nullable).is_err(),
            "a nullable default return needs null handling this slice cannot prove"
        );
        // A type-parameter return only erases to its bound. Kotlin's own
        // reflection reports the bound type in some `-jvm-default` modes, so a
        // logical subtype proof is not an ABI proof.
        let generic = "package sample\n\
             interface View { val text: String }\n\
             class Marker(override val text: String) : View\n\
             class Envelope(private val stored: Marker) { val marker: Marker get() = stored }\n\
             interface Wide { fun getMarker(): View }\n\
             interface Provider<T : Marker> : Wide {\n\
                 val envelope: Envelope\n\
                 override fun getMarker(): T = current\n\
                 val current: T\n\
             }\n";
        let bridges = forwarding_bridges(generic).unwrap();
        assert!(
            bridges.iter().all(|bridge| bridge.body.is_none()),
            "a generic default return must not be proven from a subtype alone: {bridges:?}"
        );
    }

    #[test]
    fn same_named_providers_are_never_confused_across_files() {
        // A same-named provider in another package forwards through a
        // different property chain. The proof must follow the child's own
        // resolution and that provider's exact source identity.
        let other = "package other\n\
             interface View { val text: String }\n\
             class Marker(override val text: String) : View\n\
             class Envelope(private val stored: Marker) { val marker: Marker get() = stored }\n\
             interface Wide { fun getMarker(): View }\n\
             interface Provider : Wide {\n\
                 val bag: Envelope\n\
                 override fun getMarker(): Marker = bag.marker\n\
             }\n";
        let (_root, path, index) = index_for_sources(&[
            ("Other.kt", other),
            (
                "Providers.kt",
                &provider_with("override fun getMarker(): Marker = envelope.marker"),
            ),
            ("Child.kt", FORWARDING_CHILD_BODY),
        ]);
        let bridges = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(bridges.len(), 1, "{bridges:?}");
        assert_eq!(bridges[0].target.package, "sample");
        let body = bridges[0].body.as_ref().expect("forwarding body");
        let accessors = body
            .steps
            .iter()
            .map(|step| step.accessor.as_str())
            .collect::<Vec<_>>();
        assert_eq!(accessors, ["getEnvelope", "getMarker"]);
    }

    #[test]
    fn a_changed_provider_snapshot_invalidates_the_forwarding_body() {
        let (_root, path, index) = index_for_sources(&[
            (
                "Providers.kt",
                &provider_with("override fun getMarker(): Marker = envelope.marker"),
            ),
            ("Child.kt", FORWARDING_CHILD_BODY),
        ]);
        let bridges = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert!(bridges[0].body.is_some());
        let providers = provider_with("override fun getMarker(): Marker = envelope.pick()");
        let changed = index
            .with_overlays(&[super::SourceOverlay::Replace {
                path: _root.join("Providers.kt"),
                language: SourceLanguage::Kotlin,
                source: providers,
            }])
            .unwrap();
        let bridges = changed
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert!(
            bridges.iter().all(|bridge| bridge.body.is_none()),
            "an edited default body must not keep its previous forwarding proof: {bridges:?}"
        );
    }
}
