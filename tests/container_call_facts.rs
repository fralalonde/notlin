use notlin::semantics::{
    ContainerMemberCall, SemanticProvider, SyntaxSemanticProvider, plan_container_calls,
    plan_required_calls,
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
    plan_container_calls(source, &tree, path, provider, &mut plan);
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
fun take(value: String): String = value
fun argument(value: Optional<String>): String = take(value.orElse(null))
fun assigned(value: Optional<String>): String { val result: String = value.orElse(null); return result }
"#;
    let provider = indexed(&[("Contracts.kt", source)]);
    let facts = plan(source, "Contracts.kt", &provider).container_calls;
    assert_eq!(facts.len(), 4, "{facts:#?}");
    assert_eq!(
        facts
            .iter()
            .filter(|fact| fact.result_check.is_some())
            .count(),
        3,
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
