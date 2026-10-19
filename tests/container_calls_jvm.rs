use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "notlin-container-calls-jvm-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SOURCES: [(&str, &str); 3] = [
    (
        "Pipeline.kt",
        r#"package sample.pipeline

import java.util.Optional
import java.util.stream.Stream

class Pipeline {
    fun fromList(values: List<String>): Optional<String> =
        values.stream().filter { it == "keep" }.map { it }.findFirst()

    fun fromSet(values: Set<String>): Optional<String> =
        values.stream().filter { it == "keep" }.map { it }.findFirst()

    fun fromCollection(values: Collection<String>): Optional<String> =
        values.stream().filter { it == "keep" }.map { it }.findFirst()

    fun fromStream(values: Stream<String>): Optional<String> =
        values.filter { it == "keep" }.map { it }.findAny()

    fun fromFlatStream(values: Stream<String>): Optional<String> =
        values.filter { it == "keep" }.flatMap { Samples.singleStream(it) }.map { it }.findFirst()

    fun lazyFilter(values: Collection<String>): Stream<String> =
        values.stream().filter { it == "keep" }

    fun optionalMap(value: Optional<String>): Optional<String> =
        value.filter { it == "keep" }.map { it }

    fun optionalFlatMap(value: Optional<String>): Optional<String> =
        value.flatMap { Samples.singleOptional(it) }

    fun optionalOr(value: Optional<String>): Optional<String> =
        value.or { Samples.singleOptional("fallback") }

    fun eagerDefault(value: Optional<String>): String =
        value.orElse(Samples.defaultValue())

    fun lazyDefault(value: Optional<String>): String =
        value.orElseGet { Samples.defaultValue() }

    fun nullFallback(value: Optional<String>): String = value.orElse(null)
    private fun accept(value: String): String = value
    fun argumentFallback(value: Optional<String>): String = accept(value.orElse(null))

    fun nullableFallback(value: Optional<String>): String? = value.orElse(null)

    fun directStream(values: Collection<String>): Stream<String> = values.stream()

    fun flags(value: Optional<String>): String =
        "${value.isPresent()}:${value.isEmpty()}"

    fun required(value: Optional<String>): String = value.orElseThrow()

    fun retrieved(value: Optional<String>): String = value.get()
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample.runner

import java.util.Optional
import sample.pipeline.Pipeline
import sample.pipeline.Samples

suspend fun main() {
    val pipeline = Pipeline()
    val collection = Samples.collection()
    val stream = pipeline.lazyFilter(collection)
    val readsBeforeTerminal = Samples.collectionReads()
    val lazyFirst = stream.findFirst().orElse("missing")
    val readsAfterTerminal = Samples.collectionReads()
    val present = Optional.of("present")
    val empty = Optional.empty<String>()
    val eagerPresent = pipeline.eagerDefault(present)
    val defaultsAfterEager = Samples.defaultCalls()
    val lazyPresent = pipeline.lazyDefault(present)
    val defaultsAfterLazyPresent = Samples.defaultCalls()
    val lazyEmpty = pipeline.lazyDefault(empty)
    val defaultsAfterLazyEmpty = Samples.defaultCalls()
    val missingGet = try {
        pipeline.retrieved(empty)
    } catch (error: Throwable) {
        error.javaClass.name
    }
    val missingThrow = try {
        pipeline.required(empty)
    } catch (error: Throwable) {
        error.javaClass.name
    }
    val nullFallback = try {
        Samples.callNullFallback(pipeline, empty) ?: "null"
    } catch (error: Throwable) {
        error.javaClass.name
    }
    val nullableFallback = try {
        Samples.callNullableFallback(pipeline, empty) ?: "null"
    } catch (error: Throwable) {
        error.javaClass.name
    }
    val nullStream = try {
        Samples.callDirectStream(pipeline, Samples.nullStreamCollection())
            ?.count()
            ?.toString() ?: "null"
    } catch (error: Throwable) {
        error.javaClass.name
    }
    val fallbackMessage = try { pipeline.nullFallback(empty); "returned" } catch (error: Throwable) { error.message }
    val streamMessage = try { pipeline.directStream(Samples.nullStreamCollection()); "returned" } catch (error: Throwable) { error.message }
    val argumentMessage = try { pipeline.argumentFallback(empty); "returned" } catch (error: Throwable) { error.message }
    println(
        "${pipeline.fromList(Samples.list()).orElse("missing")}|" +
            "${pipeline.fromSet(Samples.set()).orElse("missing")}|" +
            "${pipeline.fromCollection(Samples.collection()).orElse("missing")}|" +
            "$lazyFirst|reads=$readsBeforeTerminal:$readsAfterTerminal|" +
            "${pipeline.fromStream(Samples.stream()).orElse("missing")}|" +
            "${pipeline.fromFlatStream(Samples.stream()).orElse("missing")}|" +
            "${pipeline.optionalMap(Optional.of("keep")).orElse("missing")}|" +
            "${pipeline.optionalFlatMap(Optional.of("flat")).orElse("missing")}|" +
            "${pipeline.optionalOr(empty).orElse("missing")}|" +
            "$eagerPresent:$defaultsAfterEager|$lazyPresent:$defaultsAfterLazyPresent|" +
            "$lazyEmpty:$defaultsAfterLazyEmpty|" +
            "${pipeline.flags(present)}|${pipeline.flags(empty)}|$missingGet|$missingThrow|" +
            "$nullFallback|$nullableFallback|$nullStream"
    )
    println("checks=$fallbackMessage|$streamMessage|$argumentMessage")
}
"#,
    ),
    (
        "Samples.java",
        r#"package sample.pipeline;

import java.util.AbstractCollection;
import java.util.Arrays;
import java.util.Collection;
import java.util.Iterator;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.Collections;
import java.util.Optional;
import java.util.stream.Stream;

public final class Samples {
    private static final AtomicInteger DEFAULT_CALLS = new AtomicInteger();
    private static final AtomicInteger COLLECTION_READS = new AtomicInteger();

    private Samples() {}

    public static Stream<String> singleStream(String value) { return Stream.of(value); }
    public static java.util.Optional<String> singleOptional(String value) { return java.util.Optional.of(value); }

    public static List<String> list() {
        return Arrays.asList("skip", "keep", "later");
    }

    public static Set<String> set() {
        return new LinkedHashSet<>(Arrays.asList("skip", "keep", "later"));
    }

    public static Collection<String> collection() {
        return new AbstractCollection<String>() {
            private final String[] values = {"skip", "keep", "later"};

            @Override public Iterator<String> iterator() {
                return new Iterator<String>() {
                    private int index;

                    @Override public boolean hasNext() { return index < values.length; }
                    @Override public String next() {
                        COLLECTION_READS.incrementAndGet();
                        return values[index++];
                    }
                };
            }

            @Override public int size() { return values.length; }
        };
    }

    public static Collection<String> nullStreamCollection() {
        return new AbstractCollection<String>() {
            @Override public Iterator<String> iterator() {
                return Collections.emptyIterator();
            }

            @Override public int size() { return 0; }

            @Override public Stream<String> stream() { return null; }
        };
    }

    public static String callNullFallback(Pipeline pipeline, Optional<String> value) {
        return pipeline.nullFallback(value);
    }

    public static String callNullableFallback(Pipeline pipeline, Optional<String> value) {
        return pipeline.nullableFallback(value);
    }

    public static Stream<String> callDirectStream(
        Pipeline pipeline, Collection<String> values
    ) {
        return pipeline.directStream(values);
    }

    public static Stream<String> stream() {
        return Stream.of("skip", "keep", "later");
    }

    public static String defaultValue() {
        DEFAULT_CALLS.incrementAndGet();
        return "default";
    }

    public static int defaultCalls() { return DEFAULT_CALLS.get(); }
    public static int collectionReads() { return COLLECTION_READS.get(); }
}
"#,
    ),
];

fn populate(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
}

fn translate(root: &Path, lombok_enabled: bool) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .arg("--root")
        .arg(root)
        .args(["--annotations", "none", "--in-place"]);
    if lombok_enabled {
        command.arg("--lombok");
    }
    command.arg(root).output().expect("run notlin")
}

fn config(
    root: &Path,
    output: PathBuf,
    stdlib: &Path,
    lombok_enabled: bool,
) -> JvmValidationConfig {
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output,
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config.javac_args.extend(["--release".into(), "17".into()]);
    config.classpath.push(stdlib.to_path_buf());
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
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => config.kotlin_sources.push(path),
            Some("java") => config.java_sources.push(path),
            _ => {}
        }
    }
    config.run_main_class = Some("sample.runner.RunnerKt".into());
    config
}

#[test]
fn canonical_stream_and_optional_calls_translate() {
    for lombok_enabled in [false, true] {
        let scratch = Scratch::new();
        let migrated = scratch.0.join("migrated");
        populate(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            migrated.join("Pipeline.java").is_file(),
            "{}\n{}",
            String::from_utf8_lossy(&translated.stderr),
            fs::read_to_string(migrated.join("Pipeline.kt")).unwrap_or_default()
        );
        assert!(migrated.join("Runner.kt").is_file());
        assert!(migrated.join("Samples.java").is_file());
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_stream_and_optional_call_semantics() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        false,
    ))
    .expect("original Kotlin/JVM container fixture compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected.lines().next().unwrap(),
        "keep|keep|keep|keep|reads=0:2|keep|keep|keep|flat|fallback|present:1|present:1|default:2|true:false|false:true|java.util.NoSuchElementException|java.util.NoSuchElementException|java.lang.NullPointerException|null|java.lang.NullPointerException"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Pipeline.java").is_file());
        assert!(migrated.join("Runner.kt").is_file());
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin/Java container fixture compiles and runs");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
