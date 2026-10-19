use clap::Parser;
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

#[test]
fn generated_getter_configuration_is_part_of_the_migration_snapshot() {
    let scratch = Scratch::new();
    let root = fs::canonicalize(&scratch.0).unwrap();
    let input = root.join("Entry.kt");
    let source = "package sample\nclass Entry(val label: String)\n";
    fs::write(&input, source).unwrap();
    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let files = index
        .kotlin_files()
        .map(|file| (file.path.clone(), file.source_text().to_owned()))
        .collect();
    let cli = notlin::cli::Cli::parse_from([
        "notlin",
        "--lombok",
        "--annotations",
        "none",
        "--in-place",
        root.to_str().unwrap(),
    ]);
    let plan = notlin::migration_pipeline::plan_workspace_migration(
        files,
        &cli,
        &index,
        std::slice::from_ref(&root),
        |_| {},
    )
    .unwrap();
    assert!(
        plan.translation_plans
            .values()
            .any(|file| !file.generated_getters.is_empty()),
        "accepted class should describe its generated getter: {:?}",
        plan.generated_java
    );
    let config = root.join("lombok.config");
    assert!(
        plan.original_snapshots
            .iter()
            .any(|(path, snapshot)| path == &config && snapshot.is_none()),
        "consumed default configuration proof must record config absence"
    );
    notlin::migration_pipeline::verify_original_snapshots(&plan).unwrap();
    fs::write(&config, "lombok.accessors.fluent = true\n").unwrap();
    let error = notlin::migration_pipeline::verify_original_snapshots(&plan).unwrap_err();
    assert!(error.contains("lombok.config"), "{error}");
    assert_eq!(fs::read_to_string(&input).unwrap(), source);
    assert!(
        !root.join("Entry.java").exists(),
        "planning and failed verification must not write outputs"
    );
}

#[test]
fn existing_lombok_consumers_require_default_configuration_and_capture_its_snapshot() {
    let scratch = Scratch::new();
    let root = fs::canonicalize(&scratch.0).unwrap();
    let entry = root.join("Entry.java");
    let java = "package sample;\nimport lombok.Getter;\n@Getter public final class Entry { private final String label; public Entry(String label) { this.label = label; } }\n";
    let consumer = root.join("Consumer.kt");
    let kotlin = "package sample\nimport java.util.Optional\nclass Consumer { fun label(value: Optional<Entry>): String? = value.map { it.label }.orElse(null) }\n";
    fs::write(&entry, java).unwrap();
    fs::write(&consumer, kotlin).unwrap();
    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let plan = |lombok: bool| {
        let mut args = vec!["notlin", "--in-place", "--annotations", "none"];
        if lombok {
            args.push("--lombok");
        }
        args.push(root.to_str().unwrap());
        let cli = notlin::cli::Cli::parse_from(args);
        notlin::migration_pipeline::plan_workspace_migration(
            vec![(consumer.clone(), kotlin.to_owned())],
            &cli,
            &index,
            std::slice::from_ref(&root),
            |_| {},
        )
        .unwrap()
    };
    let disabled = plan(false);
    assert!(
        !disabled
            .generated_java
            .iter()
            .any(|output| output.name == "Consumer.java")
    );
    let enabled = plan(true);
    assert!(
        enabled
            .generated_java
            .iter()
            .any(|output| output.name == "Consumer.java"),
        "a direct property read must translate with a proven existing getter"
    );
    let config = root.join("lombok.config");
    assert!(
        enabled
            .original_snapshots
            .iter()
            .any(|(path, bytes)| path == &config && bytes.is_none())
    );
    notlin::migration_pipeline::verify_original_snapshots(&enabled).unwrap();
    fs::write(&config, "lombok.accessors.fluent = true\n").unwrap();
    assert!(
        notlin::migration_pipeline::verify_original_snapshots(&enabled)
            .unwrap_err()
            .contains("lombok.config")
    );
    let configured = plan(true);
    assert!(
        !configured
            .generated_java
            .iter()
            .any(|output| output.name == "Consumer.java")
    );
    notlin::migration_pipeline::verify_original_snapshots(&configured).unwrap();
    assert_eq!(fs::read_to_string(entry).unwrap(), java);
    assert_eq!(fs::read_to_string(consumer).unwrap(), kotlin);
    assert!(!root.join("Consumer.java").exists());
}

const SOURCES: [(&str, &str); 4] = [
    (
        "JavaValue.java",
        r#"package sample.pipeline;
public final class JavaValue {
    private final String value;
    public JavaValue(String value) { this.value = value; }
    public String getValue() { return value; }
}
"#,
    ),
    (
        "Pipeline.kt",
        r#"package sample.pipeline

import java.util.Optional
import java.util.stream.Stream

class Pipeline {
    private val propertyValues: List<String> = listOf("skip", "keep", "later")

    fun fromList(values: List<String>): Optional<String> =
        values.stream().filter { it == "keep" }.map { it }.findFirst()

    fun fromProperty(): Optional<String> =
        propertyValues.stream().filter { it == "keep" }.findFirst()

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

    fun constructorFallback(value: Optional<String>): RequiredValue =
        RequiredValue(value.orElse(null))

    fun nullableConstructorFallback(value: Optional<String>): NullableValue =
        NullableValue(value.orElse(null))

    fun primitiveFallback(value: Optional<Int>): Int = value.orElse(null)
    fun numericConstructorFallback(value: Optional<Int>): RequiredNumber =
        RequiredNumber(value.orElse(null))

    fun nullableFallback(value: Optional<String>): String? = value.orElse(null)

    fun literalConstructor(value: Optional<String>): JavaValue = JavaValue(value.orElse("fallback"))

    fun directStream(values: Collection<String>): Stream<String> = values.stream()

    fun flags(value: Optional<String>): String =
        "${value.isPresent()}:${value.isEmpty()}"

    fun required(value: Optional<String>): String = value.orElseThrow()

    fun retrieved(value: Optional<String>): String = value.get()
}

class RequiredValue(val value: String)
class NullableValue(val value: String?)
class RequiredNumber(val value: Int)
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
    val constructorMessage = try { pipeline.constructorFallback(empty); "returned" } catch (error: Throwable) { error.message }
    val nullableConstructor = pipeline.nullableConstructorFallback(empty).value ?: "null"
    val emptyNumber = Optional.empty<Int>()
    val primitiveMessage = try { pipeline.primitiveFallback(emptyNumber); "returned" } catch (error: Throwable) { error.message }
    val numericConstructorMessage = try { pipeline.numericConstructorFallback(emptyNumber); "returned" } catch (error: Throwable) { error.message }
    println("literal=${pipeline.literalConstructor(present).value}|${pipeline.literalConstructor(empty).value}")
    println(
        "${pipeline.fromList(Samples.list()).orElse("missing")}|" +
            "${pipeline.fromSet(Samples.set()).orElse("missing")}|" +
            "${pipeline.fromCollection(Samples.collection()).orElse("missing")}|" +
            "${pipeline.fromProperty().orElse("missing")}|" +
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
    println("constructors=$constructorMessage|$nullableConstructor|${pipeline.constructorFallback(present).value}")
    println("numbers=$primitiveMessage|$numericConstructorMessage|${pipeline.numericConstructorFallback(Optional.of(7)).value}")
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

const EAGER_SOURCES: [(&str, &str); 8] = [
    (
        "NotNull.java",
        r#"package org.jetbrains.annotations;
@java.lang.annotation.Target({java.lang.annotation.ElementType.METHOD, java.lang.annotation.ElementType.TYPE_USE})
@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)
public @interface NotNull {}
"#,
    ),
    (
        "JavaEntry.java",
        r#"package sample.eager;
public final class JavaEntry {
    private final String label;
    private final double metric;
    private final Integer number;
    public JavaEntry(String label, double metric, Integer number) {
        this.label = label;
        this.metric = metric;
        this.number = number;
    }
    public String getLabel() { return label == null ? null : Recorder.observe(label); }
    @org.jetbrains.annotations.NotNull public String getRequired() { return label; }
    public java.math.BigDecimal getAmount() { return java.math.BigDecimal.ONE; }
    public double getMetric() { return metric; }
    public Integer getNumber() { return number; }
}
"#,
    ),
    (
        "JavaGetterPipeline.kt",
        r#"package sample.eager
import java.math.BigDecimal
import java.util.Optional

class JavaGetterPipeline {
    fun labels(values: List<JavaEntry>): List<String?> = values.map { it.label }
    fun selected(values: List<JavaEntry>): Optional<String> = values.stream()
        .filter { it.amount.compareTo(BigDecimal.ONE) == 0 }.findFirst().map { it.label }
    fun zero(values: List<JavaEntry>): Optional<JavaEntry> = values.stream()
        .filter { it.metric == 0.0 }.findFirst()
    fun numbers(values: List<JavaEntry>): Optional<JavaEntry> = values.stream()
        .filter { 0 == it.number }.findFirst()
    fun optional(value: Optional<JavaEntry>): Optional<String> = value.map { it.label }
    fun required(values: List<JavaEntry>): List<String> = values.map { it.required }
}
"#,
    ),
    (
        "Pipeline.kt",
        r#"package sample.eager

class Pipeline {
    fun projected(): List<Element?> =
        Recorder.elements().map { Recorder.project(it) }.filterNot { Recorder.reject(it) }

    fun failing(): List<Element?> =
        Recorder.elements().map { Recorder.project(it) }.filterNot { Recorder.rejectWithError(it) }

    fun nullableIntFilter(values: List<Int?>): List<Int?> =
        values.filterNot { it == 2 }

    fun intIdentity(values: List<Int>): List<Int> = values.map { it }

    private fun integers(): List<Int> = Recorder.pollutedIntegers()
    fun factoryIdentity(): List<Int> = integers().map { it }

    fun names(values: List<Element>): List<String> = values.map { it.name }
    fun observed(values: List<RetainedElement>): List<String> = values.map { entry -> entry.name }
    fun numbers(values: List<RetainedElement>): List<Int?> = values.map { it.number }.filterNot { it == null }
    fun labels(values: List<LabelEntry>): List<List<String>> = values.map { it.labels }
}

class RetainedGenericPipeline {
    fun labels(values: List<LabelEntry>): List<List<Any>> = values.map { it.labels }
}

class RetainedRecordPipeline {
    fun names(values: List<RecordEntry>): List<String> = values.map { it.name }
}

class RetainedPipeline {
    fun captured(): List<String?> {
        var suffix = "x"
        return Recorder.elements().map {
            val value = it?.name?.plus(suffix)
            suffix = "y"
            value
        }.filterNot { it == "dropx" }
    }
}

class RetainedFloatingPipeline {
    fun doubles(values: List<Double?>): List<Double?> = values.filterNot { it == 0.0 }
    fun floats(values: List<Float?>): List<Float?> = values.filterNot { it == 0.0f }
}
"#,
    ),
    (
        "TypedPipeline.kt",
        r#"package sample.eager
import java.math.BigDecimal
import java.util.Optional

abstract class TypedPipeline {
    abstract fun find(): Optional<TypedEntry>

    fun fromList(values: List<TypedEntry>): Optional<String> = values.stream()
        .filter { entry -> entry.amount.compareTo(BigDecimal.ONE) == 0 }
        .findFirst().map { it.label }

    fun fromSource(): String = find().map { it.label }.orElse("")
    fun constant(values: List<TypedEntry>): Optional<String> = values.stream().map { "constant" }.findFirst()
    fun shadowed(it: List<TypedEntry>): Optional<String> = it.stream().map { it.label }.findFirst()
    fun fieldProjected(values: List<FieldEntry>): Optional<String> = values.stream()
        .filter { it.amount.compareTo(BigDecimal.ONE) == 0 }
        .findFirst().map { it.label }
    fun primitiveIdentity(values: List<Int>): Optional<Int> = values.stream().map { it }.findFirst()
    fun primitiveConstant(values: List<Int>): Optional<String> = values.stream().map { "constant" }.findFirst()
    fun reflexiveDouble(values: List<Double>): Optional<Double> = values.stream().filter { it == it }.findFirst()
    fun zeroMetric(values: List<MetricEntry>): Optional<MetricEntry> = values.stream().filter { it.value == 0.0 }.findFirst()
}
"#,
    ),
    (
        "Element.kt",
        r#"package sample.eager

class Element(val name: String)
class LabelEntry(val labels: List<String>)
class TypedEntry(val amount: java.math.BigDecimal, val label: String)
class MetricEntry(val value: Double)
class NullableMetricEntry(val value: Double?)
class RetainedOptionalMetricPipeline {
    fun zero(input: java.util.Optional<NullableMetricEntry>): java.util.Optional<Double?> =
        input.map { it.value }.filter { it == 0.0 }
}

class FieldEntry(@JvmField val amount: java.math.BigDecimal, @get:JvmName("hiddenLabel") val label: String) {
    fun getAmount(): java.math.BigDecimal = amount
    fun getLabel(): String = label
    suspend fun retained() { }
}

@JvmRecord data class RecordEntry(val name: String) {
    suspend fun retained() { }
}

class RetainedElement(private val raw: String, val number: Int?) {
    val name: String
        get() = Recorder.observe(raw)

    suspend fun retained() { }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample.eager

suspend fun main() {
    Recorder.reset()
    val pipeline = Pipeline()
    val projected = pipeline.projected()
    val mutable = projected is MutableList<*>
    (projected as MutableList<Element?>).add(null)
    println(
        "projected=${projected.joinToString(",") { it?.name ?: "null" }}|" +
            "mutable=$mutable|factory=${Recorder.factories()}|size=${Recorder.sizeCalls()}|" +
            "iterator=${Recorder.iteratorCalls()}|events=${Recorder.events()}"
    )

    Recorder.reset()
    val captured = RetainedPipeline().captured()
    println(
        "captured=${captured.joinToString(",") { it ?: "null" }}|" +
            "factory=${Recorder.factories()}|size=${Recorder.sizeCalls()}|" +
            "iterator=${Recorder.iteratorCalls()}"
    )

    Recorder.reset()
    val failure = try {
        pipeline.failing()
        "returned"
    } catch (error: Throwable) {
        error.javaClass.name
    }
    println("failure=$failure|events=${Recorder.events()}")

    val nullableInts = pipeline.nullableIntFilter(Recorder.nullableIntegers())
    println("nullableInts=${nullableInts.joinToString(",") { it?.toString() ?: "null" }}")
    val identityFailure = try {
        pipeline.intIdentity(Recorder.pollutedIntegers())
        "returned"
    } catch (error: Throwable) {
        error.javaClass.name
    }
    println("identityFailure=$identityFailure")
    val factoryFailure = try {
        pipeline.factoryIdentity()
        "returned"
    } catch (error: Throwable) {
        error.javaClass.name
    }
    println("factoryFailure=$factoryFailure")
    val floating = RetainedFloatingPipeline()
    val doubles = floating.doubles(Recorder.nullableDoubles()).joinToString(",") { it?.toString() ?: "null" }
    val floats = floating.floats(Recorder.nullableFloats()).joinToString(",") { it?.toString() ?: "null" }
    println("doubles=$doubles|floats=$floats")

    Recorder.reset()
    val entries = listOf(RetainedElement("a", null), RetainedElement("b", 7))
    val names = pipeline.names(listOf(Element("a"), Element("b")))
    val observed = pipeline.observed(entries)
    val numbers = pipeline.numbers(entries)
    println("getters=${names.joinToString(",")}|${observed.joinToString(",")}|${numbers.joinToString(",")}|events=${Recorder.events()}")

    Recorder.reset()
    val getterFailure = try {
        pipeline.observed(listOf(RetainedElement("a", null), RetainedElement("fail", 4), RetainedElement("later", 5)))
        "returned"
    } catch (error: Throwable) {
        error.javaClass.name
    }
    println("getterFailure=$getterFailure|events=${Recorder.events()}")
    val labelled = listOf(LabelEntry(listOf("x")))
    println("generic=${pipeline.labels(labelled)}|widened=${RetainedGenericPipeline().labels(labelled)}")
    println("record=${RetainedRecordPipeline().names(listOf(RecordEntry("record")))}")
    val typed = object : TypedPipeline() {
        override fun find(): java.util.Optional<TypedEntry> = java.util.Optional.of(TypedEntry(java.math.BigDecimal.ONE, "source"))
    }
    val typedValues = listOf(TypedEntry(java.math.BigDecimal.TEN, "skip"), TypedEntry(java.math.BigDecimal.ONE, "keep"))
    println("typed=${typed.fromList(typedValues).get()}|${typed.fromSource()}|${typed.constant(typedValues).get()}|${typed.shadowed(typedValues).get()}")
    val fieldValues = listOf(FieldEntry(java.math.BigDecimal.TEN, "skip"), FieldEntry(java.math.BigDecimal.ONE, "field"))
    println("fields=${typed.fieldProjected(fieldValues).get()}")
    val primitiveIdentity = try { typed.primitiveIdentity(Recorder.nullFirstIntegers()).orElse(-1).toString() } catch (error: Throwable) { error.javaClass.name }
    val primitiveConstant = try { typed.primitiveConstant(Recorder.nullFirstIntegers()).orElse("missing") } catch (error: Throwable) { error.javaClass.name }
    println("boxed=$primitiveIdentity|$primitiveConstant")
    println("primitive=${typed.reflexiveDouble(listOf(Double.NaN, 1.0)).orElse(-1.0)}")
    println("metric=${typed.zeroMetric(listOf(MetricEntry(-0.0), MetricEntry(Double.NaN))).orElse(MetricEntry(99.0)).value}")
    println("nullableMetric=${RetainedOptionalMetricPipeline().zero(java.util.Optional.of(NullableMetricEntry(-0.0))).orElse(99.0)}")
    val javaPipeline = JavaGetterPipeline()
    val javaEntries = listOf(JavaEntry("java", -0.0, null), JavaEntry("other", Double.NaN, 0), JavaEntry(null, 1.0, 2))
    Recorder.reset()
    val javaLabels = javaPipeline.labels(javaEntries)
    val javaSelected = javaPipeline.selected(javaEntries).orElse("missing")
    val javaZero = javaPipeline.zero(javaEntries).get().metric
    val javaNumber = javaPipeline.numbers(javaEntries).get().number
    println("javaGetters=$javaLabels|$javaSelected|$javaZero|$javaNumber|events=${Recorder.events()}")
    println("javaNull=${javaPipeline.optional(java.util.Optional.of(JavaEntry(null, 1.0, null))).orElse("empty")}")
    Recorder.reset()
    val javaFailure = try {
        javaPipeline.labels(listOf(JavaEntry("a", 0.0, null), JavaEntry("fail", 0.0, null), JavaEntry("later", 0.0, null)))
        "returned"
    } catch (error: Throwable) { error.javaClass.name }
    println("javaFailure=$javaFailure|events=${Recorder.events()}")
    val requiredFailure = try {
        javaPipeline.required(listOf(JavaEntry(null, 0.0, null)))
        "returned"
    } catch (error: Throwable) { "${error.javaClass.name}:${error.message}" }
    println("javaRequired=$requiredFailure")
}
"#,
    ),
    (
        "Recorder.java",
        r#"package sample.eager;

import java.util.AbstractCollection;
import java.util.Arrays;
import java.util.Iterator;
import java.util.concurrent.atomic.AtomicInteger;

public final class Recorder {
    private static final AtomicInteger FACTORIES = new AtomicInteger();
    private static final AtomicInteger SIZES = new AtomicInteger();
    private static final AtomicInteger ITERATORS = new AtomicInteger();
    private static final StringBuilder EVENTS = new StringBuilder();

    private Recorder() {}

    public static Iterable<Element> elements() {
        FACTORIES.incrementAndGet();
        return new AbstractCollection<Element>() {
            private final java.util.List<Element> values = Arrays.asList(
                new Element("drop"), new Element("skip"), null, new Element("c")
            );

            @Override public int size() {
                SIZES.incrementAndGet();
                return values.size();
            }

            @Override public Iterator<Element> iterator() {
                ITERATORS.incrementAndGet();
                return values.iterator();
            }
        };
    }

    public static java.util.List<Integer> nullableIntegers() {
        return Arrays.asList(1, null, 2, 3);
    }

    public static java.util.List<Integer> pollutedIntegers() {
        return Arrays.asList(1, null, 3);
    }

    public static java.util.List<Integer> nullFirstIntegers() {
        return Arrays.asList(null, 1);
    }

    public static java.util.List<Double> nullableDoubles() {
        return Arrays.asList(null, -0.0, 0.0, Double.NaN);
    }

    public static java.util.List<Float> nullableFloats() {
        return Arrays.asList(null, -0.0f, 0.0f, Float.NaN);
    }

    public static Element project(Element value) {
        record("A:" + name(value));
        return value;
    }

    public static String observe(String value) {
        record("R:" + value);
        if (value.equals("fail")) throw new IllegalStateException("getter failed");
        return value;
    }

    public static boolean reject(Element value) {
        record("B:" + name(value));
        return value != null && value.getName().equals("skip");
    }

    public static boolean rejectWithError(Element value) {
        record("B:" + name(value));
        if (value != null && value.getName().equals("skip")) {
            throw new IllegalStateException("stop");
        }
        return false;
    }

    private static String name(Element value) {
        return value == null ? "null" : value.getName();
    }

    private static void record(String value) {
        if (EVENTS.length() > 0) EVENTS.append(',');
        EVENTS.append(value);
    }

    public static void reset() {
        FACTORIES.set(0);
        SIZES.set(0);
        ITERATORS.set(0);
        EVENTS.setLength(0);
    }

    public static int factories() { return FACTORIES.get(); }
    public static int sizeCalls() { return SIZES.get(); }
    public static int iteratorCalls() { return ITERATORS.get(); }
    public static String events() { return EVENTS.toString(); }
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

fn populate_eager(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in EAGER_SOURCES {
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

fn eager_config(
    root: &Path,
    output: PathBuf,
    stdlib: &Path,
    lombok_enabled: bool,
) -> JvmValidationConfig {
    let mut config = config(root, output, stdlib, lombok_enabled);
    config.run_main_class = Some("sample.eager.RunnerKt".into());
    config
}

const CHAIN_SOURCES: [(&str, &str); 12] = [
    (
        "Types.kt",
        r#"package sample.types
import sample.chain.Trace

class Detail(private val raw: String) {
    val label: String get() = Trace.label(raw)
    suspend fun retained() {}
}
class OtherDetail(private val raw: String) {
    val label: String get() = Trace.label(raw)
    suspend fun retained() {}
}
class Entry(private val stored: Detail, val other: OtherDetail) {
    val detail: Detail get() = Trace.detail(stored)
    suspend fun retained() {}
}
class ConvertedDetail(val label: String)
class ConvertedEntry(val detail: ConvertedDetail)
class ConvertedNumber(val value: Int)
open class Base
class Derived : Base()
class ConvertedSubtypeEntry(val detail: Derived)
"#,
    ),
    (
        "ChainPipeline.kt",
        r#"package sample.chain
import java.math.BigDecimal
import java.util.Optional
import sample.types.Entry
import sample.types.ConvertedEntry
import sample.types.ConvertedNumber
import sample.types.ConvertedSubtypeEntry
import sample.types.Base

class ChainPipeline {
    fun converted(value: Optional<ConvertedEntry>): Optional<String> = value.map { it.detail.label }
    fun widened(values: List<ConvertedEntry>): Collection<Any?> = values.map { it.detail }
    fun boxed(values: List<ConvertedNumber>): List<Any> = values.map { it.value }
    fun inherited(values: List<ConvertedSubtypeEntry>): List<Base> = values.map { it.detail }
    fun selected(values: List<Entry>): Optional<String> = values.stream()
        .filter { it.detail.label == it.other.label }.findFirst().map { it.detail.label }
    fun javaLabel(value: Optional<JavaEntry>): Optional<String> = value.map { it.detail.label }
    fun viewed(value: Optional<EntryView>): Optional<String> = value.map { it.detail.label }
    fun javaSelected(value: Optional<JavaEntry>): Optional<JavaEntry> = value.filter { it.detail.label == "keep" }
    fun javaAmount(value: Optional<JavaEntry>): Optional<JavaEntry> = value
        .filter { it.detail.amount.compareTo(BigDecimal.ONE) == 0 }
    fun javaZero(value: Optional<JavaEntry>): Optional<JavaEntry> = value.filter { it.detail.metric == 0.0 }
}
class RetainedGenericPipeline {
    fun label(value: Optional<Substituted>): Optional<String> = value.map { it.value }
}
class RetainedCheckedPipeline {
    fun label(value: Optional<ThrowingBox>): Optional<String> = value.map { it.label }
}
"#,
    ),
    (
        "JavaEntry.java",
        r#"package sample.chain;
public final class JavaEntry implements EntryView {
    private final JavaDetail detail;
    public JavaEntry(JavaDetail detail) { this.detail = detail; }
    @Override public JavaDetail getDetail() { Trace.mark("D"); return detail; }
}
"#,
    ),
    (
        "JavaDetail.java",
        r#"package sample.chain;
public class JavaDetail extends JavaDetailParent {
    public JavaDetail(String label) { super(label); }
}
"#,
    ),
    (
        "JavaDetailParent.java",
        r#"package sample.chain;
public class JavaDetailParent implements JavaDetailContract {
    private final String label;
    public JavaDetailParent(String label) { this.label = label; }
    @Override public String getLabel() { return Trace.label(label); }
    public double getMetric() { Trace.mark("M"); return -0.0; }
}
"#,
    ),
    (
        "JavaDetailContract.java",
        r#"package sample.chain;
public interface JavaDetailContract {
    String getLabel();
    default java.math.BigDecimal getAmount() { Trace.mark("A"); return new java.math.BigDecimal("1.00"); }
}
"#,
    ),
    (
        "EntryView.java",
        r#"package sample.chain;
public interface EntryView {
    JavaDetailParent getDetail();
}
"#,
    ),
    (
        "RefinedDetail.java",
        r#"package sample.chain;
public final class RefinedDetail extends JavaDetail {
    public RefinedDetail(String label) { super(label); }
    @Override public String getLabel() { Trace.mark("O"); return super.getLabel(); }
}
"#,
    ),
    (
        "Substituted.java",
        r#"package sample.chain;
class GenericParent<T> {
    private final T value;
    GenericParent(T value) { this.value = value; }
    public T getValue() { return value; }
}
public final class Substituted extends GenericParent<String> {
    public Substituted(String value) { super(value); }
}
"#,
    ),
    (
        "ThrowingBox.java",
        r#"package sample.chain;
class ThrowingParent {
    public String getLabel() throws java.io.IOException {
        Trace.mark("C");
        throw new java.io.IOException("getter failed");
    }
}
public final class ThrowingBox extends ThrowingParent {}
"#,
    ),
    (
        "Trace.java",
        r#"package sample.chain;
import sample.types.Detail;
public final class Trace {
    private static final StringBuilder EVENTS = new StringBuilder();
    public static void mark(String event) {
        if (EVENTS.length() > 0) EVENTS.append(',');
        EVENTS.append(event);
    }
    public static Detail detail(Detail value) { mark("D"); return value; }
    public static String label(String value) {
        mark("L:" + value);
        if ("fail".equals(value)) throw new IllegalStateException("getter failed");
        return value;
    }
    public static void reset() { EVENTS.setLength(0); }
    public static String events() { return EVENTS.toString(); }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample.chain
import java.util.Optional
import sample.types.Entry
import sample.types.Detail
import sample.types.OtherDetail
import sample.types.ConvertedEntry
import sample.types.ConvertedDetail
import sample.types.ConvertedNumber
import sample.types.ConvertedSubtypeEntry
import sample.types.Derived

suspend fun main() {
    val pipeline = ChainPipeline()
    println("converted=${pipeline.converted(Optional.of(ConvertedEntry(ConvertedDetail("converted")))).orElse("empty")}")
    val detail = ConvertedDetail("same")
    val widened = pipeline.widened(listOf(ConvertedEntry(detail)))
    val boxed = pipeline.boxed(listOf(ConvertedNumber(257))).first()
    val derived = Derived()
    val inherited = pipeline.inherited(listOf(ConvertedSubtypeEntry(derived)))
    println("upcasts=${widened.size}:${widened.first() === detail}|$boxed:${boxed is Int}|${inherited.first() === derived}")
    Trace.reset()
    val selected = pipeline.selected(listOf(
        Entry(Detail("skip"), OtherDetail("different")),
        Entry(Detail("keep"), OtherDetail("keep")),
        Entry(Detail("later"), OtherDetail("later"))
    )).orElse("empty")
    println("selected=$selected|events=${Trace.events()}")
    for (label in listOf("keep", null, "fail")) {
        Trace.reset()
        val result = try {
            pipeline.javaLabel(Optional.of(JavaEntry(JavaDetail(label)))).orElse("empty")
        } catch (error: Throwable) { error.javaClass.name }
        println("label=$label:$result|events=${Trace.events()}")
    }
    Trace.reset()
    val missing = try {
        pipeline.javaLabel(Optional.of(JavaEntry(null))).orElse("empty")
    } catch (error: Throwable) { error.javaClass.name }
    println("missing=$missing|events=${Trace.events()}")
    Trace.reset()
    println("empty=${pipeline.javaLabel(Optional.empty()).orElse("empty")}|events=${Trace.events()}")
    Trace.reset()
    val nullable = pipeline.javaSelected(Optional.of(JavaEntry(JavaDetail(null)))).isPresent()
    val amount = pipeline.javaAmount(Optional.of(JavaEntry(JavaDetail("keep")))).isPresent()
    val zero = pipeline.javaZero(Optional.of(JavaEntry(JavaDetail("keep")))).isPresent()
    println("filters=$nullable:$amount:$zero|events=${Trace.events()}")
    Trace.reset()
    val viewed = pipeline.viewed(Optional.of<EntryView>(JavaEntry(RefinedDetail("override")))).orElse("empty")
    println("viewed=$viewed|events=${Trace.events()}")
    Trace.reset()
    val overridden = pipeline.javaLabel(Optional.of(JavaEntry(RefinedDetail("dynamic")))).orElse("empty")
    println("overridden=$overridden|events=${Trace.events()}")
    println("generic=${RetainedGenericPipeline().label(Optional.of(Substituted("generic"))).orElse("empty")}")
    Trace.reset()
    val checked = try {
        RetainedCheckedPipeline().label(Optional.of(ThrowingBox())).orElse("empty")
    } catch (error: Throwable) { error.javaClass.name }
    println("checked=$checked|events=${Trace.events()}")
}
"#,
    ),
];

fn populate_chains(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in CHAIN_SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
}

#[test]
fn typed_property_chains_translate_without_extracting_retained_classes() {
    for lombok_enabled in [false, true] {
        let scratch = Scratch::new();
        let migrated = scratch.0.join("chains");
        populate_chains(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        let java =
            fs::read_to_string(migrated.join("ChainPipeline.java")).unwrap_or_else(|error| {
                panic!(
                    "{error}\n{}\n{}",
                    String::from_utf8_lossy(&translated.stderr),
                    fs::read_to_string(migrated.join("ChainPipeline.kt")).unwrap_or_default()
                )
            });
        assert!(java.contains(".getDetail().getLabel()"), "{java}");
        assert!(java.contains(".getOther().getLabel()"), "{java}");
        assert!(java.contains(".getDetail().getMetric()"), "{java}");
        assert!(migrated.join("Types.kt").exists());
        for name in ["Entry", "Detail", "OtherDetail"] {
            assert!(!migrated.join(format!("{name}.java")).exists());
        }
        for name in ["ConvertedEntry", "ConvertedDetail"] {
            assert!(migrated.join(format!("{name}.java")).is_file());
        }
        let retained = fs::read_to_string(migrated.join("ChainPipeline.kt")).unwrap();
        for name in ["RetainedGenericPipeline", "RetainedCheckedPipeline"] {
            assert!(retained.contains(&format!("class {name}")), "{retained}");
            assert!(!migrated.join(format!("{name}.java")).exists());
        }
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_property_chain_evaluation_and_nulls() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("chains-original");
    populate_chains(&original);
    let chain_config = |root: &Path, output: PathBuf, lombok| {
        let mut config = config(root, output, &stdlib, lombok);
        config.run_main_class = Some("sample.chain.RunnerKt".into());
        config
    };
    let before = validate_jvm_sources(&chain_config(
        &original,
        scratch.0.join("original-classes"),
        false,
    ))
    .expect("original property chains compile and run");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .replace("\r\n", "\n")
        .trim()
        .to_owned();
    assert_eq!(
        expected,
        concat!(
            "converted=converted\n",
            "upcasts=1:true|257:true|true\n",
            "selected=keep|events=D,L:skip,L:different,D,L:keep,L:keep,D,L:keep\n",
            "label=keep:keep|events=D,L:keep\n",
            "label=null:empty|events=D,L:null\n",
            "label=fail:java.lang.IllegalStateException|events=D,L:fail\n",
            "missing=java.lang.NullPointerException|events=D\n",
            "empty=empty|events=\n",
            "filters=false:true:true|events=D,L:null,D,A,D,M\n",
            "viewed=override|events=D,O,L:override\n",
            "overridden=dynamic|events=D,O,L:dynamic\n",
            "generic=generic\n",
            "checked=java.io.IOException|events=C"
        )
    );
    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("chains-migrated-{lombok_enabled}"));
        populate_chains(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("ChainPipeline.java").is_file());
        let after = validate_jvm_sources(&chain_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            lombok_enabled,
        ))
        .expect("migrated property chains compile and run");
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .replace("\r\n", "\n")
                .trim(),
            expected,
            "property chain runtime in Lombok mode {lombok_enabled}"
        );
    }
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
fn eager_collection_calls_translate_to_shared_structured_helpers() {
    let scratch = Scratch::new();
    let migrated = scratch.0.join("eager-migrated");
    populate_eager(&migrated);
    let translated = translate(&migrated, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    let java = fs::read_to_string(migrated.join("Pipeline.java")).unwrap_or_else(|error| {
        panic!(
            "{error}\n{}\n{}",
            String::from_utf8_lossy(&translated.stderr),
            fs::read_to_string(migrated.join("Pipeline.kt")).unwrap_or_default()
        )
    });
    assert_eq!(
        java.matches("private static <E, R> java.util.List<R>")
            .count(),
        1,
        "map helper should be shared across methods:\n{java}"
    );
    assert_eq!(
        java.matches("private static <E> java.util.List<E>").count(),
        1,
        "filterNot helper should be shared across methods:\n{java}"
    );
    assert!(java.contains("java.util.Collection<?>"), "{java}");
    assert!(java.contains("java.util.function.Function<? super E, ? extends R>"));
    assert!(java.contains("java.util.function.Predicate<? super E>"));
    assert!(java.contains("Recorder.elements()"));
    let map_start = java
        .find("private static <E, R> java.util.List<R>")
        .unwrap();
    let map_end = java[map_start..]
        .find("\n    }")
        .map(|offset| map_start + offset)
        .unwrap();
    let map_helper = &java[map_start..map_end];
    let size = map_helper
        .find(".size()")
        .expect("map preallocates from collection size");
    let iteration = map_helper.find("for (E value : values)").unwrap();
    assert!(
        size < iteration,
        "size must precede iterator use:\n{map_helper}"
    );
    let filter_start = java.find("private static <E> java.util.List<E>").unwrap();
    let filter_end = java[filter_start..]
        .find("\n    }")
        .map(|offset| filter_start + offset)
        .unwrap_or(java.len());
    let filter_helper = &java[filter_start..filter_end];
    assert!(
        !filter_helper.contains(".size()"),
        "filterNot should not ask its input for size:\n{filter_helper}"
    );
    assert_eq!(java.matches("Recorder.elements()").count(), 2);
    assert!(migrated.join("Runner.kt").is_file());
    let retained_kotlin = fs::read_to_string(migrated.join("Pipeline.kt")).unwrap();
    assert!(retained_kotlin.contains("class RetainedPipeline"));
    assert!(retained_kotlin.contains("fun captured()"));
    assert!(retained_kotlin.contains("class RetainedFloatingPipeline"));
    assert!(retained_kotlin.contains("class RetainedGenericPipeline"));
    assert!(retained_kotlin.contains("class RetainedRecordPipeline"));
    assert!(
        migrated.join("TypedPipeline.java").is_file(),
        "{}",
        fs::read_to_string(migrated.join("TypedPipeline.kt")).unwrap_or_default()
    );
    assert!(
        migrated.join("JavaGetterPipeline.java").is_file(),
        "{}",
        fs::read_to_string(migrated.join("JavaGetterPipeline.kt")).unwrap_or_default()
    );
    assert!(
        fs::read_to_string(migrated.join("Element.kt"))
            .unwrap()
            .contains("class RetainedOptionalMetricPipeline")
    );
    assert!(
        !migrated
            .join("RetainedOptionalMetricPipeline.java")
            .exists()
    );
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
        expected.lines().nth(1).unwrap(),
        "keep|keep|keep|keep|keep|reads=0:2|keep|keep|keep|flat|fallback|present:1|present:1|default:2|true:false|false:true|java.util.NoSuchElementException|java.util.NoSuchElementException|java.lang.NullPointerException|null|java.lang.NullPointerException"
    );
    assert_eq!(
        expected.lines().nth(3).unwrap(),
        "constructors=orElse(...) must not be null|null|present"
    );
    assert_eq!(
        expected.lines().nth(4).unwrap(),
        "numbers=orElse(...) must not be null|orElse(...) must not be null|7"
    );

    assert_eq!(expected.lines().next().unwrap(), "literal=present|fallback");

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

#[test]
#[ignore = "requires JVM tools and Kotlin stdlib"]
fn real_toolchain_preserves_eager_collection_order_and_results() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("eager-original");
    populate_eager(&original);
    let before = validate_jvm_sources(&eager_config(
        &original,
        scratch.0.join("eager-original-classes"),
        &stdlib,
        false,
    ))
    .expect("original Kotlin/JVM eager fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .replace("\r\n", "\n")
        .trim()
        .to_owned();
    let expected_base = expected
        .strip_suffix("\nprimitive=1.0\nmetric=-0.0\nnullableMetric=-0.0\njavaGetters=[java, other, null]|java|-0.0|0|events=R:java,R:other,R:java\njavaNull=empty\njavaFailure=java.lang.IllegalStateException|events=R:a,R:fail\njavaRequired=returned")
        .unwrap_or_else(|| panic!("unexpected original Kotlin getter/numeric behavior:\n{expected}"));
    assert_eq!(
        expected_base,
        "projected=drop,null,c,null|mutable=true|factory=1|size=1|iterator=1|events=A:drop,A:skip,A:null,A:c,B:drop,B:skip,B:null,B:c\ncaptured=skipy,null,cy|factory=1|size=1|iterator=1\nfailure=java.lang.IllegalStateException|events=A:drop,A:skip,A:null,A:c,B:drop,B:skip\nnullableInts=1,null,3\nidentityFailure=java.lang.NullPointerException\nfactoryFailure=java.lang.NullPointerException\ndoubles=null,NaN|floats=null,NaN\ngetters=a,b|a,b|7|events=R:a,R:b\ngetterFailure=java.lang.IllegalStateException|events=R:a,R:fail\ngeneric=[[x]]|widened=[[x]]\nrecord=[record]\ntyped=keep|source|constant|skip\nfields=field\nboxed=java.lang.NullPointerException|constant"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("eager-migrated-{lombok_enabled}"));
        populate_eager(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Pipeline.java").is_file());
        assert!(migrated.join("Runner.kt").is_file());
        assert!(
            migrated.join("JavaGetterPipeline.java").is_file(),
            "{}",
            fs::read_to_string(migrated.join("JavaGetterPipeline.kt")).unwrap_or_default()
        );
        let after = validate_jvm_sources(&eager_config(
            &migrated,
            scratch
                .0
                .join(format!("eager-mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin/Java eager fixture compiles and runs");
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .replace("\r\n", "\n")
                .trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
