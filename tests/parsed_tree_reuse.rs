use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler::{transpile_with_tree_hint, transpile_with_workspace_hint};
use std::path::{Path, PathBuf};

#[test]
fn pre_parsed_tree_matches_fresh_translation() {
    let source = r#"
        package neutral.reuse

        data class Person(val name: String)

        fun greeting(person: Person): String = "Hello, ${person.name}"
    "#;
    let file = Path::new("tests/fixtures/parsed_tree_reuse.kt");
    let cli = Cli::parse_from(["notlin", file.to_str().unwrap()]);
    let roots: Vec<PathBuf> = Vec::new();

    let fresh = transpile_with_workspace_hint(source, file, &cli, None, &roots, None, true);

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let reused = transpile_with_tree_hint(source, &tree, file, &cli, None, &roots, None, true);

    assert_eq!(reused.0, fresh.0);
    assert_eq!(reused.1, fresh.1);
    assert_eq!(reused.2, fresh.2);
    assert_eq!(reused.3.translated, fresh.3.translated);
    assert_eq!(reused.3.untranslated, fresh.3.untranslated);
    assert_eq!(reused.3.translated_spans, fresh.3.translated_spans);
    assert_eq!(reused.3.diags_approx, fresh.3.diags_approx);
    assert_eq!(
        reused.3.attached_comment_spans,
        fresh.3.attached_comment_spans
    );
    assert_eq!(reused.3.blockers, fresh.3.blockers);
}
