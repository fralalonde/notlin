use colored::Colorize;
use std::collections::{BTreeMap, BTreeSet};
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

/// Planning policy: informational differences do not weaken program semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApproximationClass {
    SemanticLoss,
    UnresolvedAssumption,
    Informational,
}

pub fn classify_approximation(message: &str) -> ApproximationClass {
    if message.contains("function modifier `infix`")
        || message.contains("nested Companion bridge that delegates")
        || message.contains("companion call on retained Kotlin decl")
        || (message.contains("companion `operator fun invoke`")
            && message.contains("factory call routed via"))
    {
        ApproximationClass::Informational
    } else if [
        "semantics lost",
        "eager",
        "lateinit",
        "unsigned",
        "not enforced",
        "dropped",
        "ignored",
        "lossy",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        ApproximationClass::SemanticLoss
    } else {
        ApproximationClass::UnresolvedAssumption
    }
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
    /// Set when the code must not depend on the message: a retention warning
    /// names the specific element it blocked on, but its code identifies the
    /// blocker TYPE, so it is the same code the run-end table row shows.
    pub code: Option<String>,
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
            crate::paths::display(&self.file),
            self.line,
            self.col,
        )
    }

    pub fn warning_code(&self) -> String {
        self.code
            .clone()
            .unwrap_or_else(|| warning_code(&self.message))
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
            code: None,
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
            code: None,
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
            code: None,
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
            code: None,
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

/// Every way a declaration can be held back — as a TYPE, not as a message.
///
/// The code identifies the blocker kind: it is derived from [`Self::summary`],
/// which never carries parameters, so one kind is one code and one summary row
/// no matter how many declarations or which elements it covers. [`Self::detail`]
/// is the parameterized text that names the specific element for the
/// per-declaration warning line and the `NOTLIN_RETENTION_SITES` dump.
///
/// Adding a variant is the only way to add a blocker type: a reason that
/// reaches the table as free text would hash to a code per parameter value,
/// which is exactly the fragmentation this enum removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RetentionKind {
    /// A Kotlin subtype sits outside the translation set, so the supertype
    /// cannot move without breaking it.
    SubtypeOutsideTranslationSet,
    /// The declaration is an interface with a retained Kotlin implementor.
    InterfaceSubtypeRetained,
    /// A property interface has a retained Kotlin implementation whose
    /// property cannot yet be rewritten to an explicit Java getter bridge.
    PropertyInterfaceBridge,
    /// A retained Kotlin declaration inherits from it.
    RetainedInheritor,
    /// A caller omits a defaulted constructor parameter and no delegating
    /// overload can supply it: the pattern collides with another constructor
    /// after erasure, the default expression names a parameter the pattern does
    /// not supply, or the call shape could not be read. A middle default nobody
    /// omits is NOT this blocker — Java is given it by inlining the literal or by
    /// an overload for the exact pattern.
    MiddleDefaultParameter,
    /// A retained Kotlin file references it by plain name.
    ReferencedFromRetainedKotlin,
    /// It narrows a nullable Kotlin property a Kotlin supertype declares.
    NullableNarrowing,
    /// It inherits a property interface that stays Kotlin.
    RetainedPropertyInterface,
    /// Retained Kotlin smart-casts one of its properties.
    SmartCast,
    /// One of its supertypes stays Kotlin.
    RetainedSupertype,
    /// A Kotlin supertype declares members with types Java cannot override
    /// exactly; erasing them would emit raw types, which JPA rejects.
    SupertypeMemberType,
}

impl RetentionKind {
    /// All kinds, for tests and for anything that needs to enumerate the
    /// vocabulary rather than discover it from a run.
    pub const ALL: [RetentionKind; 11] = [
        RetentionKind::SubtypeOutsideTranslationSet,
        RetentionKind::InterfaceSubtypeRetained,
        RetentionKind::PropertyInterfaceBridge,
        RetentionKind::RetainedInheritor,
        RetentionKind::MiddleDefaultParameter,
        RetentionKind::ReferencedFromRetainedKotlin,
        RetentionKind::NullableNarrowing,
        RetentionKind::RetainedPropertyInterface,
        RetentionKind::SmartCast,
        RetentionKind::RetainedSupertype,
        RetentionKind::SupertypeMemberType,
    ];

    /// The non-parameterized description: the summary row for this kind, and
    /// the text its code is derived from.
    pub fn summary(self) -> &'static str {
        match self {
            Self::SubtypeOutsideTranslationSet => "a Kotlin subtype is outside the translation set",
            Self::InterfaceSubtypeRetained => "an interface subtype is itself retained in Kotlin",
            Self::PropertyInterfaceBridge => {
                "a retained Kotlin property implementation cannot preserve the Java getter ABI"
            }
            Self::RetainedInheritor => "a retained Kotlin declaration inherits from it",
            Self::MiddleDefaultParameter => {
                "a caller omits a default argument that cannot be lowered to Java"
            }

            Self::ReferencedFromRetainedKotlin => "referenced by name from retained Kotlin",
            Self::NullableNarrowing => "narrows a nullable Kotlin property",
            Self::RetainedPropertyInterface => "inherits a retained Kotlin property interface",
            Self::SmartCast => "retained Kotlin smart-casts one of its properties",
            Self::RetainedSupertype => "one of its supertypes is retained in Kotlin",
            Self::SupertypeMemberType => {
                "a Kotlin supertype declares a member with a type Java cannot override exactly"
            }
        }
    }

    /// The parameterized text template: the same blocker type, naming the
    /// specific element this declaration was blocked on (the members in
    /// conflict, the supertype, ...). Kinds with nothing to name return their
    /// summary unchanged.
    pub fn detail(self, params: &[String]) -> String {
        match self {
            Self::SupertypeMemberType if !params.is_empty() => format!(
                "Java cannot override the inherited member type safely: {}; raw-type erasure would lose the generic contract",
                params.join(", ")
            ),
            Self::MiddleDefaultParameter if !params.is_empty() => format!(
                "no delegating overload can serve the default-argument omission: {}",
                params.join("; ")
            ),
            Self::NullableNarrowing if params.len() >= 3 => format!(
                "property {} narrows nullable {} to Java-incompatible {}",
                params[0], params[1], params[2]
            ),
            Self::PropertyInterfaceBridge if params.len() >= 2 => format!(
                "property interface cannot move to Java because {} cannot be bridged: {}",
                params[0], params[1]
            ),
            _ => self.summary().to_string(),
        }
    }

    /// True for kinds that can only fire because another declaration is
    /// retained: fallout nobody can fix directly. The live signal is a site's
    /// recorded `blockers`; this is the fallback for a site whose blockers
    /// could not be resolved, so it still cannot be read as human work.
    pub fn is_cascade(self) -> bool {
        matches!(
            self,
            Self::InterfaceSubtypeRetained
                | Self::PropertyInterfaceBridge
                | Self::RetainedInheritor
                | Self::ReferencedFromRetainedKotlin
                | Self::RetainedPropertyInterface
                | Self::RetainedSupertype
        )
    }

    /// The stable code for this blocker type — on the warning line, in the
    /// site dump, and in the summary row: one kind, one code.
    pub fn code(self) -> String {
        warning_code(&retention_message(self.summary()))
    }
}

/// Canonical message for a declaration held back because residual Kotlin needs
/// it. Shared by the diagnostic and the run-end retention table so both agree
/// on the reason text — and therefore on the derived N-code.
pub fn retention_message(reason: &str) -> String {
    format!("retained: {reason}")
}

/// Source-residue message for one retained declaration. The declaration is
/// already visible directly below the comment, so this names only the exact
/// intrinsic blocker or the other declarations immediately upstream. Every
/// cascade comment therefore points to the next marker a human should inspect.
pub fn retention_site_message(
    declaration: &str,
    kind: RetentionKind,
    params: &[String],
    blocker_labels: &[String],
) -> String {
    if blocker_labels.is_empty() {
        format!(
            "retained {declaration}; root cause: {}",
            kind.detail(params)
        )
    } else {
        format!(
            "retained {declaration}; blocked by retained {}; follow those declarations to their // NOTLIN root-cause markers",
            blocker_labels.join(", ")
        )
    }
}

pub type IntrinsicRetentionMarkers = BTreeMap<(String, usize), Vec<(String, String)>>;

/// Final location-qualified residue message for one recorded retention site.
/// This is called only after the emitting fixpoint pass has recorded every
/// site, so direct blockers can be resolved to files and cascades can name
/// their terminal root causes.
pub fn retention_source_message(
    file: &Path,
    line: usize,
    intrinsic_markers: &IntrinsicRetentionMarkers,
) -> Option<(String, String)> {
    let sites = retention_sites();
    let file = crate::paths::display(file);
    let site = sites
        .iter()
        .find(|site| site.file == file && site.line == line)?;
    if site.blockers.is_empty() {
        return Some((
            site.kind.code(),
            format!(
                "root cause [{}]: {}",
                site.kind.code(),
                site.kind.detail(&site.params)
            ),
        ));
    }

    let mut by_name: BTreeMap<&str, Vec<&RetentionSite>> = BTreeMap::new();
    for candidate in &sites {
        by_name
            .entry(candidate.name.as_str())
            .or_default()
            .push(candidate);
    }
    let mut direct = site
        .blockers
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let label = site
                .blocker_labels
                .get(index)
                .map(String::as_str)
                .unwrap_or(name);
            describe_blocker(name, label, &by_name)
        })
        .collect::<Vec<_>>();
    const MAX_DIRECT: usize = 4;
    let hidden_direct = direct.len().saturating_sub(MAX_DIRECT);
    direct.truncate(MAX_DIRECT);
    if hidden_direct > 0 {
        direct.push(format!("+{hidden_direct} more direct blocker(s)"));
    }
    let mut roots = BTreeSet::new();
    let mut visited = BTreeSet::new();
    collect_retention_roots(site, &by_name, intrinsic_markers, &mut visited, &mut roots);
    let mut roots = roots.into_iter().collect::<Vec<_>>();
    const MAX_ROOTS: usize = 3;
    let hidden = roots.len().saturating_sub(MAX_ROOTS);
    roots.truncate(MAX_ROOTS);
    let root_text = if roots.is_empty() {
        "root not uniquely resolvable from the simple-name graph; follow the direct blocker marker(s)"
            .to_string()
    } else {
        let suffix = if hidden == 0 {
            String::new()
        } else {
            format!(" (+{hidden} more root(s))")
        };
        format!("root(s): {}{suffix}", roots.join("; "))
    };
    Some((
        site.kind.code(),
        format!("blocked by {}; {}", direct.join(", "), root_text),
    ))
}

fn describe_blocker(
    name: &str,
    label: &str,
    by_name: &BTreeMap<&str, Vec<&RetentionSite>>,
) -> String {
    let Some(candidates) = by_name.get(name) else {
        return format!("{label}; inspect its // NOTLIN marker");
    };
    if candidates.len() == 1 {
        let site = candidates[0];
        return format!("{} at {}", site.declaration, site.file);
    }
    let locations = candidates
        .iter()
        .map(|site| site.file.clone())
        .collect::<Vec<_>>()
        .join(", ");
    format!("ambiguous declaration {name} at {locations}")
}

fn collect_retention_roots<'a>(
    site: &'a RetentionSite,
    by_name: &BTreeMap<&str, Vec<&'a RetentionSite>>,
    intrinsic_markers: &IntrinsicRetentionMarkers,
    visited: &mut BTreeSet<(String, usize)>,
    roots: &mut BTreeSet<String>,
) {
    if !visited.insert((site.file.clone(), site.line)) {
        return;
    }
    if let Some(markers) = intrinsic_markers.get(&(site.file.clone(), site.line)) {
        for (code, message) in markers {
            roots.insert(format!(
                "{} at {} [{}]: {}",
                site.declaration, site.file, code, message
            ));
        }
    }
    if site.blockers.is_empty() {
        roots.insert(format!(
            "{} at {} [{}]: {}",
            site.declaration,
            site.file,
            site.kind.code(),
            site.kind.detail(&site.params)
        ));
        return;
    }
    for (index, blocker) in site.blockers.iter().enumerate() {
        let Some(candidates) = by_name.get(blocker.as_str()) else {
            let label = site
                .blocker_labels
                .get(index)
                .map(String::as_str)
                .unwrap_or(blocker);
            roots.insert(format!(
                "{label}; intrinsic blocker — inspect its // NOTLIN marker"
            ));
            continue;
        };
        for candidate in candidates {
            collect_retention_roots(candidate, by_name, intrinsic_markers, visited, roots);
        }
    }
}

/// The code a retention site prints — on its warning line AND in the run-end
/// table row that summarises it. Both must derive it from the same message,
/// or the code a human reads in the table greps to nothing in the log.
pub fn retention_code(kind: RetentionKind) -> String {
    kind.code()
}

/// A declaration that stayed Kotlin because residual Kotlin source still needs
/// it: a Kotlin named-argument call cannot target a Java constructor, a Kotlin
/// `val` cannot implement a Java-source getter, a Kotlin implementor cannot
/// implement a translated-away supertype's ABI. Each entry is work a human
/// accepts or resolves — hence the run-end table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RetentionSite {
    /// Which blocker TYPE held the declaration back. The code and the summary
    /// row come from here, so the vocabulary is fixed and one kind is one row.
    pub kind: RetentionKind,
    /// What the kind was instantiated with: the conflicting members, and
    /// anything else the parameterized text names. Empty for kinds that name
    /// nothing. Detail only — never part of the code or the summary row.
    pub params: Vec<String>,
    /// Source file the declaration lives in.
    pub file: String,
    /// 1-based declaration line.
    pub line: usize,
    /// Human-readable declaration identity (`class Worker`, `interface Hub`).
    pub declaration: String,
    /// Simple name of the declaration — the node the blame graph is built on.
    pub name: String,
    /// The retained declarations this one waits for. EMPTY means the blocker is
    /// intrinsic: a fact about the source that no other translation can clear,
    /// which is what makes its kind a root a human can act on. Non-empty means
    /// the declaration is fallout, and its reason is one a human cannot fix —
    /// only the roots behind these names can be.
    pub blockers: Vec<String>,
    /// Human-readable identities parallel to `blockers`; unlike the graph keys,
    /// these preserve whether each dependency is a class, interface, object,
    /// enum, record, or annotation.
    pub blocker_labels: Vec<String>,
}

/// One recorded site, before it is handed out as a [`RetentionSite`].
#[derive(Clone)]
struct Recorded {
    kind: RetentionKind,
    params: Vec<String>,
    declaration: String,
    name: String,
    blockers: Vec<String>,
    blocker_labels: Vec<String>,
}

/// Keyed by declaration site: a fixpoint reconsiders every declaration on every
/// pass, so entries must collapse to one row per declaration — keeping the LAST
/// kind, which is the one that survived to the end of the fixpoint.
static RETENTION: Mutex<BTreeMap<(String, usize), Recorded>> = Mutex::new(BTreeMap::new());

/// Record a declaration held back from translation: the blocker kind, the
/// parameters that name the specific element, and the retained declarations it
/// waits on (`blockers`).
pub fn record_retention(
    kind: RetentionKind,
    params: &[String],
    file: &Path,
    line: usize,
    declaration: &str,
    name: &str,
    blockers: &[(String, String)],
) {
    if let Ok(mut sites) = RETENTION.lock() {
        sites.insert(
            (crate::paths::display(file), line),
            Recorded {
                kind,
                params: params.to_vec(),
                declaration: declaration.to_string(),
                name: name.to_string(),
                blockers: blockers.iter().map(|(name, _)| name.clone()).collect(),
                blocker_labels: blockers.iter().map(|(_, label)| label.clone()).collect(),
            },
        );
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
                .map(|((file, line), recorded)| RetentionSite {
                    kind: recorded.kind,
                    params: recorded.params.clone(),
                    file: file.clone(),
                    line: *line,
                    declaration: recorded.declaration.clone(),
                    name: recorded.name.clone(),
                    blockers: recorded.blockers.clone(),
                    blocker_labels: recorded.blocker_labels.clone(),
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

    let mut grouped: BTreeMap<RetentionKind, BTreeMap<String, usize>> = BTreeMap::new();
    // The blame graph: which retained declarations each one waits on, plus how
    // many sites each name covers. Blockers MERGE on a name collision (the
    // retained set is name-keyed anyway), which can only make a declaration
    // harder to release — never easier — so the counts stay conservative.
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut sites_per_name: BTreeMap<String, usize> = BTreeMap::new();
    // Kinds with at least one site that has no blocker at all. A kind whose
    // every site has blockers is fallout, however its text reads.
    let mut intrinsic: BTreeSet<RetentionKind> = BTreeSet::new();
    for site in &sites {
        *grouped
            .entry(site.kind)
            .or_default()
            .entry(shorten_file(&site.file))
            .or_insert(0) += 1;
        *sites_per_name.entry(site.name.clone()).or_insert(0) += 1;
        graph
            .entry(site.name.clone())
            .or_default()
            .extend(site.blockers.iter().cloned());
        if site.blockers.is_empty() {
            intrinsic.insert(site.kind);
        }
    }

    struct Row {
        kind: RetentionKind,
        code: String,
        count: usize,
        cascade: bool,
        /// Translations that FOLLOW once this root kind is fixed, on top of its
        /// own row: every declaration whose blockers all sit inside this kind's
        /// shadow. `None` for fallout rows.
        blocking: Option<usize>,
        /// How many declarations sit downstream of this root kind at all — its
        /// shadow, whether or not another kind also holds part of it. `None`
        /// for fallout rows.
        holds: Option<usize>,
        files: Vec<(String, usize)>,
    }

    let mut rows: Vec<Row> = grouped
        .into_iter()
        .map(|(kind, files)| {
            let count = files.values().sum();
            let mut files: Vec<(String, usize)> = files.into_iter().collect();
            files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let cascade = !intrinsic.contains(&kind) || kind.is_cascade();
            Row {
                code: retention_code(kind),
                cascade,
                kind,
                count,
                blocking: None,
                holds: None,
                files,
            }
        })
        .collect();

    // What each ROOT kind holds back, two ways: `holds` is everything
    // downstream of it, `blocking` is what translates when ONLY that kind is
    // fixed — a declaration waiting on two roots follows from neither, and
    // crediting it to both would promise work the fix does not deliver.
    let mut names_by_kind: BTreeMap<RetentionKind, Vec<String>> = BTreeMap::new();
    for site in &sites {
        if site.blockers.is_empty() {
            names_by_kind
                .entry(site.kind)
                .or_default()
                .push(site.name.clone());
        }
    }
    for row in rows.iter_mut().filter(|row| !row.cascade) {
        let seeds = names_by_kind.remove(&row.kind).unwrap_or_default();
        let follows = downstream(&seeds, &graph, &sites_per_name, /*all_blockers=*/ true);
        let shadow = downstream(
            &seeds,
            &graph,
            &sites_per_name,
            /*all_blockers=*/ false,
        );
        row.blocking = Some(follows.saturating_sub(row.count));
        row.holds = Some(shadow.saturating_sub(row.count));
    }
    // Root blockers first — biggest gain at the top — then the fallout, which
    // nobody can fix directly.
    rows.sort_by(|a, b| {
        a.cascade
            .cmp(&b.cascade)
            .then(b.blocking.unwrap_or(0).cmp(&a.blocking.unwrap_or(0)))
            .then(b.holds.unwrap_or(0).cmp(&a.holds.unwrap_or(0)))
            .then(b.count.cmp(&a.count))
            .then(a.code.cmp(&b.code))
    });

    let primary: usize = rows.iter().filter(|r| !r.cascade).map(|r| r.count).sum();
    let cascade = sites.len() - primary;
    let reason_width = rows
        .iter()
        .map(|r| r.kind.summary().chars().count())
        .max()
        .unwrap_or(24)
        .clamp(24, 64);

    let mut out = String::new();
    out.push_str(&format!(
        "\nnotlin: kotlin kept — {} declaration(s), {} blocker type(s): {} need human input, {} follow a closed type hierarchy\n\n",
        sites.len(),
        rows.len(),
        primary,
        cascade
    ));
    out.push_str(&format!(
        "  {:<6}  {:>5}  {:>8}  {:>8}  {:<width$}  {}\n",
        "code",
        "root",
        "holds",
        "blocking",
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
        let column = |value: Option<usize>| match value {
            Some(value) => value.to_string(),
            None => "-".to_string(),
        };
        let holds = column(row.holds);
        let blocking = column(row.blocking);
        out.push_str(&format!(
            "  {:<6}  {:>5}  {:>8}  {:>8}  {:<width$}{marker}  {where_}\n",
            row.code,
            row.count,
            holds,
            blocking,
            shorten_reason(row.kind.summary(), reason_width),
            width = reason_width
        ));
    }
    if primary > 0 {
        out.push_str(&format!(
            "\n  {primary} declaration(s) are root blockers. `holds` = declarations left Kotlin behind that kind; `blocking` = how many of them follow from fixing that kind alone. What several roots share follows only when they are fixed together, so no single row can claim it.\n"
        ));
    }
    if std::env::var_os("NOTLIN_RETENTION_SITES").is_some() {
        out.push_str("\n  every site:\n");
        for site in &sites {
            let waits = if site.blockers.is_empty() {
                String::new()
            } else {
                format!("  waits on: {}", site.blockers.join(", "))
            };
            out.push_str(&format!(
                "    {}:{}  [{}]  {}: {}{}\n",
                site.file,
                site.line,
                site.kind.code(),
                site.name,
                retention_message(&site.kind.detail(&site.params)),
                waits
            ));
        }
    }
    Some(out)
}

/// Declarations downstream of `seeds` through the blame edges, started from
/// `seeds` and grown until nothing follows. With `all_blockers`, a declaration
/// follows only when EVERY declaration it waits on has (what fixing `seeds`
/// alone translates); without, when ANY has (the shadow the reason casts, even
/// where another reason holds part of it). Returns the number of retained SITES
/// reached — a simple name can cover more than one declaration.
fn downstream(
    seeds: &[String],
    graph: &BTreeMap<String, BTreeSet<String>>,
    sites_per_name: &BTreeMap<String, usize>,
    all_blockers: bool,
) -> usize {
    let mut reached: BTreeSet<&str> = seeds.iter().map(String::as_str).collect();
    loop {
        let mut grew = false;
        for (name, blockers) in graph {
            if blockers.is_empty() || reached.contains(name.as_str()) {
                continue;
            }
            let follows = if all_blockers {
                blockers
                    .iter()
                    .all(|blocker| reached.contains(blocker.as_str()))
            } else {
                blockers
                    .iter()
                    .any(|blocker| reached.contains(blocker.as_str()))
            };
            if follows {
                reached.insert(name.as_str());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    reached
        .iter()
        .map(|name| sites_per_name.get(*name).copied().unwrap_or(0))
        .sum()
}
