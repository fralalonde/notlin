//! Entries discovered by `read_dir` can VANISH before the index scan opens
//! them (overlapping migrator, concurrent cleanup). The scan must skip them
//! with a warning and keep indexing — like the tolerant input read — instead
//! of aborting the whole run with `fatal: …: No such file or directory`.
//!
//! The window is between listing and opening, so the test makes it wide:
//! a corpus large enough that scanning takes hundreds of milliseconds, and
//! deletions that start the moment the process spawns (startup reliably
//! precedes scanning). It asserts the outcome, never the race itself: the
//! run succeeds, warns about vanished sources, and migrates the survivors.

use std::fs;
use std::process::Command;
use std::thread;

fn notlin() -> &'static str {
    env!("CARGO_BIN_EXE_notlin")
}

#[test]
fn index_scan_skips_sources_deleted_during_scanning() {
    let root = std::env::temp_dir().join(format!("notlin-vanish-scan-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let directories = 25;
    let per_directory = 40;
    for dir in 0..directories {
        let path = root.join(format!("pkg{dir}"));
        fs::create_dir_all(&path).unwrap();
        for i in 0..per_directory {
            fs::write(
                path.join(format!("Gen{i}.kt")),
                format!("package neutral.vanish\ndata class Gen{dir}_{i}(val n: Int)\n"),
            )
            .unwrap();
        }
    }

    let child = Command::new(notlin())
        .current_dir(&root)
        .args(["--root", ".", "--lombok", "--in-place", "."])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn notlin");

    // Delete through the whole startup+scan window: an overlapping migrator
    // does not pause between listing and opening either.
    for dir in 0..directories {
        for i in 0..(per_directory / 2) {
            let _ = fs::remove_file(root.join(format!("pkg{dir}")).join(format!("Gen{i}.kt")));
        }
        thread::sleep(std::time::Duration::from_millis(2));
    }

    let output = child.wait_with_output().expect("wait notlin");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "a source listed then deleted must not abort the run:\n{stderr}"
    );
    assert!(
        stderr.contains("vanished") && stderr.contains("skipped"),
        "the run must report skipped vanished sources:\n{stderr}"
    );
    // Survivors migrated: a file kept through the run has its Java output.
    let survivor = root.join("pkg24").join("Gen24_39.java");
    assert!(
        survivor.is_file(),
        "surviving sources must migrate:\n{stderr}"
    );
    // Vanished sources left no output referencing them.
    assert!(
        !root.join("pkg0").join("Gen0_0.java").exists(),
        "a vanished source must not gain an output"
    );
    fs::remove_dir_all(root).unwrap();
}
