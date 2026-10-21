//! Run-scoped classpath facts and the classifier arena the resolver needs.
//!
//! Two things live here, both keyed to one notlin run:
//!
//! * [`ClasspathFacts`] memoizes the helper's `Members` answer per owner, so
//!   every member decision in the run reads one table instead of re-querying a
//!   process.
//! * [`intern`] gives the resolver's `&'static str` result classifiers a
//!   lifetime for names that only exist at runtime. This is the "arena" the
//!   plan called for: a deliberate, bounded leak (one entry per distinct JVM
//!   type name the run asks about) instead of rewriting `applicable()` and
//!   every one of its consumers to `Cow`/`String`. `establish()` and
//!   `established_platform_call_return_type()` return `&'static str` at a dozen
//!   call sites today; threading a shorter lifetime through them is a much
//!   larger, riskier edit that buys nothing at runtime — the strings still die
//!   with the process.

use super::protocol::MemberFacts;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// Memoized classpath member facts for the whole run.
#[derive(Default)]
pub struct ClasspathFacts {
    owners: Mutex<HashMap<String, Option<Arc<Vec<MemberFacts>>>>>,
}

impl ClasspathFacts {
    pub fn new() -> Self {
        Self::default()
    }

    /// The run's member table for `owner`. `load` runs at most once per owner
    /// and its answer — including "this fact source cannot load the type" — is
    /// what the rest of the run sees. A dead or silent helper is therefore
    /// asked once, not once per call site: every retry would fail the same way,
    /// and the planner treats the answer as `Unknown` either way.
    pub fn members(
        &self,
        owner: &str,
        load: impl FnOnce() -> Option<Vec<MemberFacts>>,
    ) -> Option<Arc<Vec<MemberFacts>>> {
        let mut owners = self.owners.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = owners.get(owner) {
            return cached.clone();
        }
        let loaded = load().map(Arc::new);
        owners.insert(owner.to_string(), loaded.clone());
        loaded
    }
}

/// Process-lifetime interner for classpath-derived names.
pub fn intern(text: &str) -> &'static str {
    static INTERNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut interned = INTERNED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(existing) = interned.get(text) {
        return existing;
    }
    let leaked: &'static str = Box::leak(text.to_string().into_boxed_str());
    interned.insert(leaked);
    leaked
}

/// Interned alternative set for one parameter position.
///
/// `applicable()` compares an argument against a slice of accepted spellings;
/// the static table stores those as `&'static [&'static str]`, so classpath
/// rows get the same shape rather than a second matcher.
pub fn intern_alternatives(types: &[String]) -> &'static [&'static str] {
    static INTERNED: OnceLock<Mutex<HashSet<&'static [&'static str]>>> = OnceLock::new();
    let names = types.iter().map(|ty| intern(ty)).collect::<Vec<_>>();
    let mut interned = INTERNED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(existing) = interned.get(names.as_slice()) {
        return existing;
    }
    let leaked: &'static [&'static str] = Box::leak(names.into_boxed_slice());
    interned.insert(leaked);
    leaked
}
