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

use super::protocol::{CallSite, MemberFacts};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// blake3 of an in-memory source, in the byte-array form the rest of notlin
/// already uses for source hashes (`transpiler`, `property_callsite`). Callers
/// planning on a snapshot pass this to `FactsRuntime::call_site_at`, which
/// compares it against the bytes actually on disk.
pub fn source_hash(source: &str) -> [u8; 32] {
    *blake3::hash(source.as_bytes()).as_bytes()
}

/// blake3 of a file's current bytes. `None` when the file cannot be read: an
/// unreadable file has no call-site answer, and mutating a cache keyed on
/// "whatever was there" would be worse than answering nothing.
pub fn file_hash(file: &Path) -> Option<[u8; 32]> {
    let bytes = std::fs::read(file).ok()?;
    Some(*blake3::hash(&bytes).as_bytes())
}

/// One file's last batch, together with the disk bytes it was produced from.
struct CallSiteMemo {
    source_hash: [u8; 32],
    /// `None` records that this run *cannot* get an answer for these bytes (no
    /// helper, a dead helper, an unreadable or contradictory answer). Negative
    /// answers of that kind are memoized on purpose: the planner treats them as
    /// `Unknown` either way, and a dead helper must not be re-asked at every
    /// call site (the fail-closed rule in `runtime.rs`). A per-request absence
    /// is not stored at all — see [`CallSiteLookup::Absent`].
    sites: Option<Arc<Vec<CallSite>>>,
}

/// What one attempt to get a file's batch produced.
///
/// The two negative shapes are kept apart because they age differently: a
/// helper that answered "no evidence for *this* request" is still healthy and
/// the question may be answerable later, while a dead channel never becomes
/// answerable in this run.
pub enum CallSiteLookup {
    /// A real batch for the hashed bytes; may legitimately be empty ("this file
    /// calls nothing we can see").
    Sites(Vec<CallSite>),
    /// The helper answered, explicitly, that it has no evidence for this
    /// request. Not memoized: the reason can be transient, and the cost of
    /// re-asking is one exchange rather than a permanent wrong negative.
    Absent,
    /// This run cannot ask (no helper, dead helper, unreadable file, or an
    /// answer that does not describe these bytes). Memoized so a dead helper is
    /// not re-asked once per call site.
    Unavailable,
}

/// Memoized call-site batches for the whole run, keyed by canonical path.
///
/// Only the newest hash per file is kept: the helper answers for the bytes it
/// read, so a batch produced from an older revision can never be served for a
/// newer one (nor the reverse) — a mismatch is a new query, never a stale hit.
#[derive(Default)]
pub struct CallSiteFacts {
    files: Mutex<HashMap<PathBuf, CallSiteMemo>>,
}

impl CallSiteFacts {
    pub fn new() -> Self {
        Self::default()
    }

    /// The batch for `file` as produced from `source_hash`-matching bytes.
    ///
    /// A memo hit requires the recorded hash to equal `source_hash` exactly,
    /// so a caller that hashed different bytes gets a fresh query rather than
    /// somebody else's answer. `load` is not called under the table lock.
    pub fn sites_at(
        &self,
        file: &Path,
        source_hash: [u8; 32],
        load: impl FnOnce() -> CallSiteLookup,
    ) -> Option<Arc<Vec<CallSite>>> {
        {
            let files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(memo) = files
                .get(file)
                .filter(|memo| memo.source_hash == source_hash)
            {
                // A matched hash is a hit even when `sites` is `None`: a helper
                // that established nothing for these bytes must not be re-asked.
                return memo.sites.clone();
            }
        }
        match load() {
            CallSiteLookup::Sites(sites) => {
                let loaded = Arc::new(sites);
                let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
                files.insert(
                    file.to_path_buf(),
                    CallSiteMemo {
                        source_hash,
                        sites: Some(loaded.clone()),
                    },
                );
                Some(loaded)
            }
            CallSiteLookup::Absent => {
                // Deliberately not memoized: the helper may be able to answer
                // the same question later, and a per-request absence is not a
                // property of these bytes.
                None
            }
            CallSiteLookup::Unavailable => {
                let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
                files.insert(
                    file.to_path_buf(),
                    CallSiteMemo {
                        source_hash,
                        sites: None,
                    },
                );
                None
            }
        }
    }
}

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
