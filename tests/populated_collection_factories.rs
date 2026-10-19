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
fun nullableList(p: Probe): List<String?> = listOf(p.key("l1", null), p.key("l2", "x"))
fun nullableSet(p: Probe): Set<String?> = setOf(p.key("s1", null), p.key("s2", "x"), p.key("s3", null))
fun arrayAsOne(): List<IntArray> = listOf(intArrayOf(1, 2))
"#;
    for root in [&original, &migrated] {
        fs::write(root.join("Factories.kt"), source).unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package factorycase
fun main() {
    suspend fun retainedHarness() {}
    val probe = Probe()
    val map = orderedMap(probe)
    val literal = literalPairs()
    val singletonNull = singletonNull()
    val list = nullableList(probe)
    val set = nullableSet(probe)
    val arrays = arrayAsOne()
    println("${probe.events}|$map|$literal|$singletonNull|$list|$set|${arrays.size}")
}
"#,
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
    baseline
        .kotlin_sources
        .extend([original.join("Factories.kt"), original.join("Runner.kt")]);
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
    assert!(
        factory_java.contains("orderedMap")
            && factory_java.contains("literalPairs")
            && factory_java.contains("singletonNull")
            && factory_java.contains("LinkedHashMap::new"),
        "map factory lowering should be emitted in the translated facade:\n{factory_java}"
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
