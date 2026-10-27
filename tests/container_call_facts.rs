use notlin::semantics::{
    ContainerMemberCall, SemanticProvider, SyntaxSemanticProvider, plan_required_calls,
    plan_syntax_lowerings,
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

fn fact_text<'a>(source: &'a str, fact: &ContainerMemberCall) -> &'a str {
    assert_eq!(
        fact.location.snapshot_hash,
        *blake3::hash(source.as_bytes()).as_bytes()
    );
    assert_eq!(fact.receiver.snapshot_hash, fact.location.snapshot_hash);
    &source[fact.location.start_byte..fact.location.end_byte]
}

fn receiver_text<'a>(source: &'a str, fact: &ContainerMemberCall) -> &'a str {
    &source[fact.receiver.start_byte..fact.receiver.end_byte]
}

#[test]
fn typed_stream_and_optional_chains_keep_only_wrapper_classifier_facts() {
    let file = "ContainerCalls.kt";
    let source = r#"package containercalls
import java.util.Optional

data class Entry(val active: Boolean)

abstract class Store {
    abstract fun find(): Optional<Entry>

    fun lookup(items: List<Entry>): Optional<Entry> =
        items.stream().filter { entry -> entry.active }.map { it }.findFirst()

    fun fromOptional(): Entry = find().map { it }.orElseThrow()
}
"#;
    let provider = indexed(&[(file, source)]);
    let plan = plan(source, file, &provider);
    let facts = &plan.container_calls;
    let stream = facts
        .iter()
        .find(|fact| {
            matches!(
                fact.receiver_type.as_str(),
                "kotlin.collections.List" | "java.util.List"
            ) && receiver_text(source, fact) == "items"
        })
        .expect("Collection.stream should have a fact");
    assert!(matches!(
        stream.receiver_type.as_str(),
        "kotlin.collections.List" | "java.util.List"
    ));
    assert_eq!(
        stream.return_type.as_deref(),
        Some("java.util.stream.Stream")
    );

    let filter = facts
        .iter()
        .find(|fact| {
            fact.receiver_type == "java.util.stream.Stream"
                && receiver_text(source, fact) == "items.stream()"
                && fact.return_type.as_deref() == Some("java.util.stream.Stream")
        })
        .expect("Stream.filter should have a fact");
    assert_eq!(filter.receiver_type, "java.util.stream.Stream");
    assert_eq!(
        filter.return_type.as_deref(),
        Some("java.util.stream.Stream")
    );

    let stream_map = facts
        .iter()
        .find(|fact| {
            fact.receiver_type == "java.util.stream.Stream"
                && receiver_text(source, fact) == "items.stream().filter { entry -> entry.active }"
                && fact.return_type.as_deref() == Some("java.util.stream.Stream")
        })
        .expect("Stream.map should have a fact");
    assert_eq!(
        stream_map.return_type.as_deref(),
        Some("java.util.stream.Stream")
    );

    let find_first = facts
        .iter()
        .find(|fact| {
            fact.receiver_type == "java.util.stream.Stream"
                && receiver_text(source, fact)
                    == "items.stream().filter { entry -> entry.active }.map { it }"
        })
        .expect("Stream.findFirst should have a fact");
    assert_eq!(
        find_first.return_type.as_deref(),
        Some("java.util.Optional")
    );

    let optional_map = facts
        .iter()
        .find(|fact| {
            fact.receiver_type == "java.util.Optional"
                && receiver_text(source, fact) == "find()"
                && fact.return_type.as_deref() == Some("java.util.Optional")
        })
        .expect("Optional.map should have a fact");
    assert_eq!(optional_map.receiver_type, "java.util.Optional");
    assert_eq!(
        optional_map.return_type.as_deref(),
        Some("java.util.Optional")
    );

    let or_else_throw = facts
        .iter()
        .find(|fact| {
            fact.receiver_type == "java.util.Optional"
                && receiver_text(source, fact) == "find().map { it }"
                && fact.return_type.is_none()
        })
        .expect("Optional.orElseThrow should be a valid terminal call");
    assert_eq!(or_else_throw.receiver_type, "java.util.Optional");
    assert_eq!(or_else_throw.return_type, None);

    let json = serde_json::to_string(find_first).unwrap();
    let restored: ContainerMemberCall = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, *find_first);
}

#[test]
fn generic_standard_container_properties_supply_only_their_outer_classifier() {
    let file = "GenericContainerProperties.kt";
    let source = r#"package containercalls
import java.util.Optional

class Box<T> {
    fun stream(): Optional<T> = Optional.empty()
}

class GenericOwner<T>(
    val items: List<T>,
    val values: Collection<T>,
    val optional: Optional<T>,
    val box: Box<T>
) {
    fun firstItem(): Optional<T> = items.stream().findFirst()
    fun firstValue(): Optional<T> = values.stream().findAny()
    fun mapOptional(): Optional<T> = optional.map { it }
    fun customStream(): Optional<T> = box.stream()
}
"#;
    let provider = indexed(&[(file, source)]);
    let facts = plan(source, file, &provider).container_calls;

    for (receiver, expected) in [
        ("items", &["java.util.List", "kotlin.collections.List"][..]),
        (
            "values",
            &["java.util.Collection", "kotlin.collections.Collection"][..],
        ),
        ("optional", &["java.util.Optional"][..]),
    ] {
        assert!(
            facts.iter().any(|fact| {
                receiver_text(source, fact) == receiver
                    && expected.contains(&fact.receiver_type.as_str())
            }),
            "missing classifier fact for {receiver}: {facts:#?}"
        );
    }
    assert!(
        facts
            .iter()
            .any(|fact| fact.receiver_type == "java.util.Optional"
                && receiver_text(source, fact) == "optional"
                && fact.return_type.as_deref() == Some("java.util.Optional")),
        "Optional<T>.map should use only the Optional classifier: {facts:#?}"
    );
    assert!(
        facts
            .iter()
            .all(|fact| receiver_text(source, fact) != "box"),
        "arbitrary generic Box<T> must not be admitted: {facts:#?}"
    );
}

#[test]
fn generic_container_property_classifier_keeps_nullable_alias_and_shadow_gates() {
    let cases = [
        (
            "NullableProperty.kt",
            r#"package containercalls

class Owner(val items: List<String>?) {
    fun first() = items.stream().findFirst()
}
"#,
        ),
        (
            "AliasedProperty.kt",
            r#"package containercalls

typealias Items = List<String>

class Owner(val items: Items) {
    fun first() = items.stream().findFirst()
}
"#,
        ),
        (
            "CustomProperty.kt",
            r#"package containercalls

class Box<T> {
    fun stream() = this
}

class Owner(val items: Box<String>) {
    fun first() = items.stream()
}
"#,
        ),
        (
            "ShadowedProperty.kt",
            r#"package containercalls

class Box<T> {
    fun stream() = this
}

class Owner(val items: List<String>) {
    fun first(items: Box<String>) = items.stream()
}
"#,
        ),
    ];
    for (file, source) in cases {
        let provider = indexed(&[(file, source)]);
        assert!(
            plan(source, file, &provider).container_calls.is_empty(),
            "{file} should not supply a standard container fact"
        );
    }

    let imported = r#"package containercalls
import external.List

class Owner(val items: List<String>) {
    fun first() = items.stream().findFirst()
}
"#;
    let external = r#"package external

class List<T>
"#;
    let provider = indexed(&[("ImportedList.kt", imported), ("ExternalList.kt", external)]);
    assert!(
        plan(imported, "ImportedList.kt", &provider)
            .container_calls
            .is_empty(),
        "an imported same-spelled generic class must not be treated as kotlin.collections.List"
    );

    let ambiguous = r#"package containercalls
import external.List
import another.List

class Owner(val items: List<String>) {
    fun first() = items.stream().findFirst()
}
"#;
    let first = r#"package external

class List<T>
"#;
    let second = r#"package another

class List<T>
"#;
    let provider = indexed(&[
        ("AmbiguousList.kt", ambiguous),
        ("ExternalList.kt", first),
        ("AnotherList.kt", second),
    ]);
    assert!(
        plan(ambiguous, "AmbiguousList.kt", &provider)
            .container_calls
            .is_empty(),
        "ambiguous generic classifier imports must not be treated as kotlin.collections.List"
    );
}

#[test]
fn optional_methods_that_return_payload_do_not_invent_a_generic_result_type() {
    let file = "OptionalPayload.kt";
    let source = r#"package containercalls
import java.util.Optional

class Entry
fun unwrap(value: Optional<Entry>): Entry = value.get()
fun isPresent(value: Optional<Entry>): Boolean = value.isPresent()
"#;
    let provider = indexed(&[(file, source)]);
    let plan = plan(source, file, &provider);
    let get = plan
        .container_calls
        .iter()
        .find(|fact| fact_text(source, fact).ends_with("get()"))
        .expect("Optional.get is a valid call");
    assert_eq!(get.receiver_type, "java.util.Optional");
    assert_eq!(get.return_type, None);
    assert!(
        plan.container_calls
            .iter()
            .any(|fact| fact_text(source, fact).ends_with("isPresent()"))
    );
}

#[test]
fn explicit_result_contracts_preserve_null_checks_without_inventing_alias_or_type_parameter_bounds()
{
    let source = r#"package containercalls
import java.util.Optional
fun required(value: Optional<String>): String = value.orElse(null)
fun nullable(value: Optional<String>): String? = value.orElse(null)
fun primitive(value: Optional<Int>): Int = value.orElse(null)
fun take(value: String): String = value
fun argument(value: Optional<String>): String = take(value.orElse(null))
fun assigned(value: Optional<String>): String { val result: String = value.orElse(null); return result }
"#;
    let provider = indexed(&[("Contracts.kt", source)]);
    let facts = plan(source, "Contracts.kt", &provider).container_calls;
    assert_eq!(facts.len(), 5, "{facts:#?}");
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.result_check.is_some())
            .count(),
        4,
        "{facts:#?}"
    );
    for source in [
        "package containercalls\nimport java.util.Optional\nfun <T> f(value: Optional<T>): T = value.orElse(null)\n",
        "package containercalls\nimport java.util.Optional\ntypealias Maybe = String?\nfun f(value: Optional<String>): Maybe = value.orElse(null)\n",
    ] {
        let provider = indexed(&[("UnknownContract.kt", source)]);
        assert!(
            plan(source, "UnknownContract.kt", &provider)
                .container_calls
                .is_empty()
        );
    }
}

#[test]
fn optional_result_contract_can_come_from_a_unique_kotlin_constructor_parameter() {
    let file = "ConstructorResultContracts.kt";
    let source = r#"package containercalls
import java.util.Optional

class PrimaryValue(val text: String)

class SecondaryValue {
    val text: String
    constructor(text: String) { this.text = text }
}

fun primary(value: Optional<String>): PrimaryValue = PrimaryValue(value.orElse(null))
fun secondary(value: Optional<String>): SecondaryValue = SecondaryValue(value.orElse(null))
"#;
    let provider = indexed(&[(file, source)]);
    let facts = plan(source, file, &provider).container_calls;
    assert_eq!(facts.len(), 2, "{facts:#?}");
    for fact in &facts {
        assert!(fact_text(source, fact).ends_with("orElse(null)"));
        assert!(matches!(
            fact.expected_type.as_ref().map(|ty| &ty.ty.ty),
            Some(notlin::semantics::KotlinType::Named { name, .. }) if name == "String"
        ));
        assert_eq!(
            fact.result_check.as_deref(),
            Some("orElse(...) must not be null")
        );
    }
}

#[test]
fn constructor_contracts_use_the_parameter_declaring_snapshot() {
    let declaration =
        "package contracts\nclass Required(val text: String)\nclass Maybe(val text: String?)\n";
    let source = r#"package uses
import java.util.Optional
import contracts.Required
import contracts.Maybe
fun required(value: Optional<String>) = Required(value.orElse(null))
fun maybe(value: Optional<String>) = Maybe(value.orElse(null))
"#;
    let provider = indexed(&[("Values.kt", declaration), ("Use.kt", source)]);
    let facts = plan(source, "Use.kt", &provider).container_calls;
    assert_eq!(facts.len(), 2, "{facts:#?}");
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.result_check.is_some())
            .count(),
        1
    );
    for fact in facts {
        let expected = fact.expected_type.unwrap();
        assert_eq!(expected.location.file, Path::new("Values.kt"));
        assert_eq!(
            expected.location.snapshot_hash,
            *blake3::hash(declaration.as_bytes()).as_bytes()
        );
        assert!(matches!(
            &declaration[expected.location.start_byte..expected.location.end_byte],
            "String" | "String?"
        ));
    }
}

#[test]
fn nonnull_literal_fallback_is_independent_of_unknown_java_argument_contract() {
    let java = r#"package containercalls;
public class JavaValue { public JavaValue(String value) {} }
"#;
    let kotlin = r#"package containercalls
import java.util.Optional
fun build(value: Optional<String>): JavaValue = JavaValue(value.orElse("fallback"))
"#;
    let provider = indexed(&[("JavaValue.java", java), ("JavaUse.kt", kotlin)]);
    let facts = plan(kotlin, "JavaUse.kt", &provider).container_calls;
    let [fact] = facts.as_slice() else {
        panic!("{facts:#?}")
    };
    assert_eq!(
        fact.receiver_payload,
        Some(notlin::semantics::TypeRef::parse("java.lang.String"))
    );
    let fallback = fact.nonnull_fallback.as_ref().expect("literal proof");
    assert_eq!(
        &kotlin[fallback.start_byte..fallback.end_byte],
        "\"fallback\""
    );
    assert_eq!(
        fallback.snapshot_hash,
        *blake3::hash(kotlin.as_bytes()).as_bytes()
    );
    assert!(fact.expected_type.is_none());
    assert!(fact.result_check.is_none());
}

#[test]
fn uncertain_constructor_contracts_remain_unplanned() {
    let generic = r#"package containercalls
import java.util.Optional

class GenericValue<T>(val value: T)
fun build(value: Optional<String>): GenericValue<String> = GenericValue(value.orElse(null))
"#;
    let provider = indexed(&[("GenericConstructor.kt", generic)]);
    assert!(
        plan(generic, "GenericConstructor.kt", &provider)
            .container_calls
            .is_empty()
    );

    let overloaded = r#"package containercalls
import java.util.Optional

class OverloadedValue(val value: String) {
    constructor(value: CharSequence) : this(value.toString())
}
fun build(value: Optional<String>): OverloadedValue = OverloadedValue(value.orElse(null))
"#;
    let provider = indexed(&[("OverloadedConstructor.kt", overloaded)]);
    assert!(
        plan(overloaded, "OverloadedConstructor.kt", &provider)
            .container_calls
            .is_empty()
    );

    let java = r#"package containercalls;
public class JavaValue { public final String value; public JavaValue(String value) { this.value = value; } }
"#;
    let kotlin = r#"package containercalls
import java.util.Optional
fun build(value: Optional<String>): JavaValue = JavaValue(value.orElse(null))
"#;
    let provider = indexed(&[("JavaValue.java", java), ("JavaUse.kt", kotlin)]);
    assert!(
        plan(kotlin, "JavaUse.kt", &provider)
            .container_calls
            .is_empty()
    );

    let named = r#"package containercalls
import java.util.Optional
class NamedValue(val value: String)
fun build(value: Optional<String>): NamedValue = NamedValue(value = value.orElse(null))
"#;
    let provider = indexed(&[("NamedConstructor.kt", named)]);
    assert!(
        plan(named, "NamedConstructor.kt", &provider)
            .container_calls
            .is_empty()
    );

    for declaration in [
        "class Value(val value: String = \"default\")",
        "class Value(vararg val values: String)",
        "@JvmInline value class Value(val value: String)",
        "class Payload\nclass Value(val value: Payload) {\n    companion object {\n        operator fun invoke(value: String?): Value = Value(Payload())\n    }\n}",
    ] {
        let source = format!(
            "package containercalls\nimport java.util.Optional\n{declaration}\n\
             fun build(value: Optional<String>) = Value(value.orElse(null))\n"
        );
        let provider = indexed(&[("UncertainConstructor.kt", &source)]);
        assert!(
            plan(&source, "UncertainConstructor.kt", &provider)
                .container_calls
                .is_empty(),
            "{declaration} must not establish a constructor parameter contract"
        );
    }
}

#[test]
fn unsupported_container_shapes_and_stale_source_remain_unplanned() {
    for (file, source) in [
        (
            "Nullable.kt",
            "package containercalls\nimport java.util.Optional\nclass Entry\nfun f(value: Optional<Entry>?): Optional<Entry>? = value?.map { it }",
        ),
        (
            "Alias.kt",
            "package containercalls\nimport java.util.Optional as Box\nclass Entry\nfun f(value: Box<Entry>): Box<Entry> = value.map { it }",
        ),
        (
            "CustomOptional.kt",
            "package containercalls\nclass Optional<T> {\n    fun map(block: (T) -> T): Optional<T> = this\n}\nclass Entry\nfun f(value: Optional<Entry>): Optional<Entry> = value.map { it }",
        ),
        (
            "UnknownStar.kt",
            "package containercalls\nimport uncertain.*\nclass Entry\nfun f(value: Optional<Entry>): Optional<Entry> = value.map { it }",
        ),
        (
            "Map.kt",
            "package containercalls\nfun f(value: Map<String, String>) = value.stream()",
        ),
        (
            "BadArity.kt",
            "package containercalls\nimport java.util.Optional\nclass Entry\nfun f(value: Optional<Entry>) = value.map()",
        ),
        (
            "BadOrElse.kt",
            "package containercalls\nimport java.util.Optional\nclass Entry\nfun f(value: Optional<Entry>) = value.orElse()",
        ),
        (
            "BadOrElseGet.kt",
            "package containercalls\nimport java.util.Optional\nclass Entry\nfun f(value: Optional<Entry>) = value.orElseGet()",
        ),
        (
            "Named.kt",
            "package containercalls\nimport java.util.Optional\nclass Entry\nfun f(value: Optional<Entry>) = value.orElse(other = Entry())",
        ),
        (
            "Spread.kt",
            "package containercalls\nclass Entry\nfun f(value: List<Entry>, args: Array<Entry>) = value.stream(*args)",
        ),
    ] {
        let provider = indexed(&[(file, source)]);
        let plan = plan(source, file, &provider);
        assert!(plan.container_calls.is_empty(), "{file}: {plan:#?}");
    }

    let file = "Stale.kt";
    let original = "package containercalls\nimport java.util.Optional\nclass Entry\nfun f(value: Optional<Entry>) = value.map { it }";
    let provider = indexed(&[(file, original)]);
    let changed = original.replace("map { it }", "filter { true }");
    assert!(plan(&changed, file, &provider).container_calls.is_empty());
}
