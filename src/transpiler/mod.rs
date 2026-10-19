use crate::cli::{Annotations, Cli, UntranslatableMode};
use crate::diagnostics::{Diagnostics, FileCoverage};
use crate::transpiler::unit::Unit;
use crate::workspace::SourceIndex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

mod default_getter_plan;
pub mod expr;
pub mod fixpoint;
pub mod java;
pub mod kt;
pub(crate) mod retention_queries;
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
    let normalized = source_without_notlin_markers(source);
    let tree = parser.parse(&*normalized, None).expect("parse failed");
    if profile {
        record_profile(&PROFILE_PARSE_NS, started.elapsed());
    }
    tree
}

/// Residue markers are Notlin metadata, not Kotlin syntax. The Kotlin grammar
/// can bind a comment inserted between annotations as an `annotated_expression`
/// boundary, changing which annotations belong to the following declaration on
/// the next invocation. Blank marker bytes while preserving every byte offset
/// and newline so AST nodes still slice the original source correctly.
fn source_without_notlin_markers(source: &str) -> std::borrow::Cow<'_, str> {
    if !source
        .lines()
        .any(|line| line.trim_start().starts_with("// NOTLIN:"))
    {
        return std::borrow::Cow::Borrowed(source);
    }
    let mut bytes = source.as_bytes().to_vec();
    let mut start = 0usize;
    while start < bytes.len() {
        let end = bytes[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| start + offset)
            .unwrap_or(bytes.len());
        let line = &source[start..end];
        if line.trim_start().starts_with("// NOTLIN:") {
            for byte in &mut bytes[start..end] {
                if *byte != b'\r' {
                    *byte = b' ';
                }
            }
        }
        start = end.saturating_add(1);
    }
    std::borrow::Cow::Owned(String::from_utf8(bytes).expect("marker blanking preserves UTF-8"))
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
    pub retained_hint: Option<&'a std::collections::HashSet<crate::semantics::SymbolId>>,
    /// Cycle-pinned declarations are retained independently of eligibility.
    /// Their plan keeps the original candidate owner and records an explicit
    /// ownership-cycle reason.
    pub forced_retained: Option<&'a std::collections::BTreeMap<crate::semantics::SymbolId, usize>>,
    pub semantic_provider: Option<&'a dyn crate::semantics::SemanticProvider>,
    pub function_callsite_retention: Option<
        &'a std::collections::BTreeMap<
            crate::semantics::SymbolId,
            Vec<crate::function_callsite::CallsiteRepairDiagnostic>,
        >,
    >,
    pub top_level_property_callsite_retention:
        Option<&'a crate::top_level_property_callsite::RetentionMap>,
    pub indexed_path: Option<&'a Path>,
}

/// Explicit planning result. Preflight ownership is computed before lowering;
/// final ownership includes blockers discovered by the existing backend.
pub struct PlannedTranslation {
    pub plan: crate::translation_plan::TranslationPlan,
    /// Accepted, strictly parsed Java syntax. Candidate source from the unit
    /// lowerer is never emitted directly.
    pub accepted: crate::planning::AcceptedTranslationPlan,
    /// Declaration identities present in accepted Java output. Unlike planned
    /// ownership, this excludes declarations the lowerer ultimately retained.
    pub emitted_owners: Vec<crate::semantics::SymbolId>,
    /// Accepted source declaration identities grouped by generated Java path.
    pub emitted_file_owners: Vec<(String, Vec<crate::semantics::SymbolId>)>,
    pub java_files: Vec<(String, String)>,
    /// True when a prepared candidate could not be accepted as valid Java.
    pub emission_failed: bool,
    pub errors: usize,
    pub warnings: usize,
    pub coverage: FileCoverage,
}

impl PlannedTranslation {
    fn into_legacy_result(self) -> (Vec<(String, String)>, usize, usize, FileCoverage) {
        (
            self.accepted.emit(),
            self.errors,
            self.warnings,
            self.coverage,
        )
    }
}

/// Plan and lower a source snapshot without reparsing it on retention probes.
pub fn plan_with_tree_hint(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    cli: &Cli,
    scope: WorkspaceScope<'_>,
    silent: bool,
) -> PlannedTranslation {
    lower_with_plan(source, tree, file, cli, scope, silent, None)
}

/// Static semantic work for one immutable source snapshot and workspace
/// provider. Values are round-local; callers must discard them when the
/// source/index overlay or CLI approximation mode changes.
pub(crate) struct StaticPlanFacts {
    identity_path: PathBuf,
    source_hash: [u8; 32],
    allow_approximations: bool,
    plan: crate::translation_plan::TranslationPlan,
}

pub(crate) fn prepare_static_plan_facts(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    cli: &Cli,
    scope: WorkspaceScope<'_>,
) -> StaticPlanFacts {
    let identity_path = source_identity_path(source, file, scope);
    let provider = scope
        .semantic_provider
        .expect("static workspace facts require their round-local provider");
    StaticPlanFacts {
        identity_path: identity_path.to_path_buf(),
        source_hash: *blake3::hash(source.as_bytes()).as_bytes(),
        allow_approximations: cli.allow_approximations,
        plan: build_static_plan(source, tree, identity_path, cli, provider),
    }
}

pub(crate) fn plan_with_static_facts_hint(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    cli: &Cli,
    scope: WorkspaceScope<'_>,
    silent: bool,
    static_facts: &StaticPlanFacts,
) -> PlannedTranslation {
    lower_with_plan(source, tree, file, cli, scope, silent, Some(static_facts))
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
    lower_with_plan(source, tree, file, cli, scope, silent, None).into_legacy_result()
}

fn lower_with_plan(
    source: &str,
    tree: &tree_sitter::Tree,
    file: &Path,
    cli: &Cli,
    scope: WorkspaceScope<'_>,
    silent: bool,
    static_facts: Option<&StaticPlanFacts>,
) -> PlannedTranslation {
    let facts_started = profile_enabled().then(Instant::now);
    let identity_path = source_identity_path(source, file, scope);
    let local_provider = scope.semantic_provider.is_none().then(|| {
        let mut snapshots = vec![(identity_path.to_path_buf(), source.to_owned())];
        if let Some(index) = scope.index {
            snapshots.extend(
                index
                    .files
                    .iter()
                    .filter(|snapshot| snapshot.path != identity_path)
                    .map(|snapshot| (snapshot.path.clone(), snapshot.source_text().to_owned())),
            );
        }
        let mut provider = crate::semantics::SyntaxSemanticProvider::new(snapshots);
        if let Some(index) = scope.index {
            provider.enrich_workspace_type_facts(index);
        }
        provider
    });
    let semantic_provider: &dyn crate::semantics::SemanticProvider = scope
        .semantic_provider
        .or_else(|| {
            local_provider
                .as_ref()
                .map(|provider| provider as &dyn crate::semantics::SemanticProvider)
        })
        .expect("a local or workspace semantic provider is available");
    let mut plan = if let Some(static_facts) = static_facts {
        assert_eq!(static_facts.identity_path, identity_path);
        assert_eq!(
            static_facts.source_hash,
            *blake3::hash(source.as_bytes()).as_bytes(),
            "static semantic facts must match the exact source snapshot"
        );
        assert_eq!(static_facts.allow_approximations, cli.allow_approximations);
        static_facts.plan.clone()
    } else {
        build_static_plan(source, tree, identity_path, cli, semantic_provider)
    };
    let retained_with_forced = scope
        .forced_retained
        .filter(|forced| !forced.is_empty())
        .map(|forced| {
            let mut retained = scope.retained_hint.cloned().unwrap_or_default();
            retained.extend(forced.keys().cloned());
            retained
        });
    let retained_hint = retained_with_forced.as_ref().or(scope.retained_hint);
    crate::semantics::plan_required_property_references(
        source,
        tree,
        identity_path,
        semantic_provider,
        retained_hint.unwrap_or(&std::collections::HashSet::new()),
        &mut plan,
    );
    if let Some(started) = facts_started
        && started.elapsed().as_millis() >= 250
    {
        eprintln!(
            "NOTLIN_PROFILE semantic facts {}: {:.3}s",
            crate::paths::display(identity_path),
            started.elapsed().as_secs_f64()
        );
    }
    let local_callsite_retention = scope.function_callsite_retention.is_none().then(|| {
        let mut snapshots =
            std::collections::BTreeMap::from([(identity_path.to_path_buf(), source.to_owned())]);
        if let Some(index) = scope.index {
            snapshots.extend(
                index
                    .files
                    .iter()
                    .filter(|snapshot| snapshot.path != identity_path)
                    .map(|snapshot| (snapshot.path.clone(), snapshot.source_text().to_owned())),
            );
        }
        crate::function_callsite::functions_requiring_kotlin_retention(
            &snapshots,
            &semantic_provider
                .symbols()
                .iter()
                .map(|symbol| symbol.id.clone())
                .collect::<Vec<_>>(),
        )
    });
    let callsite_retention = scope
        .function_callsite_retention
        .or(local_callsite_retention.as_ref())
        .expect("callsite facts are available");
    let local_property_callsite_retention = scope
        .top_level_property_callsite_retention
        .is_none()
        .then(|| {
            let snapshots = if let Some(index) = scope.index {
                index
                    .files
                    .iter()
                    .map(|f| (f.path.clone(), f.source_text().to_owned()))
                    .collect()
            } else {
                std::collections::BTreeMap::from([(identity_path.to_path_buf(), source.to_owned())])
            };
            crate::top_level_property_callsite::properties_requiring_kotlin_retention_in_selection(
                &snapshots,
                &semantic_provider
                    .symbols()
                    .iter()
                    .map(|s| s.id.clone())
                    .collect::<Vec<_>>(),
                semantic_provider,
                &std::collections::HashSet::from([identity_path.to_path_buf()]),
            )
        });
    let property_callsite_retention = scope
        .top_level_property_callsite_retention
        .or(local_property_callsite_retention.as_ref())
        .expect("top-level property callsite facts are available");
    for decision in &mut plan.declarations {
        let Some(diagnostics) = property_callsite_retention.get(&decision.symbol_id) else {
            continue;
        };
        let reason = crate::translation_plan::RetentionReason::PreparationBlocker {
            kind: crate::translation_plan::PreparationBlockerKind::UnresolvedAssumption,
            message: diagnostics
                .first()
                .map(|d| d.message.clone())
                .unwrap_or_else(|| {
                    "top-level property call sites cannot be safely rewritten".into()
                }),
        };
        if !decision.retention_reasons.contains(&reason) {
            decision.retention_reasons.push(reason);
        }
        decision.candidate_owner = crate::translation_plan::BackendOwner::Kotlin;
    }
    let getter_started = profile_enabled().then(Instant::now);
    let default_provider_retention = scope.index.and_then(|index| {
        let source_file = index.source_file(identity_path)?;
        let empty_retained = std::collections::HashSet::new();
        let retained = retained_hint.unwrap_or(&empty_retained);
        let mut reasons = std::collections::HashMap::new();
        for indexed_declaration in &source_file.declarations {
            let reason = index.default_property_provider_needs_physical_getter(
                indexed_declaration,
                retained,
                scope.roots,
            );
            if let Some(reason) = reason {
                reasons.insert(
                    crate::semantics::workspace_symbol(index, indexed_declaration),
                    reason,
                );
            }
        }
        Some(reasons)
    });
    if let Some(started) = getter_started
        && started.elapsed().as_millis() >= 250
    {
        eprintln!(
            "NOTLIN_PROFILE getter contracts {}: {:.3}s",
            crate::paths::display(identity_path),
            started.elapsed().as_secs_f64()
        );
    }
    for declaration in &mut plan.declarations {
        if let Some(reasons) = callsite_retention.get(&declaration.symbol_id) {
            declaration.candidate_owner = crate::translation_plan::BackendOwner::Kotlin;
            for reason in reasons {
                declaration.retention_reasons.push(
                    crate::translation_plan::RetentionReason::PreparationBlocker {
                        kind: crate::translation_plan::PreparationBlockerKind::CompatibilityRule,
                        message: format!(
                            "{} (caller {} at byte {})",
                            reason.message,
                            reason.file.display(),
                            reason.start_byte
                        ),
                    },
                );
            }
        }
        if let Some(period) = scope
            .forced_retained
            .and_then(|forced| forced.get(&declaration.symbol_id))
        {
            declaration
                .retention_reasons
                .push(crate::translation_plan::RetentionReason::OwnershipCycle { period: *period });
        }
        if let Some(reason) = default_provider_retention
            .as_ref()
            .and_then(|reasons| reasons.get(&declaration.symbol_id))
        {
            declaration.candidate_owner = crate::translation_plan::BackendOwner::Kotlin;
            declaration.retention_reasons.push(
                crate::translation_plan::RetentionReason::PreparationBlocker {
                    kind: crate::translation_plan::PreparationBlockerKind::CompatibilityRule,
                    message: reason.clone(),
                },
            );
        }
    }
    if let Some(index) = scope.index {
        default_getter_plan::plan_default_getters(
            index,
            identity_path,
            retained_hint.unwrap_or(&std::collections::HashSet::new()),
            scope.roots,
            &mut plan,
        );
    }
    let effectful_file_initializer =
        crate::top_level_property_callsite::has_effectful_top_level_initializer(
            tree.root_node(),
            source,
        );
    let annotated_file_facade = tree
        .root_node()
        .named_children(&mut tree.root_node().walk())
        .any(|node| node.kind() == "file_annotation");
    if effectful_file_initializer || annotated_file_facade {
        let blockers = plan
            .declarations
            .iter()
            .filter(|decision| {
                decision.symbol_id.owner_path.is_empty()
                    && matches!(decision.symbol_id.kind.as_str(), "function" | "property")
                    && (decision.candidate_owner == crate::translation_plan::BackendOwner::Kotlin
                        || !decision.retention_reasons.is_empty()
                        || retained_hint.is_some_and(|retained| {
                            retention_queries::contains(retained, &decision.symbol_id)
                        }))
            })
            .map(|decision| decision.symbol_id.clone())
            .collect::<Vec<_>>();
        if !blockers.is_empty() {
            for decision in &mut plan.declarations {
                if !decision.symbol_id.owner_path.is_empty()
                    || !matches!(decision.symbol_id.kind.as_str(), "function" | "property")
                {
                    continue;
                }
                decision.candidate_owner = crate::translation_plan::BackendOwner::Kotlin;
                let (kind, message, dependency_label) = if effectful_file_initializer {
                    (
                        crate::translation_plan::PreparationBlockerKind::SemanticLoss,
                        "top-level declarations share an effectful initializer with retained Kotlin code; a partial move would change file initialization order or timing",
                        "file initialization",
                    )
                } else {
                    (
                        crate::translation_plan::PreparationBlockerKind::CompatibilityRule,
                        "file facade annotations require top-level declarations to share ownership with retained Kotlin members",
                        "annotated file facade",
                    )
                };
                let reason = crate::translation_plan::RetentionReason::PreparationBlocker {
                    kind,
                    message: message.into(),
                };
                if !decision.retention_reasons.contains(&reason) {
                    decision.retention_reasons.push(reason);
                }
                for blocker in &blockers {
                    if blocker == &decision.symbol_id {
                        continue;
                    }
                    let dependency = crate::translation_plan::SymbolDependency {
                        from: decision.symbol_id.clone(),
                        spelling: format!("{dependency_label}: {}", blocker.name),
                        resolution: crate::semantics::FactStatus::Established(blocker.clone()),
                    };
                    if !plan.dependencies.contains(&dependency) {
                        plan.dependencies.push(dependency);
                    }
                }
            }
        }
    }
    let workspace = scope.index;
    let translation_roots = scope.roots;
    let workspace_file = Some(identity_path);
    let profile = profile_enabled();
    let mut diags = Diagnostics::new();
    for diagnostic in plan
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == "A001")
    {
        let prefix = &source[..diagnostic.location.start_byte.min(source.len())];
        diags.push(crate::diagnostics::Diagnostic {
            severity: crate::diagnostics::Severity::Warning,
            kind: crate::diagnostics::DiagnosticKind::Approximated,
            message: diagnostic.message.clone(),
            file: file.to_path_buf(),
            line: prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
            col: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
            code: Some(diagnostic.code.clone()),
        });
    }
    let annots = match cli.annotations {
        Annotations::Jetbrains => types::AnnotationSet::Jetbrains,
        Annotations::Jspecify => types::AnnotationSet::Jspecify,
        Annotations::None => types::AnnotationSet::None,
    };
    let untranslatable_as_error = matches!(cli.untranslatable, UntranslatableMode::Error);

    let started = Instant::now();
    // Preparation stage: the existing lowerer produces candidate text and
    // coverage. Candidates do not become output until strict Java parsing
    // below accepts the complete set.
    let (candidate_java_files, approx_diags, mut coverage, type_facts) = {
        let mut unit = Unit::new(
            source,
            file,
            &mut diags,
            annots,
            crate::transpiler::unit::UnitOptions {
                untranslatable_as_error,
                lombok: cli.lombok,
                in_place: cli.migrates_in_place(),
                allow_approximations: cli.allow_approximations,
            },
        )
        .with_workspace_selection(workspace, translation_roots, workspace_file);
        unit.retained_hint = retained_hint;
        unit.semantic_provider = Some(semantic_provider);
        let java_files = unit.run_planned(tree.root_node(), &plan);
        let java_files = java_files
            .into_iter()
            .map(|(path, source)| crate::planning::PreparedJavaFile {
                owners: unit.output_owners.remove(&path).unwrap_or_default(),
                snapshot_hash: unit.source_hash,
                path,
                source,
            })
            .collect();
        let mut coverage = std::mem::take(&mut unit.coverage);
        let approx = std::mem::take(&mut coverage.diags_approx);
        let type_facts = std::mem::take(&mut unit.type_facts);
        (java_files, approx, coverage, type_facts)
    };
    if profile {
        record_profile(&PROFILE_UNIT_NS, started.elapsed());
        if started.elapsed().as_millis() >= 250 {
            eprintln!(
                "NOTLIN_PROFILE lowering {}: {:.3}s",
                crate::paths::display(identity_path),
                started.elapsed().as_secs_f64()
            );
        }
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
    if profile {
        record_profile(&PROFILE_DIAGNOSTICS_NS, started.elapsed());
    }
    plan.reconcile(source, &coverage)
        .expect("lowering uses the analyzed source snapshot");
    plan.type_facts.extend(type_facts);
    let acceptance_started = profile_enabled().then(Instant::now);
    let (accepted, emission_failed) = match crate::planning::AcceptedTranslationPlan::accept(
        plan.clone(),
        candidate_java_files,
        Some(semantic_provider),
    ) {
        Ok(accepted) => (accepted, false),
        Err(error) => {
            plan.diagnostics.push(crate::semantics::SemanticDiagnostic {
                code: "E001".into(),
                message: error.to_string(),
                location: crate::semantics::SourceLocation {
                    file: file.to_path_buf(),
                    snapshot_hash: plan.source_hash,
                    start_byte: 0,
                    end_byte: source.len(),
                },
            });
            diags.push(crate::diagnostics::Diagnostic {
                severity: crate::diagnostics::Severity::Error,
                kind: crate::diagnostics::DiagnosticKind::Parse,
                message: error.to_string(),
                file: file.to_path_buf(),
                line: 1,
                col: 1,
                code: None,
            });
            (
                crate::planning::AcceptedTranslationPlan::rejected(plan.clone()),
                true,
            )
        }
    };
    if emission_failed {
        coverage.translated_spans.clear();
        coverage.attached_comment_spans.clear();
    }
    let java_files = accepted.emit();
    let emitted_owners = accepted.emitted_owners().to_vec();
    let emitted_file_owners = accepted
        .files()
        .iter()
        .map(|file| (file.path.clone(), file.owners.clone()))
        .collect();
    let plan = accepted.translation().clone();
    if let Some(started) = acceptance_started
        && started.elapsed().as_millis() >= 250
    {
        eprintln!(
            "NOTLIN_PROFILE structured acceptance {}: {:.3}s",
            crate::paths::display(identity_path),
            started.elapsed().as_secs_f64()
        );
    }
    let (errors, warnings) = (diags.error_count(), diags.warning_count());
    if !silent {
        diags.print();
    }
    if profile {
        PROFILE_TRANSLATIONS.fetch_add(1, Ordering::Relaxed);
    }
    PlannedTranslation {
        plan,
        accepted,
        emitted_owners,
        emitted_file_owners,
        java_files,
        emission_failed,
        errors,
        warnings,
        coverage,
    }
}

fn source_identity_path<'a>(source: &str, file: &'a Path, scope: WorkspaceScope<'a>) -> &'a Path {
    scope
        .indexed_path
        .or_else(|| {
            scope.index.and_then(|index| {
                index
                    .source_file(file)
                    .map(|snapshot| snapshot.path.as_path())
                    .or_else(|| {
                        if file.is_absolute() {
                            return None;
                        }
                        let mut matching = index.files.iter().filter(|snapshot| {
                            snapshot.path.file_name() == file.file_name()
                                && snapshot.source_text() == source
                        });
                        let first = matching.next()?;
                        matching.next().is_none().then_some(first.path.as_path())
                    })
            })
        })
        .unwrap_or(file)
}

fn build_static_plan(
    source: &str,
    tree: &tree_sitter::Tree,
    identity_path: &Path,
    cli: &Cli,
    provider: &dyn crate::semantics::SemanticProvider,
) -> crate::translation_plan::TranslationPlan {
    let mut plan = crate::translation_plan::analyze(source, tree.root_node(), identity_path);
    plan.plan_type_losses(source, tree.root_node(), cli.allow_approximations);
    plan.plan_constructor_metadata_losses(identity_path, provider, cli.allow_approximations);
    plan.plan_jackson_default_constructor_losses(identity_path, provider, cli.allow_approximations);
    crate::semantics::plan_enum_when_lowerings(source, tree, identity_path, provider, &mut plan);
    crate::semantics::plan_todo_lowerings(source, tree, identity_path, provider, &mut plan);
    crate::semantics::plan_reference_string_calls(source, tree, identity_path, provider, &mut plan);
    crate::semantics::plan_container_calls(source, tree, identity_path, provider, &mut plan);
    crate::semantics::plan_reified_class_calls(source, tree, identity_path, provider, &mut plan);
    crate::semantics::plan_constructor_delegations(
        source,
        tree,
        identity_path,
        provider,
        &mut plan,
    );
    crate::semantics::plan_bottom_return_losses(
        source,
        tree,
        identity_path,
        provider,
        cli.allow_approximations,
        &mut plan,
    );
    crate::semantics::plan_required_calls(
        source,
        tree,
        identity_path,
        provider,
        cli.allow_approximations,
        &mut plan,
    );
    crate::semantics::plan_alias_type_losses(
        source,
        tree,
        provider,
        &mut plan,
        cli.allow_approximations,
    );
    crate::semantics::plan_builtin_unit_type_bindings(source, tree, provider, &mut plan);
    plan
}
