//! JVM helper policy and construction for the semantic index.
//!
//! Source declarations and call-site observations remain separate: the optional
//! runtime answers only the external-fact accessors on SemanticProvider.
//! Declaration accessors always read the source index.

use crate::cli::JvmFactsMode;
use crate::facts::protocol::CallSite;
use crate::facts::runtime::{self, FactsRuntime};
use crate::semantics::SyntaxSemanticProvider;
use std::path::Path;
use std::sync::Arc;

/// Source index with an optional run-scoped JVM facts runtime.
/// Kept at this path for callers that construct providers from CLI policy.
pub type FactsProvider = SyntaxSemanticProvider;

impl SyntaxSemanticProvider {
    fn from_parts(mut inner: Self, runtime: Option<Arc<FactsRuntime>>) -> Self {
        inner.facts_runtime = runtime;
        inner
    }

    /// Use the source index without external facts.
    pub fn transparent(inner: SyntaxSemanticProvider) -> Self {
        Self::from_parts(inner, None)
    }

    /// Attach an already-installed run runtime to the source index.
    pub fn with_runtime(inner: SyntaxSemanticProvider, runtime: Arc<FactsRuntime>) -> Self {
        Self::from_parts(inner, Some(runtime))
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
        self.facts_runtime.is_some()
    }

    /// The run's call-site batch for `file`, read from the helper's view of the
    /// file on disk (`FactsRuntime::call_sites`). `None` when this run has no
    /// helper or the batch could not be established — callers treat that as
    /// `Unknown`, never as "no calls".
    ///
    /// This is the whole batch; required-call resolution goes through
    /// [`crate::semantics::SemanticProvider::established_call_site`], which is the same answer
    /// narrowed to one hash-guarded offset.
    pub fn call_sites(&self, file: &Path) -> Option<Arc<Vec<CallSite>>> {
        self.facts_runtime.as_ref()?.call_sites(file)
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
        self.facts_runtime
            .as_ref()?
            .call_site_at(file, byte_offset, expected_source_hash)
    }
}
