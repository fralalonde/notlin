use colored::Colorize;
use std::fmt;
use std::path::{Path, PathBuf};

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
            self.kind.code(),
            self.file.display(),
            self.line,
            self.col,
        )
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
        for d in &self.items {
            eprintln!("{}", d.render());
        }
        let summary = format!(
            "{} error(s), {} warning(s)",
            self.error_count(),
            self.warning_count()
        );
        let summary = if self.error_count() > 0 {
            summary.red().to_string()
        } else if self.warning_count() > 0 {
            summary.yellow().to_string()
        } else {
            summary.green().to_string()
        };
        eprintln!("{}", summary);
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
