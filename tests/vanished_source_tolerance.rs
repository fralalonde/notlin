//! A source file listed by collect_inputs can VANISH between collection and
//! reading (overlapping notlin run, concurrent cleanup). The run must skip
//! it with a warning, keep migrating the remaining files, and exit 0.

use std::fs;
use std::process::Command;
use std::thread;
use std::time::Duration;

#[test]
fn source_missing_after_indexing_is_removed_from_the_migration_snapshot() {
    use clap::Parser;
    use notlin::{cli::Cli, migration_pipeline, workspace::SourceIndex};

    let root = std::env::temp_dir().join(format!("notlin-vanished-index-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let vanished = root.join("Vanished.kt");
    let survivor = root.join("Survivor.kt");
    fs::write(&vanished, "class Vanished\n").unwrap();
    fs::write(&survivor, "class Survivor\n").unwrap();

    let root = fs::canonicalize(&root).unwrap();
    let vanished = root.join("Vanished.kt");
    let survivor = root.join("Survivor.kt");
    let index = SourceIndex::discover(&root).unwrap();
    fs::remove_file(&vanished).unwrap();
    let cli = Cli::parse_from(["notlin", "--root", root.to_str().unwrap(), "--in-place"]);

    let plan = migration_pipeline::plan_workspace_migration(
        vec![(survivor.clone(), fs::read_to_string(&survivor).unwrap())],
        &cli,
        &index,
        std::slice::from_ref(&root),
        |_| {},
    )
    .expect("the remaining source should still be planned");

    assert!(
        migration_pipeline::verify_original_snapshots(&plan).is_ok(),
        "a source already gone before planning must not remain in the index snapshot"
    );
    assert!(
        plan.generated_java
            .iter()
            .all(|generated| generated.origin != vanished)
    );
    assert!(
        plan.generated_java
            .iter()
            .any(|generated| generated.origin == survivor),
        "the surviving source should still be migrated"
    );
    let _ = fs::remove_dir_all(root);
}

fn notlin() -> &'static str {
    env!("CARGO_BIN_EXE_notlin")
}

/// Reproduce, deterministically: extend the generated-file list to widen the
/// migration window, then delete one fixture through a helper after the run
/// has collected inputs (small sleep), mirroring an overlapping migrator.
#[test]
fn vanished_source_is_skipped_not_fatal() {
    let root = std::env::temp_dir().join(format!("notlin-vanished-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("one")).unwrap();
    fs::create_dir_all(root.join("two")).unwrap();
    for i in 0..40 {
        fs::write(
            root.join(format!("one/Gen{i}.kt")),
            format!("package neutral.vanished\ndata class Gen{i}(val n: Int)\n"),
        )
        .unwrap();
    }
    for i in 0..40 {
        fs::write(
            root.join(format!("two/Other{i}.kt")),
            format!("package neutral.vanished\ndata class Other{i}(val n: Int)\n"),
        )
        .unwrap();
    }

    let child = Command::new(notlin())
        .args([
            "--root",
            root.to_str().unwrap(),
            "--lombok",
            "--in-place",
            root.join("one").to_str().unwrap(),
            root.join("two").to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn notlin");
    // Give the child time to collect inputs, then remove one collected file.
    thread::sleep(Duration::from_millis(2));
    let _ = fs::remove_file(root.join("one/Gen0.kt"));
    let output = child.wait_with_output().expect("wait notlin");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "a vanished source must not abort the whole migration:\n{stderr}"
    );

    // The surviving files were still migrated.
    let survivor = root.join("two/Other1.java");
    assert!(survivor.exists(), "surviving file must migrate:\n{stderr}");
    // The vanished file stayed behind (it was already deleted, so nothing new).
    let _ = fs::remove_dir_all(root);
}
