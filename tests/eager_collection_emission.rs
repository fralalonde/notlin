use clap::Parser;
use notlin::semantics::{EagerCollectionOperation, SyntaxSemanticProvider};
use notlin::transpiler::{PlannedTranslation, WorkspaceScope, plan_with_tree_hint};
use std::path::{Path, PathBuf};

fn translate(source: &str) -> PlannedTranslation {
    let file = Path::new("Operations.kt");
    let provider = SyntaxSemanticProvider::new([(PathBuf::from(file), source.to_owned())]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture: {source}");
    plan_with_tree_hint(
        source,
        &tree,
        file,
        &notlin::cli::Cli::parse_from(["notlin", "--annotations", "none", "Operations.kt"]),
        WorkspaceScope {
            semantic_provider: Some(&provider),
            ..Default::default()
        },
        true,
    )
}

fn java(result: &PlannedTranslation) -> String {
    assert!(!result.emission_failed, "{:?}", result.plan.diagnostics);
    assert_eq!(result.errors, 0, "{:?}", result.plan.diagnostics);
    result
        .java_files
        .iter()
        .map(|(_, source)| source.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn exact_eager_chains_have_explicit_helpers_and_origins() {
    let result = translate(
        r#"package sample.eager
class Operations {
    fun values(input: List<String>): List<String> = input.map { it }.filterNot { it == "skip" }
}
"#,
    );
    let java = java(&result);
    assert!(java.contains("notlinFilterNot(notlinMap(input,"), "{java}");
    assert!(!java.contains(".stream()"), "{java}");
    assert_eq!(result.warnings, 0, "{java}");
    assert_eq!(result.plan.eager_collection_helpers.len(), 2, "{java}");
    for helper in &result.plan.eager_collection_helpers {
        assert!(
            result
                .plan
                .bridges
                .iter()
                .any(|bridge| bridge.id == helper.id && bridge.origin == helper.origin)
        );
        assert!(
            result
                .plan
                .provenance
                .iter()
                .any(|origin| origin.generated == helper.id && origin.origin == helper.origin)
        );
        let mut targets = Vec::new();
        let mut root = result.accepted.files()[0].unit.root.clone();
        root.visit_mut(&mut |node| {
            if node.origin_target.as_ref() == Some(&helper.id) {
                targets.push(node.kind.clone());
            }
        });
        assert!(
            targets.iter().any(|kind| kind == "method_declaration"),
            "{targets:?}"
        );
        assert!(
            targets.iter().any(|kind| kind == "method_invocation"),
            "{targets:?}"
        );
    }
}

#[test]
fn names_avoid_source_declarations_and_lambda_parameters() {
    let result = translate(
        r#"package sample.eager
class Operations {
    fun notlinMap(): String = "unused"
    fun values(input: List<String>, notlinItem: String): List<String> = input.map { it }
}
"#,
    );
    let java = java(&result);
    let helper = result
        .plan
        .eager_collection_helpers
        .first()
        .expect("map helper");
    assert_ne!(helper.name, "notlinMap");
    assert_ne!(helper.parameter_name, "notlinItem");
    assert!(
        java.contains(&format!(
            "{}(input, {} -> {}",
            helper.name, helper.parameter_name, helper.parameter_name
        )),
        "{java}"
    );
}

#[test]
fn constant_lambda_still_has_one_parameter_and_helpers_stay_with_emitted_owners() {
    let result = translate(
        r#"package sample.eager
fun values(input: List<String>): List<String> = input.map { "constant" }
suspend fun retained(input: List<String>): List<String> = input.map { it }
"#,
    );
    let java = java(&result);
    assert!(!java.contains("() ->"), "{java}");
    assert_eq!(result.plan.eager_collection_helpers.len(), 1, "{java}");
    let helper = &result.plan.eager_collection_helpers[0];
    assert_eq!(helper.origin.name, "values");
    assert_eq!(helper.operation, EagerCollectionOperation::Map);
    assert!(java.contains("-> \"constant\""), "{java}");
}

#[test]
fn helper_identity_ignores_body_whitespace_and_generated_display_name() {
    let first = translate(
        "package sample.eager\nclass Operations {\n fun values(input: List<String>): List<String> = input.map { it }\n}\n",
    );
    let second = translate(
        "package sample.eager\nclass Operations {\n fun notlinMap(): String = \"occupied\"\n fun values(input: List<String>): List<String> = input.map { \"changed\" }\n}\n",
    );
    let first = &first.plan.eager_collection_helpers[0];
    let second = &second.plan.eager_collection_helpers[0];
    assert_eq!(first.id, second.id);
    assert_ne!(first.name, second.name);
}

#[test]
fn primitive_and_nullable_element_bindings_keep_distinct_equality_rules() {
    let result = translate(
        r#"package sample.eager
class Operations {
    fun values(input: List<Int>): List<Int> = input.map { it }
    fun nullable(input: List<Int?>): List<Int?> = input.filterNot { it == 2 }
}
"#,
    );
    let java = java(&result);
    assert!(
        java.contains("int notlinItemValue = notlinItem; return notlinItemValue;"),
        "{java}"
    );
    assert!(java.contains("Objects.equals(notlinItem, 2)"), "{java}");
}

#[test]
fn property_projection_emits_the_proven_getter_and_keeps_nullable_result_binding() {
    let result = translate(
        r#"package sample.eager
class Entry(val count: Int?)
class Operations {
    fun values(input: List<Entry>): List<Int?> = input.map { it.count }.filterNot { it == 0 }
}
"#,
    );
    let java = java(&result);
    assert!(java.contains("getCount()"), "{java}");
    assert!(
        java.contains("Objects.equals("),
        "nullable primitive filter must preserve boxed equality: {java}"
    );

    let projection = result
        .plan
        .eager_collection_calls
        .iter()
        .find_map(|fact| fact.property_projection.as_ref())
        .expect("property projection fact");
    assert_eq!(projection.target.name, "count");
    assert!(!projection.java_parameter_name.is_empty());

    let mut getter_targets = Vec::new();
    for accepted_file in result.accepted.files() {
        let mut root = accepted_file.unit.root.clone();
        root.visit_mut(&mut |node| {
            if node.kind == "method_invocation"
                && node.origin_target.as_ref() == Some(&projection.target)
            {
                getter_targets.push(node.origin_target.clone().unwrap());
            }
        });
    }
    assert_eq!(
        getter_targets,
        vec![projection.target.clone()],
        "accepted Java getter invocation should retain the exact Kotlin property origin"
    );
}

#[test]
fn container_property_calls_keep_explicit_equivalent_method_targets() {
    let result = translate(
        r#"package sample.eager
import java.util.Optional
class Entry(@get:JvmName("hiddenName") val name: String) {
    fun getName(): String = name
    suspend fun retained() { }
}
class Operations {
    fun values(input: List<Entry>): Optional<String> = input.stream().map { it.name }.findFirst()
}
"#,
    );
    let java = java(&result);
    assert!(java.contains(".getName()"), "{java}");
    assert!(!java.contains(".hiddenName()"), "{java}");
    let projection = &result
        .plan
        .container_calls
        .iter()
        .find_map(|fact| fact.lambda_binding.as_ref())
        .unwrap()
        .property_accesses[0]
        .projection;
    let target = projection
        .getter_target
        .as_ref()
        .expect("explicit method proof");
    assert_eq!(target.kind, "function");
    assert_eq!(target.name, "getName");
    let mut count = 0;
    for accepted in result.accepted.files() {
        accepted.unit.root.clone().visit_mut(&mut |node| {
            if node.kind == "method_invocation" && node.origin_target.as_ref() == Some(target) {
                count += 1;
            }
        });
    }
    assert_eq!(count, 1, "{java}");
    let dependencies = &result
        .plan
        .container_calls
        .iter()
        .find(|fact| fact.lambda_binding.is_some())
        .unwrap()
        .dependencies;
    assert!(dependencies.contains(target));
    assert!(dependencies.contains(&projection.target));
}

#[test]
fn same_spelled_getters_keep_distinct_origins_and_avoid_source_parameters() {
    let result = translate(
        r#"package sample.eager
class FirstEntry(val name: String)
class SecondEntry(val name: String)
class Operations {
    fun first(input: List<FirstEntry>, notlinValue: String): List<String> = input.map { it.name }
    fun second(input: List<SecondEntry>): List<String> = input.map { entry -> entry.name }
}
"#,
    );
    let java = java(&result);
    let projections = result
        .plan
        .eager_collection_calls
        .iter()
        .filter_map(|call| call.property_projection.as_ref())
        .collect::<Vec<_>>();
    assert_eq!(projections.len(), 2, "{java}");
    assert_ne!(projections[0].target, projections[1].target);
    assert_ne!(
        projections[0].java_parameter_name,
        projections[1].java_parameter_name
    );
    for projection in projections {
        assert_ne!(projection.java_parameter_name, "notlinValue");
        let mut count = 0;
        for file in result.accepted.files() {
            file.unit.root.clone().visit_mut(&mut |node| {
                if node.kind == "method_invocation"
                    && node.origin_target.as_ref() == Some(&projection.target)
                {
                    count += 1;
                }
            });
        }
        assert_eq!(count, 1, "{java}");
    }
}
