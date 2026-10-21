//! A `SemanticProvider` decorator that answers from the JVM facts helper.
//!
//! The decorator wraps the syntax provider and only *adds* facts: every
//! override either forwards unchanged or answers where the syntax provider (and
//! the static fallback table in `semantics::platform_members`) had nothing.
//! It never downgrades an answer, so a run with a helper is a superset of the
//! same run without one.
//!
//! The helper itself is run-scoped (`facts::runtime`): this type holds a handle
//! to the shared runtime, not its own process, so the two providers a run
//! constructs share one helper and one memo table.

use crate::cli::JvmFactsMode;
use crate::facts::protocol::{CallSite, MemberFacts, SiteStatus};
use crate::facts::runtime::{self, FactsRuntime};
use crate::semantics::{
    FactStatus, InheritedPropertyAccess, JavaPropertyRead, KotlinConstructorMetadataDependency,
    KotlinType, MapAliasTypeArguments, MemberReceiverCapability, ReferenceReturnNullability,
    SemanticProvider, SemanticSymbol, SymbolId, SyntaxSemanticProvider, TypeAliasDefinition,
    TypeRef,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

pub struct FactsProvider {
    inner: SyntaxSemanticProvider,
    runtime: Option<Arc<FactsRuntime>>,
    /// Call-site facts this provider handed out, keyed by the identity it
    /// minted for each ([`crate::facts::call_site_symbol`]).
    ///
    /// The identity is not an indexed declaration, so the syntax index cannot
    /// answer anything about it. Keeping the fact here is what lets the typed
    /// view of the same call — its K2 result type — answer for that id instead
    /// of falling through to "unknown".
    call_facts: Mutex<BTreeMap<SymbolId, CallSite>>,
}

impl FactsProvider {
    fn from_parts(inner: SyntaxSemanticProvider, runtime: Option<Arc<FactsRuntime>>) -> Self {
        Self {
            inner,
            runtime,
            call_facts: Mutex::new(BTreeMap::new()),
        }
    }
}

impl FactsProvider {
    /// Transparent wrapper: no helper, every answer comes from `inner`.
    pub fn transparent(inner: SyntaxSemanticProvider) -> Self {
        Self::from_parts(inner, None)
    }

    /// Wrapper over an already-installed run runtime.
    pub fn with_runtime(inner: SyntaxSemanticProvider, runtime: Arc<FactsRuntime>) -> Self {
        Self::from_parts(inner, Some(runtime))
    }

    /// The call-site fact this provider minted `member` from, if it did.
    fn call_fact(&self, member: &SymbolId) -> Option<CallSite> {
        self.call_facts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(member)
            .cloned()
    }

    /// Start (or join) the run's helper according to the CLI policy. `On` fails
    /// loudly; `Auto` silently keeps the transparent wrapper when the helper is
    /// unavailable.
    pub fn from_policy(inner: SyntaxSemanticProvider, mode: JvmFactsMode) -> Result<Self, String> {
        Ok(match runtime::install(mode)? {
            Some(runtime) => Self::with_runtime(inner, runtime),
            None => Self::transparent(inner),
        })
    }

    /// Policy-based construction for call paths that cannot fail (the local
    /// provider used by stdin runs). `main` already rejects `--jvm-facts=on`
    /// without a jar before any work happens, so an error here is a spawn
    /// failure: log it and continue without facts rather than abort.
    pub fn policy_or_transparent(inner: SyntaxSemanticProvider, mode: JvmFactsMode) -> Self {
        match runtime::install(mode) {
            Ok(Some(runtime)) => Self::with_runtime(inner, runtime),
            Ok(None) => Self::transparent(inner),
            Err(error) => {
                log::warn!("{error}; continuing without JVM facts");
                Self::transparent(inner)
            }
        }
    }

    /// Whether this run has a live helper. `false` means every answer is the
    /// syntax provider's.
    pub fn has_helper(&self) -> bool {
        self.runtime.is_some()
    }

    /// The run's call-site batch for `file`, read from the helper's view of the
    /// file on disk (`FactsRuntime::call_sites`). `None` when this run has no
    /// helper or the batch could not be established — callers treat that as
    /// `Unknown`, never as "no calls".
    ///
    /// This is the whole batch; required-call resolution goes through
    /// [`SemanticProvider::established_call_site`], which is the same answer
    /// narrowed to one hash-guarded offset.
    pub fn call_sites(&self, file: &Path) -> Option<Arc<Vec<CallSite>>> {
        self.runtime.as_ref()?.call_sites(file)
    }

    /// One hash-guarded call site: see `FactsRuntime::call_site_at` for the
    /// soundness rule. `expected_source_hash` must be the blake3 of the source
    /// being planned; a mismatch with the bytes on disk yields `None` without
    /// consulting the helper (speculative overlays get no facts).
    pub fn call_site_at(
        &self,
        file: &Path,
        byte_offset: u64,
        expected_source_hash: &[u8; 32],
    ) -> Option<CallSite> {
        self.runtime
            .as_ref()?
            .call_site_at(file, byte_offset, expected_source_hash)
    }
}

impl SemanticProvider for FactsProvider {
    fn has_classpath_facts(&self) -> bool {
        self.runtime.is_some()
    }

    fn classpath_member_facts(&self, owner: &str) -> Option<Arc<Vec<MemberFacts>>> {
        self.runtime.as_ref()?.member_facts(owner)
    }

    /// One Track B call site, hash-guarded end to end.
    ///
    /// Three gates, in order, and each one is a way to answer *nothing*:
    /// the file must be a Kotlin source the session indexed (the helper never
    /// parses Java), the run must have a helper, and the helper must have a
    /// site *at that exact offset* for bytes hashing to `source_hash`. The
    /// last gate is the soundness rule: `FactsRuntime::call_site_at` refuses
    /// when the snapshot being planned is not the text on disk, so a candidate
    /// revision or an in-memory overlay never inherits disk facts.
    ///
    /// Only `Established` sites leave this function. The fact is remembered
    /// under the identity minted for it, so the typed view
    /// ([`Self::explicit_nongeneric_kotlin_return`]) can answer for that id.
    fn established_call_site(
        &self,
        file: &Path,
        byte_offset: u64,
        source_hash: &[u8; 32],
    ) -> Option<CallSite> {
        if !file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("kt"))
        {
            return None;
        }
        let site = self
            .runtime
            .as_ref()?
            .call_site_at(file, byte_offset, source_hash)?;
        if site.status != SiteStatus::Established {
            return None;
        }
        let id = crate::facts::call_site_symbol(file, &site)?;
        let mut facts = self
            .call_facts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        facts.insert(id, site.clone());
        // The memo is per provider and bounded by the calls this run asked
        // about; two providers in one run each hold their own copy, which is
        // cheap and keeps the decorator free of a shared lock across the run.
        Some(site)
    }

    /// The K2 result type of a call this provider established.
    ///
    /// The trait's contract says *nongeneric*: K2 renders parameterized results
    /// (`kotlin.collections.Iterator<kotlin.Any?>`) with substituted arguments,
    /// which is a stronger fact than this method promises to carry, so a
    /// parameterized rendering is withheld rather than narrowed. Nullability is
    /// already folded into the `TypeRef` from the wire's `result_nullable`.
    fn explicit_nongeneric_kotlin_return(&self, member: &SymbolId) -> Option<TypeRef> {
        if crate::facts::is_fact_symbol(member) {
            let site = self.call_fact(member)?;
            let ty = crate::facts::call_site_result_type(&site)?;
            return match &ty.ty {
                KotlinType::Named { arguments, .. } if arguments.is_empty() => Some(ty),
                _ => None,
            };
        }
        self.inner.explicit_nongeneric_kotlin_return(member)
    }

    fn resolve(&self, name: &str) -> FactStatus<SymbolId> {
        self.inner.resolve(name)
    }

    fn symbols(&self) -> &[SemanticSymbol] {
        self.inner.symbols()
    }

    fn kotlin_constructor_metadata_dependency(
        &self,
        owner: &SymbolId,
    ) -> Option<&KotlinConstructorMetadataDependency> {
        self.inner.kotlin_constructor_metadata_dependency(owner)
    }

    fn has_jackson_default_constructor_semantics(&self, owner: &SymbolId) -> bool {
        self.inner.has_jackson_default_constructor_semantics(owner)
    }

    fn symbols_in_file(&self, file: &Path) -> Vec<&SemanticSymbol> {
        self.inner.symbols_in_file(file)
    }

    fn symbols_named(&self, name: &str) -> Vec<&SemanticSymbol> {
        self.inner.symbols_named(name)
    }

    fn type_aliases(&self) -> Vec<((String, String), TypeAliasDefinition)> {
        self.inner.type_aliases()
    }

    fn type_alias_definition(&self, package: &str, name: &str) -> Option<TypeAliasDefinition> {
        self.inner.type_alias_definition(package, name)
    }

    fn has_type_alias_in_package(&self, package: &str, name: &str) -> bool {
        self.inner.has_type_alias_in_package(package, name)
    }

    fn source_for_file(&self, file: &Path) -> Option<&str> {
        self.inner.source_for_file(file)
    }

    fn builtin_jvm_type_descriptor(&self, file: &Path, spelling: &str) -> Option<&'static str> {
        self.inner.builtin_jvm_type_descriptor(file, spelling)
    }

    fn returned_receiver_capability(
        &self,
        member: &SymbolId,
    ) -> FactStatus<MemberReceiverCapability> {
        self.inner.returned_receiver_capability(member)
    }

    fn is_public_instance_callable(&self, member: &SymbolId) -> bool {
        self.inner.is_public_instance_callable(member)
    }

    fn has_plain_instance_property_getter(&self, property: &SymbolId) -> bool {
        self.inner.has_plain_instance_property_getter(property)
    }

    fn equivalent_property_getter(&self, property: &SymbolId) -> Option<SymbolId> {
        self.inner.equivalent_property_getter(property)
    }

    fn java_property_read(&self, owner: &SymbolId, property: &str) -> Option<JavaPropertyRead> {
        self.inner.java_property_read(owner, property)
    }

    fn enum_entries(&self, owner: &SymbolId) -> FactStatus<Vec<SymbolId>> {
        self.inner.enum_entries(owner)
    }

    fn java_field_named(&self, owner: &SymbolId, name: &str) -> FactStatus<bool> {
        self.inner.java_field_named(owner, name)
    }

    fn is_ordinary_jvm_reference(&self, owner: &SymbolId) -> FactStatus<bool> {
        self.inner.is_ordinary_jvm_reference(owner)
    }

    fn inherited_property_access(
        &self,
        owner: &SymbolId,
        name: &str,
    ) -> FactStatus<InheritedPropertyAccess> {
        self.inner.inherited_property_access(owner, name)
    }

    fn has_stable_noarg_jvm_signature(&self, member: &SymbolId) -> bool {
        self.inner.has_stable_noarg_jvm_signature(member)
    }

    fn reference_return_nullability(
        &self,
        member: &SymbolId,
    ) -> Option<ReferenceReturnNullability> {
        self.inner.reference_return_nullability(member)
    }

    fn has_plain_java_package_nullability(&self, package: &str) -> bool {
        self.inner.has_plain_java_package_nullability(package)
    }

    fn is_nonsealed_interface(&self, owner: &SymbolId) -> bool {
        self.inner.is_nonsealed_interface(owner)
    }

    fn inherited_members<'a>(&'a self, owner: &SymbolId, name: &str) -> Vec<&'a SemanticSymbol> {
        self.inner.inherited_members(owner, name)
    }

    fn resolve_inherited_noarg_call(&self, owner: &SymbolId, name: &str) -> FactStatus<SymbolId> {
        self.inner.resolve_inherited_noarg_call(owner, name)
    }

    fn has_complete_inheritance(&self, owner: &SymbolId) -> bool {
        self.inner.has_complete_inheritance(owner)
    }

    fn instance_contract_parents(&self, owner: &SymbolId) -> FactStatus<Vec<SymbolId>> {
        self.inner.instance_contract_parents(owner)
    }

    fn resolve_type_in_file(&self, file: &Path, package: &str, name: &str) -> FactStatus<SymbolId> {
        self.inner.resolve_type_in_file(file, package, name)
    }

    fn map_alias_type_arguments(
        &self,
        file: &Path,
        package: &str,
        spelling: &str,
    ) -> FactStatus<MapAliasTypeArguments> {
        self.inner.map_alias_type_arguments(file, package, spelling)
    }

    fn companion_members<'a>(&'a self, owner: &SymbolId, name: &str) -> Vec<&'a SemanticSymbol> {
        self.inner.companion_members(owner, name)
    }

    fn has_default_companion_jvm_abi(&self, member: &SymbolId) -> bool {
        self.inner.has_default_companion_jvm_abi(member)
    }

    fn java_property_getters<'a>(
        &'a self,
        owner: &SymbolId,
        property: &str,
    ) -> Vec<&'a SemanticSymbol> {
        self.inner.java_property_getters(owner, property)
    }

    fn is_java_static_member(&self, symbol: &SymbolId) -> bool {
        self.inner.is_java_static_member(symbol)
    }

    fn is_java_callable_member(&self, symbol: &SymbolId) -> bool {
        self.inner.is_java_callable_member(symbol)
    }
}
