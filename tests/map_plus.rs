use clap::Parser;
use std::path::PathBuf;

/// Kotlin `Map<A,B> + Map<A,B>` (Map.plus) has no Java member and cannot be
/// lowered soundly in expression position: the result requires a fresh
/// HashMap plus putAll, which is a statement-shape transform. The calling
/// declaration must taint instead of emitting a broken `+` expression.
#[test]
fn map_plus_operator_taints_caller() {
    let source = "package neutral.mapp\nfun wider(m: Map<String, Int>, n: Map<String, Int>): Map<String, Int> {\n    return m + n\n}\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    let m = files
        .iter()
        .map(|(_, c)| c.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !m.contains(".plus(") && !m.contains("m + n") && !m.contains("m+n"),
        "Map `+` lowered to a nonexistent Java member: {m}"
    );
    // Conservative outcome: the declaration either stays Kotlin (no output)
    // or surfaces a taint error — never a broken `.plus(...)` member.
    assert!(
        m.trim().is_empty() || errors > 0,
        "Map plus silently emitted broken Java: {m}"
    );
}

#[test]
fn map_literal_plus_map_taints_caller() {
    let source = "package neutral.mapp\nfun merge(other: Map<String, Int>): Map<String, Int> = mapOf(\"a\" to 1) + other\n";
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    let output = files
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !output.contains("Map.ofEntries") || !output.contains(" + other"),
        "map literal plus leaked invalid Java: {output}"
    );
    assert!(
        output.trim().is_empty() || errors > 0,
        "map literal plus silently emitted Java: {output}"
    );
}

#[test]
fn map_literal_plus_interface_property_taints_caller() {
    let source = concat!(
        "package neutral.mapp\n",
        "interface Defaults { companion object { val base: Map<String, Int> = mapOf() } }\n",
        "fun merge(): Map<String, Int> = mapOf(\"a\" to 1) + Defaults.base\n"
    );
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    let output = files
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !output.contains(".plus("),
        "map literal plus interface property leaked invalid Java: {output}"
    );
    assert!(
        output.trim().is_empty() || errors > 0,
        "map literal plus interface property silently emitted Java: {output}"
    );
}

/// `@get`-backed property access `h.getContexts() + other.getContexts()`
/// where the getter returns a Map must taint: the Java expression form for
/// Map concatenation does not exist.
#[test]
fn map_plus_via_property_getter_taints() {
    use std::fs;
    let root = std::env::temp_dir().join(format!("notlin-mapq-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let source = concat!(
        "package neutral.mapq\n",
        "data class Hold(val contexts: Map<String, Int>) {\n",
        "    fun wider(o: Hold): Map<String, Int> {\n",
        "        return contexts + o.contexts\n",
        "    }\n",
        "}\n"
    );
    fs::write(root.join("M.kt"), source).unwrap();
    let cli = clap::Parser::parse_from(vec![
        "notlin",
        "--root",
        root.to_string_lossy().as_ref(),
        "M.kt",
    ]);
    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let (files, errors, _warnings, _cov) = notlin::transpiler::transpile_with_workspace(
        source,
        &PathBuf::from("M.kt"),
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    let m = files
        .iter()
        .map(|(_, c)| c.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !m.contains("+ o.contexts()") && !m.contains(".plus("),
        "Map `+` lowered to a nonexistent Java member: {m}"
    );
    assert!(
        m.trim().is_empty() || errors > 0,
        "Map plus silently emitted broken Java: {m}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// Static companion Map properties have to retain their declared Map type
/// through workspace lookup. Without that context `First.BASE + Second.BASE`
/// was misclassified as a user operator and emitted as Java `.plus(...)`.
#[test]
fn map_plus_via_two_static_properties_taints() {
    use std::fs;
    let root = std::env::temp_dir().join(format!("notlin-map-static-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let source = concat!(
        "package neutral.mapstatic\n",
        "interface First { companion object { val BASE: Map<String, Int> = mapOf() } }\n",
        "interface Second { companion object { val BASE: Map<String, Int> = mapOf() } }\n",
        "fun merge(): Map<String, Int> = First.BASE + Second.BASE\n"
    );
    let path = root.join("M.kt");
    fs::write(&path, source).unwrap();
    let cli = clap::Parser::parse_from(vec![
        "notlin",
        "--root",
        root.to_string_lossy().as_ref(),
        "M.kt",
    ]);
    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let (files, _errors, _warnings, _cov) = notlin::transpiler::transpile_with_workspace(
        source,
        &PathBuf::from("M.kt"),
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    let output = files
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !output.contains(".plus("),
        "static Map plus leaked a nonexistent Java member: {output}"
    );
    assert!(
        !output.contains("Map<String, Integer> merge"),
        "unsupported static Map merge must not be emitted: {output}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// An unqualified uppercase companion property belongs to the enclosing
/// class. Its Map type must survive inference when it is mixed with another
/// class's static Map property.
#[test]
fn map_plus_via_current_class_static_property_taints() {
    let source = concat!(
        "package neutral.mapcurrent\n",
        "interface Other { companion object { val BASE: Map<String, Int> = mapOf() } }\n",
        "data class Holder(val properties: Map<String, Int> = BASE + Other.BASE) {\n",
        "    companion object { val BASE: Map<String, Int> = mapOf() }\n",
        "}\n"
    );
    let cli = notlin::cli::Cli::parse_from(["notlin", "M.kt"]);
    let (files, _errors, _warnings, _cov) =
        notlin::transpiler::transpile(source, &PathBuf::from("M.kt"), &cli);
    let output = files
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !output.contains(".plus("),
        "current-class static Map plus leaked a nonexistent Java member: {output}"
    );
    assert!(
        !output.contains("class Holder"),
        "unsupported static Map merge must not emit Holder: {output}"
    );
}
