//! Unit tests for the --in-place migration striper.

use notlin::diagnostics::FileCoverage;
use notlin::migrate;

#[test]
fn strip_removes_translated_spans() {
    let source = "class A {}\n\nclass B {}\n";
    let mut cov = FileCoverage::default();
    // A translates (0..10), B doesn't.
    // "class A {}" is bytes 0..10.
    cov.translated_spans.push((0, 10));
    let out = migrate::strip_translated(source, &cov);
    assert_eq!(out, "\nclass B {}\n");
}

#[test]
fn strip_swallows_trailing_newline() {
    let source = "val a = 1\nval b = 2\n";
    let mut cov = FileCoverage::default();
    // "val a = 1" is bytes 0..9
    cov.translated_spans.push((0, 9));
    let out = migrate::strip_translated(source, &cov);
    // The newline right after "val a = 1" must go with it.
    assert!(!out.starts_with('\n'), "got: {:?}", out);
    assert!(out.contains("val b = 2"));
}

#[test]
fn fully_translated_means_no_untranslated() {
    let cov = FileCoverage {
        translated: vec!["A".into()],
        untranslated: vec![],
        translated_spans: vec![(0, 10)],
        attached_comment_spans: vec![],
        blockers: vec![],
    };
    assert!(cov.is_fully_translated());
    assert!(!cov.is_partially_translated());
}

#[test]
fn taint_makes_partial() {
    let cov = FileCoverage {
        translated: vec![],
        untranslated: vec!["Bad".into()],
        translated_spans: vec![(5, 20)],
        attached_comment_spans: vec![],
        blockers: vec![],
    };
    assert!(cov.is_partially_translated());
    assert!(!cov.is_fully_translated());
}

#[test]
fn tidy_collapses_blank_runs() {
    let messy = "\n\n\nclass A {}\n\n\n\nclass B {}\n\n";
    let out = migrate::tidy(messy);
    assert_eq!(out, "class A {}\n\nclass B {}\n");
}

#[test]
fn migration_on_tmpdir_trim_and_delete() {
    let dir = std::env::temp_dir().join(format!("notlin-mig-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // fully-translatable file -> deleted
    let full = dir.join("Full.kt");
    std::fs::write(&full, "class Full(val a: Int)\n").unwrap();
    let cov_full = FileCoverage {
        translated_spans: vec![(0, 21)],
        ..Default::default()
    };
    let out = migrate::migrate(&full, "class Full(val a: Int)\n", &cov_full).unwrap();
    assert!(matches!(out, migrate::MigrateOutcome::Deleted));
    assert!(!full.exists());

    // partially-translated file -> trimmed, untranslatable part kept
    let partial = dir.join("Partial.kt");
    let src = "class Ok(val a: Int)\n\nvalue class Bad(val x: Int)\n";
    std::fs::write(&partial, src).unwrap();
    let cov_partial = FileCoverage {
        translated_spans: vec![(0, 20)], // class Ok(val a: Int)
        untranslated: vec!["Bad".into()],
        ..Default::default()
    };
    let out = migrate::migrate(&partial, src, &cov_partial).unwrap();
    match out {
        migrate::MigrateOutcome::Trimmed { remaining_bytes } => {
            let kept = std::fs::read_to_string(&partial).unwrap();
            assert!(kept.contains("value class Bad"));
            assert!(!kept.contains("class Ok"));
            assert!(remaining_bytes < src.len());
        }
        other => panic!("expected Trimmed, got {:?}", other),
    }

    std::fs::remove_dir_all(&dir).ok();
}
