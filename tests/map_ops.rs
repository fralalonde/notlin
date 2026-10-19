use clap::Parser;
use std::path::PathBuf;

/// Kotlin Map op `filterValues { pred }` lowers to Java via the entrySet
/// stream: filter on `getValue()` then collect into an insertion-ordered map.
/// Emitting `map.filterValues(...)` verbatim breaks javac — no such Java Map
/// member exists.
#[test]
fn map_filter_values_preserves_order_and_accepts_null_values() {
    let source = "package neutral.mapa\nfun select(m: Map<String, Int?>): Map<String, Int?> = m.filterValues { v -> v == null || v > 0 }\n";
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
        m.contains("entrySet().stream()")
            && m.contains("java.util.LinkedHashMap::new")
            && m.contains(".getKey(),")
            && m.contains(".getValue())")
            && m.contains("java.util.LinkedHashMap::putAll")
            && !m.contains("Collectors.toMap"),
        "Map.filterValues must lower to a null-safe ordered collector: {m}"
    );
}

#[test]
fn map_keys_overwrite_duplicate_keys_without_losing_order() {
    let source = "package neutral.mapkeys\nfun remap(m: Map<String, String?>): Map<String, String?> = m.mapKeys { e -> e.key }\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    let java = files
        .iter()
        .find(|(name, _)| name == "M.java")
        .map(|(_, code)| code.as_str())
        .expect("M.java");
    assert!(java.contains(".put("), "{java}");
    assert!(java.contains(".getValue()"), "{java}");
    assert!(java.contains("java.util.LinkedHashMap::new"), "{java}");
    assert!(java.contains("java.util.LinkedHashMap::putAll"), "{java}");
    assert!(!java.contains("Collectors.toMap"), "{java}");
}

/// Kotlin `Iterable.associateBy { k }` lowers to
/// `stream().collect(LinkedHashMap::new, ...)`; Kotlin Map has no
/// associateBy and Java none either — but the List/Iterable form does map.
#[test]
fn associate_by_collects_to_map() {
    let source = "package neutral.mapb\nfun tv(cs: List<String>): Map<Int, String> = cs.associateBy { it.length }\n";
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
        m.contains(".stream().collect(java.util.LinkedHashMap::new")
            && m.contains(".put(")
            && m.contains("java.util.LinkedHashMap::putAll")
            && !m.contains("Collectors.toMap"),
        "associateBy must lower to a null-safe ordered collector: {m}"
    );
    assert!(
        !m.contains(".associateBy("),
        "associateBy member must not survive: {m}"
    );
}

#[test]
fn associate_by_with_value_selector_uses_second_lambda() {
    let source = "package neutral.mapc\ndata class Entry(val key: String, val value: Int)\nfun tv(cs: List<Entry>): Map<String, Int> = cs.associateBy({ it.key }, { it.value })\nfun make() = Entry(\"x\", 1)\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "--allow-approximations", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    assert_eq!(errors, 0);
    let java = files
        .iter()
        .find(|(name, _)| name == "M.java")
        .map(|(_, code)| code.as_str())
        .expect("M.java");
    assert!(java.contains(".put("), "{java}");
    assert!(java.contains("getKey()"), "{java}");
    assert!(java.contains("getValue()"), "{java}");
    assert!(java.contains("new Entry(\"x\", 1)"), "{java}");
}

#[test]
fn filter_not_and_plus_element_lower_to_streams() {
    let source = "package neutral.mapd\nfun add(xs: List<String>, item: String): List<String> = xs.filterNot { it == item } + item\nfun remove(xs: List<String>, item: String): List<String> = xs.filterNot { it == item }\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "--allow-approximations", "M.kt"]);
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
