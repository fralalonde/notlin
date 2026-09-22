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
        diags_approx: vec![],
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
        diags_approx: vec![],
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
fn blockers_anchor_above_their_element_not_segment_start() {
    // Two kept classes, each with a blocker anchored inside it. The second
    // blocker must land directly above ITS element's line, not at the head
    // of the kept segment (where the first class starts).
    let source = "class Good(val a: Int)\n\nclass Bad1 {\n    companion object Factory {\n        fun make(): Bad1 = Bad1()\n    }\n}\n\nclass Bad2 {\n    companion object Factory {\n        fun make(): Bad2 = Bad2()\n    }\n}\n";
    let mut cov = FileCoverage::default();
    cov.translated_spans.push((0, 19)); // "class Good(val a: Int)\n"
    // anchor mid-Bad1 and mid-Bad2 (the companion_object starts)
    let bad1_comp = source.find("companion object Factory").unwrap();
    let bad2_comp = source[bad1_comp + 1..]
        .find("companion object Factory")
        .unwrap()
        + bad1_comp
        + 1;
    cov.blockers
        .push((bad1_comp, "// NOTLIN: N001 bad1 blocker\n".to_string()));
    cov.blockers
        .push((bad2_comp, "// NOTLIN: N001 bad2 blocker\n".to_string()));
    let out = migrate::strip_translated(source, &cov);
    // Both classes kept, both comments directly above their own companion
    // object line (the anchored element), not above the class header and
    // not at the head of the kept segment.
    let comp_lines: Vec<usize> = out
        .lines()
        .enumerate()
        .filter(|(_, l)| l.trim() == "companion object Factory {")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(comp_lines.len(), 2);
    for (line_idx, marker) in comp_lines.iter().zip(["bad1 blocker", "bad2 blocker"]) {
        let above = out.lines().nth(line_idx - 1).unwrap();
        assert_eq!(
            above.trim(),
            format!("// NOTLIN: N001 {}", marker),
            "blocker not directly above its element"
        );
        assert!(
            above.starts_with("    "),
            "comment must keep element indentation"
        );
    }
    assert!(!out.starts_with("// NOTLIN:"));
}

#[test]
fn blocker_keeps_elements_indentation() {
    // A blocker anchored at an indented member must not eat the indent.
    let source = "class Ok(val a: Int)\n\nclass Deep {\n    companion object Factory {\n        fun make(): Deep = Deep()\n    }\n}\n";
    let mut cov = FileCoverage::default();
    cov.translated_spans.push((0, 20));
    let anchor = source.find("companion object").unwrap();
    cov.blockers
        .push((anchor, "// NOTLIN: N001 deep blocker\n".to_string()));
    let out = migrate::strip_translated(source, &cov);
    assert!(
        out.contains("    // NOTLIN: N001 deep blocker\n    companion object Factory"),
        "comment lost indent: {:?}",
        out
    );
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
