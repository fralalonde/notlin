//! Lightweight, opt-in profiling hooks for workspace analysis.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static ENABLED: OnceLock<bool> = OnceLock::new();
static GETTER_CALLS: AtomicU64 = AtomicU64::new(0);
static GETTER_NODES: AtomicU64 = AtomicU64::new(0);
static GETTER_NANOS: AtomicU64 = AtomicU64::new(0);
static GETTER_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static GETTER_CACHE_MISSES: AtomicU64 = AtomicU64::new(0);
static GETTER_CACHE_REUSED: AtomicU64 = AtomicU64::new(0);
static GETTER_CACHE_INVALIDATED: AtomicU64 = AtomicU64::new(0);

/// Cached so ordinary runs pay only one branch at each profiling boundary.
pub(crate) fn enabled() -> bool {
    *ENABLED.get_or_init(|| std::env::var_os("NOTLIN_PROFILE").is_some())
}

pub(crate) fn getter_cache_hit() {
    if enabled() {
        GETTER_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn getter_cache_miss() {
    if enabled() {
        GETTER_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn getter_cache_overlay(reused: usize, invalidated: usize) {
    if enabled() {
        GETTER_CACHE_REUSED.fetch_add(reused as u64, Ordering::Relaxed);
        GETTER_CACHE_INVALIDATED.fetch_add(invalidated as u64, Ordering::Relaxed);
    }
}

/// Optional per-query timer for getter/hierarchy analysis. Call `visit` for
/// each owner/node explored; dropped timers are aggregated and reported by
/// `report_getter_traversals` at a caller-selected phase boundary.
pub(crate) struct GetterTraversal {
    started: Option<Instant>,
    nodes: u64,
}

impl GetterTraversal {
    pub(crate) fn start() -> Self {
        Self {
            started: enabled().then(Instant::now),
            nodes: 0,
        }
    }

    pub(crate) fn visit(&mut self) {
        if self.started.is_some() {
            self.nodes = self.nodes.saturating_add(1);
        }
    }
}

impl Drop for GetterTraversal {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            GETTER_CALLS.fetch_add(1, Ordering::Relaxed);
            GETTER_NODES.fetch_add(self.nodes, Ordering::Relaxed);
            GETTER_NANOS.fetch_add(
                started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
        }
    }
}

/// Emit and reset aggregate getter traversal metrics at a planner phase edge.
pub(crate) fn report_getter_traversals(phase: &str) {
    if !enabled() {
        return;
    }
    let calls = GETTER_CALLS.swap(0, Ordering::Relaxed);
    let nodes = GETTER_NODES.swap(0, Ordering::Relaxed);
    let nanos = GETTER_NANOS.swap(0, Ordering::Relaxed);
    let cache_hits = GETTER_CACHE_HITS.swap(0, Ordering::Relaxed);
    let cache_misses = GETTER_CACHE_MISSES.swap(0, Ordering::Relaxed);
    let cache_reused = GETTER_CACHE_REUSED.swap(0, Ordering::Relaxed);
    let cache_invalidated = GETTER_CACHE_INVALIDATED.swap(0, Ordering::Relaxed);
    eprintln!(
        "NOTLIN_PROFILE getter traversal {phase}: calls={calls} nodes={nodes} elapsed={:.3}s cache_hits={cache_hits} cache_misses={cache_misses} overlay_reused={cache_reused} overlay_invalidated={cache_invalidated}",
        nanos as f64 / 1_000_000_000.0,
    );
}
