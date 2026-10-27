use notlin::semantics::{
    ContainerMemberCall, KotlinType, SemanticProvider, SyntaxSemanticProvider, TypeRef,
    plan_eager_collection_calls, plan_required_calls, plan_syntax_lowerings,
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
    plan_syntax_lowerings(source, &tree, path, provider, &mut plan);
    plan_required_calls(source, &tree, path, provider, false, &mut plan);
    plan
}

fn text<'a>(source: &'a str, location: &notlin::semantics::SourceLocation) -> &'a str {
    assert_eq!(
        location.snapshot_hash,
        *blake3::hash(source.as_bytes()).as_bytes()
    );
    &source[location.start_byte..location.end_byte]
}

fn call<'a>(
    source: &str,
    facts: &'a [ContainerMemberCall],
    member: &str,
) -> &'a ContainerMemberCall {
    let matches = facts
        .iter()
        .filter(|fact| text(source, &fact.location).trim_end().ends_with(member))
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "expected one {member} fact: {facts:#?}");
    matches[0]
}

fn call_with_receiver<'a>(
    source: &str,
    facts: &'a [ContainerMemberCall],
    member: &str,
    receiver: &str,
) -> &'a ContainerMemberCall {
    let matches = facts
        .iter()
        .filter(|fact| {
            text(source, &fact.location).trim_end().ends_with(member)
                && text(source, &fact.receiver).contains(receiver)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "expected one {member} on receiver containing {receiver}: {facts:#?}"
    );
    matches[0]
}

fn named(ty: &TypeRef, expected: &str) -> bool {
    matches!(&ty.ty, KotlinType::Named { name, .. } if name == expected)
}

fn has_filter(source: &str, facts: &[ContainerMemberCall], filter_suffix: &str) -> bool {
    facts.iter().any(|fact| {
        text(source, &fact.location)
            .trim_end()
            .ends_with(filter_suffix)
    })
}

#[test]
fn java_getter_chains_keep_method_targets_and_platform_nullability() {
    let entry_file = "types/Entry.java";
    let entry_source = r#"package payload.types;
public final class Entry {
    public java.math.BigDecimal getAmount() { return java.math.BigDecimal.ONE; }
    public String getLabel() { return null; }
    public double getMetric() { return 0.0; }
}
"#;
    let file = "app/Pipeline.kt";
    let source = r#"package payload.app
import java.math.BigDecimal
import java.util.Optional
import payload.types.Entry

class Pipeline {
    fun selected(items: List<Entry>): Optional<String> = items.stream()
        .filter { it.amount.compareTo(BigDecimal.ONE) == 0 }
        .findFirst().map { it.label }
    fun zero(items: List<Entry>): Optional<Entry> = items.stream()
        .filter { it.metric == 0.0 }.findFirst()
}
"#;
    let provider = indexed(&[(entry_file, entry_source), (file, source)]);
    let facts = plan(source, file, &provider).container_calls;
    let mapped = call(source, &facts, ".map { it.label }");
    let binding = mapped.lambda_binding.as_ref().unwrap();
    let projection = &binding.property_accesses[0].projection;
    assert_eq!(projection.target.name, "getLabel");
    assert_eq!(projection.target.kind, "function");
    assert_eq!(projection.target.file, Path::new(entry_file));
    assert!(mapped.dependencies.contains(&projection.target));
    assert_eq!(
        projection.java_property_read.as_ref().unwrap().nullability,
        notlin::semantics::ReferenceReturnNullability::Platform
    );
    assert!(
        projection.result_type.nullable,
        "platform does not establish non-null"
    );
    let filter = call(source, &facts, ".filter { it.metric == 0.0 }");
    let projection = &filter.lambda_binding.as_ref().unwrap().property_accesses[0].projection;
    assert_eq!(projection.result_type, TypeRef::parse("Double"));
    assert!(
        filter
            .dependencies
            .iter()
            .any(|id| id.name == "getMetric" && id.file == Path::new(entry_file))
    );
}

#[test]
fn stream_optional_chain_carries_generic_payload_and_scoped_lambdas() {
    let entry_file = "types/Entry.kt";
    let entry_source = r#"package payload.types
import java.math.BigDecimal

class Entry(
    val amount: BigDecimal,
    val label: String
)
"#;
    let file = "app/Pipeline.kt";
    let source = r#"package payload.app
import java.math.BigDecimal
import java.util.Optional
import payload.types.Entry

abstract class Pipeline {
    abstract fun find(): Optional<Entry>

    fun fromStream(items: List<Entry>): String =
        items.stream()
            .filter { entry -> entry.amount.compareTo(BigDecimal.ONE) == 0 }
            .findFirst()
            .map { it.label }
            .orElse("")

    fun fromOptional(it: String): String =
        find().map { it.label }.orElse(it)
}
"#;
    let provider = indexed(&[(entry_file, entry_source), (file, source)]);
    let facts = plan(source, file, &provider).container_calls;

    let stream = call(source, &facts, "items.stream()");
    assert!(named(
        stream.receiver_payload.as_ref().unwrap(),
        "payload.types.Entry"
    ));
    assert_eq!(
        stream.result_payload,
        Some(TypeRef::parse("payload.types.Entry"))
    );

    let filter = call(
        source,
        &facts,
        ".filter { entry -> entry.amount.compareTo(BigDecimal.ONE) == 0 }",
    );
    let binding = filter.lambda_binding.as_ref().expect("typed filter lambda");
    assert_eq!(binding.parameter_name, "entry");
    assert!(named(&binding.input_type, "payload.types.Entry"));
    assert!(text(source, &binding.location).starts_with("{ entry ->"));
    assert!(text(source, &binding.body).contains("entry.amount.compareTo"));
    assert!(binding.property_accesses.iter().any(|access| {
        access.projection.target.name == "amount"
            && access.projection.target.kind == "property"
            && access.projection.target.file == Path::new(entry_file)
            && text(source, &access.location) == "entry.amount"
    }));
    assert!(filter.dependencies.iter().any(|id| {
        id.file == Path::new(entry_file) && id.name == "Entry" && id.kind == "class"
    }));

    let first = call(source, &facts, ".findFirst()");
    assert_eq!(
        first.result_payload,
        Some(TypeRef::parse("payload.types.Entry"))
    );

    let map = call_with_receiver(source, &facts, ".map { it.label }", "items.stream()");
    let binding = map.lambda_binding.as_ref().expect("typed implicit lambda");
    assert_eq!(binding.parameter_name, "it");
    assert!(named(&binding.input_type, "payload.types.Entry"));
    assert_eq!(text(source, &binding.body), "it.label");
    let label = binding
        .property_accesses
        .iter()
        .find(|access| access.location.start_byte == binding.body.start_byte)
        .expect("projection has exact source access identity");
    assert_eq!(label.projection.target.name, "label");
    assert_eq!(label.projection.target.kind, "property");
    assert_eq!(label.projection.target.file, Path::new(entry_file));
    assert!(map.dependencies.iter().any(|id| {
        id.file == Path::new(entry_file) && id.name == "Entry" && id.kind == "class"
    }));
    assert_eq!(
        label.projection.result_type,
        TypeRef::parse("java.lang.String")
    );
    assert_eq!(map.result_payload, Some(TypeRef::parse("java.lang.String")));
    assert!(
        !map.dependencies
            .iter()
            .any(|id| { id.file == Path::new(file) && id.name == "find" && id.kind == "function" }),
        "stream branch must not acquire the unrelated source method"
    );

    let optional = call_with_receiver(source, &facts, "find().map { it.label }", "find()");
    let binding = optional.lambda_binding.as_ref().unwrap();
    assert_eq!(binding.parameter_name, "it");
    assert!(named(&binding.input_type, "payload.types.Entry"));
    assert!(
        optional
            .dependencies
            .iter()
            .any(|id| { id.file == Path::new(file) && id.name == "find" && id.kind == "function" })
    );
    assert!(optional.dependencies.iter().any(|id| {
        id.file == Path::new(entry_file) && id.name == "label" && id.kind == "property"
    }));
    assert_eq!(
        optional.result_payload,
        Some(TypeRef::parse("java.lang.String"))
    );
}

#[test]
fn unsupported_payloads_and_nested_lambdas_do_not_leak_bindings() {
    let cases = [
        (
            "Star.kt",
            r#"package payload.negative
fun first(items: List<*>): Any? = items.stream().findFirst().map { it.toString() }.orElse(null)
"#,
        ),
        (
            "Out.kt",
            r#"package payload.negative
class Entry(val label: String)
fun first(items: List<out Entry>): String? = items.stream().findFirst().map { it.label }.orElse(null)
"#,
        ),
        (
            "Nullable.kt",
            r#"package payload.negative
class Entry(val label: String)
fun first(items: List<Entry?>): String? = items.stream().findFirst().map { it.label }.orElse(null)
"#,
        ),
        (
            "NullableMetric.kt",
            r#"package payload.negative
class Entry(val value: Double?)
fun first(items: List<Entry>) = items.stream().filter { it.value == 0.0 }.findFirst()
"#,
        ),
        (
            "GenericOwner.kt",
            r#"package payload.negative
import java.util.Optional
class Owner<T>(val values: List<T>, val optional: Optional<T>) {
    fun first(): Optional<T> = values.stream().findFirst()
    fun mapped(): Optional<T> = optional.map { it }
}
"#,
        ),
        (
            "AliasPayload.kt",
            r#"package payload.negative
import java.util.Optional
class Entry(val label: String)
typealias EntryAlias = Entry
abstract class Repository {
    abstract fun find(): Optional<EntryAlias>
    fun use(): String = find().map { it.label }.orElse("")
}
"#,
        ),
        (
            "Nested.kt",
            r#"package payload.negative
class Entry(val label: String)
class Owner(val entries: List<Entry>) {
    fun nested(): List<String> = entries.stream().map { entry -> entries.map { entry.label } }.findFirst().orElse(emptyList())
}
"#,
        ),
    ];
    for (file, source) in cases {
        let provider = indexed(&[(file, source)]);
        let facts = plan(source, file, &provider).container_calls;
        assert!(
            facts.iter().all(|fact| fact.lambda_binding.is_none()),
            "{file} must not leak a source lambda binding: {facts:#?}"
        );
        if file != "GenericOwner.kt" {
            assert!(
                facts.iter().all(|fact| {
                    !text(source, &fact.location)
                        .trim_end()
                        .ends_with(".map { it.label }")
                }),
                "{file} has a lambda requiring a payload, so an unbound map fact must not be accepted: {facts:#?}"
            );
        }
    }
}

#[test]
fn direct_optional_source_return_requires_explicit_non_generic_signature() {
    let file = "OptionalSource.kt";
    let source = r#"package payload.source
import java.util.Optional

class Entry(val label: String)
abstract class Repository {
    abstract fun find(): Optional<Entry>
    fun inferred() = find()
    fun <T> generic(): Optional<T> = Optional.empty()
    fun use(): String = find().map { it.label }.orElse("")
    fun useInferred(): String = inferred().map { it.label }.orElse("")
    fun useGeneric(): String = generic<Entry>().map { it.label }.orElse("")
}

"#;
    let provider = indexed(&[(file, source)]);
    let facts = plan(source, file, &provider).container_calls;
    let supported = call_with_receiver(source, &facts, ".map { it.label }", "find()");
    assert!(supported.lambda_binding.is_some());
    assert!(named(
        supported.receiver_payload.as_ref().unwrap(),
        "payload.source.Entry"
    ));
    assert!(supported.dependencies.iter().any(|id| id.name == "find"));

    for unsupported in ["inferred().map", "generic<Entry>().map"] {
        assert!(
            facts
                .iter()
                .all(|fact| !text(source, &fact.location).contains(unsupported)
                    || fact.lambda_binding.is_none()),
            "{unsupported} must not establish a typed lambda: {facts:#?}"
        );
        let receiver = unsupported.trim_end_matches(".map");
        assert!(
            facts.iter().all(|fact| {
                !text(source, &fact.location)
                    .trim_end()
                    .ends_with(".map { it.label }")
                    || !text(source, &fact.receiver).contains(receiver)
            }),
            "{unsupported} must not be accepted as an unbound property-selector lambda: {facts:#?}"
        );
    }
}

#[test]
fn nullable_floating_payload_from_map_does_not_bypass_equality_retention() {
    let file = "NullableMap.kt";
    let source = r#"package payload.nullable
import java.util.Optional
class Entry(val value: Double?)
fun zero(input: Optional<Entry>) = input.map { it.value }.filter { it == 0.0 }
"#;
    let provider = indexed(&[(file, source)]);
    let plan = plan(source, file, &provider);
    assert!(
        plan.container_calls
            .iter()
            .any(|fact| { text(source, &fact.location).ends_with(".map { it.value }") }),
        "the nullable projection itself remains supported"
    );
    assert!(
        !plan
            .container_calls
            .iter()
            .any(|fact| { text(source, &fact.location).ends_with(".filter { it == 0.0 }") }),
        "mapped nullable floating payload must retain IEEE equality"
    );
    assert!(
        !plan
            .declarations
            .iter()
            .find(|decl| decl.symbol_id.name == "zero")
            .unwrap()
            .retention_reasons
            .is_empty()
    );
}

#[test]
fn nested_generic_projection_requires_exact_context_instead_of_kotlin_widening() {
    let file = "NestedGeneric.kt";
    let source = r#"package payload.generic
import java.util.Optional
class Entry(val labels: List<String>)
fun exact(value: Optional<Entry>): Optional<List<String>> = value.map { it.labels }
fun widened(value: Optional<Entry>): Optional<List<Any>> = value.map { it.labels }
"#;
    let provider = indexed(&[(file, source)]);
    let plan = plan(source, file, &provider);
    let maps = plan
        .container_calls
        .iter()
        .filter(|fact| text(source, &fact.location).ends_with(".map { it.labels }"))
        .collect::<Vec<_>>();
    assert_eq!(maps.len(), 1, "{maps:#?}");
    assert!(maps[0].lambda_binding.is_some());
    assert!(
        !plan
            .declarations
            .iter()
            .find(|decl| decl.symbol_id.name == "widened")
            .unwrap()
            .retention_reasons
            .is_empty()
    );
}

#[test]
fn referential_numeric_identity_does_not_lower_as_java_numeric_equality() {
    let entry_file = "types/Entry.java";
    let entry_source = r#"package payload.types;
public final class Entry {
    public Integer getNumber() { return null; }
}

"#;
    let file = "app/Pipeline.kt";
    let boxed_source = r#"package payload.app
import payload.types.Entry

class Pipeline {
    fun boxed(items: List<Entry>) = items.stream()
        .filter { it.number === 0 }
        .findFirst()
}
"#;
    let boxed_provider = indexed(&[(entry_file, entry_source), (file, boxed_source)]);
    let boxed_facts = plan(boxed_source, file, &boxed_provider).container_calls;
    assert!(
        !has_filter(boxed_source, &boxed_facts, ".filter { it.number === 0 }"),
        "a nullable boxed Java Integer compared by identity must not become an unboxing numeric test"
    );

    let stream_source = r#"package payload.app
import java.util.stream.Stream

class Pipeline {
    fun same(stream: Stream<Int>) = stream.filter { it === it }.findFirst()
}
"#;
    let stream_provider = indexed(&[(file, stream_source)]);
    let stream_facts = plan(stream_source, file, &stream_provider).container_calls;
    assert!(
        !has_filter(stream_source, &stream_facts, ".filter { it === it }"),
        "referential comparison of boxed Stream<Int> elements needs identity-specific lowering"
    );

    let eager_source = r#"package payload.app
import kotlin.collections.filterNot
import kotlin.collections.map
import payload.types.Entry

class Pipeline {
    fun selected(items: List<Entry>) = items
        .map { it.number }
        .filterNot { it === 0 }
}
"#;
    let eager_provider = indexed(&[(entry_file, entry_source), (file, eager_source)]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(eager_source, None).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "invalid fixture:\n{eager_source}"
    );
    let path = Path::new(file);
    let mut eager_plan = notlin::translation_plan::analyze(eager_source, tree.root_node(), path);
    plan_eager_collection_calls(eager_source, &tree, path, &eager_provider, &mut eager_plan);
    assert!(
        !eager_plan.eager_collection_calls.iter().any(|fact| {
            text(eager_source, &fact.location)
                .trim_end()
                .ends_with(".filterNot { it === 0 }")
        }),
        "referential comparison after a boxed numeric map must wait for identity-aware emission"
    );
}

#[test]
fn chained_lambda_property_reads_carry_exact_typed_getter_hops() {
    let file = "ChainedProperty.kt";
    let source = r#"package payload.chain
import java.math.BigDecimal

class Detail(val type: Detail)
class Entry(val data: Detail, val values: List<Detail>, val amount: BigDecimal)

fun nested(items: List<Entry>) = items.stream()
    .filter { entry -> entry.data.type == entry.data }
fun parenthesized(items: List<Entry>) = items.stream()
    .filter { entry -> (entry.data).type == entry.data }
fun indexed(items: List<Entry>) = items.stream()
    .filter { entry -> entry.values[0].type == entry.values[0] }
fun wrapped(items: List<Entry>) = items.stream()
    .filter { entry -> (entry).data == entry.data }
fun direct(items: List<Entry>) = items.stream()
    .filter { entry -> entry.amount.compareTo(BigDecimal.ONE) == 0 }
"#;
    let provider = indexed(&[(file, source)]);
    let plan = plan(source, file, &provider);

    let indexed_suffix = ".filter { entry -> entry.values[0].type == entry.values[0] }";
    assert!(
        plan.container_calls
            .iter()
            .all(|fact| !text(source, &fact.location).ends_with(indexed_suffix)),
        "indexed access must not receive a typed container fact: {:#?}",
        plan.container_calls
    );
    assert!(
        !plan
            .declarations
            .iter()
            .find(|declaration| declaration.symbol_id.name == "indexed")
            .expect("function declaration is indexed")
            .retention_reasons
            .is_empty(),
        "indexed access must remain retained when its receiver is unproven"
    );

    for (suffix, function, expected_path) in [
        (
            ".filter { entry -> entry.data.type == entry.data }",
            "nested",
            vec!["getData", "getType"],
        ),
        (
            ".filter { entry -> (entry.data).type == entry.data }",
            "parenthesized",
            vec!["getData", "getType"],
        ),
        (
            ".filter { entry -> (entry).data == entry.data }",
            "wrapped",
            vec!["getData"],
        ),
    ] {
        let fact = call(source, &plan.container_calls, suffix);
        let binding = fact.lambda_binding.as_ref().expect("typed lambda");
        let access = binding
            .property_accesses
            .iter()
            .find(|access| {
                text(source, &access.location).ends_with(".data.type")
                    || text(source, &access.location).ends_with("data).type")
                    || (expected_path.len() == 1
                        && text(source, &access.location).ends_with(".data"))
            })
            .expect("terminal property hop");
        assert_eq!(access.getter_path, expected_path, "{function}");
        let expected_receiver = match function {
            "parenthesized" => "(entry.data)",
            "wrapped" => "(entry)",
            _ => "entry.data",
        };
        assert_eq!(
            text(source, &access.receiver),
            expected_receiver,
            "{function} receiver span"
        );
        assert!(!access.receiver_type.nullable, "{function} receiver type");
    }

    let supported = call(
        source,
        &plan.container_calls,
        ".filter { entry -> entry.amount.compareTo(BigDecimal.ONE) == 0 }",
    );
    assert!(supported.lambda_binding.is_some());
    assert!(supported.dependencies.iter().any(|target| {
        target.name == "amount" && target.kind == "property" && target.file == Path::new(file)
    }));
}

#[test]
fn property_chain_dependencies_include_each_declaring_type_and_reject_unproven_hops() {
    let leaf = "package payload.leaf\nclass Leaf(val label: String)\n";
    let entry = "package payload.types\nimport payload.leaf.Leaf\nclass Entry(val leaf: Leaf)\n";
    let source = r#"package payload.app
import java.util.Optional
import payload.types.Entry
fun label(value: Optional<Entry>): Optional<String> = value.map { it.leaf.label }
"#;
    let file = "app/Pipeline.kt";
    let provider = indexed(&[
        ("leaf/Leaf.kt", leaf),
        ("types/Entry.kt", entry),
        (file, source),
    ]);
    let plan = plan(source, file, &provider);
    let fact = call(source, &plan.container_calls, ".map { it.leaf.label }");
    for (kind, name, path) in [
        ("class", "Entry", "types/Entry.kt"),
        ("class", "Leaf", "leaf/Leaf.kt"),
        ("property", "leaf", "types/Entry.kt"),
        ("property", "label", "leaf/Leaf.kt"),
    ] {
        assert!(
            fact.dependencies
                .iter()
                .any(|id| id.kind == kind && id.name == name && id.file == Path::new(path)),
            "missing {kind} {name}: {:?}",
            fact.dependencies
        );
    }
    let accesses = &fact.lambda_binding.as_ref().unwrap().property_accesses;
    let final_access = accesses
        .iter()
        .find(|access| access.getter_path.len() == 2)
        .unwrap();
    assert!(named(&final_access.receiver_type, "payload.leaf.Leaf"));

    let unsupported = r#"package payload.unknown
import java.util.Optional
class Detail(val label: String)
class NullableEntry(val detail: Detail?)
class GenericDetail<T>(val label: T)
class GenericEntry(val detail: GenericDetail<String>)
class RenamedDetail(@get:JvmName("readLabel") val label: String)
class RenamedEntry(val detail: RenamedDetail)
fun nullable(value: Optional<NullableEntry>) = value.map { it.detail?.label }
fun generic(value: Optional<GenericEntry>) = value.map { it.detail.label }
fun renamed(value: Optional<RenamedEntry>) = value.map { it.detail.label }
"#;
    let provider = indexed(&[(file, unsupported)]);
    let plan = self::plan(unsupported, file, &provider);
    for suffix in [".map { it.detail?.label }", ".map { it.detail.label }"] {
        assert!(
            plan.container_calls
                .iter()
                .all(|fact| !text(unsupported, &fact.location).ends_with(suffix)),
            "unproven hop acquired a container fact: {:?}",
            plan.container_calls
        );
    }
    for function in ["nullable", "generic", "renamed"] {
        assert!(
            !plan
                .declarations
                .iter()
                .find(|declaration| declaration.symbol_id.name == function)
                .unwrap()
                .retention_reasons
                .is_empty(),
            "{function} must be retained"
        );
    }
}
