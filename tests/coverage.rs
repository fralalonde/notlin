use clap::Parser;
use notlin::{
    cli::Cli, coverage::CoverageReport, migration_pipeline::plan_workspace_migration,
    workspace::SourceIndex,
};
use std::fs;
use std::io::Write;

#[test]
fn counts_original_owners_across_rounds_not_facades_or_members() {
    let root = std::env::temp_dir().join(format!("notlin-coverage-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let path = root.join("Example.kt");
    let source = "package neutral.coverage\nfun first(): Int = 1\nfun second(): Int = 2\nclass Box { fun member(): Int = 3 }\nsuspend fun deferred(): Int = 4\n";
    fs::write(&path, source).unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", root.to_str().unwrap()]);
    let plan = plan_workspace_migration(
        vec![(path.clone(), source.into())],
        &cli,
        &index,
        std::slice::from_ref(&root),
        |_| {},
    )
    .unwrap();
    let report = CoverageReport::collect(
        plan.translation_plans.values(),
        plan.generated_java
            .iter()
            .map(|output| (output.path.as_path(), output.owners.as_slice())),
    )
    .unwrap();
    assert_eq!(
        (report.original, report.translated, report.retained),
        (4, 3, 1)
    );
    assert_eq!(plan.generated_java.len(), 2, "two functions share a facade");
    assert_eq!(report.retained_by_reason.get("T001"), Some(&1));
    let retention = report.retention_summary();
    assert!(retention.contains("Kotlin retained — 1 of 4 original declarations"));
    assert!(retention.contains("T001 1"));
    assert!(retention.contains("categories may overlap"));
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        source,
        "planning must not apply migration"
    );
    let mut inconsistent = plan.translation_plans.values().next().unwrap().clone();
    let accepted = report
        .declarations
        .iter()
        .find(|owner| owner.translated)
        .unwrap();
    inconsistent
        .declarations
        .retain(|owner| owner.symbol_id != accepted.symbol);
    assert!(
        CoverageReport::collect(
            [&inconsistent],
            plan.generated_java
                .iter()
                .map(|output| (output.path.as_path(), output.owners.as_slice()))
        )
        .is_err()
    );
    let duplicate = plan.translation_plans.values().next().unwrap();
    assert!(CoverageReport::collect([duplicate, duplicate], std::iter::empty()).is_err());
    // Planned ownership without an accepted output never counts as translation.
    let no_output =
        CoverageReport::collect(plan.translation_plans.values(), std::iter::empty()).unwrap();
    assert_eq!((no_output.translated, no_output.retained), (0, 4));
    let mut repeated = duplicate.clone();
    repeated.declarations[0].retention_reasons.extend([
        notlin::translation_plan::RetentionReason::UnknownRequiredFact {
            name: "missing".into(),
        },
        notlin::translation_plan::RetentionReason::PreparationBlocker {
            kind: notlin::translation_plan::PreparationBlockerKind::CompatibilityRule,
            message: "generic test blocker".into(),
        },
        notlin::translation_plan::RetentionReason::UnknownRequiredFact {
            name: "missing".into(),
        },
    ]);
    let repeated = CoverageReport::collect([&repeated], std::iter::empty()).unwrap();
    assert_eq!(
        repeated.unresolved_symbols.get("missing"),
        Some(&1),
        "repeated calls must not inflate the owner count"
    );
    assert_eq!(repeated.retained_by_reason.get("P001"), Some(&1));
    assert!(repeated.retention_summary().contains("P001 1"));

    let translated_owner = report
        .declarations
        .iter()
        .find(|owner| owner.translated)
        .unwrap()
        .symbol
        .clone();
    let two_outputs = CoverageReport::collect(
        plan.translation_plans.values(),
        [
            (
                std::path::Path::new("One.java"),
                std::slice::from_ref(&translated_owner),
            ),
            (
                std::path::Path::new("Two.java"),
                std::slice::from_ref(&translated_owner),
            ),
        ],
    )
    .unwrap();
    assert_eq!(
        two_outputs.translated, 1,
        "owners count once across outputs"
    );
    assert_eq!(
        two_outputs
            .declarations
            .iter()
            .find(|owner| owner.symbol == translated_owner)
            .unwrap()
            .outputs
            .len(),
        2
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cli_reports_only_this_invocations_original_kotlin() {
    let root = std::env::temp_dir().join(format!("notlin-coverage-cli-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Example.kt"),
        "fun first(): Int = 1\nfun second(): Int = 2\nsuspend fun later(): Int = 3\n",
    )
    .unwrap();
    let report_path = root.join("coverage.json");
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
            .arg("--root")
            .arg(&root)
            .arg("--coverage-report")
            .arg(&report_path)
            .arg("--in-place")
            .arg(&root)
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        first_stderr.contains("notlin: Kotlin retained — 1 of 3 original declarations"),
        "default retention report should use exact original owner counts: {first_stderr}"
    );
    assert!(
        !first_stderr.contains("partial diagnostic-site hints"),
        "name-based diagnostic hints are opt-in: {first_stderr}"
    );
    let first: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(first["original"], 3);
    assert_eq!(first["translated"], 2);
    assert_eq!(first["retained"], 1);
    let first_java_outputs = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "java"))
        .map(|path| (path.clone(), fs::read(&path).unwrap()))
        .collect::<Vec<_>>();
    assert!(!first_java_outputs.is_empty());
    let second = run();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(second["original"], 1);
    assert_eq!(
        second["translated"], 0,
        "previous Java output is not fresh coverage"
    );
    assert_eq!(second["retained"], 1);
    for (path, expected_bytes) in first_java_outputs {
        assert_eq!(
            fs::read(&path).unwrap_or_else(|error| {
                panic!(
                    "previous generated output {} disappeared: {error}",
                    path.display()
                )
            }),
            expected_bytes,
            "a later in-place run with only retained Kotlin must preserve prior Java output byte-for-byte"
        );
    }
    let source_path = root.join("Example.kt");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&source_path)
        .unwrap()
        .write_all(b"\nfun third(): Int = 3\n")
        .unwrap();
    let third_run = run();
    assert!(
        third_run.status.success(),
        "{}",
        String::from_utf8_lossy(&third_run.stderr)
    );
    let third_report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(third_report["original"], 2);
    assert_eq!(third_report["translated"], 1);
    assert_eq!(third_report["retained"], 1);
    let facade = fs::read_to_string(root.join("Example.java")).unwrap();
    for function in ["first", "second", "third"] {
        assert!(
            facade.contains(&format!("{function}(")),
            "subsequent migration must preserve historical facade member {function}:\n{facade}"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn strict_veto_and_stdout_use_accepted_ownership() {
    let root = std::env::temp_dir().join(format!("notlin-coverage-strict-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let source = "fun first(): Int = 1\nsuspend fun later(): Int = 3\n";
    let input = root.join("Example.kt");
    fs::write(&input, source).unwrap();
    let report_path = root.join("coverage.json");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&root)
        .arg("--coverage-report")
        .arg(&report_path)
        .arg("--untranslatable=error")
        .arg("--in-place")
        .arg(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(&input).unwrap(), source);
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(report["original"], 2);
    assert_eq!(report["translated"], 0);
    assert_eq!(report["retained"], 2);

    fs::write(&input, "fun first(): Int = 1\nfun second(): Int = 2\n").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--coverage-report")
        .arg(&report_path)
        .arg(&input)
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(report["translated"], 2);
    assert_eq!(report["retained"], 0);
    assert!(!output.stdout.is_empty());
    assert!(input.exists(), "stdout generation must preserve source");
    fs::remove_dir_all(root).unwrap();
}
