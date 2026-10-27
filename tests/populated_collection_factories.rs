use clap::Parser;
use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_JVM_DIR: AtomicUsize = AtomicUsize::new(0);

struct JvmScratch(PathBuf);

impl Drop for JvmScratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn translated(source: &str) -> (String, usize, usize) {
    let cli = notlin::cli::Cli::parse_from(["notlin", "fixture.kt"]);
    let (files, errors, warnings, _) =
        notlin::transpiler::transpile(source, Path::new("fixture.kt"), &cli);
    let java = files
        .into_iter()
        .map(|(_, java)| java)
        .collect::<Vec<_>>()
        .join("\n");
    (java, errors, warnings)
}

fn translated_with_provider(
    source: &str,
    file: &str,
    provider_sources: &[(&str, &str)],
) -> (String, usize, usize) {
    let provider = notlin::semantics::SyntaxSemanticProvider::new(
        provider_sources
            .iter()
            .map(|(path, contents)| (PathBuf::from(path), (*contents).to_owned())),
    );
    let cli = notlin::cli::Cli::parse_from(["notlin", file]);
    let (files, errors, warnings, _) = notlin::transpiler::transpile_with_workspace_hint(
        source,
        Path::new(file),
        &cli,
        notlin::transpiler::WorkspaceScope {
            semantic_provider: Some(&provider),
            ..Default::default()
        },
        true,
    );
    let java = files
        .into_iter()
        .map(|(_, java)| java)
        .collect::<Vec<_>>()
        .join("\n");
    (java, errors, warnings)
}

#[test]
fn populated_factories_preserve_nulls_duplicates_and_map_overwrite_order() {
    let source = r#"package neutral.factories
fun values(a: String?, b: String?): List<String?> = listOf(a, b)
fun singleton(a: IntArray): List<IntArray> = listOf(a)
fun unique(a: String?, b: String?): Set<String?> = setOf(a, b, a)
fun entries(k1: String?, k2: String?, v: Int?): Map<String?, Int?> = mapOf(k1 to v, k2 to null, k1 to 9)
fun literalPairs(): Map<String?, Int?> = mapOf("dup" to 1, null to null, "dup" to 9)
fun singletonNull(): Map<String?, Int?> = mapOf(null to null)
val propertyPairs: Map<String?, Int?> = mapOf(null to null)
class DefaultPairs(val values: Map<String?, Int?> = mapOf(null to null))
"#;
    let (java, errors, warnings) = translated(source);
    assert_eq!(errors, 0, "{java}");
    assert_eq!(
        warnings, 0,
        "populated stdlib factories and `to` pairs are exact here:\n{java}"
    );
    assert!(
        java.contains("new ArrayList<>(java.util.Arrays.asList(a, b))"),
        "list factory must preserve order and nullable elements:\n{java}"
    );
    assert!(
        java.contains("new ArrayList<>(java.util.Collections.singletonList(a))"),
        "single array-valued elements must not be flattened by varargs:\n{java}"
    );
    assert!(
        java.contains("new LinkedHashSet<>(java.util.Arrays.asList(a, b, a))"),
        "set factory must accept nulls and deduplicate in insertion order:\n{java}"
    );
    assert!(
        java.contains("Stream.<java.util.Map.Entry<String, Integer>>of(new java.util.AbstractMap.SimpleImmutableEntry<>(k1, v), new java.util.AbstractMap.SimpleImmutableEntry<>(k2, null), new java.util.AbstractMap.SimpleImmutableEntry<>(k1, 9))")
            && java.contains("collect(java.util.LinkedHashMap::new")
            && java.contains(".put(")
            && java.contains("java.util.LinkedHashMap::putAll"),
        "map factory must use ordered last-write-wins puts and permit null keys/values:\n{java}"
    );
    assert!(
        java.contains("literalPairs()")
            && java.contains("singletonNull()")
            && java.contains("propertyPairs")
            && java.contains("DefaultPairs"),
        "explicit function, property, and constructor types should provide a map witness:\n{java}"
    );
    assert!(!java.contains("Map.ofEntries"), "{java}");
}

#[test]
fn unknown_factory_pair_shapes_are_not_lowered_as_stdlib_map_entries() {
    let source = r#"package neutral.factories
fun expanded(xs: Array<Pair<String, Int>>): Map<String, Int> = mapOf(*xs)
"#;
    let (java, errors, _) = translated(source);
    assert_eq!(errors, 0);
    assert!(
        !java.contains("expanded(") && !java.contains("Map.ofEntries"),
        "spread map factories must remain Kotlin instead of guessing a Pair schema:\n{java}"
    );
}

#[test]
fn inferred_all_null_map_factory_is_retained_without_an_expected_type() {
    let source = "package neutral.factories\nfun inferredNullMap() = mapOf(null to null)\n";
    let (java, errors, _) = translated(source);
    assert_eq!(errors, 0);
    assert!(
        !java.contains("inferredNullMap(") && !java.contains("Stream.<java.util.Map.Entry"),
        "without a proven Map<K, V> context, retain the Kotlin declaration instead of guessing Java type arguments:\n{java}"
    );
}

#[test]
fn same_package_map_aliases_supply_null_bearing_contexts_in_all_supported_positions() {
    let source = r#"package neutral.aliases
typealias NullableMap = Map<String?, Int?>
fun returned(): NullableMap = mapOf(null to null)
class Holder {
    companion object {
        fun companionReturned(): NullableMap = mapOf(null to null)
    }
    fun localValue() {
        val local: NullableMap = mapOf(null to null)
    }
}
"#;
    let (java, errors, warnings) =
        translated_with_provider(source, "Aliases.kt", &[("Aliases.kt", source)]);
    assert_eq!(errors, 0, "{java}");
    // The source alias stays Kotlin, and the companion receives its existing
    // informational bridge diagnostic. Neither blocks these three contexts.
    assert_eq!(warnings, 2, "{java}");
    assert!(
        java.matches("Stream.<java.util.Map.Entry<java.lang.String, java.lang.Integer>>of(new java.util.AbstractMap.SimpleImmutableEntry<>(null, null))").count() >= 3,
        "function return, companion return, and local property contexts should all provide exact Java arguments:\n{java}"
    );
    assert!(
        java.contains("java.util.Map<java.lang.String, java.lang.Integer> returned()")
            && java
                .contains("java.util.Map<java.lang.String, java.lang.Integer> companionReturned()")
            && !java.contains("NullableMap"),
        "the alias itself must be erased from generated Java signatures:\n{java}"
    );
}

#[test]
fn explicit_and_as_imported_map_aliases_supply_context_and_erase_the_alias() {
    let declarations = "package neutral.aliasdefs\ntypealias NullableMap = kotlin.collections.Map<String?, Int?>\n";
    let explicit = "package neutral.callers\nimport neutral.aliasdefs.NullableMap\nfun explicit(): NullableMap = mapOf(null to null)\n";
    let (java, errors, warnings) = translated_with_provider(
        explicit,
        "Explicit.kt",
        &[("Aliases.kt", declarations), ("Explicit.kt", explicit)],
    );
    assert_eq!(errors, 0, "{java}");
    assert_eq!(warnings, 0, "{java}");
    assert!(
        java.contains("java.util.Map<java.lang.String, java.lang.Integer> explicit()")
            && java.contains("Stream.<java.util.Map.Entry<java.lang.String, java.lang.Integer>>of")
            && !java.contains("NullableMap"),
        "an explicit imported alias should affect both the factory witness and the emitted signature:\n{java}"
    );

    let renamed = "package neutral.callers\nimport neutral.aliasdefs.NullableMap as ResultMap\nfun renamed(): ResultMap = mapOf(null to null)\nfun label(): String = \"ResultMap\"\nfun ResultMap(): String = \"ResultMap\"\nfun localName() { val ResultMap = \"ResultMap\"; println(ResultMap) }\n";
    let (java, errors, warnings) = translated_with_provider(
        renamed,
        "Renamed.kt",
        &[("Aliases.kt", declarations), ("Renamed.kt", renamed)],
    );
    assert_eq!(errors, 0, "{java}");
    assert_eq!(warnings, 0, "{java}");
    assert!(
        java.contains("java.util.Map<java.lang.String, java.lang.Integer> renamed()")
            && java.contains("Stream.<java.util.Map.Entry<java.lang.String, java.lang.Integer>>of")
            && java.contains("\"ResultMap\"")
            && java.contains("ResultMap()")
            && !java.contains("ResultMap renamed()"),
        "an `as` imported alias should erase to the proven Java map type:\n{java}"
    );
}

#[test]
fn map_alias_query_retains_unproven_wildcard_shadowed_and_ambiguous_bindings() {
    use notlin::semantics::{FactStatus, SemanticProvider};

    let wildcard = "package neutral.callers\nimport neutral.aliasdefs.*\nfun retained(): NullableMap = mapOf(null to null)\n";
    let definitions = "package neutral.aliasdefs\ntypealias NullableMap = Map<String?, Int?>\n";
    let provider = notlin::semantics::SyntaxSemanticProvider::new([
        (PathBuf::from("Aliases.kt"), definitions.to_owned()),
        (PathBuf::from("Wildcard.kt"), wildcard.to_owned()),
    ]);
    assert_eq!(
        provider.map_alias_type_arguments(
            Path::new("Wildcard.kt"),
            "neutral.callers",
            "NullableMap"
        ),
        FactStatus::Unknown,
        "a wildcard import cannot establish alias identity"
    );

    let ambiguous = "package neutral.callers\nimport neutral.aliasdefs.NullableMap as Selected\nimport neutral.otherdefs.NullableMap as Selected\nfun retained(): Selected = mapOf(null to null)\n";
    let other_definitions =
        "package neutral.otherdefs\ntypealias NullableMap = MutableMap<String?, Int?>\n";
    let provider = notlin::semantics::SyntaxSemanticProvider::new([
        (PathBuf::from("Aliases.kt"), definitions.to_owned()),
        (PathBuf::from("Other.kt"), other_definitions.to_owned()),
        (PathBuf::from("Ambiguous.kt"), ambiguous.to_owned()),
    ]);
    assert_eq!(
        provider.map_alias_type_arguments(Path::new("Ambiguous.kt"), "neutral.callers", "Selected"),
        FactStatus::Unknown,
        "two explicit imports with the same visible alias are ambiguous"
    );

    let shadowed = "package neutral.callers\nimport neutral.aliasdefs.NullableMap as Selected\nclass Selected\nfun retained(): Selected = mapOf(null to null)\n";
    let provider = notlin::semantics::SyntaxSemanticProvider::new([
        (PathBuf::from("Aliases.kt"), definitions.to_owned()),
        (PathBuf::from("Shadowed.kt"), shadowed.to_owned()),
    ]);
    assert_eq!(
        provider.map_alias_type_arguments(Path::new("Shadowed.kt"), "neutral.callers", "Selected"),
        FactStatus::Unknown,
        "a source type with the same visible name prevents alias proof"
    );
}

#[test]
fn map_alias_facts_ignore_comment_text_and_keep_argument_names_origin_scoped() {
    use notlin::semantics::{FactStatus, SemanticProvider};

    let definitions = "package neutral.aliasdefs\ntypealias NullableMap = Map<String?, Int?>\n";
    let commented = "package neutral.callers\n/*\nimport neutral.aliasdefs.NullableMap as Selected\n*/\nfun retained(): Selected = mapOf(null to null)\n";
    let provider = notlin::semantics::SyntaxSemanticProvider::new([
        (PathBuf::from("Aliases.kt"), definitions.to_owned()),
        (PathBuf::from("Comment.kt"), commented.to_owned()),
    ]);
    assert_eq!(
        provider.map_alias_type_arguments(Path::new("Comment.kt"), "neutral.callers", "Selected"),
        FactStatus::Unknown,
        "an import-shaped comment is not a binding"
    );

    let caller = "package neutral.callers\nimport neutral.aliasdefs.NullableMap\nclass String\nclass Int\nfun use(): NullableMap = mapOf(null to null)\n";
    let provider = notlin::semantics::SyntaxSemanticProvider::new([
        (PathBuf::from("Aliases.kt"), definitions.to_owned()),
        (PathBuf::from("Caller.kt"), caller.to_owned()),
    ]);
    assert_eq!(
        provider.map_alias_type_arguments(Path::new("Caller.kt"), "neutral.callers", "NullableMap"),
        FactStatus::Established(notlin::semantics::MapAliasTypeArguments {
            key: "java.lang.String".to_owned(),
            value: "java.lang.Integer".to_owned(),
        }),
        "caller types named String and Int must not rebind the alias declaration's generic arguments"
    );
}

#[test]
fn map_alias_query_rejects_generic_projected_cyclic_and_custom_map_targets() {
    use notlin::semantics::{FactStatus, SemanticProvider};

    for (name, declaration) in [
        ("GenericMap", "typealias GenericMap<T> = Map<String, T>"),
        (
            "ProjectedMap",
            "typealias ProjectedMap = Map<out String, Int>",
        ),
        (
            "CycleA",
            "typealias CycleA = CycleB\ntypealias CycleB = CycleA",
        ),
        (
            "ShadowedMap",
            "class Map<K, V>\ntypealias ShadowedMap = Map<String, Int>",
        ),
        (
            "HiddenMap",
            "class Outer { typealias HiddenMap = Map<String, Int> }",
        ),
    ] {
        let provider = notlin::semantics::SyntaxSemanticProvider::new([(
            PathBuf::from("Invalid.kt"),
            format!("package neutral.aliases\n{declaration}\n"),
        )]);
        assert_eq!(
            provider.map_alias_type_arguments(Path::new("Invalid.kt"), "neutral.aliases", name),
            FactStatus::Unknown,
            "{name} must not manufacture a concrete map witness"
        );
    }
}

#[test]
fn heterogeneous_map_alias_arguments_preserve_builtin_identities() {
    use notlin::semantics::{FactStatus, MapAliasTypeArguments, SemanticProvider};

    let provider = notlin::semantics::SyntaxSemanticProvider::new([(
        PathBuf::from("Numbers.kt"),
        "package neutral.aliases\ntypealias Values = Map<Any?, Number?>\n".to_owned(),
    )]);
    assert_eq!(
        provider.map_alias_type_arguments(Path::new("Numbers.kt"), "neutral.aliases", "Values"),
        FactStatus::Established(MapAliasTypeArguments {
            key: "java.lang.Object".into(),
            value: "java.lang.Number".into(),
        })
    );
}

#[test]
fn map_alias_chain_uses_declaration_imports_and_rejects_cross_file_shadowing() {
    use notlin::semantics::{FactStatus, SemanticProvider};

    let base = "package neutral.aliasdefs\ntypealias NullableMap = Map<String?, Int?>\n";
    let alias = "package neutral.aliaschain\nimport neutral.aliasdefs.NullableMap as Imported\ntypealias Reexported = Imported\ntypealias Qualified = neutral.aliasdefs.NullableMap\n";
    let caller = "package neutral.consumer\nimport neutral.aliaschain.Reexported\nfun use(value: Reexported) = value\n";
    let provider = notlin::semantics::SyntaxSemanticProvider::new([
        (PathBuf::from("Aliases.kt"), base.to_owned()),
        (PathBuf::from("Reexports.kt"), alias.to_owned()),
        (PathBuf::from("Caller.kt"), caller.to_owned()),
    ]);
    for name in ["Reexported", "Qualified"] {
        assert_eq!(
            provider.map_alias_type_arguments(
                Path::new("Reexports.kt"),
                "neutral.aliaschain",
                name
            ),
            FactStatus::Established(notlin::semantics::MapAliasTypeArguments {
                key: "java.lang.String".to_owned(),
                value: "java.lang.Integer".to_owned(),
            }),
            "each alias hop must use that declaration's own import context"
        );
    }
    assert_eq!(
        provider.map_alias_type_arguments(Path::new("Caller.kt"), "neutral.consumer", "Reexported"),
        FactStatus::Established(notlin::semantics::MapAliasTypeArguments {
            key: "java.lang.String".to_owned(),
            value: "java.lang.Integer".to_owned(),
        }),
        "a cross-file alias remains visible through its explicit import"
    );

    let conflicting_map = "package neutral.aliasdefs\nclass Map<K, V>\n";
    let provider = notlin::semantics::SyntaxSemanticProvider::new([
        (PathBuf::from("Aliases.kt"), base.to_owned()),
        (PathBuf::from("Reexports.kt"), alias.to_owned()),
        (PathBuf::from("Conflict.kt"), conflicting_map.to_owned()),
        (PathBuf::from("Caller.kt"), caller.to_owned()),
    ]);
    assert_eq!(
        provider.map_alias_type_arguments(Path::new("Caller.kt"), "neutral.consumer", "Reexported"),
        FactStatus::Unknown,
        "a same-package classifier in another indexed file prevents proving the alias target"
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_populated_factory_behavior() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let id = NEXT_JVM_DIR.fetch_add(1, Ordering::Relaxed);
    let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "target/jvm-validation/populated-factories-{}-{id}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&scratch);
    let _cleanup = JvmScratch(scratch.clone());
    let original = scratch.join("original");
    let migrated = scratch.join("migrated");
    fs::create_dir_all(&original).unwrap();
    fs::create_dir_all(&migrated).unwrap();
    let source = r#"package factorycase
import factorycase.NullableMap as NullableFactoryMap
class Probe {
    val events: MutableList<String> = mutableListOf()
    var sequence: Int = 0
    fun key(label: String, value: String?): String? { sequence += 1; events.add("$sequence:$label"); return value }
    fun value(label: String, value: Int?): Int? { sequence += 1; events.add("$sequence:$label"); return value }
}
fun orderedMap(p: Probe): Map<String?, Int?> = mapOf(
    p.key("k1", "dup") to p.value("v1", 1),
    p.key("k2", null) to p.value("v2", null),
    p.key("k3", "dup") to p.value("v3", 9)
)
fun literalPairs(): Map<String?, Int?> = mapOf("dup" to 1, null to null, "dup" to 9)
fun singletonNull(): Map<String?, Int?> = mapOf(null to null)
fun aliasedMap(): NullableFactoryMap = mapOf(null to null, "dup" to 2)
fun heterogeneousMap(): HeterogeneousMap = mapOf(null to 1, "kind" to "x", 2 to null)
fun heterogeneousList(): List<Any?> = listOf(null, "x", 2)
class AliasContexts {
    companion object {
        fun companionMap(): NullableFactoryMap = mapOf(null to null)
    }
    fun localMap(): NullableFactoryMap {
        val local: NullableFactoryMap = mapOf(null to null)
        return local
    }
}
fun nullableList(p: Probe): List<String?> = listOf(p.key("l1", null), p.key("l2", "x"))
fun nullableSet(p: Probe): Set<String?> = setOf(p.key("s1", null), p.key("s2", "x"), p.key("s3", null))
fun arrayAsOne(): List<IntArray> = listOf(intArrayOf(1, 2))
"#;
    for root in [&original, &migrated] {
        fs::write(root.join("Factories.kt"), source).unwrap();
        fs::write(root.join("SourceStar.kt"), "package factorycase\nimport factorymodels.*\nfun sourceStarMap(): Map<String, Int> = mapOf(\"key\" to 7)\n").unwrap();
        fs::write(
            root.join("Marker.kt"),
            "package factorymodels\nclass Marker\n",
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package factorycase
fun main() {
    suspend fun retainedHarness() {}
    val probe = Probe()
    val map = orderedMap(probe)
    val literal = literalPairs()
    val singletonNull = singletonNull()
    val aliases = AliasContexts()
    val aliased = aliasedMap()
    val mixedMap = heterogeneousMap()
    val mixedList = heterogeneousList()
    val companionMap = AliasContexts.companionMap()
    val localMap = aliases.localMap()
    val list = nullableList(probe)
    val set = nullableSet(probe)
    val arrays = arrayAsOne()
    println("${probe.events}|$map|$literal|$singletonNull|$aliased|$mixedMap|$mixedList|$companionMap|$localMap|$list|$set|${arrays.size}|${sourceStarMap()}")
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("Aliases.kt"),
            "package factorycase\ntypealias NullableMap = Map<String?, Int?>\ntypealias HeterogeneousMap = Map<Any?, Any?>\n",
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.join("baseline classes"));
    baseline
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    baseline
        .javac_args
        .extend(["--release".into(), "17".into()]);
    baseline.kotlin_sources.extend([
        original.join("Factories.kt"),
        original.join("Aliases.kt"),
        original.join("Runner.kt"),
        original.join("Marker.kt"),
        original.join("SourceStar.kt"),
    ]);
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("factorycase.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original factories compile and run");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();

    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(&migrated)
        .output()
        .expect("run notlin");
    assert!(
        translation.status.success(),
        "notlin failed: {}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let generated = migrated.join("Factories.java");
    assert!(
        migrated.join("Runner.kt").is_file(),
        "the suspend local helper should keep the runtime harness in Kotlin"
    );
    assert!(
        generated.is_file(),
        "Factories.kt should migrate; stderr: {}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let mut after =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.join("migrated classes"));
    after
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    after.javac_args.extend(["--release".into(), "17".into()]);
    after.kotlin_sources = fs::read_dir(&migrated)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    after.java_sources = fs::read_dir(&migrated)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    assert!(
        after.java_sources.contains(&generated),
        "generated facade must be compiled with all translated classes"
    );
    let factory_java = fs::read_to_string(&generated).unwrap();
    assert!(migrated.join("SourceStar.java").is_file());
    assert!(
        factory_java.contains("orderedMap")
            && factory_java.contains("literalPairs")
            && factory_java.contains("singletonNull")
            && factory_java.contains("heterogeneousMap()")
            && factory_java.contains("heterogeneousList()")
            && factory_java.contains("LinkedHashMap::new"),
        "map factory lowering should be emitted in the translated facade:\n{factory_java}"
    );
    let contexts_java = fs::read_to_string(migrated.join("AliasContexts.java")).unwrap();
    assert!(
        factory_java.contains("java.util.Map<java.lang.String, java.lang.Integer> aliasedMap()")
            && contexts_java
                .contains("java.util.Map<java.lang.String, java.lang.Integer> companionMap()")
            && contexts_java
                .contains("java.util.Map<java.lang.String, java.lang.Integer> localMap()")
            && factory_java
                .contains("java.util.Map<java.lang.Object, java.lang.Object> heterogeneousMap()")
            && !factory_java.contains("NullableFactoryMap")
            && !contexts_java.contains("NullableFactoryMap"),
        "cross-file aliases must be erased from every generated Java signature:\n{factory_java}\n{contexts_java}"
    );
    after.classpath.push(stdlib);
    after.run_main_class = Some("factorycase.RunnerKt".into());
    let translated = validate_jvm_sources(&after).expect("translated factories compile and run");
    let actual = translated.commands.last().unwrap().stdout.trim();
    assert_eq!(
        actual, expected,
        "translated JVM behavior must match Kotlin"
    );
}
