use notlin::semantics::{
    FactStatus, SemanticProvider, SyntaxSemanticProvider, plan_required_calls,
};
use std::path::{Path, PathBuf};

fn provider(files: &[(&str, &str)]) -> SyntaxSemanticProvider {
    for (path, source) in files {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        assert!(
            !parser.parse(source, None).unwrap().root_node().has_error(),
            "invalid fixture {path}:\n{source}"
        );
    }
    SyntaxSemanticProvider::new(
        files
            .iter()
            .map(|(path, source)| (PathBuf::from(path), (*source).to_owned())),
    )
}

fn callable(index: &SyntaxSemanticProvider, name: &str) -> bool {
    let methods = index
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| symbol.id.kind == "function" && symbol.id.parameters.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(methods.len(), 1, "expected one {name} method");
    index.is_java_callable_member(&methods[0].id)
}

#[test]
fn ordinary_kotlin_noarg_functions_have_a_proven_java_call_name() {
    let source = r#"package members
class Key
enum class Flag {
    FIRST
}
interface Api {
    fun getKey(): Key
    fun count(): Int
    fun text(): String
    fun flag(): Flag
}
class Box<T> {
    fun value(): T = TODO()
}
"#;
    let index = provider(&[("Api.kt", source)]);
    for name in ["getKey", "count", "text", "flag", "value"] {
        assert!(callable(&index, name), "{name}");
    }
}

#[test]
fn kotlin_source_callability_rejects_changed_jvm_names_and_unsupported_signatures() {
    let cases = [
        "internal fun read(): String = \"x\"",
        "protected fun read(): String = \"x\"",
        "private fun read(): String = \"x\"",
        "suspend fun read(): String = \"x\"",
        "inline fun read(): String = \"x\"",
        "@JvmName(\"renamed\") fun read(): String = \"x\"",
        "@JvmSynthetic fun read(): String = \"x\"",
        "@JvmStatic fun read(): String = \"x\"",
        "@Throws(java.io.IOException::class) fun read(): String = \"x\"",
        "@kotlin.jvm.Throws(java.io.IOException::class) fun read(): String = \"x\"",
        "fun <T> read(): T = TODO()",
        "fun read(): String? = null",
        "fun read(): Missing = TODO()",
        "fun read() = \"inferred\"",
    ];
    for declaration in cases {
        let source = format!("package members\nclass Api {{\n{declaration}\n}}\n");
        let index = provider(&[("Api.kt", &source)]);
        assert!(!callable(&index, "read"), "{declaration}");
    }
    let source = "package members\nimport kotlin.jvm.JvmName as Renamed\nclass Api {\n@Renamed(\"other\") fun read(): String = \"x\"\n}\n";
    assert!(!callable(&provider(&[("Api.kt", source)]), "read"));
    let checked = "package members\nimport kotlin.jvm.Throws as Checked\nclass Api {\n@Checked(java.io.IOException::class) fun read(): String = \"x\"\n}\n";
    assert!(!callable(&provider(&[("Api.kt", checked)]), "read"));
}

#[test]
fn kotlin_source_callability_does_not_invent_value_class_or_alias_abis() {
    let cases = [
        "@JvmInline value class Api(val raw: String) {\nfun read(): String = raw\n}",
        "@JvmInline value class Token(val raw: String)\ninterface Api {\nfun read(): Token\n}",
        "@JvmInline value class Token(val raw: String)\ninterface Api<T : Token> {\nfun read(): T\n}",
        "typealias Text = String\ninterface Api {\nfun read(): Text\n}",
        "interface Api {\nfun read(): List<String>\n}",
    ];
    for declarations in cases {
        let source = format!("package members\n{declarations}\n");
        let index = provider(&[("Api.kt", &source)]);
        assert!(!callable(&index, "read"), "{source}");
    }
    let index = provider(&[
        ("Text.kt", "package types\ntypealias Text = String\n"),
        (
            "Api.kt",
            "package members\nimport types.Text as Result\ninterface Api {\nfun read(): Result\n}\n",
        ),
    ]);
    assert!(!callable(&index, "read"));
}

#[test]
fn exact_overload_identity_and_java_member_spelling_are_required() {
    let index = provider(&[(
        "Api.kt",
        "package members\nclass Api {\nfun read(): String = \"x\"\nfun read(n: Int): String = \"y\"\n}\nfun Api.extension(): String = \"x\"\nfun topLevel(): String = \"x\"\n",
    )]);
    let methods = index.symbols_named("read");
    assert_eq!(methods.len(), 2);
    for method in methods {
        assert_eq!(
            index.is_java_callable_member(&method.id),
            method.id.parameters.is_empty()
        );
    }
    assert!(!callable(&index, "extension"));
    assert!(!callable(&index, "topLevel"));
    for name in ["default", "has-dash", "white space"] {
        let source = format!("package members\nclass Api {{\nfun `{name}`(): String = \"x\"\n}}\n");
        let index = provider(&[("Api.kt", &source)]);
        let method = index
            .symbols()
            .iter()
            .find(|symbol| symbol.id.kind == "function")
            .unwrap();
        assert!(!index.is_java_callable_member(&method.id), "{name}");
    }
}

#[test]
fn duplicate_or_stale_source_symbols_do_not_prove_a_java_call() {
    let first = "package members\nclass Api {\nfun read(): String = \"first\"\n}\n";
    let duplicate = provider(&[("Api.kt", first), ("Api.kt", first)]);
    for symbol in duplicate.symbols_named("read") {
        assert!(!duplicate.is_java_callable_member(&symbol.id));
    }
    let changed = provider(&[
        ("Api.kt", first),
        (
            "Api.kt",
            "package members\nclass Api {\nfun other(): String = \"new\"\n}\n",
        ),
    ]);
    assert!(!callable(&changed, "read"));
}

#[test]
fn chained_call_proof_uses_the_resolved_producers_type_scope() {
    let source = "package consumer\nimport producer.Source\nclass Payload {\nfun getKey(): Int = 1\n}\nfun read(source: Source): String = source.getPayload().getKey()\n";
    let index = provider(&[
        (
            "Source.kt",
            "package producer\nimport model.Payload\nclass Source {\nfun getPayload(): Payload = Payload()\n}\n",
        ),
        (
            "Payload.kt",
            "package model\nclass Payload {\nfun getKey(): String = \"right\"\n}\n",
        ),
        ("Consumer.kt", source),
    ]);
    let path = Path::new("Consumer.kt");
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), path);
    plan_required_calls(source, &tree, path, &index, false, &mut plan);
    let dependency = plan
        .dependencies
        .iter()
        .find(|dependency| dependency.spelling == "getKey")
        .unwrap();
    let FactStatus::Established(id) = &dependency.resolution else {
        panic!("{dependency:?}");
    };
    assert_eq!(id.package, "model");
    assert_eq!(id.file, Path::new("Payload.kt"));
    assert!(index.is_java_callable_member(id));
}
