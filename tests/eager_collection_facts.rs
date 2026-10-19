use notlin::semantics::{
    EagerCollectionOperation, SemanticProvider, SyntaxSemanticProvider, plan_eager_collection_calls,
};
use std::path::{Path, PathBuf};

fn indexed(files: &[(&str, &str)]) -> SyntaxSemanticProvider {
    for (path, source) in files {
        let extension = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str());
        let language = match extension {
            Some("kt" | "kts") => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Some("java") => tree_sitter_java::LANGUAGE.into(),
            _ => continue,
        };
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "invalid fixture {path}:\n{source}"
        );
    }
    SyntaxSemanticProvider::new(
        files
            .iter()
            .map(|(path, source)| (PathBuf::from(path), (*source).to_owned()))
            .collect::<Vec<_>>(),
    )
}

fn plan(
    source: &str,
    file: &str,
    provider: &impl SemanticProvider,
) -> notlin::translation_plan::TranslationPlan {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture:\n{source}");
    let path = Path::new(file);
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), path);
    plan_eager_collection_calls(source, &tree, path, provider, &mut plan);
    plan
}

fn call_text<'a>(
    source: &'a str,
    plan: &notlin::translation_plan::TranslationPlan,
) -> Vec<&'a str> {
    plan.eager_collection_calls
        .iter()
        .map(|fact| {
            assert_eq!(
                fact.location.snapshot_hash,
                *blake3::hash(source.as_bytes()).as_bytes()
            );
            assert_eq!(fact.receiver.snapshot_hash, fact.location.snapshot_hash);
            assert_eq!(fact.lambda.snapshot_hash, fact.location.snapshot_hash);
            assert_eq!(fact.body.snapshot_hash, fact.location.snapshot_hash);
            &source[fact.location.start_byte..fact.location.end_byte]
        })
        .collect()
}

#[test]
fn exact_eager_map_filter_not_and_chained_receivers_are_planned() {
    let file = "Eager.kt";
    let source = r#"package eager
import kotlin.collections.map
import kotlin.collections.filterNot

fun mark(value: Int): Int = value

fun process(values: List<Int>): List<Int> {
    return values.map { item -> item }
        .filterNot { it % 2 == 0 }
        .map { mark(it) }
        .map({ it })
}
"#;
    let provider = indexed(&[(file, source)]);
    let plan = plan(source, file, &provider);
    let texts = call_text(source, &plan);
    assert_eq!(texts.len(), 4, "facts: {:#?}", plan.eager_collection_calls);
    assert!(texts.iter().any(|text| text.contains("values.map")));
    assert!(texts.iter().any(|text| text.contains("filterNot")));
    assert!(texts.iter().any(|text| text.contains("mark(it)")));
    assert!(texts.iter().any(|text| text.contains("map({ it })")));

    let identity = plan
        .eager_collection_calls
        .iter()
        .find(|fact| fact.operation == EagerCollectionOperation::Map)
        .unwrap();
    assert_eq!(identity.parameter_name, "item");
    assert_eq!(identity.receiver_type, "kotlin.collections.List");
    assert!(identity.element_type.is_some());
    assert_eq!(identity.owner.name, "process");
    let filter = plan
        .eager_collection_calls
        .iter()
        .find(|fact| fact.operation == EagerCollectionOperation::FilterNot)
        .unwrap();
    assert_eq!(filter.parameter_name, "it");
    assert!(filter.element_type.is_some());
}

#[test]
fn direct_public_property_projections_keep_getter_identity_and_result_types() {
    let file = "Projection.kt";
    let source = r#"package eager
class Entry(val label: String, val count: Int, val optionalCount: Int?, val labels: List<String>) {
    var reads: Int = 0
    val computed: String
        get() { reads++; return label }
}

fun implicit(values: List<Entry>): List<String> = values.map { it.label }
fun named(values: List<Entry>): List<Int?> = values.map { entry -> entry.optionalCount }
fun computed(values: List<Entry>): List<String> = values.map { entry -> entry.computed }
fun genericResult(values: List<Entry>): List<List<String>> = values.map { it.labels }
"#;
    let provider = indexed(&[(file, source)]);
    let planned = plan(source, file, &provider);
    assert_eq!(
        planned.eager_collection_calls.len(),
        4,
        "{:#?}",
        planned.eager_collection_calls
    );

    for (owner, property, result_type, getter) in [
        ("implicit", "label", "java.lang.String", "getLabel"),
        ("named", "optionalCount", "Int?", "getOptionalCount"),
        ("computed", "computed", "java.lang.String", "getComputed"),
        (
            "genericResult",
            "labels",
            "kotlin.collections.List<java.lang.String>",
            "getLabels",
        ),
    ] {
        let fact = planned
            .eager_collection_calls
            .iter()
            .find(|fact| fact.owner.name == owner)
            .unwrap();
        let projection = fact
            .property_projection
            .as_ref()
            .expect("property projection");
        assert_eq!(projection.target.name, property);
        assert_eq!(projection.getter_name, getter);
        assert_eq!(
            projection.result_type,
            notlin::semantics::TypeRef::parse(result_type)
        );
        assert!(
            planned.dependencies.iter().any(|dependency| {
                dependency.from == fact.owner
                    && dependency.resolution
                        == notlin::semantics::FactStatus::Established(projection.target.clone())
            }),
            "projection target must be a required symbol dependency: {projection:?}"
        );
    }
}

#[test]
fn unsupported_property_projection_shapes_remain_unplanned() {
    let files: Vec<(&str, &str, &str)> = vec![
        (
            "JvmField.kt",
            "package eager\nclass Entry {\n    @JvmField val label: String = \"\"\n}\nfun mapped(values: List<Entry>): List<String> = values.map { it.label }\n",
            "@JvmField",
        ),
        (
            "JvmName.kt",
            "package eager\nclass Entry {\n    @get:JvmName(\"renamed\") val label: String = \"\"\n}\nfun mapped(values: List<Entry>): List<String> = values.map { it.label }\n",
            "@get:JvmName",
        ),
        (
            "Private.kt",
            "package eager\nclass Entry {\n    private val label: String = \"\"\n}\nfun mapped(values: List<Entry>): List<String> = values.map { it.label }\n",
            "private getter",
        ),
        (
            "Internal.kt",
            "package eager\nclass Entry {\n    internal val label: String = \"\"\n}\nfun mapped(values: List<Entry>): List<String> = values.map { it.label }\n",
            "internal getter",
        ),
        (
            "IsConvention.kt",
            "package eager\nclass Entry(val isReady: Boolean)\nfun mapped(values: List<Entry>): List<Boolean> = values.map { it.isReady }\n",
            "isX convention",
        ),
        (
            "Record.kt",
            "package eager\n@JvmRecord data class Entry(val label: String)\nfun mapped(values: List<Entry>): List<String> = values.map { it.label }\n",
            "record component accessor ABI",
        ),
        (
            "AliasedRecord.kt",
            "package eager\nimport kotlin.jvm.JvmRecord as Record\n@Record data class Entry(val label: String)\nfun mapped(values: List<Entry>): List<String> = values.map { it.label }\n",
            "aliased record annotation",
        ),
        (
            "NullableElement.kt",
            "package eager\nclass Entry(val label: String)\nfun mapped(values: List<Entry?>): List<String?> = values.map { it?.label }\n",
            "nullable element",
        ),
        (
            "GenericElement.kt",
            "package eager\nclass Entry<T>(val value: T)\nfun mapped(values: List<Entry<String>>): List<String> = values.map { it.value }\n",
            "generic element substitution",
        ),
        (
            "ValueClassResult.kt",
            "package eager\n@JvmInline value class Label(val value: String)\nclass Entry(val label: Label)\nfun mapped(values: List<Entry>): List<Label> = values.map { it.label }\n",
            "value-class property result may have a mangled JVM getter",
        ),
    ];
    for (file, source, reason) in files {
        let provider = indexed(&[(file, source)]);
        let planned = plan(source, file, &provider);
        assert!(
            planned.eager_collection_calls.is_empty(),
            "{reason} must not establish a property projection: {:#?}",
            planned.eager_collection_calls
        );
    }

    let model = "package model\nclass Label\nclass Entry(val label: Label)\n";
    let consumer = "package consumer\nimport model.Entry\nclass Label\nfun mapped(values: List<Entry>): List<Label> = values.map { it.label }\n";
    let provider = indexed(&[("Model.kt", model), ("Consumer.kt", consumer)]);
    assert!(
        plan(consumer, "Consumer.kt", &provider)
            .eager_collection_calls
            .is_empty(),
        "same-spelled result types with different file bindings must not project"
    );
}

#[test]
fn nested_generic_projection_requires_an_exact_contextual_element_type() {
    let source = r#"package eager
class Entry(val labels: List<String>)
fun precise(values: List<Entry>): List<List<String>> = values.map { it.labels }
fun widened(values: List<Entry>): List<List<Any>> = values.map { it.labels }
fun inferred(values: List<Entry>) = values.map { it.labels }
"#;
    let provider = indexed(&[("Nested.kt", source)]);
    let planned = plan(source, "Nested.kt", &provider);
    let owners = planned
        .eager_collection_calls
        .iter()
        .map(|call| call.owner.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(owners, ["precise"]);
}

#[test]
fn flat_projection_context_accepts_proven_upcasts_and_preserves_nullability() {
    let source = r#"package eager
open class Base
class Label : Base()
class Entry(val label: Label, val optionalLabel: Label?)

fun exact(values: List<Entry>): List<Label> = values.map { it.label }
fun base(values: List<Entry>): List<Base> = values.map { it.label }
fun any(values: List<Entry>): List<Any> = values.map { it.label }
fun nullable(values: List<Entry>): List<Base?> = values.map { it.label }
fun nullableList(values: List<Entry>): List<Base>? = values.map { it.label }
fun collection(values: List<Entry>): Collection<Base> = values.map { it.label }
fun iterable(values: List<Entry>): Iterable<Base> = values.map { it.label }
fun anyContainer(values: List<Entry>): Any = values.map { it.label }
fun anyNullable(values: List<Entry>): Any = values.map { it.optionalLabel }
fun unsafe(values: List<Entry>): List<Base> = values.map { it.optionalLabel }
class NumberEntry(val count: Int)
fun boxedAny(values: List<NumberEntry>): List<Any> = values.map { it.count }
"#;
    let provider = indexed(&[("FlatUpcasts.kt", source)]);
    let planned = plan(source, "FlatUpcasts.kt", &provider);
    let owners = planned
        .eager_collection_calls
        .iter()
        .map(|call| call.owner.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        owners,
        [
            "exact",
            "base",
            "any",
            "nullable",
            "nullableList",
            "collection",
            "iterable",
            "anyContainer",
            "anyNullable",
            "boxedAny",
        ]
    );
    let base = planned
        .eager_collection_calls
        .iter()
        .find(|call| call.owner.name == "base")
        .unwrap();
    for name in ["Label", "Base"] {
        let projection = base.property_projection.as_ref().unwrap();
        let id = projection
            .dependencies
            .iter()
            .find(|id| id.name == name && id.kind == "class")
            .expect("upcast preserves its inheritance evidence");
        assert!(
            planned
                .dependencies
                .iter()
                .any(|dependency| dependency.from == base.owner
                    && dependency.resolution
                        == notlin::semantics::FactStatus::Established(id.clone()))
        );
    }
}

#[test]
fn imported_typealias_property_results_do_not_authorize_projection() {
    let model = r#"package model
class Label
typealias LabelAlias = Label
@JvmInline value class Token(val raw: String)
typealias TokenAlias = Token
"#;
    let consumer = r#"package consumer
import model.LabelAlias
import model.TokenAlias

class LabelEntry(val label: LabelAlias)
class ValueEntry(val token: TokenAlias)
class LabelCollectionEntry(val labels: List<LabelAlias>)

fun label(values: List<LabelEntry>): List<LabelAlias> = values.map { it.label }
fun value(values: List<ValueEntry>): List<TokenAlias> = values.map { it.token }
fun labels(values: List<LabelCollectionEntry>): List<List<LabelAlias>> = values.map { it.labels }
"#;
    let provider = indexed(&[("Model.kt", model), ("Consumer.kt", consumer)]);
    let planned = plan(consumer, "Consumer.kt", &provider);
    assert!(
        planned.eager_collection_calls.is_empty(),
        "an imported typealias result needs explicit expansion and JVM ABI proof: {:#?}",
        planned.eager_collection_calls
    );
}

#[test]
fn nullable_floating_equality_requires_ieee_aware_lowering() {
    for ty in ["Float", "Double", "kotlin.Float", "kotlin.Double"] {
        let zero = if ty.ends_with("Float") { "0.0f" } else { "0.0" };
        for comparison in [format!("it == {zero}"), format!("{zero} != it")] {
            let source = format!(
                "package eager\nfun select(values: List<{ty}?>): List<{ty}?> = values.filterNot {{ {comparison} }}\n"
            );
            let provider = indexed(&[("Floating.kt", &source)]);
            assert!(
                plan(&source, "Floating.kt", &provider)
                    .eager_collection_calls
                    .is_empty(),
                "{ty}: {comparison} must retain its Kotlin equality semantics"
            );
        }
    }
}

#[test]
fn canonical_import_proves_stdlib_against_unrelated_wildcards() {
    let file = "Imported.kt";
    let source = r#"package eager
import dependency.helpers.*
import kotlin.collections.map

fun <T> mapped(values: kotlin.collections.List<T>): kotlin.collections.List<T> = values.map { it }
"#;
    let provider = indexed(&[(file, source)]);
    let planned = plan(source, file, &provider);
    assert_eq!(call_text(source, &planned).len(), 1);

    let wildcard_only = r#"package eager
import dependency.helpers.*

fun mapped(values: List<String>): List<String> = values.map { it }
"#;
    let provider = indexed(&[(file, wildcard_only)]);
    assert!(
        plan(wildcard_only, file, &provider)
            .eager_collection_calls
            .is_empty()
    );
}

#[test]
fn indexed_and_local_extension_shadows_are_rejected_but_unrelated_methods_are_not() {
    let shadow_file = "Shadow.kt";
    let shadow = r#"package eager

fun List<Int>.map(transform: (Int) -> Int): List<Int> = this

fun mapped(values: List<Int>): List<Int> = values.map { it }
"#;
    let provider = indexed(&[(shadow_file, shadow)]);
    assert!(
        plan(shadow, shadow_file, &provider)
            .eager_collection_calls
            .is_empty()
    );

    let local = r#"package eager

fun mapped(values: List<Int>): List<Int> {
    fun List<Int>.map(transform: (Int) -> Int): List<Int> = this
    return values.map { it }
}
"#;
    let provider = indexed(&[(shadow_file, local)]);
    assert!(
        plan(local, shadow_file, &provider)
            .eager_collection_calls
            .is_empty()
    );

    let unrelated = r#"package eager

class Other {
    fun map(): Int = 1
}

fun mapped(values: List<Int>): List<Int> = values.map { it }
"#;
    let provider = indexed(&[(shadow_file, unrelated)]);
    assert_eq!(
        call_text(unrelated, &plan(unrelated, shadow_file, &provider)).len(),
        1
    );
}

#[test]
fn imported_extension_alias_and_noncanonical_imports_stay_unknown() {
    let extension_file = "Other.kt";
    let extension = r#"package other

fun List<Int>.filterNot(predicate: (Int) -> Boolean): List<Int> = this
"#;
    let caller_file = "Caller.kt";
    let caller = r#"package eager
import other.filterNot

fun filtered(values: List<Int>): List<Int> = values.filterNot { it == 0 }
"#;
    let provider = indexed(&[(extension_file, extension), (caller_file, caller)]);
    assert!(
        plan(caller, caller_file, &provider)
            .eager_collection_calls
            .is_empty()
    );

    let alias = r#"package eager
import other.filterNot as keep

fun filtered(values: List<Int>): List<Int> = values.filterNot { it == 0 }
"#;
    let provider = indexed(&[(extension_file, extension), (caller_file, alias)]);
    assert!(
        plan(alias, caller_file, &provider)
            .eager_collection_calls
            .is_empty()
    );
}

#[test]
fn nullable_alias_unknown_and_stale_receivers_are_retained() {
    let file = "Unknown.kt";
    let nullable = r#"package eager

fun mapped(values: List<Int>?): List<Int> = values.map { it }
"#;
    let provider = indexed(&[(file, nullable)]);
    assert!(
        plan(nullable, file, &provider)
            .eager_collection_calls
            .is_empty()
    );

    let alias = r#"package eager
typealias Numbers = List<Int>

fun mapped(values: Numbers): List<Int> = values.map { it }
"#;
    let provider = indexed(&[(file, alias)]);
    assert!(
        plan(alias, file, &provider)
            .eager_collection_calls
            .is_empty()
    );

    let custom = r#"package eager
class Box<T>(val items: List<T>)

fun mapped(box: Box<Int>): List<Int> = box.items.map { it }
"#;
    let provider = indexed(&[(file, custom)]);
    assert!(
        plan(custom, file, &provider)
            .eager_collection_calls
            .is_empty(),
        "generic property element substitution remains unproven"
    );

    let old = r#"package eager
fun mapped(values: List<Int>): List<Int> = values.map { it }
"#;
    let new = old.replace("Int", "Long");
    let provider = indexed(&[(file, old)]);
    assert!(
        plan(&new, file, &provider)
            .eager_collection_calls
            .is_empty()
    );
}

#[test]
fn unsupported_lambda_control_unit_and_mutable_captures_are_rejected() {
    let file = "Lambdas.kt";
    let source = r#"package eager

fun log(value: Int): Unit { }

fun unsupported(values: List<Int>): List<Int> {
    var offset = 0
    val captured = values.filterNot { it == offset }
    offset = 1
    val unitBody = values.map { log(it) }
    val jump = values.map { return@unsupported it }
    val nested = values.map { values.map { it } }
    return values.map { it }
}
"#;
    let provider = indexed(&[(file, source)]);
    let plan = plan(source, file, &provider);
    let texts = call_text(source, &plan);
    assert_eq!(texts.len(), 1, "facts: {:#?}", plan.eager_collection_calls);
    assert!(texts.contains(&"values.map { it }"));
    assert!(
        texts
            .iter()
            .all(|text| !text.contains("values.map { values.map"))
    );
}

#[test]
fn comments_cannot_manufacture_canonical_import_facts() {
    let file = "CommentImport.kt";
    let source = r#"package eager
/*
import kotlin.collections.map
*/
import dependency.helpers.*

fun mapped(values: List<Int>): List<Int> = values.map { it }
"#;
    let provider = indexed(&[(file, source)]);
    assert!(
        plan(source, file, &provider)
            .eager_collection_calls
            .is_empty()
    );
}

#[test]
fn java_void_bodies_are_rejected_and_platform_boxed_results_do_not_seed_chains() {
    let java = r#"package eager;
public final class JavaRecorder {
    public static void consume(int value) { }
    public static Integer boxed(int value) { return value; }
}
"#;
    let caller = r#"package eager
fun invalid(values: List<Int>): List<Unit> = values.map { JavaRecorder.consume(it) }
"#;
    let provider = indexed(&[("JavaRecorder.java", java), ("JavaCalls.kt", caller)]);
    assert!(
        plan(caller, "JavaCalls.kt", &provider)
            .eager_collection_calls
            .is_empty()
    );
    let chained = r#"package eager
fun platform(values: List<Int>) = values.map { JavaRecorder.boxed(it) }.filterNot { it == null }
"#;
    let provider = indexed(&[("JavaRecorder.java", java), ("JavaCalls.kt", chained)]);
    let result = plan(chained, "JavaCalls.kt", &provider);
    assert_eq!(call_text(chained, &result).len(), 1);
    assert!(call_text(chained, &result)[0].contains(".map"));
}

#[test]
fn source_java_reference_results_can_continue_eager_stages() {
    let source = r#"package eager
class Entry(val name: String)
class Pipeline {
    fun process(): List<Entry?> = Recorder.elements().map { Recorder.project(it) }.filterNot { Recorder.reject(it) }
}
"#;
    let java = r#"package eager;
public final class Recorder {
    public static Iterable<Entry> elements() { return null; }
    public static Entry project(Entry value) { return value; }
    public static boolean reject(Entry value) { return false; }
}
"#;
    let provider = indexed(&[("Pipeline.kt", source), ("Recorder.java", java)]);
    let result = plan(source, "Pipeline.kt", &provider);
    assert_eq!(
        call_text(source, &result).len(),
        2,
        "{:?}",
        call_text(source, &result)
    );
}

#[test]
fn unknown_element_substitution_and_contextual_numeric_literals_stay_kotlin() {
    for source in [
        "package eager\nclass Int\nfun process(values: List<Int>): List<Int> = values.map { it }\n",
        "package eager\nfun process(values: List<String>): List<Long> = values.map { 1 }\n",
        "package eager\nfun <T> values(input: List<T>): List<T> = input\nfun process(input: List<Int>): List<Int> = values(input).map { it }\n",
    ] {
        let provider = indexed(&[("Unknown.kt", source)]);
        assert!(
            plan(source, "Unknown.kt", &provider)
                .eager_collection_calls
                .is_empty(),
            "{source}"
        );
    }
}
