//! Per-preparation reads of immutable ownership facts. These are scheduling
//! dependencies, including negative reads, rather than eligibility decisions.
use crate::semantics::SymbolId;
use std::cell::RefCell;
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct RetentionReads {
    symbols: HashSet<SymbolId>,
    names: HashSet<String>,
}

impl RetentionReads {
    pub(super) fn affected_by(&self, delta: &HashSet<SymbolId>) -> bool {
        delta
            .iter()
            .any(|symbol| self.symbols.contains(symbol) || self.names.contains(&symbol.name))
    }
}

thread_local! {
    static READS: RefCell<Option<RetentionReads>> = const { RefCell::new(None) };
}

pub(super) struct Probe {
    previous: Option<Option<RetentionReads>>,
}

impl Probe {
    pub(super) fn start() -> Self {
        Self {
            previous: Some(READS.with(|reads| reads.replace(Some(RetentionReads::default())))),
        }
    }

    pub(super) fn finish(mut self) -> RetentionReads {
        let previous = self.previous.take().expect("active retention probe");
        READS
            .with(|reads| reads.replace(previous))
            .unwrap_or_default()
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // finish has already restored the prior probe. Only restore on unwind.
        if let Some(previous) = self.previous.take() {
            READS.with(|reads| {
                reads.replace(previous);
            });
        }
    }
}

pub(crate) fn contains(retained: &HashSet<SymbolId>, symbol: &SymbolId) -> bool {
    READS.with(|reads| {
        if let Some(reads) = reads.borrow_mut().as_mut()
            && !reads.symbols.contains(symbol)
        {
            reads.symbols.insert(symbol.clone());
        }
    });
    retained.contains(symbol)
}

pub(crate) fn contains_name(retained: &HashSet<SymbolId>, name: &str) -> bool {
    READS.with(|reads| {
        if let Some(reads) = reads.borrow_mut().as_mut() {
            reads.names.insert(name.to_owned());
        }
    });
    retained.iter().any(|symbol| symbol.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str) -> SymbolId {
        SymbolId {
            module: "sample".into(),
            package: "sample".into(),
            file: "Sample.kt".into(),
            owner_path: vec![],
            kind: "class".into(),
            name: name.into(),
            receiver: None,
            parameters: vec![],
        }
    }

    #[test]
    fn negative_reads_and_name_fallbacks_invalidate_cached_results() {
        let probe = Probe::start();
        let missing = symbol("Missing");
        assert!(!contains(&HashSet::new(), &missing));
        assert!(!contains_name(&HashSet::new(), "Other"));
        let reads = probe.finish();
        assert!(reads.affected_by(&HashSet::from([missing])));
        assert!(reads.affected_by(&HashSet::from([symbol("Other")])));
        assert!(!reads.affected_by(&HashSet::from([symbol("Unrelated")])));
    }

    #[test]
    fn nested_probes_restore_outer_collection() {
        let outer = Probe::start();
        contains(&HashSet::new(), &symbol("First"));
        let inner = Probe::start();
        contains(&HashSet::new(), &symbol("Inner"));
        let inner_reads = inner.finish();
        contains(&HashSet::new(), &symbol("Last"));
        let outer_reads = outer.finish();
        assert!(inner_reads.affected_by(&HashSet::from([symbol("Inner")])));
        assert!(outer_reads.affected_by(&HashSet::from([symbol("First"), symbol("Last")])));
        assert!(!outer_reads.affected_by(&HashSet::from([symbol("Inner")])));
    }

    #[test]
    fn unwinding_restores_the_previous_probe() {
        let outer = Probe::start();
        let _ = std::panic::catch_unwind(|| {
            let _inner = Probe::start();
            contains(&HashSet::new(), &symbol("Discarded"));
            panic!("abort preparation");
        });
        contains(&HashSet::new(), &symbol("Outer"));
        let reads = outer.finish();
        assert!(reads.affected_by(&HashSet::from([symbol("Outer")])));
        assert!(!reads.affected_by(&HashSet::from([symbol("Discarded")])));
    }
}
