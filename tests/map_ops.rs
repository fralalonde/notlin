use clap::Parser;
use std::path::PathBuf;

/// Kotlin Map op `filterValues { pred }` lowers to Java via the entrySet
/// stream: filter on `getValue()` then `Collectors.toMap(getKey, getValue)`.
/// Emitting `map.filterValues(...)` verbatim breaks javac — no such Java Map
/// member exists.
#[test]
fn map_filter_values_collects_to_map() {
    let source =
        "package neutral.mapa\nfun select(m: Map<String, Int>) = m.filterValues { v -> v > 0 }\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    let m = files
        .iter()
        .find(|(n, _)| n == "M.java")
        .map(|(_, c)| c.as_str())
        .expect("M.java");
    assert!(
        m.contains("entrySet().stream()") && m.contains("java.util.stream.Collectors.toMap("),
        "Map.filterValues must lower to entrySet stream + toMap: {m}"
    );
}

/// Kotlin `Iterable.associateBy { k }` lowers to
/// `stream().collect(Collectors.toMap(keyfn, v -> v))`; Kotlin Map has no
/// associateBy and Java none either — but the List/Iterable form does map.
#[test]
fn associate_by_collects_to_map() {
    let source = "package neutral.mapb\nfun tv(cs: List<String>) = cs.associateBy { it.length }\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    let m = files
        .iter()
        .find(|(n, _)| n == "M.java")
        .map(|(_, c)| c.as_str())
        .expect("M.java");
    assert!(
        m.contains(".stream().collect(java.util.stream.Collectors.toMap("),
        "associateBy must lower to toMap: {m}"
    );
    assert!(
        !m.contains(".associateBy("),
        "associateBy member must not survive: {m}"
    );
}

#[test]
fn associate_by_with_value_selector_uses_second_lambda() {
    let source = "package neutral.mapc\ndata class Entry(val key: String, val value: Int)\nfun tv(cs: List<Entry>) = cs.associateBy({ it.key }, { it.value })\nfun make() = Entry(\"x\", 1)\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    let java = files
        .iter()
        .find(|(name, _)| name == "M.java")
        .map(|(_, code)| code.as_str())
        .expect("M.java");
    assert!(
        java.contains("v -> v.getKey(), v -> v.getValue()"),
        "{java}"
    );
    assert!(java.contains("new Entry(\"x\", 1)"), "{java}");
}

#[test]
fn filter_not_and_plus_element_lower_to_streams() {
    let source = "package neutral.mapd\nfun add(xs: List<String>, item: String): List<String> = xs.filterNot { it == item } + item\nfun remove(xs: List<String>, item: String): List<String> = xs.filterNot { it == item }\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    let java = files
        .iter()
        .find(|(name, _)| name == "M.java")
        .map(|(_, code)| code.as_str())
        .expect("M.java");
    assert!(
        java.contains("filter(it -> !(Objects.equals(it, item)))"),
        "{java}"
    );
    assert!(java.contains("Stream.concat("), "{java}");
    assert!(!java.contains("filterNot"), "{java}");
}
