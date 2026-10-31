use clap::Parser;
use std::path::PathBuf;

fn java(source: &str) -> String {
    let cli = notlin::cli::Cli::parse_from(["notlin", "M.kt"]);
    let (files, errors, _, _) = notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    let output = files
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(errors, 0, "{output}");
    output
}

#[test]
fn map_operator_and_member_calls_use_the_same_static_copy_operation() {
    for body in ["left + right", "left.plus(right)"] {
        let source = format!(
            "package sample\nfun merge(left: Map<String, Int>, right: Map<String, Int>): Map<String, Int> = {body}\n"
        );
        let output = java(&source);
        assert!(
            output.contains("kotlin.collections.MapsKt.plus(left, (java.util.Map) (right))"),
            "{output}"
        );
    }
}

#[test]
fn map_literals_merge_with_parameters() {
    let output = java(
        "package sample\nfun merge(other: Map<String, Int>): Map<String, Int> = mapOf(\"a\" to 1) + other\n",
    );
    assert!(
        output.contains("kotlin.collections.MapsKt.plus("),
        "{output}"
    );
    assert!(
        output.contains("public static Map<String,Integer> merge"),
        "{output}"
    );
}

#[test]
fn map_literals_merge_with_companion_properties() {
    let output = java(
        "package sample\ninterface Defaults {\n companion object {\n  val base: Map<String, Int> = mapOf()\n }\n}\nfun merge(): Map<String, Int> = mapOf(\"a\" to 1) + Defaults.base\n",
    );
    assert!(
        output.contains("kotlin.collections.MapsKt.plus("),
        "{output}"
    );
    assert!(
        output.contains("public static Map<String,Integer> merge"),
        "{output}"
    );
}

#[test]
fn declared_constructor_property_types_select_map_algebra() {
    let output = java(
        "package sample\nclass Holder(private val contexts: Map<String, Int>) {\n fun merge(other: Holder): Map<String, Int> = this.contexts + other.contexts\n}\n",
    );
    assert!(
        output.contains("kotlin.collections.MapsKt.plus("),
        "{output}"
    );
    assert!(output.contains("class Holder"), "{output}");
}

#[test]
fn distinct_companion_maps_merge_without_losing_their_declared_types() {
    let output = java(
        "package sample\ninterface First {\n companion object {\n  val BASE: Map<String, Int> = mapOf()\n }\n}\ninterface Second {\n companion object {\n  val BASE: Map<String, Int> = mapOf()\n }\n}\nfun merge(): Map<String, Int> = First.BASE + Second.BASE\n",
    );
    assert!(
        output.contains("kotlin.collections.MapsKt.plus("),
        "{output}"
    );
    assert!(
        output.contains("public static Map<String,Integer> merge"),
        "{output}"
    );
}

#[test]
fn unqualified_companion_maps_and_nested_merges_translate() {
    let output = java(
        "package sample\ninterface Other {\n companion object {\n  val BASE: Map<String, Int> = mapOf()\n }\n}\nclass Holder {\n companion object {\n  val BASE: Map<String, Int> = mapOf()\n  val combined: Map<String, Int> = BASE + Other.BASE + mapOf()\n }\n}\n",
    );
    assert!(output.contains("class Holder"), "{output}");
    assert!(
        output.matches("kotlin.collections.MapsKt.plus(").count() >= 2,
        "{output}"
    );
}
