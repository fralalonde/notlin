//! Coverage of original Kotlin ownership boundaries in one invocation.
//!
//! Java files, generated methods and declarations migrated in earlier runs are
//! deliberately excluded from the denominator. A class (including its members)
//! is one boundary; top-level functions and properties are separate boundaries.

use crate::semantics::SymbolId;
use crate::translation_plan::{
    BackendOwner, DeclarationId, DeclarationKind, RetentionReason, SymbolDependency,
    TranslationPlan,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct DeclarationCoverage {
    pub symbol: SymbolId,
    /// Original snapshot hash and byte span, separate from persistent identity.
    pub location: DeclarationId,
    pub kind: DeclarationKind,
    pub translated: bool,
    pub outputs: Vec<PathBuf>,
    pub reasons: Vec<RetentionReason>,
    /// Recorded semantic dependencies; this graph is currently partial.
    pub dependencies: Vec<SymbolDependency>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoverageReport {
    pub version: u32,
    pub scope: &'static str,
    pub original: usize,
    pub translated: usize,
    pub retained: usize,
    /// Counts overlap when a retained declaration has multiple blocker categories.
    pub retained_by_reason: BTreeMap<String, usize>,
    /// Each retained owner counts once for each required unresolved spelling.
    pub unresolved_symbols: BTreeMap<String, usize>,
    pub ambiguous_symbols: BTreeMap<String, usize>,
    pub declarations: Vec<DeclarationCoverage>,
}

impl CoverageReport {
    /// Use accepted output ownership, never output-file or emitted-method counts.
    pub fn collect<'a>(
        plans: impl IntoIterator<Item = &'a TranslationPlan>,
        outputs: impl IntoIterator<Item = (&'a std::path::Path, &'a [SymbolId])>,
    ) -> Result<Self, String> {
        let mut owners = BTreeMap::<SymbolId, BTreeSet<PathBuf>>::new();
        for (path, symbols) in outputs {
            for symbol in symbols {
                owners
                    .entry(symbol.clone())
                    .or_default()
                    .insert(path.into());
            }
        }
        let mut seen = BTreeSet::new();
        let mut declarations = Vec::new();
        let mut retained_by_reason = BTreeMap::new();
        let mut unresolved_symbols = BTreeMap::new();
        let mut ambiguous_symbols = BTreeMap::new();
        for plan in plans {
            let mut dependencies_by_from = BTreeMap::<SymbolId, Vec<SymbolDependency>>::new();
            for dependency in &plan.dependencies {
                dependencies_by_from
                    .entry(dependency.from.clone())
                    .or_default()
                    .push(dependency.clone());
            }
            for declaration in &plan.declarations {
                let symbol = &declaration.symbol_id;
                if !seen.insert(symbol.clone()) {
                    return Err(format!("duplicate coverage identity: {}", symbol.name));
                }
                let outputs = owners.remove(symbol).unwrap_or_default();
                let translated = !outputs.is_empty();
                if translated && declaration.final_owner == Some(BackendOwner::Kotlin) {
                    return Err(format!(
                        "retained declaration has accepted Java output: {}",
                        symbol.name
                    ));
                }
                if !translated {
                    let categories = declaration
                        .retention_reasons
                        .iter()
                        .map(|reason| reason.code())
                        .collect::<BTreeSet<_>>();
                    if categories.is_empty() {
                        *retained_by_reason.entry("unclassified".into()).or_default() += 1;
                    }
                    for code in categories {
                        *retained_by_reason.entry(code.into()).or_default() += 1;
                    }
                    let mut unknown = BTreeSet::new();
                    let mut ambiguous = BTreeSet::new();
                    for reason in &declaration.retention_reasons {
                        match reason {
                            RetentionReason::UnknownRequiredFact { name } => {
                                unknown.insert(name.clone());
                            }
                            RetentionReason::AmbiguousRequiredFact { name, .. } => {
                                ambiguous.insert(name.clone());
                            }
                            _ => {}
                        }
                    }
                    for name in unknown {
                        *unresolved_symbols.entry(name).or_default() += 1;
                    }
                    for name in ambiguous {
                        *ambiguous_symbols.entry(name).or_default() += 1;
                    }
                }
                declarations.push(DeclarationCoverage {
                    symbol: symbol.clone(),
                    location: declaration.id.clone(),
                    kind: declaration.kind,
                    translated,
                    outputs: outputs.into_iter().collect(),
                    reasons: declaration.retention_reasons.clone(),
                    dependencies: dependencies_by_from.remove(symbol).unwrap_or_default(),
                });
            }
        }
        if let Some(symbol) = owners.keys().next() {
            return Err(format!(
                "accepted output has no original Kotlin owner: {}",
                symbol.name
            ));
        }
        declarations.sort_by(|a, b| a.symbol.cmp(&b.symbol));
        let original = declarations.len();
        let translated = declarations
            .iter()
            .filter(|declaration| declaration.translated)
            .count();
        Ok(Self {
            version: 1,
            scope: "invocation_original_kotlin_ownership_boundaries",
            original,
            translated,
            retained: original - translated,
            retained_by_reason,
            unresolved_symbols,
            ambiguous_symbols,
            declarations,
        })
    }

    pub fn summary(&self) -> String {
        let percent = if self.original == 0 {
            0.0
        } else {
            100.0 * self.translated as f64 / self.original as f64
        };
        format!(
            "{} translated ({percent:.1}%) · {} retained · {} original declarations in this run",
            self.translated, self.retained, self.original
        )
    }

    /// Exact retained original-owner counts, grouped by recorded reason code.
    /// A single declaration can carry several reason codes, so these counts
    /// intentionally overlap and must not be summed to infer total retention.
    pub fn retention_summary(&self) -> String {
        let reasons = self
            .retained_by_reason
            .iter()
            .map(|(code, count)| format!("{code} {count}"))
            .collect::<Vec<_>>()
            .join(" · ");
        format!(
            "notlin: Kotlin retained — {} of {} original declarations\n  retention reasons by owner (categories may overlap): {}\n",
            self.retained,
            self.original,
            if reasons.is_empty() { "none" } else { &reasons }
        )
    }
}
