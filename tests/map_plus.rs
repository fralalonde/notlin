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
