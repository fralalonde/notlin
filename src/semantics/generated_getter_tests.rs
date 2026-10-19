use super::{FactStatus, ReferenceReturnNullability, SemanticProvider, SyntaxSemanticProvider};
use crate::java_ir::{JavaElement, JavaSyntaxNode, parse_java, render};
use crate::lombok_getter_contracts::{self, GeneratedGetterContract};
use crate::planning::AcceptedJavaFile;
use crate::transpiler::{WorkspaceScope, parse_tree, plan_with_tree_hint};
use crate::workspace::{SourceIndex, SourceLanguage, SourceOverlay};
use clap::Parser;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const ENTRY_KT: &str = r#"package sample

class Entry(val label: String, val amount: java.math.BigDecimal)
"#;

const CONSUMER_KT: &str = r#"package sample

class Consumer(private val entries: List<Entry>) {
    fun matchingLabel(): String? = entries.stream()
        .filter { entry -> entry.amount.compareTo(java.math.BigDecimal.ONE) == 0 }
        .findFirst()
        .map { entry -> entry.label }
        .orElse(null)
}
"#;

const ENTRY_JAVA: &str = r#"package sample;

import lombok.Getter;

@Getter
public final class Entry {
    private final String label;
    private final java.math.BigDecimal amount;

    public Entry(String label, java.math.BigDecimal amount) {
        this.label = label;
        this.amount = amount;
    }
}
"#;

const RUNNER_KT: &str = r#"package sample

fun main() {
    val entries = listOf(
        Entry("other", java.math.BigDecimal("2.0")),
        Entry("scaled", java.math.BigDecimal("1.00"))
    )
    println(Consumer(entries).matchingLabel() ?: "null")
}
"#;

static NEXT_SCRATCH: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "notlin-generated-getter-jvm-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create unique JVM fixture directory");
        Self(fs::canonicalize(path).expect("canonicalize fixture directory"))
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().expect("fixture file parent"))
            .expect("create fixture source directory");
        fs::write(&path, contents).expect("write JVM fixture source");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let temp = fs::canonicalize(std::env::temp_dir()).expect("canonical temp directory");
        if self.0.starts_with(&temp)
            && self.0.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with("notlin-generated-getter-jvm-")
            })
        {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn jvm_config(
    kotlin_sources: Vec<PathBuf>,
    java_sources: Vec<PathBuf>,
    output_dir: PathBuf,
    stdlib: &Path,
    lombok_enabled: bool,
) -> crate::jvm_validation::JvmValidationConfig {
    let mut config = crate::jvm_validation::JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output_dir,
    );
    config.kotlin_sources = kotlin_sources;
    config.java_sources = java_sources;
    config.classpath.push(stdlib.to_path_buf());
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config.javac_args.extend(["--release".into(), "17".into()]);
    if lombok_enabled {
        config.classpath.push(PathBuf::from(
            std::env::var_os("NOTLIN_LOMBOK").expect("NOTLIN_LOMBOK"),
        ));
        config.javac_args.push("-proc:full".into());
        let mut plugin = std::ffi::OsString::from("-Xplugin=");
        plugin.push(
            std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").expect("NOTLIN_KOTLIN_LOMBOK_PLUGIN"),
        );
        config.kotlinc_args.push(plugin);
    } else {
        config.javac_args.push("-proc:none".into());
    }
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

fn virtual_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-temp")
        .join("generated-getter-contracts")
        .join(name)
}

fn original_provider() -> (SyntaxSemanticProvider, crate::semantics::SymbolId) {
    let path = virtual_path("Entry.kt");
    let provider = SyntaxSemanticProvider::new([(path.clone(), ENTRY_KT.to_owned())]);
    let owners = provider
        .symbols()
        .iter()
        .filter(|symbol| symbol.id.name == "Entry" && symbol.id.kind == "class")
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    let [owner] = owners.as_slice() else {
        panic!("expected one source Entry declaration")
    };
    (provider, owner.clone())
}

fn accepted_entry(owner: crate::semantics::SymbolId) -> AcceptedJavaFile {
    let path = virtual_path("Entry.java");
    let unit = parse_java(ENTRY_JAVA).expect("valid accepted Java fixture");
    AcceptedJavaFile {
        path: path.to_string_lossy().into_owned(),
        unit,
        owners: vec![owner],
    }
}

fn contracts() -> (AcceptedJavaFile, Vec<GeneratedGetterContract>) {
    let (provider, owner) = original_provider();
    let accepted = accepted_entry(owner);
    let contracts = lombok_getter_contracts::collect(&accepted, &provider);
    (accepted, contracts)
}

fn overlaid_index(java_source: &str, include_consumer: bool) -> SourceIndex {
    let mut overlays = vec![SourceOverlay::Replace {
        path: virtual_path("Entry.java"),
        language: SourceLanguage::Java,
        source: java_source.to_owned(),
    }];
    if include_consumer {
        overlays.push(SourceOverlay::Replace {
            path: virtual_path("Consumer.kt"),
            language: SourceLanguage::Kotlin,
            source: CONSUMER_KT.to_owned(),
        });
    }
    SourceIndex::default().with_overlays(&overlays).unwrap()
}

fn provider_for(index: &SourceIndex) -> SyntaxSemanticProvider {
    SyntaxSemanticProvider::new(
        index
            .files
            .iter()
            .map(|file| (file.path.clone(), file.source_text().to_owned())),
    )
}

fn plan_consumer(
    index: &SourceIndex,
    provider: &SyntaxSemanticProvider,
) -> crate::transpiler::PlannedTranslation {
    plan_consumer_source(CONSUMER_KT, index, provider)
}

fn plan_consumer_source(
    source: &str,
    index: &SourceIndex,
    provider: &SyntaxSemanticProvider,
) -> crate::transpiler::PlannedTranslation {
    let cli =
        crate::cli::Cli::parse_from(["notlin", "--lombok", "--annotations", "none", "Consumer.kt"]);
    let tree = parse_tree(source);
    let path = virtual_path("Consumer.kt");
    plan_with_tree_hint(
        source,
        &tree,
        &path,
        &cli,
        WorkspaceScope {
            index: Some(index),
            semantic_provider: Some(provider),
            indexed_path: Some(&path),
            ..WorkspaceScope::default()
        },
        true,
    )
}

const CHILD_JAVA: &str = r#"package sample;
public final class Child extends Entry {
    public Child(String label, java.math.BigDecimal amount) { super(label, amount); }
}
"#;

fn inherited_index(parent: &str, consumer: &str) -> SourceIndex {
    overlaid_index(parent, false)
        .with_overlays(&[
            SourceOverlay::Replace {
                path: virtual_path("Child.java"),
                language: SourceLanguage::Java,
                source: CHILD_JAVA.to_owned(),
            },
            SourceOverlay::Replace {
                path: virtual_path("Consumer.kt"),
                language: SourceLanguage::Kotlin,
                source: consumer.to_owned(),
            },
        ])
        .unwrap()
}

#[test]
fn inherited_generated_getters_preserve_source_and_accepted_origin_targets() {
    let parent = ENTRY_JAVA.replace("public final class", "public class");
    let consumer = CONSUMER_KT.replace("List<Entry>", "List<Child>");
    for accepted_origin in [false, true] {
        let mut index = inherited_index(&parent, &consumer);
        index.set_default_lombok_accessors([virtual_path("Entry.java")]);
        if accepted_origin {
            let origin = SyntaxSemanticProvider::new([(
                virtual_path("Entry.kt"),
                ENTRY_KT.replace("class Entry", "open class Entry"),
            )]);
            let owner = java_owner(&provider_for(&index));
            let kotlin_owner = origin
                .symbols()
                .iter()
                .find(|symbol| symbol.id.kind == "class" && symbol.id.name == owner.name)
                .unwrap()
                .id
                .clone();
            let accepted = AcceptedJavaFile {
                path: virtual_path("Entry.java").to_string_lossy().into_owned(),
                unit: parse_java(&parent).unwrap(),
                owners: vec![kotlin_owner],
            };
            let contracts = lombok_getter_contracts::collect(&accepted, &origin);
            assert_eq!(contracts.len(), 2);
            // Match the accepted renderer's exact output snapshot.
            index = inherited_index(&render(&accepted.unit), &consumer);
            index.set_default_lombok_accessors([virtual_path("Entry.java")]);
            index.set_generated_getters(contracts);
        }
        let mut provider = provider_for(&index);
        provider.enrich_workspace_type_facts(&index);
        let parent_owner = java_owner(&provider);
        let child = provider
            .symbols()
            .iter()
            .find(|symbol| symbol.id.name == "Child" && symbol.id.kind == "class")
            .unwrap()
            .id
            .clone();
        let direct = provider.java_property_read(&parent_owner, "label").unwrap();
        let inherited = provider.java_property_read(&child, "label").unwrap();
        assert_eq!(inherited.target, direct.target);
        assert_eq!(inherited.location, direct.location);
        assert!(inherited.dependencies.contains(&parent_owner));
        assert!(inherited.dependencies.contains(&child));
        assert_eq!(
            inherited.target.file.extension().unwrap(),
            if accepted_origin { "kt" } else { "java" }
        );

        let planned = plan_consumer_source(&consumer, &index, &provider);
        assert!(!planned.emission_failed);
        assert_eq!(planned.java_files.len(), 1);
        let mut targets = Vec::new();
        collect_origin_targets(&planned.accepted.files()[0].unit.root, &mut targets);
        assert!(targets.contains(&inherited.target));

        let changed = index
            .with_overlays_using_cache_from(
                &[SourceOverlay::Replace {
                    path: virtual_path("Entry.java"),
                    language: SourceLanguage::Java,
                    source: parent.replace("@Getter", ""),
                }],
                &index,
            )
            .unwrap();
        provider.enrich_workspace_type_facts(&changed);
        assert!(provider.java_property_read(&child, "label").is_none());
        let mut fresh = provider_for(&changed);
        fresh.enrich_workspace_type_facts(&changed);
        assert!(fresh.java_property_read(&child, "label").is_none());
    }
}

#[test]
fn existing_lombok_source_requires_a_profile_and_keeps_field_provenance() {
    let mut index = overlaid_index(ENTRY_JAVA, true);
    let mut provider = provider_for(&index);
    let owner = java_owner(&provider);
    provider.enrich_workspace_type_facts(&index);
    assert!(provider.java_property_read(&owner, "label").is_none());

    index.set_default_lombok_accessors([virtual_path("Entry.java")]);
    provider.enrich_workspace_type_facts(&index);
    let read = provider.java_property_read(&owner, "label").unwrap();
    let mut field = owner.clone();
    field.owner_path.push(owner.name.clone());
    field.kind = "field".into();
    field.name = "label".into();
    assert_eq!(
        read.target,
        super::SymbolId::generated(&field, "lombok:getter:getLabel()")
    );
    assert_eq!(read.method_name, "getLabel");
    assert_eq!(read.nullability, ReferenceReturnNullability::Platform);
    assert!(read.dependencies.contains(&field));
    assert!(read.dependencies.contains(&owner));

    let planned = plan_consumer(&index, &provider);
    assert!(!planned.emission_failed);
    assert_eq!(planned.java_files.len(), 1);
    assert!(planned.plan.dependencies.iter().any(|dependency| {
        matches!(&dependency.resolution, FactStatus::Established(target) if *target == read.target)
    }));
    let mut targets = Vec::new();
    collect_origin_targets(&planned.accepted.files()[0].unit.root, &mut targets);
    assert!(targets.contains(&read.target));
}

#[test]
fn source_profile_overlay_recomputes_getters_and_rejects_stale_providers() {
    let mut index = overlaid_index(ENTRY_JAVA, false);
    index.set_default_lombok_accessors([virtual_path("Entry.java")]);
    let mut stale = provider_for(&index);
    let owner = java_owner(&stale);
    stale.enrich_workspace_type_facts(&index);
    assert!(stale.java_property_read(&owner, "label").is_some());

    let changed = index
        .with_overlays_using_cache_from(
            &[SourceOverlay::Replace {
                path: virtual_path("Entry.java"),
                language: SourceLanguage::Java,
                source: ENTRY_JAVA.replace("@Getter", ""),
            }],
            &index,
        )
        .unwrap();
    assert!(changed.has_default_lombok_accessors(&virtual_path("Entry.java")));
    let mut fresh = provider_for(&changed);
    fresh.enrich_workspace_type_facts(&changed);
    assert!(fresh.java_property_read(&owner, "label").is_none());
    stale.enrich_workspace_type_facts(&changed);
    assert!(stale.java_property_read(&owner, "label").is_none());

    let deleted = index
        .with_overlays_using_cache_from(
            &[SourceOverlay::Delete {
                path: virtual_path("Entry.java"),
            }],
            &index,
        )
        .unwrap();
    assert!(!deleted.has_default_lombok_accessors(&virtual_path("Entry.java")));
}

#[test]
fn source_inference_preserves_accepted_origin_identity_and_ambiguity() {
    let (accepted, mut contracts) = contracts();
    let java_source = render(&accepted.unit);
    let mut index = overlaid_index(&java_source, false);
    index.set_default_lombok_accessors([virtual_path("Entry.java")]);
    index.set_generated_getters(contracts.clone());
    let mut provider = provider_for(&index);
    let owner = java_owner(&provider);
    provider.enrich_workspace_type_facts(&index);
    let target = contracts
        .iter()
        .find(|contract| contract.property_name == "label")
        .unwrap()
        .target
        .clone();
    assert_eq!(
        provider.java_property_read(&owner, "label").unwrap().target,
        target
    );

    let mut conflict = contracts
        .iter()
        .find(|contract| contract.property_name == "label")
        .unwrap()
        .clone();
    conflict.target = super::SymbolId::generated(&conflict.origin, "conflicting-getter");
    contracts.push(conflict);
    index.set_generated_getters(contracts);
    provider.enrich_workspace_type_facts(&index);
    assert!(provider.java_property_read(&owner, "label").is_none());
}

fn java_owner(provider: &SyntaxSemanticProvider) -> crate::semantics::SymbolId {
    let owners = provider
        .symbols()
        .iter()
        .filter(|symbol| {
            symbol.id.file == virtual_path("Entry.java")
                && symbol.id.name == "Entry"
                && symbol.id.kind == "class"
        })
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    let [owner] = owners.as_slice() else {
        panic!("expected one Java Entry owner")
    };
    owner.clone()
}

fn collect_origin_targets(node: &JavaSyntaxNode, targets: &mut Vec<crate::semantics::SymbolId>) {
    if node.kind == "method_invocation"
        && let Some(target) = &node.origin_target
    {
        targets.push(target.clone());
    }
    for child in &node.children {
        let child = match child {
            JavaElement::Declaration(node)
            | JavaElement::Type(node)
            | JavaElement::Expression(node)
            | JavaElement::Statement(node)
            | JavaElement::Annotation(node)
            | JavaElement::Import(node)
            | JavaElement::Other(node) => Some(node),
            JavaElement::Token(_) => None,
        };
        if let Some(child) = child {
            collect_origin_targets(child, targets);
        }
    }
}

#[test]
fn accepted_getter_contract_enriches_java_property_reads_and_translation_dependencies() {
    let (accepted, contracts) = contracts();
    assert_eq!(
        contracts.len(),
        2,
        "both accepted fields should have getters"
    );
    assert!(contracts.iter().all(|contract| {
        contract.output_path == virtual_path("Entry.java")
            && contract.output_hash == *blake3::hash(render(&accepted.unit).as_bytes()).as_bytes()
            && contract.target
                == crate::semantics::SymbolId::generated(
                    &contract.origin,
                    &format!("lombok:getter:{}()", contract.method_name),
                )
    }));

    let java_source = render(&accepted.unit);
    let mut index = overlaid_index(&java_source, true);
    index.set_generated_getters(contracts.clone());
    let mut provider = provider_for(&index);
    let owner = java_owner(&provider);
    assert!(provider.java_property_read(&owner, "label").is_none());

    provider.enrich_workspace_type_facts(&index);
    let label_read = provider
        .java_property_read(&owner, "label")
        .expect("accepted Lombok getter contract should enrich the Java read");
    assert_eq!(label_read.method_name, "getLabel");
    assert_eq!(
        label_read.target,
        contracts
            .iter()
            .find(|c| c.property_name == "label")
            .unwrap()
            .target
    );
    assert_eq!(label_read.nullability, ReferenceReturnNullability::Platform);
    assert!(!label_read.jvm_primitive);
    assert!(label_read.dependencies.contains(&owner));

    let cli = crate::cli::Cli::try_parse_from(["notlin", "--lombok", "Consumer.kt"])
        .expect("test CLI arguments parse");
    let tree = parse_tree(CONSUMER_KT);
    let consumer_path = virtual_path("Consumer.kt");
    let planned = plan_with_tree_hint(
        CONSUMER_KT,
        &tree,
        &consumer_path,
        &cli,
        WorkspaceScope {
            index: Some(&index),
            semantic_provider: Some(&provider),
            indexed_path: Some(&consumer_path),
            ..WorkspaceScope::default()
        },
        true,
    );
    assert!(
        !planned.emission_failed,
        "accepted Java output should be available"
    );
    assert_eq!(planned.java_files.len(), 1);
    let generated_targets = contracts
        .iter()
        .map(|contract| contract.target.clone())
        .collect::<Vec<_>>();
    assert!(planned.plan.dependencies.iter().any(|dependency| {
        matches!(&dependency.resolution, FactStatus::Established(target) if generated_targets.contains(target))
    }), "consumer dependencies should name generated getter symbols, not spellings");
    let mut invocation_targets = Vec::new();
    for file in planned.accepted.files() {
        collect_origin_targets(&file.unit.root, &mut invocation_targets);
    }
    for target in generated_targets {
        assert!(invocation_targets.contains(&target));
        assert!(planned.plan.dependencies.iter().any(|dependency| {
            matches!(&dependency.resolution, FactStatus::Established(found) if found == &target)
        }));
    }
}

#[test]
fn matching_overlay_carries_contract_but_changed_or_deleted_output_drops_it() {
    let (accepted, contracts) = contracts();
    let java_source = render(&accepted.unit);
    let mut index = overlaid_index(&java_source, false);
    index.set_generated_getters(contracts.clone());

    let same = index
        .with_overlays_using_cache_from(&[], &index)
        .expect("unchanged overlay index");
    assert_eq!(same.generated_getters(), contracts.as_slice());

    let mut changed_java = java_source.clone();
    changed_java.push_str("\n// changed snapshot\n");
    let changed = index
        .with_overlays_using_cache_from(
            &[SourceOverlay::Replace {
                path: virtual_path("Entry.java"),
                language: SourceLanguage::Java,
                source: changed_java,
            }],
            &index,
        )
        .expect("changed Java overlay");
    assert!(changed.generated_getters().is_empty());

    let deleted = index
        .with_overlays_using_cache_from(
            &[SourceOverlay::Delete {
                path: virtual_path("Entry.java"),
            }],
            &index,
        )
        .expect("deleted Java overlay");
    assert!(deleted.generated_getters().is_empty());
}

#[test]
fn conflicting_property_contracts_remain_ambiguous() {
    let (accepted, mut contracts) = contracts();
    let java_source = render(&accepted.unit);
    let mut duplicate = contracts
        .iter()
        .find(|contract| contract.property_name == "label")
        .unwrap()
        .clone();
    duplicate.method_name = "getLabelAlternative".to_owned();
    duplicate.target = crate::semantics::SymbolId::generated(
        &duplicate.origin,
        "lombok:getter:getLabelAlternative()",
    );
    duplicate.jvm_signature.name = duplicate.method_name.clone();
    contracts.push(duplicate);

    let mut index = overlaid_index(&java_source, false);
    index.set_generated_getters(contracts);
    let mut provider = provider_for(&index);
    let owner = java_owner(&provider);
    provider.enrich_workspace_type_facts(&index);

    assert!(provider.java_property_read(&owner, "label").is_none());
}

#[test]
fn provider_index_snapshot_mismatch_does_not_enrich_generated_reads() {
    let (accepted, contracts) = contracts();
    let java_source = render(&accepted.unit);
    let mut index = overlaid_index(&java_source, true);
    index.set_generated_getters(contracts);
    let provider = provider_for(&index);
    let owner = java_owner(&provider);

    // The provider intentionally omits Consumer.kt, so it is not the exact
    // indexed workspace snapshot and cannot consume the generated facts.
    let mut provider = SyntaxSemanticProvider::new([(virtual_path("Entry.java"), java_source)]);
    assert!(provider.java_property_read(&owner, "label").is_none());
    provider.enrich_workspace_type_facts(&index);
    assert!(provider.java_property_read(&owner, "label").is_none());
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn accepted_lombok_getter_contract_matches_original_kotlin_runtime() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();

    let original_entry = scratch.write("original-Entry.kt", ENTRY_KT);
    let original_consumer = scratch.write("original-Consumer.kt", CONSUMER_KT);
    let runner = scratch.write("Runner.kt", RUNNER_KT);
    let original = crate::jvm_validation::validate_jvm_sources(&jvm_config(
        vec![original_entry, original_consumer, runner.clone()],
        Vec::new(),
        scratch.0.join("original-classes"),
        &stdlib,
        false,
    ))
    .expect("original Kotlin producer, consumer, and runner compile and execute");
    let expected = original.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(expected, "scaled");

    let (accepted, contracts) = contracts();
    assert_eq!(contracts.len(), 2);
    assert!(contracts.iter().any(|contract| {
        contract.property_name == "label"
            && contract.nullability == ReferenceReturnNullability::Platform
    }));
    let entry_java = render(&accepted.unit);
    let mut index = overlaid_index(&entry_java, true);
    index.set_generated_getters(contracts.clone());
    let mut provider = provider_for(&index);
    let entry_owner = java_owner(&provider);
    provider.enrich_workspace_type_facts(&index);
    let label_read = provider
        .java_property_read(&entry_owner, "label")
        .expect("accepted Lombok getter read is available");
    assert_eq!(label_read.nullability, ReferenceReturnNullability::Platform);
    assert_eq!(label_read.method_name, "getLabel");

    let cli =
        crate::cli::Cli::parse_from(["notlin", "--lombok", "--annotations", "none", "Consumer.kt"]);
    let tree = parse_tree(CONSUMER_KT);
    let consumer_path = virtual_path("Consumer.kt");
    let planned = plan_with_tree_hint(
        CONSUMER_KT,
        &tree,
        &consumer_path,
        &cli,
        WorkspaceScope {
            index: Some(&index),
            semantic_provider: Some(&provider),
            indexed_path: Some(&consumer_path),
            ..WorkspaceScope::default()
        },
        true,
    );
    assert!(!planned.emission_failed);
    let consumer_java = planned
        .java_files
        .iter()
        .find(|(name, _)| name.ends_with("Consumer.java"))
        .map(|(_, source)| source.clone())
        .expect("accepted Consumer.java output");
    let targets = contracts
        .iter()
        .map(|contract| contract.target.clone())
        .collect::<Vec<_>>();
    for target in &targets {
        assert!(planned.plan.dependencies.iter().any(|dependency| {
            matches!(&dependency.resolution, FactStatus::Established(found) if found == target)
        }));
    }

    let migrated_entry = scratch.write("Entry.java", &entry_java);
    let migrated_consumer = scratch.write("Consumer.java", &consumer_java);
    let migrated = crate::jvm_validation::validate_jvm_sources(&jvm_config(
        vec![runner],
        vec![migrated_entry, migrated_consumer],
        scratch.0.join("migrated-classes"),
        &stdlib,
        true,
    ))
    .expect("generated Java producer and consumer compile and execute with retained Kotlin runner");
    assert_eq!(migrated.commands.last().unwrap().stdout.trim(), expected);
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn existing_lombok_source_getters_preserve_mixed_runtime_behavior() {
    let scratch = Scratch::new();
    let stdlib = PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap());
    let parent = ENTRY_JAVA.replace("public final class", "public class");
    let consumer_source = CONSUMER_KT.replace("List<Entry>", "List<Child>");
    let entry = scratch.write("Entry.java", &parent);
    let child = scratch.write("Child.java", CHILD_JAVA);
    let consumer = scratch.write("Consumer.kt", &consumer_source);
    let runner = scratch.write(
        "Runner.kt",
        r#"package sample
fun main() {
    val values = listOf(Child("other", java.math.BigDecimal("2.0")),
        Child("scaled", java.math.BigDecimal("1.00")))
    println(Consumer(values).matchingLabel())
    println(Consumer(emptyList()).matchingLabel() ?: "none")
    println(Consumer(listOf(Child(null, java.math.BigDecimal.ONE))).matchingLabel() ?: "empty")
}
"#,
    );
    let before = crate::jvm_validation::validate_jvm_sources(&jvm_config(
        vec![consumer, runner.clone()],
        vec![entry.clone(), child.clone()],
        scratch.0.join("original-classes"),
        &stdlib,
        true,
    ))
    .expect("original Kotlin consumer compiles against generated Lombok getters");
    let expected = before.commands.last().unwrap().stdout.replace("\r\n", "\n");
    assert_eq!(expected.trim(), "scaled\nnone\nempty");

    let mut index = inherited_index(&parent, &consumer_source);
    index.set_default_lombok_accessors([virtual_path("Entry.java")]);
    let mut provider = provider_for(&index);
    provider.enrich_workspace_type_facts(&index);
    let planned = plan_consumer_source(&consumer_source, &index, &provider);
    assert!(!planned.emission_failed);
    let java = planned
        .java_files
        .iter()
        .find(|(path, _)| path.ends_with("Consumer.java"))
        .expect("consumer must actually translate");
    let consumer = scratch.write("Consumer.java", &java.1);
    let after = crate::jvm_validation::validate_jvm_sources(&jvm_config(
        vec![runner],
        vec![entry, child, consumer],
        scratch.0.join("migrated-classes"),
        &stdlib,
        true,
    ))
    .expect("migrated Java consumer compiles and runs against existing Lombok source");
    assert_eq!(
        after.commands.last().unwrap().stdout.replace("\r\n", "\n"),
        expected
    );
}

#[test]
#[ignore = "requires javac, javap, and Lombok"]
fn lombok_generated_getters_copy_supported_return_annotations() {
    let scratch = Scratch::new();
    let not_null = scratch.write(
        "org/jetbrains/annotations/NotNull.java",
        r#"package org.jetbrains.annotations;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
@Target({ElementType.FIELD, ElementType.METHOD, ElementType.PARAMETER})
@Retention(RetentionPolicy.CLASS)
public @interface NotNull {}
"#,
    );
    let nullable = scratch.write(
        "org/jetbrains/annotations/Nullable.java",
        r#"package org.jetbrains.annotations;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
@Target({ElementType.FIELD, ElementType.METHOD, ElementType.PARAMETER})
@Retention(RetentionPolicy.CLASS)
public @interface Nullable {}
"#,
    );
    let fixture = scratch.write(
        "AnnotatedGetterFields.java",
        r#"package sample;
import lombok.Getter;
import org.jetbrains.annotations.NotNull;
import org.jetbrains.annotations.Nullable;
@Getter
public final class AnnotatedGetterFields {
    @NotNull private final String required;
    @Nullable private final String optional;
    @lombok.NonNull private final String lombokRequired;

    public AnnotatedGetterFields(String required, String optional, String lombokRequired) {
        this.required = required;
        this.optional = optional;
        this.lombokRequired = lombokRequired;
    }
}
"#,
    );
    let mut config = jvm_config(
        Vec::new(),
        vec![not_null, nullable, fixture],
        scratch.0.join("classes"),
        Path::new("unused-stdlib"),
        true,
    );
    config.classpath.clear();
    config.classpath.push(PathBuf::from(
        std::env::var_os("NOTLIN_LOMBOK").expect("NOTLIN_LOMBOK"),
    ));
    config.run_main_class = None;
    crate::jvm_validation::validate_jvm_sources(&config)
        .expect("javac should generate the configured Lombok getters");

    let java = PathBuf::from(std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"));
    let javap_name = if cfg!(windows) { "javap.exe" } else { "javap" };
    let javap = java
        .parent()
        .expect("Java executable has a parent directory")
        .join(javap_name);
    let output = std::process::Command::new(&javap)
        .args(["-v", "-p", "-classpath"])
        .arg(&config.output_dir)
        .arg("sample.AnnotatedGetterFields")
        .output()
        .expect("run javap beside the configured Java runtime");
    assert!(
        output.status.success(),
        "javap failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let disassembly = String::from_utf8(output.stdout).expect("javap output is UTF-8");
    for (method, annotation) in [
        ("getRequired", "org.jetbrains.annotations.NotNull"),
        ("getOptional", "org.jetbrains.annotations.Nullable"),
        ("getLombokRequired", "lombok.NonNull"),
    ] {
        let block = javap_method_block(&disassembly, method);
        assert!(
            block.contains(annotation),
            "expected {annotation} on generated method {method}, but method block was:\n{block}"
        );
    }
}

fn javap_method_block<'a>(disassembly: &'a str, method: &str) -> &'a str {
    let marker = format!(" {method}();");
    let lines = disassembly.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| line.trim_end().ends_with(&marker))
        .unwrap_or_else(|| panic!("javap did not emit method {method}:\n{disassembly}"));
    let end = (start + 1..lines.len())
        .find(|&index| {
            let trimmed = lines[index].trim_start();
            trimmed == "}"
                || (["public ", "private ", "protected "]
                    .iter()
                    .any(|modifier| trimmed.starts_with(modifier))
                    && trimmed.ends_with(");"))
        })
        .unwrap_or(lines.len());
    // Keep the returned slice borrowed from javap output; identify boundaries
    // by searching the corresponding line substrings in the original text.
    let start_text = lines[start];
    let from = disassembly.find(start_text).unwrap();
    let end_text = lines.get(end).copied().unwrap_or("");
    let to = if end < lines.len() {
        disassembly[from..]
            .find(end_text)
            .map(|offset| from + offset)
            .unwrap_or(disassembly.len())
    } else {
        disassembly.len()
    };
    &disassembly[from..to]
}
