//! The run-end retention table: what had to stay Kotlin, why, how many, and
//! where. This is the report a human works from, so its grouping, ordering and
//! N-codes are pinned here.

use notlin::diagnostics::{clear_retention, record_retention, retention_report};
use std::path::Path;

const MIDDLE_DEFAULT: &str =
    "its default-argument constructor leaves a middle parameter defaulted, which Java cannot express";
const NULLABLE_NARROWING: &str = "it narrows a nullable Kotlin property";
const INTERFACE_SUBTYPE: &str = "an interface subtype is itself retained in Kotlin";
const RETAINED_SUPERTYPE: &str = "one of its supertypes is retained in Kotlin";

#[test]
fn report_groups_by_reason_with_counts_and_locations() {
    clear_retention();

    // Two declarations in one file, one in another: the `where` column has to
    // aggregate per file and rank the file with more hits first.
    record_retention(
        MIDDLE_DEFAULT,
        Path::new("app-core/src/main/java/neutral/app/dto.kt"),
        10,
    );
    record_retention(
        MIDDLE_DEFAULT,
        Path::new("app-core/src/main/java/neutral/app/dto.kt"),
        24,
    );
    record_retention(
        MIDDLE_DEFAULT,
        Path::new("app-core/src/main/java/neutral/app/model.kt"),
        577,
    );
    record_retention(
        NULLABLE_NARROWING,
        Path::new("app-config/src/main/java/neutral/config/views.kt"),
        176,
    );
    record_retention(
        INTERFACE_SUBTYPE,
        Path::new("app-core/src/main/java/neutral/app/model.kt"),
        100,
    );
    record_retention(
        RETAINED_SUPERTYPE,
        Path::new("app-core/src/main/java/neutral/app/model.kt"),
        200,
    );

    let report = retention_report().expect("a report once sites are recorded");

    // Headline: totals split into human-actionable and cascade fallout.
    assert!(report.contains("6 declaration(s)"), "{report}");
    assert!(report.contains("4 need human input"), "{report}");
    assert!(report.contains("2 follow a closed type hierarchy"), "{report}");

    // Reason codes are derived from the message, exactly as the diagnostics do.
    assert!(report.contains("N6101"), "middle-defaulted code: {report}");
    assert!(report.contains("N5FEA"), "interface-subtype code: {report}");
    assert!(report.contains("NAB90"), "retained-supertype code: {report}");

    // Counts and locations: `module: dir/file.kt (hits)`.
    assert!(report.contains("app-core: app/dto.kt (2)"), "{report}");
    assert!(report.contains("app-config: config/views.kt"), "{report}");

    // Root reasons come before cascade ones, so the human-actionable rows are
    // the first thing read. Rows are matched by code here: a long reason is
    // elided to keep the table a table.
    let primary = report.find("N6101").expect("middle-defaulted row");
    let cascade = report.find("N5FEA").expect("cascade row");
    assert!(primary < cascade, "roots must precede cascade:\n{report}");
    assert!(report.contains("(cascade)"), "{report}");
}

#[test]
fn no_report_without_retention() {
    clear_retention();
    assert!(retention_report().is_none());
}
