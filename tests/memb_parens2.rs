use clap::Parser;
use notlin::transpiler;
use std::path::PathBuf;

fn emit(body: &str) -> (usize, Vec<String>) {
    let source = format!(
        "package neutral.dh\ndata class Hold(val contexts: Map<String, Int>) {{\n{}}}\n",
        body
    );
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "M.kt"]);
    let (files, errors, _warnings, _cov) =
        notlin::transpiler::transpile(&source, &PathBuf::from("M.kt"), &cli);
    (
        errors,
        files
            .into_iter()
            .map(|(n, c)| format!("{n}: {c}"))
            .collect(),
    )
}

#[test]
fn isolates_hashcode_drop() {
    let variants = [
        "override fun hashCode(): Int = contexts.hashCode()",
        "override fun toString(): String = \"Hold\"\nfun bucket(): Int = contexts.hashCode()",
        "override fun equals(other: Any?): Boolean = contexts.hashCode() == 0\nfun a(k: String): Boolean = contexts.containsKey(k)",
    ];
    for v in variants {
        let (errors, files) = emit(v);
        println!("VARIANT `{v}` errors={errors} files={}", files.len());
        for f in files {
            println!(
                "  {}",
                f.lines()
                    .find(|l| l.contains("hashCode") || l.contains("return"))
                    .unwrap_or("")
            );
        }
    }
}
