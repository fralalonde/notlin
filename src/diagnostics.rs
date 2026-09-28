use colored::Colorize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        write!(f, "{}", s.bold())
    }
}

/// Where a diagnostic came from: the construct that triggered it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// Construct has no Java counterpart
    Untranslatable,
    /// Translated, but semantics may differ
    Approximated,
    /// Recoverable parse oddity
    Parse,
}

impl DiagnosticKind {
    pub fn code(&self) -> &'static str {
        match self {
            DiagnosticKind::Untranslatable => "N001",
            DiagnosticKind::Approximated => "N002",
            DiagnosticKind::Parse => "N003",
        }
    }
}

/// Stable code for one warning reason. Identical messages share a code;
/// different messages do not share the coarse N001/N002/N003 bucket.
pub fn warning_code(message: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in message.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("N{:04X}", hash & 0xFFFF)
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub kind: DiagnosticKind,
    pub message: String,
    pub file: PathBuf,
    pub line: usize,
    pub col: usize,
}

impl Diagnostic {
    pub fn render(&self) -> String {
        format!(
            "{}: {} [{}]\n  --> {}:{}:{}",
            self.severity,
            self.message,
            self.warning_code(),
            crate::paths::display(self.file),
            self.line,
            self.col,
        )
    }

    pub fn warning_code(&self) -> String {
        warning_code(&self.message)
    }
}

/// Accumulates diagnostics for a run; decides failure at the end.
#[derive(Debug, Default)]
pub struct Diagnostics {
    pub items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, d: Diagnostic) {
        log::debug!("diagnostic: {:?}", d);
        self.items.push(d);
    }

    #[allow(dead_code)]
    pub fn error(
        &mut self,
        kind: DiagnosticKind,
        node: &tree_sitter::Node,
        file: &Path,
        msg: impl Into<String>,
    ) {
        self.push(Diagnostic {
            severity: Severity::Error,
            kind,
            message: msg.into(),
            file: file.to_path_buf(),
            line: node.start_position().row + 1,
            col: node.start_position().column + 1,
        });
    }

    #[allow(dead_code)]
    pub fn warn(
        &mut self,
        kind: DiagnosticKind,
        node: &tree_sitter::Node,
        file: &Path,
        msg: impl Into<String>,
    ) {
        self.push(Diagnostic {
            severity: Severity::Warning,
            kind,
            message: msg.into(),
            file: file.to_path_buf(),
            line: node.start_position().row + 1,
            col: node.start_position().column + 1,
        });
    }

    pub fn error_count(&self) -> usize {
        self.items
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.items
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count()
    }

    pub fn print(&self) {
        for diagnostic in &self.items {
            eprintln!("{}", diagnostic.render());
        }
    }

    #[allow(dead_code)]
    pub fn has_errors(&self) -> bool {
        self.error_count() > 0
    }

    /// Warning-severity parse diagnostic (free function-style helper).
    pub fn warn_parse(&mut self, node: tree_sitter::Node, file: &Path, msg: impl Into<String>) {
        self.push(Diagnostic {
            severity: Severity::Warning,
            kind: DiagnosticKind::Parse,
            message: msg.into(),
            file: file.to_path_buf(),
            line: node.start_position().row + 1,
            col: node.start_position().column + 1,
        });
    }

    /// Warning-severity approximation diagnostic.
    pub fn warn_approx(&mut self, node: tree_sitter::Node, file: &Path, msg: impl Into<String>) {
        self.push(Diagnostic {
            severity: Severity::Warning,
            kind: DiagnosticKind::Approximated,
            message: msg.into(),
            file: file.to_path_buf(),
            line: node.start_position().row + 1,
            col: node.start_position().column + 1,
        });
    }
}

/// Per-file coverage accounting, driving the in-place migration policy.
#[derive(Debug, Default)]
pub struct FileCoverage {
    /// Top-level and member declarations successfully emitted to Java.
    pub translated: Vec<String>,
    /// Declarations that could not be translated (untranslatable constructs
    /// inside them, or no Java counterpart at all).
    pub untranslated: Vec<String>,
    /// Declaration source spans (byte ranges) that were translated — used to
    /// strip them from the .kt file in --in-place mode.
    pub translated_spans: Vec<(usize, usize)>,
    /// Byte ranges of comments attached to translated declarations, so the
    /// doc-comment travels with the code into the Java file conceptually.
    /// N002 approximations recorded during translation: (byte anchor, message,
    /// line, col). Flushed to console diagnostics by the caller after
    /// translation so ordering matches source position.
    pub diags_approx: Vec<(usize, String, usize, usize)>,
    pub attached_comment_spans: Vec<(usize, usize)>,
    /// Blocking diagnostics attached to untranslated deletions, mapped by
    /// insertion byte offset (start of the untranslated span): rendered
    /// `// NOTLIN: <CODE> <message>` stubs for --in-place residue.
    pub blockers: Vec<(usize, String)>,
}

impl FileCoverage {
    pub fn is_fully_translated(&self) -> bool {
        !self.translated_spans.is_empty() && self.untranslated.is_empty()
    }

    pub fn is_partially_translated(&self) -> bool {
        !self.translated_spans.is_empty() && !self.untranslated.is_empty()
    }
}

/// Canonical message for a declaration held back because residual Kotlin needs
/// it. Shared by the diagnostic and the run-end retention table so both agree
/// on the reason text — and therefore on the derived N-code.
pub fn retention_message(reason: &str) -> String {
    format!("workspace Kotlin implementation requires this declaration to remain Kotlin: {reason}")
}

/// A declaration that stayed Kotlin because residual Kotlin source still needs
/// it: a Kotlin named-argument call cannot target a Java constructor, a Kotlin
/// `val` cannot implement a Java-source getter, a Kotlin implementor cannot
/// implement a translated-away supertype's ABI. Each entry is work a human
/// accepts or resolves — hence the run-end table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RetentionSite {
    /// Why the declaration was held back (`kotlin_retention_reason` text).
    pub reason: &'static str,
    /// Source file the declaration lives in.
    pub file: String,
    /// 1-based declaration line.
    pub line: usize,
}

/// Secondary retention: the declaration stays Kotlin only because another one
/// does, via a mechanically closed type hierarchy. Reported separately so the
/// reasons a human can act on stay visible above the fallout.
const CASCADE_REASONS: [&str; 2] = [
    "one of its supertypes is retained in Kotlin",
    "an interface subtype is itself retained in Kotlin",
];

/// Keyed by declaration site: a fixpoint reconsiders every declaration on every
/// pass, so entries must collapse to one row per declaration — keeping the LAST
/// reason, which is the one that survived to the end of the fixpoint.
static RETENTION: Mutex<BTreeMap<(String, usize), &'static str>> = Mutex::new(BTreeMap::new());

/// Record a declaration held back from translation.
pub fn record_retention(reason: &'static str, file: &Path, line: usize) {
    if let Ok(mut sites) = RETENTION.lock() {
        sites.insert((crate::paths::display(file).to_string(), line), reason);
    }
}

/// Forget every recorded site (tests start from a clean table).
pub fn clear_retention() {
    if let Ok(mut sites) = RETENTION.lock() {
        sites.clear();
    }
}

/// Everything recorded so far, ordered by file then declaration.
pub fn retention_sites() -> Vec<RetentionSite> {
    RETENTION
        .lock()
        .map(|sites| {
            sites
                .iter()
                .map(|((file, line), reason)| RetentionSite {
                    reason,
                    file: file.clone(),
                    line: *line,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `module: dir/file.kt` — enough to find the file without the package prefix.
fn shorten_file(path: &str) -> String {
    // The stored form is already the display form (relative to the translation
    // root); this only drops the module's package path so the column stays a
    // table row instead of a path dump.
    let display = crate::paths::display(Path::new(path));
    let parts: Vec<&str> = display.split('/').filter(|part| !part.is_empty()).collect();
    match parts.len() {
        0 => display,
        1 => parts[0].to_string(),
        _ => {
            let tail = parts[parts.len() - 2..].join("/");
            if parts.len() > 2 {
                format!("{}: {}", parts[0], tail)
            } else {
                tail
            }
        }
    }
}

fn shorten_reason(reason: &str, width: usize) -> String {
    if reason.chars().count() <= width {
        return reason.to_string();
    }
    let mut out: String = reason.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Run-end retention table: what stayed Kotlin, why, how much of it, and where.
///
/// `None` when nothing was held back. Set `NOTLIN_RETENTION_SITES` to append
/// every individual site as `file:line`.
pub fn retention_report() -> Option<String> {
    let sites = retention_sites();
    if sites.is_empty() {
        return None;
    }

    let mut grouped: BTreeMap<&'static str, BTreeMap<String, usize>> = BTreeMap::new();
    for site in &sites {
        *grouped
            .entry(site.reason)
            .or_default()
            .entry(shorten_file(&site.file))
            .or_insert(0) += 1;
    }

    struct Row {
        code: String,
        reason: &'static str,
        count: usize,
        cascade: bool,
        files: Vec<(String, usize)>,
    }

    let mut rows: Vec<Row> = grouped
        .into_iter()
        .map(|(reason, files)| {
            let count = files.values().sum();
            let mut files: Vec<(String, usize)> = files.into_iter().collect();
            files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            Row {
                code: warning_code(&retention_message(reason)),
                cascade: CASCADE_REASONS.contains(&reason),
                reason,
                count,
                files,
            }
        })
        .collect();
    // Human-actionable reasons first, then the cascade; within each, biggest.
    rows.sort_by(|a, b| {
        a.cascade
            .cmp(&b.cascade)
            .then(b.count.cmp(&a.count))
            .then(a.code.cmp(&b.code))
    });

    let primary: usize = rows.iter().filter(|r| !r.cascade).map(|r| r.count).sum();
    let cascade = sites.len() - primary;
    let reason_width = rows
        .iter()
        .map(|r| r.reason.chars().count())
        .max()
        .unwrap_or(24)
        .clamp(24, 64);

    let mut out = String::new();
    out.push_str(&format!(
        "\nnotlin: kotlin kept — {} declaration(s), {} reason(s): {} need human input, {} follow a closed type hierarchy\n\n",
        sites.len(),
        rows.len(),
        primary,
        cascade
    ));
    out.push_str(&format!(
        "  {:<6}  {:>5}  {:<width$}  {}\n",
        "code",
        "count",
        "reason",
        "where",
        width = reason_width
    ));
    for row in &rows {
        let shown: Vec<String> = row
            .files
            .iter()
            .take(4)
            .map(|(label, count)| {
                if *count > 1 {
                    format!("{label} ({count})")
                } else {
                    label.clone()
                }
            })
            .collect();
        let hidden = row.files.len().saturating_sub(shown.len());
        let where_ = if hidden > 0 {
            format!("{}, (+{hidden} file(s))", shown.join(", "))
        } else {
            shown.join(", ")
        };
        let marker = if row.cascade { "  (cascade)" } else { "" };
        out.push_str(&format!(
            "  {:<6}  {:>5}  {:<width$}{marker}  {where_}\n",
            row.code,
            row.count,
            shorten_reason(row.reason, reason_width),
            width = reason_width
        ));
    }
    if primary > 0 {
        out.push_str(&format!(
            "\n  {primary} declaration(s) need a human decision; each one fixed releases its hierarchy.\n"
        ));
    }
    if std::env::var_os("NOTLIN_RETENTION_SITES").is_some() {
        out.push_str("\n  every site:\n");
        for site in &sites {
            out.push_str(&format!(
                "    {}:{}  {}\n",
                site.file,
                site.line,
                retention_message(site.reason)
            ));
        }
    }
    Some(out)
}
