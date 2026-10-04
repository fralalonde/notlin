//! The run-end retention table: what had to stay Kotlin, why, how many, and
//! where. This is the report a human works from, so its grouping, ordering and
//! N-codes are pinned here.

use notlin::diagnostics::{
    RetentionKind,
    RetentionKind::{
        InterfaceSubtypeRetained, MiddleDefaultParameter, NullableNarrowing, RetainedSupertype,
        SupertypeMemberType,
    },
    clear_retention, record_retention, retention_code, retention_message, retention_report,
    retention_site_message, retention_source_message, warning_code,
};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// The retention table is process-global and cargo runs the tests of one binary
/// on parallel threads: every test here takes this lock, so one test's sites
/// cannot show up in another's counts (the `where` column and the blocking
/// numbers are exact, not merely present).
fn lock_table() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record a site through the shared signature: blocker kind, the parameters
/// that instantiate its template, file, line, declaration name, and the
/// retained declarations it waits on.
fn record(kind: RetentionKind, file: &str, line: usize, name: &str, blockers: &[&str]) {
    record_params(kind, &[], file, line, name, blockers);
}

fn record_params(
    kind: RetentionKind,
    params: &[&str],
    file: &str,
    line: usize,
    name: &str,
    blockers: &[&str],
) {
    let params: Vec<String> = params.iter().map(|value| value.to_string()).collect();
    let blockers = blockers
        .iter()
        .map(|blocker| (blocker.to_string(), format!("declaration {blocker}")))
        .collect::<Vec<_>>();
    record_retention(
        kind,
        &params,
        Path::new(file),
        line,
        &format!("declaration {name}"),
        name,
        &blockers,
    );
}

fn row_for<'a>(report: &'a str, code: &str) -> &'a str {
    report
        .lines()
        .find(|line| line.contains(code))
        .unwrap_or_else(|| panic!("{code} row in:\n{report}"))
}

/// The three count columns of a table row: how many declarations carry that
/// reason, how many sit downstream of it, and how many follow from fixing it
/// alone.
fn counts(row: &str) -> (usize, usize, usize) {
    let fields: Vec<&str> = row.split_whitespace().collect();
    (
        fields[1].parse().expect("root column"),
        fields[2].parse().expect("holds column"),
        fields[3].parse().expect("blocking column"),
    )
}

#[test]
fn report_groups_by_kind_with_counts_and_locations() {
    let _guard = lock_table();
    clear_retention();

    // Two declarations in one file, one in another: the `where` column has to
    // aggregate per file and rank the file with more hits first.
    for line in [10, 24] {
        record(
            MiddleDefaultParameter,
            "app-core/src/main/java/neutral/app/dto.kt",
            line,
            "DtoA",
            &[],
        );
    }
    record(
        MiddleDefaultParameter,
        "app-core/src/main/java/neutral/app/model.kt",
        577,
        "Model",
        &[],
    );
    record(
        NullableNarrowing,
        "app-config/src/main/java/neutral/config/views.kt",
        176,
        "View",
        &[],
    );
    // A retained subtype keeps the interface Kotlin, and that interface in turn
    // keeps its own supertype: fallout the table must not offer as human work.
    record(
        InterfaceSubtypeRetained,
        "app-core/src/main/java/neutral/app/model.kt",
        100,
        "Hub",
        &["Impl"],
    );
    record(
        RetainedSupertype,
        "app-core/src/main/java/neutral/app/model.kt",
        200,
        "Impl",
        &["Base"],
    );

    let report = retention_report().expect("a report once sites are recorded");

    // Headline: totals split into human-actionable and cascade fallout.
    assert!(report.contains("6 declaration(s)"), "{report}");
    assert!(report.contains("4 need human input"), "{report}");
    assert!(
        report.contains("2 follow a closed type hierarchy"),
        "{report}"
    );

    // The kind's code, and the row text a human reads, come from the same
    // description: one blocker type is one row, whatever it is blocked on. The
    // codes are pinned through `retention_code` rather than as literals because
    // they are derived from the kind's wording — rewording a kind is meant to
    // move its code, and a literal here would only pin the wording.
    let middle_default = retention_code(MiddleDefaultParameter);
    assert!(
        report.contains(&middle_default),
        "middle-defaulted code: {report}"
    );
    assert!(
        report.contains(&retention_code(InterfaceSubtypeRetained)),
        "interface-subtype code: {report}"
    );
    assert!(
        report.contains(&retention_code(RetainedSupertype)),
        "retained-supertype code: {report}"
    );
    // A long description is elided to keep the table a table, so the row is
    // matched on its opening words.
    let shown: String = MiddleDefaultParameter.summary().chars().take(40).collect();
    assert!(
        report.contains(&shown),
        "the row shows the kind's description: {report}"
    );

    // Counts and locations: `module: dir/file.kt (hits)`.
    assert!(report.contains("app-core: app/dto.kt (2)"), "{report}");
    assert!(report.contains("app-config: config/views.kt"), "{report}");

    // Root kinds come before cascade ones, so the human-actionable rows are
    // the first thing read. Rows are matched by code here: a long reason is
    // elided to keep the table a table.
    let primary = report.find(&middle_default).expect("middle-defaulted row");
    let cascade = report
        .find(&retention_code(InterfaceSubtypeRetained))
        .expect("cascade row");
    assert!(primary < cascade, "roots must precede cascade:\n{report}");
    assert!(report.contains("(cascade)"), "{report}");
}

#[test]
fn retained_source_comments_name_the_declaration_and_next_blocker() {
    let root = retention_site_message(
        "class Dto",
        MiddleDefaultParameter,
        &[
            "constructor Dto(owner, locale) cannot serve omission from method Repository.find()"
                .to_string(),
        ],
        &[],
    );
    assert_eq!(
        root,
        "retained class Dto; root cause: no delegating overload can serve the default-argument omission: constructor Dto(owner, locale) cannot serve omission from method Repository.find()"
    );

    let cascade = retention_site_message(
        "interface Hub",
        InterfaceSubtypeRetained,
        &[],
        &["class Worker".to_string(), "object Registry".to_string()],
    );
    assert_eq!(
        cascade,
        "retained interface Hub; blocked by retained class Worker, object Registry; follow those declarations to their // NOTLIN root-cause markers"
    );
}

#[test]
fn final_retention_comment_names_direct_site_and_terminal_root() {
    let _guard = lock_table();
    clear_retention();
    let no_params = Vec::new();
    let no_blockers = Vec::new();
    record_retention(
        MiddleDefaultParameter,
        &["method Repository.find() omits constructor parameter locale".to_string()],
        Path::new("module/src/Root.kt"),
        7,
        "class Root",
        "Root",
        &no_blockers,
    );
    record_retention(
        RetainedSupertype,
        &no_params,
        Path::new("module/src/Middle.kt"),
        11,
        "class Middle",
        "Middle",
        &[("Root".to_string(), "class Root".to_string())],
    );
    record_retention(
        InterfaceSubtypeRetained,
        &no_params,
        Path::new("module/src/Leaf.kt"),
        13,
        "interface Leaf",
        "Leaf",
        &[("Middle".to_string(), "class Middle".to_string())],
    );

    let (_, message) =
        retention_source_message(Path::new("module/src/Leaf.kt"), 13, &Default::default())
            .expect("the recorded leaf marker");
    assert!(
        message.contains("retained interface Leaf")
            && message.contains("blocked by class Middle at module/src/Middle.kt")
            && message.contains("class Root at module/src/Root.kt")
            && message.contains("method Repository.find()"),
        "{message}"
    );
}

#[test]
fn no_report_without_retention() {
    let _guard = lock_table();
    clear_retention();
    assert!(retention_report().is_none());
}

/// The code a table row shows has to be the code the warning lines print: the
/// table is only useful if a human can grep the log for the row they pick.
#[test]
fn table_codes_match_the_codes_the_warnings_print() {
    let _guard = lock_table();
    clear_retention();
    record(
        MiddleDefaultParameter,
        "app-core/src/main/java/neutral/app/dto.kt",
        10,
        "Dto",
        &[],
    );

    let report = retention_report().expect("a report once a site is recorded");
    let code = retention_code(MiddleDefaultParameter);
    assert!(report.contains(&code), "table row code: {report}");
    // The same code the warning lines carry: both derive from the kind, and the
    // warning appends the specific blocked element to it.
    assert_eq!(
        code,
        warning_code(&retention_message(MiddleDefaultParameter.summary()))
    );

    // Generating the code from the BARE description instead is the failure this
    // pins: the table would show a code no warning line ever prints.
    assert_ne!(
        code,
        warning_code(MiddleDefaultParameter.summary()),
        "a bare-reason code must not be the table code"
    );
}

/// The row a human should fix first is the one that unblocks the most work, and
/// the number beside it is that work — not just the row's own declarations.
#[test]
fn rows_are_ordered_by_what_they_hold_back() {
    let _guard = lock_table();
    clear_retention();

    // One root keeps a small hierarchy Kotlin (Base -> Mid -> Leaf); another
    // keeps a single declaration. The cascade rows carry blockers, so they are
    // fallout and rank under their root instead of beside it.
    record(
        MiddleDefaultParameter,
        "app-core/src/main/java/neutral/app/base.kt",
        5,
        "Base",
        &[],
    );
    record(
        RetainedSupertype,
        "app-core/src/main/java/neutral/app/mid.kt",
        6,
        "Mid",
        &["Base"],
    );
    record(
        RetainedSupertype,
        "app-core/src/main/java/neutral/app/leaf.kt",
        7,
        "Leaf",
        &["Mid"],
    );
    record(
        NullableNarrowing,
        "app-config/src/main/java/neutral/config/views.kt",
        8,
        "Solo",
        &[],
    );

    let report = retention_report().expect("a report once sites are recorded");

    // Two roots, two declarations that follow them.
    assert!(report.contains("2 need human input"), "{report}");
    assert!(
        report.contains("2 follow a closed type hierarchy"),
        "{report}"
    );

    let first = report
        .find(&retention_code(MiddleDefaultParameter))
        .expect("the root holding back the most");
    let second = report
        .find(&retention_code(NullableNarrowing))
        .expect("the smaller root");
    assert!(
        first < second,
        "the root holding back the most comes first:\n{report}"
    );

    // Base holds 2 declarations beyond its own (Mid, then Leaf through Mid) and
    // fixing it alone translates both; Solo holds none: nothing waits on it.
    let (base_root, base_holds, base_blocking) =
        counts(row_for(&report, &retention_code(MiddleDefaultParameter)));
    let (solo_root, solo_holds, solo_blocking) =
        counts(row_for(&report, &retention_code(NullableNarrowing)));
    assert_eq!(
        (base_root, base_holds, base_blocking),
        (1, 2, 2),
        "{report}"
    );
    assert_eq!(
        (solo_root, solo_holds, solo_blocking),
        (1, 0, 0),
        "{report}"
    );

    // Fallout rows report no holds/blocking of their own: their count is
    // already inside their root's number, and a human cannot fix them directly.
    for line in report
        .lines()
        .filter(|line| line.contains(&retention_code(RetainedSupertype)))
    {
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(
            (&fields[2], &fields[3]),
            (&"-", &"-"),
            "fallout row shows no holds/blocking of its own: {line}"
        );
    }
}

/// A declaration waiting on two roots is released only when both are fixed, so
/// neither row may claim it — otherwise a row's number promises work the fix
/// does not deliver.
#[test]
fn a_declaration_waiting_on_two_roots_is_credited_to_neither() {
    let _guard = lock_table();
    clear_retention();

    record(
        MiddleDefaultParameter,
        "app-core/src/main/java/neutral/app/base.kt",
        5,
        "Base",
        &[],
    );
    record(
        NullableNarrowing,
        "app-config/src/main/java/neutral/config/views.kt",
        8,
        "View",
        &[],
    );
    // Waits on both roots: fixing either one alone leaves it Kotlin.
    record(
        RetainedSupertype,
        "app-core/src/main/java/neutral/app/mid.kt",
        6,
        "Mid",
        &["Base", "View"],
    );
    // Waits on Mid alone, so it follows only when Mid does — which needs both.
    record(
        RetainedSupertype,
        "app-core/src/main/java/neutral/app/leaf.kt",
        7,
        "Leaf",
        &["Mid"],
    );

    let report = retention_report().expect("a report once sites are recorded");
    for code in [
        retention_code(MiddleDefaultParameter),
        retention_code(NullableNarrowing),
    ] {
        let (_, holds, blocking) = counts(row_for(&report, &code));
        assert_eq!(
            blocking, 0,
            "{code} must not claim a declaration another root also holds:\n{report}"
        );
        // The shadow is still reported: the declarations ARE downstream of both
        // roots, they just do not follow from either one alone.
        assert_eq!(holds, 2, "{code} shadow:\n{report}");
    }
}

/// Fallout is recognised by its blockers, not by how its reason reads: a reason
/// that sounds like an intrinsic fact but waits on another declaration is still
/// fallout, and must not be offered to a human as work.
#[test]
fn blockers_not_wording_decide_what_is_fallout() {
    let _guard = lock_table();
    clear_retention();

    record(
        MiddleDefaultParameter,
        "app-core/src/main/java/neutral/app/base.kt",
        5,
        "Base",
        &[],
    );
    record(
        NullableNarrowing,
        "app-config/src/main/java/neutral/config/views.kt",
        8,
        "View",
        &["Base"],
    );

    let report = retention_report().expect("a report once sites are recorded");
    assert!(
        report.contains("1 need human input"),
        "only the intrinsic row is human work:\n{report}"
    );
    assert!(
        report.contains("1 follow a closed type hierarchy"),
        "{report}"
    );
}

/// Reason codes are constant per blocker TYPE. One kind blocked on different
/// members has to stay ONE row with ONE code — that is what makes the table a
/// worklist instead of a per-declaration dump — while the code a warning prints
/// still names the specific element.
#[test]
fn one_kind_is_one_code_however_the_parameters_read() {
    let _guard = lock_table();
    clear_retention();

    record_params(
        SupertypeMemberType,
        &["items"],
        "app-core/src/main/java/neutral/app/model.kt",
        10,
        "ModelA",
        &[],
    );
    record_params(
        SupertypeMemberType,
        &["barcodes", "type"],
        "app-core/src/main/java/neutral/app/other.kt",
        20,
        "ModelB",
        &[],
    );

    let report = retention_report().expect("a report once sites are recorded");
    let code = retention_code(SupertypeMemberType);
    let rows: Vec<&str> = report.lines().filter(|line| line.contains(&code)).collect();
    assert_eq!(
        rows.len(),
        1,
        "both sites of one kind collapse into one row:\n{report}"
    );
    assert!(
        report.contains("2 need human input"),
        "both are human work:\n{report}"
    );

    // The parameters are not lost: the warning line and the per-site dump name
    // the members, through the kind's template.
    let detail = SupertypeMemberType.detail(&["barcodes".to_string(), "type".to_string()]);
    assert!(
        detail.starts_with("a Kotlin supertype declares barcodes, type"),
        "{detail}"
    );
    assert_ne!(
        detail,
        SupertypeMemberType.summary(),
        "the template is the parameterized text, the summary is not"
    );
    assert!(
        !SupertypeMemberType.summary().contains("barcodes"),
        "the summary carries no parameters: {}",
        SupertypeMemberType.summary()
    );
}

/// Every kind is a distinct blocker type with a distinct code, and a kind with
/// no parameters still has a template — its own description.
#[test]
fn every_kind_has_a_stable_code_of_its_own() {
    let mut codes: Vec<String> = Vec::new();
    for kind in RetentionKind::ALL {
        let code = retention_code(kind);
        assert_eq!(code, retention_code(kind), "stable across calls: {code}");
        assert_eq!(
            code,
            warning_code(&retention_message(kind.summary())),
            "the code identifies the kind, so it comes from its description"
        );
        // A kind with no parameters prints its description verbatim — which is
        // what the summary row already shows.
        assert_eq!(kind.detail(&[]), kind.summary(), "{kind:?}");
        assert!(!kind.summary().contains('{'), "{kind:?}");
        codes.push(code);
    }
    let unique: std::collections::BTreeSet<&String> = codes.iter().collect();
    assert_eq!(unique.len(), codes.len(), "codes collide: {codes:?}");
}
