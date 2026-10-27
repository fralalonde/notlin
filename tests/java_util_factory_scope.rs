use clap::Parser;
use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider, plan_required_calls};
use notlin::translation_plan::{BackendOwner, RetentionReason};
use std::path::{Path, PathBuf};

fn analyze(
    source: &str,
) -> (
    notlin::translation_plan::TranslationPlan,
    SyntaxSemanticProvider,
) {
    let path = PathBuf::from("Factory.kt");
    let provider = SyntaxSemanticProvider::new(vec![(path.clone(), source.to_owned())]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), &path);
    plan_required_calls(source, &tree, &path, &provider, false, &mut plan);
    (plan, provider)
}

fn function_owner(plan: &notlin::translation_plan::TranslationPlan, name: &str) -> BackendOwner {
    plan.declarations
        .iter()
        .find(|declaration| declaration.symbol_id.name == name)
        .unwrap()
        .candidate_owner
}

#[test]
fn partial_or_modified_indexes_do_not_claim_complete_inheritance() {
    let file = PathBuf::from("Parent.kt");
    let source = "package sample\nclass Parent\n";
    let mut provider = SyntaxSemanticProvider::default();
    provider.index_file(&file, source);
    let owner = provider.symbols_named("Parent")[0].id.clone();
    assert!(!provider.has_complete_inheritance(&owner));

    let mut provider = SyntaxSemanticProvider::new([(file, source.into())]);
    assert!(provider.has_complete_inheritance(&owner));
    provider.index_file(
        Path::new("Child.kt"),
        "package sample\nclass Child: ExternalBase()\n",
    );
    assert!(!provider.has_complete_inheritance(&owner));
}

#[test]
fn constructor_defaults_do_not_search_inherited_instance_factory_members() {
    for parent in ["java.io.Serializable", "ExternalMarker"] {
        let source = format!(
            "package sample\nimport java.util.*\nclass Message(val values: Map<String, Any> = hashMapOf()) : {parent}\n"
        );
        let (plan, _) = analyze(&source);
        assert_eq!(
            function_owner(&plan, "Message"),
            BackendOwner::Java,
            "{parent}: {plan:?}"
        );
    }
}

#[test]
fn constructor_defaults_still_check_companion_factory_members_and_parameters() {
    let cases = [
        "class Message(val values: Map<String, Any> = hashMapOf()) : java.io.Serializable { companion object { val hashMapOf: () -> Map<String, Any> = TODO() } }",
        "class Message(val values: Map<String, Any> = hashMapOf()) : java.io.Serializable { companion object : ExternalBase() }",
        "class Message(hashMapOf: () -> Map<String, Any>, val values: Map<String, Any> = hashMapOf()) : java.io.Serializable",
    ];
    for declaration in cases {
        let source = format!("package sample\nimport java.util.*\n{declaration}\n");
        let (plan, _) = analyze(&source);
        assert_eq!(
            function_owner(&plan, "Message"),
            BackendOwner::Kotlin,
            "{declaration}: {plan:?}"
        );
    }
}

#[test]
fn java_util_wildcard_accepts_structurally_proven_populated_factories() {
    let source = r#"package sample
import java.util.*
fun entries(key: String?, value: Int?): Map<String?, Int?> = mapOf(key to value, null to 3)
fun nullableValues(): List<String?> = listOf(null, "value")
"#;
    let (plan, _) = analyze(source);
    for name in ["entries", "nullableValues"] {
        assert_eq!(function_owner(&plan, name), BackendOwner::Java);
        let declaration = plan
            .declarations
            .iter()
            .find(|declaration| declaration.symbol_id.name == name)
            .unwrap();
        assert!(!declaration.retention_reasons.iter().any(|reason| matches!(
            reason,
            RetentionReason::UnknownRequiredFact { name: required }
                if matches!(required.as_str(), "mapOf" | "listOf")
        )));
    }
}

#[test]
fn java_util_wildcard_keeps_the_existing_empty_factory_contracts() {
    let source = "package sample\nimport java.util.*\nfun names(): List<String> = emptyList()\nfun tags(): Set<String> = emptySet()\nfun entries(): Map<String, String> = emptyMap()\n";
    let (plan, _) = analyze(source);
    for name in ["names", "tags", "entries"] {
        assert_eq!(function_owner(&plan, name), BackendOwner::Java, "{name}");
    }
}

#[test]
fn unrelated_package_to_function_does_not_block_map_factory_proof() {
    let source = "package sample\nimport java.util.*\nfun entries(): Map<String, Int> = mapOf(\"key\" to 1)\n";
    let path = PathBuf::from("Caller.kt");
    let provider = SyntaxSemanticProvider::new(vec![
        (path.clone(), source.to_owned()),
        (
            PathBuf::from("Unrelated.kt"),
            "package unrelated\ninfix fun String.to(value: Int): Pair<String, Int> = Pair(this, value)\n"
                .to_owned(),
        ),
    ]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), &path);
    plan_required_calls(source, &tree, &path, &provider, false, &mut plan);
    assert_eq!(function_owner(&plan, "entries"), BackendOwner::Java);
}

#[test]
fn unrelated_member_to_does_not_block_top_level_map_factory_proof() {
    let source = "package sample\nimport java.util.*\nfun entries(): Map<String, Int> = mapOf(\"key\" to 1)\n";
    let path = PathBuf::from("Caller.kt");
    let provider = SyntaxSemanticProvider::new(vec![
        (path.clone(), source.to_owned()),
        (
            PathBuf::from("Unrelated.kt"),
            "package sample\nclass SwitchContext { infix fun String.to(value: Int): Pair<String, Int> = Pair(this, value) }\n"
                .to_owned(),
        ),
    ]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), &path);
    plan_required_calls(source, &tree, &path, &provider, false, &mut plan);
    assert_eq!(function_owner(&plan, "entries"), BackendOwner::Java);
}

#[test]
fn arbitrary_stars_aliases_and_spreads_still_retain_factory_callers() {
    let cases = [
        (
            "wildcard",
            "import java.util.*\nimport external.*\n",
            "mapOf(\"a\" to 1)",
        ),
        (
            "alias",
            "import java.util.*\nimport external.buildMap as mapOf\n",
            "mapOf(\"a\" to 1)",
        ),
        ("spread", "import java.util.*\n", "mapOf(*pairs)"),
        ("local-shadow", "import java.util.*\n", "mapOf(\"a\")"),
    ];
    for (name, imports, expression) in cases {
        let function_name = name.replace('-', "_");
        let declaration = if name == "local-shadow" {
            format!(
                "fun {function_name}(mapOf: (String) -> Map<String, Int>): Map<String, Int> = {expression}"
            )
        } else {
            format!("fun {function_name}(pairs: Array<Pair<String, Int>>): Any = {expression}")
        };
        let source = format!("package sample\n{imports}{declaration}\n");
        let (plan, _) = analyze(&source);
        assert_eq!(
            function_owner(&plan, &function_name),
            BackendOwner::Kotlin,
            "{name} should not gain the java.util wildcard factory proof"
        );
    }
}

#[test]
fn indexed_source_package_stars_admit_factories_unless_the_name_is_declared() {
    let source =
        "package sample\nimport models.*\nfun entries(): Map<String, Int> = mapOf(\"key\" to 1)\n";
    for (dependency, expected) in [
        ("package models\nclass Entry\n", BackendOwner::Java),
        (
            "package models\nfun mapOf(value: Pair<String, Int>): Map<String, Int> = TODO()\n",
            BackendOwner::Kotlin,
        ),
        (
            "package models\ninfix fun String.to(value: Int): Pair<String, Int> = TODO()\n",
            BackendOwner::Kotlin,
        ),
    ] {
        let path = PathBuf::from("Caller.kt");
        let provider = SyntaxSemanticProvider::new(vec![
            (path.clone(), source.to_owned()),
            (PathBuf::from("Models.kt"), dependency.to_owned()),
        ]);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), &path);
        plan_required_calls(source, &tree, &path, &provider, false, &mut plan);
        assert_eq!(function_owner(&plan, "entries"), expected, "{dependency}");
    }
}

#[test]
fn same_package_factory_shadow_is_not_rewritten_as_a_stdlib_factory() {
    let source = r#"package sample
import java.util.*
fun mapOf(pair: Pair<String, Int>): Map<String, Int> = emptyMap()
fun caller(): Map<String, Int> = mapOf("key" to 1)
"#;
    let cli = notlin::cli::Cli::parse_from(["notlin", "Factory.kt"]);
    let (files, _, _, _) = notlin::transpiler::transpile(source, Path::new("Factory.kt"), &cli);
    let java = files
        .into_iter()
        .map(|(_, output)| output)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !java.contains("Stream.of("),
        "a same-package mapOf function must not be mistaken for the stdlib factory:\n{java}"
    );
}

#[test]
fn enclosing_member_factory_shadow_is_visible_in_nested_class() {
    let source = r#"package sample
import java.util.*
class Outer {
    fun mapOf(pair: Pair<String, Int>): Map<String, Int> = emptyMap()
    inner class Inner {
        fun caller(): Map<String, Int> = mapOf("key" to 1)
    }
}

"#;
    let cli = notlin::cli::Cli::parse_from(["notlin", "Factory.kt"]);
    let (files, _, _, _) = notlin::transpiler::transpile(source, Path::new("Factory.kt"), &cli);
    let java = files
        .into_iter()
        .map(|(_, output)| output)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !java.contains("Stream.of("),
        "an enclosing member named mapOf must not be mistaken for the stdlib factory:\n{java}"
    );
}

#[test]
fn local_and_inherited_infix_to_members_block_map_factory_proof() {
    let sources = [
        (
            "extension-receiver",
            "caller",
            "package sample\nimport java.util.*\nfun ExternalReceiver.caller(): Map<String, Int> = mapOf(\"key\" to 1)\n",
        ),
        (
            "named-companion-member",
            "Holder",
            "package sample\nimport java.util.*\nclass Holder { companion object Factory { infix fun String.to(value: Int): Pair<String, Int> = Pair(this, value)\n fun caller(): Map<String, Int> = mapOf(\"key\" to 1) } }\n",
        ),
        (
            "inherited",
            "Child",
            r#"package sample
import java.util.*
open class Base {
    infix fun String.to(value: Int): Pair<String, Int> = Pair(this, value)
}
class Child : Base() {
    fun caller(): Map<String, Int> = mapOf("key" to 1)
}
"#,
        ),
        (
            "local",
            "caller",
            r#"package sample
import java.util.*
fun caller(): Map<String, Int> {
    infix fun String.to(value: Int): Pair<String, Int> = Pair(this, value)
    return mapOf("key" to 1)
}
"#,
        ),
        (
            "unresolved-parent",
            "Child",
            r#"package sample
import java.util.*
class Child : ExternalBase() {
    fun caller(): Map<String, Int> = mapOf("key" to 1)
}
"#,
        ),
        (
            "own-member",
            "Child",
            r#"package sample
import java.util.*
class Child {
    infix fun String.to(value: Int): Pair<String, Int> = Pair(this, value)
    fun caller(): Map<String, Int> = mapOf("key" to 1)
}
"#,
        ),
        (
            "outer-member",
            "Outer",
            r#"package sample
import java.util.*
open class Outer {
    infix fun String.to(value: Int): Pair<String, Int> = Pair(this, value)
    inner class Inner {
        fun caller(): Map<String, Int> = mapOf(("key" to 1))
    }
}
"#,
        ),
        (
            "object-member",
            "Holder",
            r#"package sample
import java.util.*
object Holder {
    infix fun String.to(value: Int): Pair<String, Int> = Pair(this, value)
    fun caller(): Map<String, Int> = mapOf("key" to 1)
}
"#,
        ),
        (
            "companion-member",
            "Holder",
            r#"package sample
import java.util.*
class Holder {
    companion object {
        infix fun String.to(value: Int): Pair<String, Int> = Pair(this, value)
        fun caller(): Map<String, Int> = mapOf("key" to 1)
    }
}
"#,
        ),
        (
            "aliased-supertype",
            "Child",
            r#"package sample
import java.util.*
import external.ExternalBase as Base
open class Base
class Child : Base() {
    fun caller(): Map<String, Int> = mapOf("key" to 1)
}
"#,
        ),
    ];
    for (name, owner, source) in sources {
        let (plan, _) = analyze(source);
        let declaration = plan
            .declarations
            .iter()
            .find(|declaration| declaration.symbol_id.name == owner);
        assert!(
            declaration.is_some(),
            "{name}: declarations = {:?}",
            plan.declarations
        );
        assert_eq!(
            declaration.unwrap().candidate_owner,
            BackendOwner::Kotlin,
            "{name} lexical/member extension named `to` must not be bypassed"
        );
        assert!(
            plan.declarations
                .iter()
                .any(|declaration| declaration.retention_reasons.iter().any(
                    |reason| matches!(reason,
                        RetentionReason::UnknownRequiredFact { name } if name == "mapOf"
                    )
                )),
            "{name} must reject the factory proof itself, independently of other unsupported calls"
        );
    }
}
