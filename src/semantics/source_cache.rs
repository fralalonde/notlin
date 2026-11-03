//! Cache file-local syntax collection, never resolved workspace facts.

use super::{AnalyzedSource, FileRelations, FileSemanticFacts};
use crate::workspace::SourceFile;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

struct CollectedFacts {
    facts: FileSemanticFacts,
    relations: FileRelations,
}

struct Entry {
    hash: blake3::Hash,
    facts: OnceLock<CollectedFacts>,
}

#[derive(Default)]
pub(crate) struct SourceAnalysisCache {
    entries: HashMap<PathBuf, Arc<Entry>>,
}

impl fmt::Debug for SourceAnalysisCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceAnalysisCache")
            .field("files", &self.entries.len())
            .finish()
    }
}

impl SourceAnalysisCache {
    pub(crate) fn new(files: &[SourceFile]) -> Self {
        Self {
            entries: files
                .iter()
                .map(|file| {
                    (
                        file.path.clone(),
                        Arc::new(Entry {
                            hash: blake3::hash(file.source_text().as_bytes()),
                            facts: OnceLock::new(),
                        }),
                    )
                })
                .collect(),
        }
    }

    pub(crate) fn carry_from(&mut self, previous: &Self) {
        for (path, entry) in &mut self.entries {
            if let Some(old) = previous
                .entries
                .get(path)
                .filter(|old| old.hash == entry.hash)
            {
                *entry = old.clone();
            }
        }
    }

    pub(super) fn collect(&self, files: &[SourceFile]) -> Vec<AnalyzedSource> {
        crate::transpiler::fixpoint::install_parallel(|| {
            files
                .par_iter()
                .map(|file| {
                    let hash = blake3::hash(file.source_text().as_bytes());
                    let Some(entry) = self
                        .entries
                        .get(&file.path)
                        .filter(|entry| entry.hash == hash)
                    else {
                        // Public SourceIndex.files also permits manually assembled
                        // snapshots; an absent/mismatching entry must not answer.
                        return AnalyzedSource::collect((
                            file.path.clone(),
                            file.source_text().to_owned(),
                        ));
                    };
                    let collected = entry.facts.get_or_init(|| {
                        let (facts, relations) =
                            FileSemanticFacts::collect(&file.path, file.source_text());
                        CollectedFacts { facts, relations }
                    });
                    AnalyzedSource {
                        file: file.path.clone(),
                        source: file.source_text().to_owned(),
                        facts: collected.facts.clone(),
                        relations: collected.relations.clone(),
                    }
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantics::{FactStatus, SemanticProvider, SyntaxSemanticProvider};
    use crate::workspace::{SourceIndex, SourceLanguage, SourceOverlay};
    use std::fmt::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "notlin-semantic-cache-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }
        fn replacement(&self, name: &str, source: &str) -> SourceOverlay {
            SourceOverlay::Replace {
                path: self.0.join(name),
                language: if name.ends_with(".java") {
                    SourceLanguage::Java
                } else {
                    SourceLanguage::Kotlin
                },
                source: source.into(),
            }
        }
        fn index(&self, files: &[(&str, &str)]) -> SourceIndex {
            SourceIndex::default()
                .with_overlays(
                    &files
                        .iter()
                        .map(|(name, source)| self.replacement(name, source))
                        .collect::<Vec<_>>(),
                )
                .unwrap()
        }
    }

    struct Digest(blake3::Hasher);
    impl std::fmt::Write for Digest {
        fn write_str(&mut self, value: &str) -> std::fmt::Result {
            self.0.update(value.as_bytes());
            Ok(())
        }
    }

    /// Debug covers every provider field. Sort the one unordered map before
    /// hashing, and stream the representation to avoid a giant temporary string.
    fn fingerprint(mut provider: SyntaxSemanticProvider) -> blake3::Hash {
        let parents = std::mem::take(&mut provider.parents)
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut digest = Digest(blake3::Hasher::new());
        write!(&mut digest, "{provider:#?}\n{parents:#?}").unwrap();
        digest.0.finalize()
    }

    fn fresh(index: &SourceIndex) -> SyntaxSemanticProvider {
        SyntaxSemanticProvider::new(
            index
                .files
                .iter()
                .map(|file| (file.path.clone(), file.source_text().to_owned())),
        )
    }

    fn assert_equivalent(index: &SourceIndex) -> blake3::Hash {
        let mut expected = fresh(index);
        expected.enrich_workspace_type_facts(index);
        let expected = fingerprint(expected);
        for _ in 0..2 {
            let mut actual = SyntaxSemanticProvider::from_workspace(index);
            actual.enrich_workspace_type_facts(index);
            assert_eq!(fingerprint(actual), expected);
        }
        expected
    }

    const PARENT: &str = "package sample\ninterface Parent\n";
    const CHILD: &str = "package sample\nclass Child(val value:Alias=\"default\"):Parent {companion object {fun create()=Child()}\n@Throws(java.io.IOException::class) fun read():String=value}\n";

    #[test]
    fn cached_collection_preserves_all_cold_and_warm_provider_facts() {
        let fixture = Fixture::new();
        let index = fixture.index(&[
            ("Parent.kt", PARENT), ("Child.kt", CHILD),
            ("Alias.kt", "package sample\ntypealias Alias=String\n"),
            ("Holder.java", "package sample; public class Holder<T extends Parent> { public String field; @org.jspecify.annotations.Nullable public T getValue(){return null;} public static String create(){return \"value\";} }"),
            ("package-info.java", "@unknown.NullMarked package sample;"),
            ("Types.kt", "package sample\n@JvmInline value class Token(val code:Int)\nenum class Choice {FIRST,SECOND}\n@com.fasterxml.jackson.annotation.JsonIgnoreProperties(ignoreUnknown=true) class Dto(val label:String=\"default\")\n"),
        ]);
        assert_equivalent(&index);
        assert!(
            index
                .semantic_sources
                .entries
                .values()
                .all(|entry| entry.facts.get().is_some())
        );
        let new_index = index.with_overlays(&[]).unwrap();
        assert_equivalent(&new_index);
        for (path, entry) in &index.semantic_sources.entries {
            assert!(Arc::ptr_eq(
                entry,
                &new_index.semantic_sources.entries[path]
            ));
        }
        for module in ["first", "second"] {
            let cached = SyntaxSemanticProvider::from_analyzed_sources(
                module.into(),
                index.semantic_sources.collect(&index.files),
            );
            let fresh = SyntaxSemanticProvider::new_in_module(
                module,
                index
                    .files
                    .iter()
                    .map(|file| (file.path.clone(), file.source_text().to_owned())),
            );
            assert_eq!(fingerprint(cached), fingerprint(fresh));
        }
    }

    #[test]
    fn unchanged_files_rebind_after_dependency_changes_additions_and_deletions() {
        let fixture = Fixture::new();
        let index = fixture.index(&[
            ("Parent.kt", PARENT),
            ("Child.kt", CHILD),
            ("Alias.kt", "package sample\ntypealias Alias=String\n"),
        ]);
        let original = assert_equivalent(&index);
        let changed = index
            .with_overlays(&[
                fixture.replacement("Parent.kt", "package sample\ninterface Parent:Other\n"),
                fixture.replacement("Other.kt", "package sample\ninterface Other\n"),
                fixture.replacement("Alias.kt", "package sample\ntypealias Alias=Double\n"),
            ])
            .unwrap();
        let child_path = fixture.0.join("Child.kt");
        assert!(Arc::ptr_eq(
            &index.semantic_sources.entries[&child_path],
            &changed.semantic_sources.entries[&child_path]
        ));
        assert_ne!(assert_equivalent(&changed), original);
        let provider = SyntaxSemanticProvider::from_workspace(&changed);
        let FactStatus::Established(child) = provider.resolve("Child") else {
            panic!("missing Child")
        };
        let FactStatus::Established(other) = provider.resolve("Other") else {
            panic!("missing Other")
        };
        assert!(provider.inherits_type(&child, &other));
        let deleted = changed
            .with_overlays(&[
                SourceOverlay::Delete {
                    path: fixture.0.join("Other.kt"),
                },
                SourceOverlay::Delete {
                    path: fixture.0.join("Alias.kt"),
                },
            ])
            .unwrap();
        assert_equivalent(&deleted);
        let provider = SyntaxSemanticProvider::from_workspace(&deleted);
        assert!(matches!(provider.resolve("Other"), FactStatus::Unknown));
        assert!(provider.aliases().is_empty());
        let duplicate = changed
            .with_overlays(&[
                fixture.replacement("OtherParent.kt", "package sample\ninterface Parent\n")
            ])
            .unwrap();
        assert_equivalent(&duplicate);
        assert!(matches!(
            SyntaxSemanticProvider::from_workspace(&duplicate).resolve("Parent"),
            FactStatus::Ambiguous(_)
        ));
    }

    #[test]
    fn source_edits_same_length_replacements_and_public_snapshot_changes_are_fresh() {
        let fixture = Fixture::new();
        let source = "package sample\nclass Alpha {fun getValue():String=\"one\"}\n";
        let index = fixture.index(&[("Types.kt", source)]);
        let original = assert_equivalent(&index);
        let replacement = source.replace("Alpha", "Bravo");
        assert_eq!(source.len(), replacement.len());
        let changed = index
            .with_overlays(&[fixture.replacement("Types.kt", &replacement)])
            .unwrap();
        assert_ne!(assert_equivalent(&changed), original);
        assert!(!Arc::ptr_eq(
            &index.semantic_sources.entries[&fixture.0.join("Types.kt")],
            &changed.semantic_sources.entries[&fixture.0.join("Types.kt")]
        ));
        let mut manual = fixture.index(&[("Types.kt", source)]);
        assert_equivalent(&manual);
        manual.files = changed.files.clone();
        assert_equivalent(&manual);
        manual.files[0].path = fixture.0.join("Renamed.kt");
        assert_equivalent(&manual);
        let mut empty = SourceIndex::default();
        empty.files = manual.files;
        assert_equivalent(&empty);
    }

    #[test]
    #[ignore = "requires NOTLIN_SEMANTIC_BENCH_REFERENCE and a report path"]
    fn benchmark_reference_semantic_collection_without_source_io() {
        use std::time::Instant;
        let root = std::env::var_os("NOTLIN_SEMANTIC_BENCH_REFERENCE").unwrap();
        // Existing discovery/cache I/O happens before timing either provider.
        let index = SourceIndex::discover(std::path::Path::new(&root)).unwrap();
        let started = Instant::now();
        let expected = fresh(&index);
        let first_fresh_seconds = started.elapsed().as_secs_f64();
        let expected = fingerprint(expected);
        let started = Instant::now();
        let cold = SyntaxSemanticProvider::from_workspace(&index);
        let cold_cached_seconds = started.elapsed().as_secs_f64();
        assert_eq!(fingerprint(cold), expected);
        let mut rounds = Vec::new();
        for round in 0..3 {
            let mut measured = [0.0; 2];
            for cached in if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let started = Instant::now();
                let provider = if cached {
                    SyntaxSemanticProvider::from_workspace(&index)
                } else {
                    fresh(&index)
                };
                measured[usize::from(cached)] = started.elapsed().as_secs_f64();
                assert_eq!(fingerprint(provider), expected);
            }
            rounds.push(
                serde_json::json!({"fresh_seconds":measured[0],"cached_seconds":measured[1]}),
            );
        }
        let report = serde_json::json!({
            "files":index.files.len(),"provider_fingerprint":expected.to_hex().to_string(),
            "first_fresh_seconds":first_fresh_seconds,"cold_cached_seconds":cold_cached_seconds,
            "rounds":rounds,"equivalent":true,"source_io_excluded_from_timing":true,
        });
        let target = std::env::var_os("NOTLIN_SEMANTIC_BENCH_REPORT").unwrap();
        std::fs::write(target, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        eprintln!("semantic collection benchmark: {report}");
    }
}
