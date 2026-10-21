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
use crate::facts::protocol::MemberFacts;
use crate::facts::runtime::{self, FactsRuntime};
use crate::semantics::{
    FactStatus, InheritedPropertyAccess, JavaPropertyRead, KotlinConstructorMetadataDependency,
    MapAliasTypeArguments, MemberReceiverCapability, ReferenceReturnNullability, SemanticProvider,
    SemanticSymbol, SymbolId, SyntaxSemanticProvider, TypeAliasDefinition, TypeRef,
};
use std::path::Path;
use std::sync::Arc;

pub struct FactsProvider {
    inner: SyntaxSemanticProvider,
    runtime: Option<Arc<FactsRuntime>>,
}

impl FactsProvider {
    /// Transparent wrapper: no helper, every answer comes from `inner`.
    pub fn transparent(inner: SyntaxSemanticProvider) -> Self {
        Self {
            inner,
            runtime: None,
        }
    }

    /// Wrapper over an already-installed run runtime.
    pub fn with_runtime(inner: SyntaxSemanticProvider, runtime: Arc<FactsRuntime>) -> Self {
        Self {
            inner,
            runtime: Some(runtime),
        }
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
}

impl SemanticProvider for FactsProvider {
    fn has_classpath_facts(&self) -> bool {
        self.runtime.is_some()
    }

    fn classpath_member_facts(&self, owner: &str) -> Option<Arc<Vec<MemberFacts>>> {
        self.runtime.as_ref()?.member_facts(owner)
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

    fn explicit_nongeneric_kotlin_return(&self, member: &SymbolId) -> Option<TypeRef> {
        self.inner.explicit_nongeneric_kotlin_return(member)
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
