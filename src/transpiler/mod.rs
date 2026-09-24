use crate::cli::{Annotations, Cli, UntranslatableMode};
use crate::diagnostics::{Diagnostics, FileCoverage};
use crate::transpiler::unit::Unit;
use crate::workspace::SourceIndex;
use std::path::{Path, PathBuf};

pub mod expr;
pub mod java;
pub mod kt;
pub mod stmt;
pub mod types;
pub mod unit;

/// Pretty-print the raw tree-sitter parse tree (debug aid).
pub fn dump_ast(source: &str) -> String {
    let tree = parse(source);
    let mut out = String::new();
    out.push_str(&format!("{} (named)\n", tree.root_node().kind()));
    render_node(tree.root_node(), source, 1, &mut out);
    out
}

fn parse(source: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .expect("failed to load kotlin grammar");
    parser.parse(source, None).expect("parse failed")
}

fn render_node(node: tree_sitter::Node, source: &str, depth: usize, out: &mut String) {
    let mut cursor = node.walk();
    let mut go = cursor.goto_first_child();
    while go {
        let field = cursor.field_name().unwrap_or("");
        let child = cursor.node();
        let text = child.utf8_text(source.as_bytes()).unwrap_or("");
        let leaf = if child.child_count() == 0 && !text.is_empty() {
            format!(" {:?}", text)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "{}{}{}{}{}\n",
            "  ".repeat(depth),
            if field.is_empty() {
                String::new()
            } else {
                format!("{}: ", field)
            },
            child.kind(),
            if child.is_named() { " (named)" } else { "" },
            leaf,
        ));
        if child.child_count() > 0 {
            render_node(child, source, depth + 1, out);
        }
        go = cursor.goto_next_sibling();
    }
}

/// Transpile one file without workspace compatibility context.
pub fn transpile(
    source: &str,
    file: &Path,
    cli: &Cli,
) -> (Vec<(String, String)>, usize, usize, FileCoverage) {
    transpile_with_workspace(source, file, cli, None, &[])
}

/// Transpile one file with a source-level workspace index and selected roots.
pub fn transpile_with_workspace(
    source: &str,
    file: &Path,
    cli: &Cli,
    workspace: Option<&SourceIndex>,
    translation_roots: &[PathBuf],
) -> (Vec<(String, String)>, usize, usize, FileCoverage) {
    let tree = parse(source);
    let mut diags = Diagnostics::new();
    let annots = match cli.annotations {
        Annotations::Jetbrains => types::AnnotationSet::Jetbrains,
        Annotations::Jspecify => types::AnnotationSet::Jspecify,
        Annotations::None => types::AnnotationSet::None,
    };
    let untranslatable_as_error = matches!(cli.untranslatable, UntranslatableMode::Error);

    let (java_files, approx_diags, coverage) = {
        let mut unit = Unit::new(
            source,
            file,
            &mut diags,
            annots,
            crate::transpiler::unit::UnitOptions {
                untranslatable_as_error,
                lombok: cli.lombok,
                commons_lang: cli.commons_lang,
                in_place: cli.in_place,
            },
        )
        .with_workspace(workspace, translation_roots);
        let java_files = unit.run(tree.root_node());
        let mut coverage = std::mem::take(&mut unit.coverage);
        let approx = std::mem::take(&mut coverage.diags_approx);
        (java_files, approx, coverage)
    };

    let mut approx_diags = approx_diags;
    approx_diags.sort_by_key(|(off, _, _, _)| *off);
    for (_, msg, line, col) in approx_diags {
        diags.push(crate::diagnostics::Diagnostic {
            severity: crate::diagnostics::Severity::Warning,
            kind: crate::diagnostics::DiagnosticKind::Approximated,
            message: msg,
            file: file.to_path_buf(),
            line,
            col,
        });
    }
    if tree.root_node().has_error() {
        diags.warn_parse(
            tree.root_node(),
            file,
            "source contains syntax errors; output is best-effort",
        );
    }
    let (errors, warnings) = (diags.error_count(), diags.warning_count());
    diags.print();
    (java_files, errors, warnings, coverage)
}
