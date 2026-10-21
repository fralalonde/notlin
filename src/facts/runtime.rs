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

use super::classpath::ClasspathFacts;
use super::client::FactsClient;
use super::protocol::{Answer, MemberFacts, Query};
use crate::cli::JvmFactsMode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

pub struct FactsRuntime {
    /// `None` once the helper is known dead, so later queries fail fast.
    client: Mutex<Option<FactsClient>>,
    member_facts: ClasspathFacts,
    dead: AtomicBool,
}

impl FactsRuntime {
    /// Start any program that speaks the line protocol (test seam, also used by
    /// the jar path with a `java -jar` command).
    pub fn with_command(command: std::process::Command) -> Result<Arc<Self>, String> {
        let client = FactsClient::start_with_command(command)?;
        Ok(Arc::new(Self {
            client: Mutex::new(Some(client)),
            member_facts: ClasspathFacts::new(),
            dead: AtomicBool::new(false),
        }))
    }

    /// Start the helper jar with the java binary chosen by the environment.
    pub fn start(jar: &std::path::Path) -> Result<Arc<Self>, String> {
        Self::with_command(FactsClient::jar_command(jar))
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
        if self.dead.load(Ordering::Relaxed) {
            return None;
        }
        let mut held = self.client.lock().unwrap_or_else(PoisonError::into_inner);
        let answer = held.as_mut()?.query(query);
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
}

/// Process-wide slot. `None` means "this run went without facts", which is a
/// stable decision: the helper is never started later by another code path.
static RUNTIME: OnceLock<Option<Arc<FactsRuntime>>> = OnceLock::new();

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
