//! The run-scoped facts runtime: one helper process, one memo table.
//!
//! Phase 0 gave every `FactsProvider` its own `FactsClient`, and notlin
//! constructs more than one provider per run (the fixpoint provider and the
//! per-unit local provider). That spawned the helper twice and split the memo
//! table. This module owns the single process instead: `install()` is keyed on
//! a process-wide `OnceLock`, so the first provider that needs facts starts the
//! helper and every later one shares it (and its answers).
//!
//! Failure policy is unchanged from Phase 0: a helper that cannot start, dies,
//! or answers garbage yields `None`, which callers map to `Unknown`. Only
//! `--jvm-facts=on` turns a startup failure into a run error.

use super::classpath::{self, CallSiteFacts, ClasspathFacts};
use super::client::FactsClient;
use super::protocol::{Answer, CallSite, MemberFacts, Query};
use crate::cli::JvmFactsMode;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

pub struct FactsRuntime {
    /// `None` once the helper is known dead, so later queries fail fast.
    client: Mutex<Option<FactsClient>>,
    member_facts: ClasspathFacts,
    call_site_facts: CallSiteFacts,
    dead: AtomicBool,
}

impl FactsRuntime {
    /// Start a custom helper implementing all query families (test seam).
    /// Production jars use `start` so disabled Kotlin queries remain disabled.
    pub fn with_command(command: std::process::Command) -> Result<Arc<Self>, String> {
        Ok(Self::from_client(FactsClient::start_with_command(command)?))
    }

    fn from_client(client: FactsClient) -> Arc<Self> {
        Arc::new(Self {
            client: Mutex::new(Some(client)),
            member_facts: ClasspathFacts::new(),
            call_site_facts: CallSiteFacts::new(),
            dead: AtomicBool::new(false),
        })
    }

    /// Start the helper jar with the java binary chosen by the environment.
    pub fn start(jar: &std::path::Path) -> Result<Arc<Self>, String> {
        Ok(Self::from_client(FactsClient::start(jar)?))
    }

    /// Memoized `Members` answer for one owner. `None` means the helper
    /// established nothing about the owner: not loaded, dead, or unusable.
    pub fn member_facts(&self, owner: &str) -> Option<Arc<Vec<MemberFacts>>> {
        self.member_facts.members(owner, || {
            match self.query(Query::ClassMembers {
                owner: owner.to_string(),
            }) {
                Some(Answer::Members { members, .. }) => members,
                _ => None,
            }
        })
    }

    /// One memoized exchange, or `None` when the helper cannot be trusted for
    /// this run. The first I/O failure marks it dead: a killed helper must not
    /// cost a failed write at every later call site.
    pub fn query(&self, query: Query) -> Option<Answer> {
        self.exchange(query, false)
    }

    /// One exchange that always reaches the helper, for content-addressed
    /// questions (`ResolveCalls`): the client's per-query cache would otherwise
    /// answer a re-ask about the same path with the batch it fetched for an
    /// earlier revision of the file. Dead-helper bookkeeping is shared with
    /// [`FactsRuntime::query`].
    pub fn query_uncached(&self, query: Query) -> Option<Answer> {
        self.exchange(query, true)
    }

    fn exchange(&self, query: Query, uncached: bool) -> Option<Answer> {
        if self.dead.load(Ordering::Relaxed) {
            return None;
        }
        let mut held = self.client.lock().unwrap_or_else(PoisonError::into_inner);
        let client = held.as_mut()?;
        // No response is expected for a disabled query family. Do not write it,
        // and do not confuse missing capability with a dead Track A helper.
        if !client.supports_query(&query) {
            return None;
        }
        let answer = if uncached {
            client.query_uncached(query)
        } else {
            client.query(query)
        };
        match answer {
            Some(answer) => Some(answer),
            None => {
                self.dead.store(true, Ordering::Relaxed);
                // Drops the client, which reaps the helper process.
                *held = None;
                None
            }
        }
    }

    /// Every call site the helper can see in `file`, memoized per
    /// (canonical path, blake3 of the file's bytes at query time).
    ///
    /// `None` means this run cannot establish them — no helper, dead helper, a
    /// file the helper cannot read, or a batch that failed to decode. Callers
    /// map that to `Unknown`; it is never an error, and an empty batch is a
    /// real answer ("no calls visible"), not `None`.
    ///
    /// The helper is asked about the *canonical* path so the bytes it opens are
    /// the bytes hashed here. As with any hashing scheme there is a window
    /// between hashing and the helper's own read; the guard below is what
    /// keeps notlin honest about it (`call_site_at`).
    pub fn call_sites(&self, file: &Path) -> Option<Arc<Vec<CallSite>>> {
        let (canonical, hash) = disk_snapshot(file)?;
        self.call_site_facts
            .sites_at(&canonical, hash, || self.load_call_sites(&canonical))
    }

    /// One call site at `byte_offset`, but only if the caller can prove it is
    /// planning on the bytes the helper saw.
    ///
    /// SOUNDNESS GATE: the Analysis API answers for the file ON DISK; notlin
    /// plans on virtual snapshots — a hash mismatch means the snapshot may
    /// contain text the helper never saw. `expected_source_hash` must be the
    /// blake3 of the exact source the caller is planning on
    /// (`facts::classpath::source_hash`). When it differs from the current disk
    /// bytes this returns `None` **without querying the helper**, so a
    /// speculative overlay never inherits disk facts and a stale hash is never
    /// answered from the memo.
    ///
    /// On a match the batch for those bytes is used (memo hit or fresh query),
    /// and only an offset the helper actually reported is returned.
    pub fn call_site_at(
        &self,
        file: &Path,
        byte_offset: u64,
        expected_source_hash: &[u8; 32],
    ) -> Option<CallSite> {
        let (canonical, hash) = disk_snapshot(file)?;
        if hash != *expected_source_hash {
            log::debug!(
                "call-site query refused for {}: snapshot hash does not match the file on disk",
                canonical.display()
            );
            return None;
        }
        let sites = self
            .call_site_facts
            .sites_at(&canonical, hash, || self.load_call_sites(&canonical))?;
        sites
            .iter()
            .find(|site| site.byte_offset == byte_offset)
            .cloned()
    }

    /// The batch exchange itself. An answer that names another file is not
    /// trusted: the whole point of the canonical path is that the batch
    /// describes the bytes we hashed.
    ///
    /// Uncached on the client as well: the run's memo keyed on content hash is
    /// the only memo this answer may come from, so a new revision always asks.
    fn load_call_sites(&self, canonical: &Path) -> Option<Vec<CallSite>> {
        match self.query_uncached(Query::ResolveCalls {
            file: canonical.to_path_buf(),
        }) {
            Some(Answer::CallSites { file, sites }) if same_file(&file, canonical) => Some(sites),
            Some(Answer::CallSites { file, .. }) => {
                log::debug!(
                    "helper answered call sites for {} instead of {}; ignoring",
                    file.display(),
                    canonical.display()
                );
                None
            }
            _ => None,
        }
    }
}

/// Canonical path plus the hash of its bytes right now. Both halves are
/// required: the memo key is the canonical path, and the hash is what makes a
/// cached batch trustworthy for the revision being planned.
fn disk_snapshot(file: &Path) -> Option<(PathBuf, [u8; 32])> {
    let canonical = std::fs::canonicalize(file).ok()?;
    let hash = classpath::file_hash(&canonical)?;
    Some((canonical, hash))
}

/// Whether the helper's echoed path names the file we asked about. A helper
/// may normalize the spelling; only the same file counts.
fn same_file(answered: &Path, requested: &Path) -> bool {
    answered == requested
        || std::fs::canonicalize(answered)
            .map(|path| path == requested)
            .unwrap_or(false)
}

/// Process-wide slot. `None` means "this run went without facts", which is a
/// stable decision: the helper is never started later by another code path.
static RUNTIME: OnceLock<Option<Arc<FactsRuntime>>> = OnceLock::new();

/// What the run wants the helper child process to index.
///
/// This is *run configuration*, not process environment: it is applied to the
/// child's `Command` ([`super::client::FactsClient::jar_command`]), so a notlin
/// process never mutates its own environment and two runs in one process (the
/// tests) cannot leak sources into each other.
///
/// Values are already rendered (`join_paths`) because the child reads them as
/// single colon-separated strings.
#[derive(Default)]
struct ChildConfig {
    kotlin_sources: Option<std::ffi::OsString>,
    classpath: Option<std::ffi::OsString>,
}

static CHILD_CONFIG: Mutex<ChildConfig> = Mutex::new(ChildConfig {
    kotlin_sources: None,
    classpath: None,
});

/// Tell the helper which Kotlin sources this run is planning on.
///
/// Entries may be directories or `.kt` files (a file contributes its parent
/// directory, exactly as the helper's `NOTLIN_KOTLIN_SOURCES` contract says);
/// the paths are canonicalized here because the helper resolves them against
/// its own working directory. Only the source *roots* are passed, deduplicated
/// in input order: the Analysis API indexes by root, so two files in one
/// directory are one root.
///
/// Called once per run, before the first provider is built. A run with no
/// entries (stdin input) leaves the helper's own environment alone.
pub fn configure_kotlin_sources<I, P>(entries: I)
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut roots: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let path = entry.as_ref();
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let root = if path.is_dir() {
            path
        } else {
            match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                _ => continue,
            }
        };
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    if roots.is_empty() {
        return;
    }
    let rendered = std::env::join_paths(&roots).ok();
    let mut config = CHILD_CONFIG.lock().unwrap_or_else(PoisonError::into_inner);
    config.kotlin_sources = rendered;
}

/// Tell the helper which jars/class directories the run validates against.
///
/// The validation config's classpath is the same one the Java side compiles
/// against, so Kotlin call sites resolve through it too. Explicit configuration
/// wins over an inherited `NOTLIN_CLASSPATH`; when nothing is configured the
/// child simply inherits whatever the user exported.
pub fn configure_classpath<I, P>(entries: I)
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let entries = entries
        .into_iter()
        .map(|entry| entry.as_ref().to_path_buf())
        .collect::<Vec<_>>();
    if entries.is_empty() {
        return;
    }
    let rendered = std::env::join_paths(&entries).ok();
    let mut config = CHILD_CONFIG.lock().unwrap_or_else(PoisonError::into_inner);
    config.classpath = rendered;
}

/// The `NOTLIN_KOTLIN_SOURCES` value for the child: configured roots first,
/// else whatever the caller exported, else nothing.
pub(super) fn kotlin_sources_env() -> Option<std::ffi::OsString> {
    let configured = CHILD_CONFIG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .kotlin_sources
        .clone();
    configured
        .or_else(|| std::env::var_os("NOTLIN_KOTLIN_SOURCES").filter(|value| !value.is_empty()))
}

/// The `NOTLIN_CLASSPATH` value for the child, when this run configured one.
pub(super) fn classpath_env() -> Option<std::ffi::OsString> {
    CHILD_CONFIG
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .classpath
        .clone()
}

/// Whether the helper jar carries Track B, and therefore wants `--kotlin`.
///
/// Detection is by file name (`…-full.jar`, the artifact `jvm-facts/build.sh`
/// produces): the slim jar has no Kotlin code at all, and the full jar answers
/// nothing for `ResolveCalls` unless the flag is passed. `NOTLIN_JVM_FACTS_KOTLIN`
/// (`0`/`false` or `1`/`true`) overrides the guess for a renamed jar.
fn is_full_jar(jar: &Path) -> bool {
    specifies_full_jar(jar, std::env::var_os("NOTLIN_JVM_FACTS_KOTLIN").as_deref())
}

/// The pure half of [`is_full_jar`]: does this jar carry Track B?
///
/// `override_value` is the `NOTLIN_JVM_FACTS_KOTLIN` value (`0`/`false`/`off`/
/// `no` forces off, anything else forces on) or `None` to fall back to the file
/// name. Split out so the decision is testable without mutating the process
/// environment.
pub fn specifies_full_jar(jar: &Path, override_value: Option<&std::ffi::OsStr>) -> bool {
    if let Some(value) = override_value {
        let value = value.to_string_lossy().to_ascii_lowercase();
        return !matches!(value.as_str(), "" | "0" | "false" | "off" | "no");
    }
    jar.file_name()
        .is_some_and(|name| name.to_string_lossy().contains("-full"))
}

/// Whether to launch the helper with `--kotlin`.
///
/// Only when both halves are true: the jar is the full one, and there are
/// Kotlin sources to index. The second half is not optional — the full jar
/// treats `--kotlin` without `NOTLIN_KOTLIN_SOURCES` as a startup failure
/// (exit 2) because a session with no module can never answer anything, and
/// that would take Track A down with it. Without the flag the full jar still
/// serves `Ping`/`ClassMembers`. The client records this launch mode and skips
/// `ResolveCalls` locally, returning no fact without disabling Track A.
/// A full jar with no sources therefore runs as a Track A helper.
pub(super) fn pass_kotlin_flag(jar: &Path) -> bool {
    if !is_full_jar(jar) {
        return false;
    }
    let present = kotlin_sources_env().is_some();
    if !present {
        log::debug!(
            "jvm facts helper is the full jar but no Kotlin sources are configured; launching without --kotlin (call sites stay Unknown)"
        );
    }
    present
}

/// Resolve the run's runtime under a fact-source policy.
///
/// `Off` never consults or populates the slot (a run told to go without facts
/// cannot accidentally pick up a helper). `On` reports startup failure. `Auto`
/// degrades to "no facts" like Phase 0 did.
pub fn install(mode: JvmFactsMode) -> Result<Option<Arc<FactsRuntime>>, String> {
    if matches!(mode, JvmFactsMode::Off) {
        return Ok(None);
    }
    if let Some(installed) = RUNTIME.get() {
        return Ok(installed.clone());
    }
    let started = match mode {
        JvmFactsMode::On => {
            let Some(jar) = FactsClient::jar_path() else {
                return Err("--jvm-facts=on requires NOTLIN_JVM_FACTS_JAR".to_string());
            };
            Some(FactsRuntime::start(&jar)?)
        }
        _ => match FactsClient::jar_path() {
            Some(jar) => match FactsRuntime::start(&jar) {
                Ok(runtime) => Some(runtime),
                Err(error) => {
                    log::debug!("{error}; continuing without JVM facts");
                    None
                }
            },
            None => None,
        },
    };
    // Providers are constructed sequentially today, but losing a race must not
    // keep a second helper alive: the loser drops its process here.
    if RUNTIME.set(started).is_err() {
        log::debug!("another provider already installed the JVM facts helper");
    }
    Ok(RUNTIME.get().and_then(Option::clone))
}

/// The installed runtime, if the run has one. Test seam.
pub fn installed() -> Option<Arc<FactsRuntime>> {
    RUNTIME.get().and_then(Option::clone)
}
