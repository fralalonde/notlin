use std::fs;
use std::path::PathBuf;

/// Same member-call shape but with a `private val` backing field
/// (`private val contexts: Map<K, V>`): the index must still record the
/// property type so algebra member calls taint.
#[test]
fn map_plus_member_private_val_taints() {
    let root = std::env::temp_dir().join(format!("notlin-plusp-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let source = concat!(
        "package neutral.plusp\n",
        "class Hold private constructor(\n",
        "    private val contexts: Map<String, Int>\n",
        ") {\n",
        "    fun merged(o: Hold): Hold {\n",
        "        return Hold(this.contexts + o.contexts)\n",
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
        !m.contains(".plus("),
        "Map plus on a private-val receiver survived: {m}"
    );
    assert!(
        m.trim().is_empty() || errors > 0,
        "broken Java emitted: {m}"
    );
    let _ = fs::remove_dir_all(&root);
}
