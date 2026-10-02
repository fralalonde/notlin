use crate::cli::{Annotations, Cli, UntranslatableMode};
use crate::diagnostics::{Diagnostics, FileCoverage};
use crate::transpiler::unit::Unit;
use crate::workspace::SourceIndex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub mod expr;
pub mod fixpoint;
pub mod java;
pub mod kt;
pub mod stmt;
pub mod types;
pub mod unit;

static PROFILE_ENABLED: OnceLock<bool> = OnceLock::new();
static PROFILE_PARSE_NS: AtomicU64 = AtomicU64::new(0);
static PROFILE_UNIT_NS: AtomicU64 = AtomicU64::new(0);
static PROFILE_DIAGNOSTICS_NS: AtomicU64 = AtomicU64::new(0);
static PROFILE_JAVA_OUTPUT_NS: AtomicU64 = AtomicU64::new(0);
static PROFILE_TRANSLATIONS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default)]
pub struct TranslationProfile {
    pub parses: Duration,
    pub units: Duration,
    pub diagnostics: Duration,
    pub java_output: Duration,
    pub translations: u64,
}

fn profile_enabled() -> bool {
    *PROFILE_ENABLED.get_or_init(|| std::env::var_os("NOTLIN_PROFILE").is_some())
}

fn record_profile(total: &AtomicU64, elapsed: Duration) {
    total.fetch_add(
        elapsed.as_nanos().try_into().unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

pub(crate) fn record_java_output_profile(elapsed: Duration) {
    if profile_enabled() {
        record_profile(&PROFILE_JAVA_OUTPUT_NS, elapsed);
    }
}

/// Aggregate profile for all per-file translations in this process. Populated
/// only when `NOTLIN_PROFILE=1`; normal translations take no timing samples.
pub fn translation_profile() -> TranslationProfile {
    TranslationProfile {
        parses: Duration::from_nanos(PROFILE_PARSE_NS.load(Ordering::Relaxed)),
        units: Duration::from_nanos(PROFILE_UNIT_NS.load(Ordering::Relaxed)),
        diagnostics: Duration::from_nanos(PROFILE_DIAGNOSTICS_NS.load(Ordering::Relaxed)),
        java_output: Duration::from_nanos(PROFILE_JAVA_OUTPUT_NS.load(Ordering::Relaxed)),
        translations: PROFILE_TRANSLATIONS.load(Ordering::Relaxed),
    }
}

/// Pretty-print the raw tree-sitter parse tree (debug aid).
pub fn dump_ast(source: &str) -> String {
    let tree = parse_tree(source);
    let mut out = String::new();
    out.push_str(&format!("{} (named)\n", tree.root_node().kind()));
    render_node(tree.root_node(), source, 1, &mut out);
    out
}

pub(crate) fn parse_tree(source: &str) -> tree_sitter::Tree {
    let profile = profile_enabled();
    let started = Instant::now();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .expect("failed to load kotlin grammar");
    let tree = parser.parse(source, None).expect("parse failed");
    if profile {
        record_profile(&PROFILE_PARSE_NS, started.elapsed());
    }
    tree
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

/// Workspace facts a translation needs beyond the file on disk. They always
/// travel together: the index that answers cross-file questions, the roots
/// the user selected, the retained set produced by the retention fixpoint
/// (probe passes only) and the indexed path of this file, which differs from
/// its disk path when the file came out of the workspace index.
#[derive(Clone, Copy, Default)]
pub struct WorkspaceScope<'a> {
    pub index: Option<&'a SourceIndex>,
    pub roots: &'a [PathBuf],
    pub retained_hint: Option<&'a std::collections::HashSet<String>>,
    pub indexed_path: Option<&'a Path>,
}

/// Transpile one file with a source-level workspace index and selected roots.
pub fn transpile_with_workspace(
    source: &str,
    file: &Path,
    cli: &Cli,
    workspace: Option<&SourceIndex>,
    translation_roots: &[PathBuf],
) -> (Vec<(String, String)>, usize, usize, FileCoverage) {
    transpile_with_workspace_hint(
        source,
        file,
        cli,
        WorkspaceScope {
            index: workspace,
            roots: translation_roots,
            ..Default::default()
        },
        /*silent=*/ true,
    )
}

/// Probe variant used by the retention fixpoint: `retained_hint` feeds the
/// subtype rule (`Some` = fixpoint pass, `None` = conservative catch-all);
/// `silent` suppresses console diagnostic printing for probe passes (the
/// final pass prints its own diagnostics once).
pub fn transpile_with_workspace_hint(
    source: &str,
    file: &Path,
    cli: &Cli,
    scope: WorkspaceScope<'_>,
    silent: bool,
) -> (Vec<(String, String)>, usize, usize, FileCoverage) {
    let tree = parse_tree(source);
    transpile_with_tree_hint(source, &tree, file, cli, scope, silent)
}

/// Translate one Kotlin source with a tree-sitter tree parsed by the caller.
/// Workspace fixpointing can reuse the same immutable tree across multiple
/// retention probes because neither parsing nor translation mutates it.
pub fn transpile_with_tree_hint(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    cli: &Cli,
    scope: WorkspaceScope<'_>,
    silent: bool,
) -> (Vec<(String, String)>, usize, usize, FileCoverage) {
    transpile_with_tree_hint_selection(source, tree, file, cli, scope, silent)
}

pub(crate) fn transpile_with_tree_hint_selection(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    cli: &Cli,
    scope: WorkspaceScope<'_>,
    silent: bool,
) -> (Vec<(String, String)>, usize, usize, FileCoverage) {
    let workspace = scope.index;
    let translation_roots = scope.roots;
    let retained_hint = scope.retained_hint;
    let workspace_file = scope.indexed_path;
    let profile = profile_enabled();
    let mut diags = Diagnostics::new();
    let annots = match cli.annotations {
        Annotations::Jetbrains => types::AnnotationSet::Jetbrains,
        Annotations::Jspecify => types::AnnotationSet::Jspecify,
        Annotations::None => types::AnnotationSet::None,
    };
    let untranslatable_as_error = matches!(cli.untranslatable, UntranslatableMode::Error);

    let started = Instant::now();
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
                in_place: cli.migrates_in_place(),
            },
        )
        .with_workspace_selection(workspace, translation_roots, workspace_file);
        unit.retained_hint = retained_hint;
        let java_files = unit.run(tree.root_node());
        let mut coverage = std::mem::take(&mut unit.coverage);
        let approx = std::mem::take(&mut coverage.diags_approx);
        (java_files, approx, coverage)
    };
    if profile {
        record_profile(&PROFILE_UNIT_NS, started.elapsed());
    }

    let started = Instant::now();
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
            code: None,
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
    if !silent {
        diags.print();
    }
    if profile {
        record_profile(&PROFILE_DIAGNOSTICS_NS, started.elapsed());
        PROFILE_TRANSLATIONS.fetch_add(1, Ordering::Relaxed);
    }
    (java_files, errors, warnings, coverage)
}
