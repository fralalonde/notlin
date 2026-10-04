//! Workspace-level retention fixpoint: retention is order-dependent, so the
//! CLI computes a least-fixpoint of the retained set over probe passes BEFORE
//! writing anything. A hub interface retains only when one of its Kotlin
//! subtypes is ITSELF retained (intrinsically tainted — declaration
//! annotation, enum-entries ABI, KClass...); a clean hub-and-implementor
//! family translates together. The naive top-down scope-loosening
//! (translating hub+implementor whenever both are selected) over-reached:
//! target builds failed on 5499 kotlinc errors because subtypes retained by
//! unrelated rules were suddenly implementing translated-away interfaces.

use clap::Parser;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Hub + clean implementor in the same selection: BOTH translate. The
/// implementor is a plain class with no intrinsic blockers, so the fixpoint
/// never seeds it and the hub's subtype rule lets go.
#[test]
fn clean_hub_and_implementor_translate_together() {
    let root = Path::new("tests/tmp_scratch_fixpoint_clean");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("hub.kt"),
        "package neutral.fixpoint\n\
         \n\
         interface Speaker {\n\
         \x20   fun speak(): String\n\
         }\n\
         \n\
         class Dog : Speaker {\n\
         \x20   override fun speak(): String = \"woof\"\n\
         }\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let speaker_java = fs::read_to_string(root.join("Speaker.java")).unwrap_or_default();
    let dog_java = fs::read_to_string(root.join("Dog.java")).unwrap_or_default();
    assert!(
        !speaker_java.is_empty() && !dog_java.is_empty(),
        "clean hub and implementor must both translate.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("remain Kotlin"),
        "no retention diagnostic expected:\n{stderr}"
    );
    // Both were stripped from the .kt source.
    let hub_kt = fs::read_to_string(root.join("hub.kt")).unwrap_or_default();
    assert!(
        !hub_kt.contains("class Dog"),
        "translated declarations must be stripped in-place; remaining:\n{hub_kt}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn covariant_override_translates_with_its_interface_hierarchy() {
    let root = Path::new("tests/tmp_scratch_fixpoint_covariant");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("activity.kt"),
        "package neutral.fixpoint.covariant\n\
         interface ActivityInfo\n\
         class JobInfo : ActivityInfo\n\
         interface Activity { fun getInfo(): ActivityInfo }\n\
         class JobActivity : Activity {\n\
         \x20   override fun getInfo(): JobInfo = JobInfo()\n\
         }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");

    let activity = fs::read_to_string(root.join("Activity.java")).unwrap_or_default();
    let job = fs::read_to_string(root.join("JobActivity.java")).unwrap_or_default();
    assert!(
        activity.contains("ActivityInfo getInfo()")
            && job.contains("JobInfo getInfo()")
            && !root.join("activity.kt").exists(),
        "a Java-compatible covariant override and its hierarchy must translate together:\n{stderr}\n{activity}\n{job}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn covariant_override_matches_the_full_overloaded_signature() {
    let root = Path::new("tests/tmp_scratch_fixpoint_covariant_overload");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("api.kt"),
        "package neutral.fixpoint.covariant.overload\n\
         interface BaseInfo\n\
         class JobInfo : BaseInfo\n\
         interface Activity {\n\
         \x20   fun getInfo(index: Int): BaseInfo\n\
         \x20   fun getInfo(name: String): String\n\
         }\n\
         class JobActivity : Activity {\n\
         \x20   override fun getInfo(index: Int): JobInfo = JobInfo()\n\
         \x20   override fun getInfo(name: String): String = name\n\
         }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");
    let job = fs::read_to_string(root.join("JobActivity.java")).unwrap_or_default();
    assert!(
        job.contains("JobInfo getInfo(int index)")
            && job.contains("String getInfo(String name)")
            && !root.join("api.kt").exists(),
        "overloaded members must be paired by parameter signature before covariance is classified:\n{stderr}\n{job}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn translated_kotlin_caller_releases_enum_and_supertype_in_one_run() {
    let root = Path::new("tests/tmp_scratch_fixpoint_retained_enum");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("types.kt"),
        "package neutral.fixpoint3\n\
         \n\
         interface Speaker {\n\
         \x20   fun speak(): String\n\
         }\n\
         \n\
         enum class Mood : Speaker {\n\
         \x20   CALM;\n\
         \x20   override fun speak(): String = \"calm\"\n\
         }\n",
    )
    .unwrap();
    fs::write(
        root.join("usage.kt"),
        "package neutral.fixpoint3\nfun currentMood(): Mood = Mood.CALM\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");
    let speaker_java = fs::read_to_string(root.join("Speaker.java")).unwrap_or_default();
    let mood_java = fs::read_to_string(root.join("Mood.java")).unwrap_or_default();
    let usage_java = fs::read_to_string(root.join("Usage.java")).unwrap_or_default();
    assert!(
        !speaker_java.is_empty() && !mood_java.is_empty() && !usage_java.is_empty(),
        "the translated caller must release the enum and its supertype in the same invocation\n{stderr}"
    );
    assert!(!root.join("types.kt").exists());
    assert!(!root.join("usage.kt").exists());
    let _ = fs::remove_dir_all(root);
}

/// Hub + a Java-representable Kotlin annotation: the annotation declaration,
/// implementor, and hub all translate in one fixpoint. A marker annotation has
/// a direct Java `@interface` representation and is not an intrinsic blocker.
#[test]
fn marker_annotation_implementor_translates_with_hub() {
    let root = Path::new("tests/tmp_scratch_fixpoint_tainted");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("mine.kt"),
        "package neutral.fixpoint2\n\
         \n\
         annotation class KotlinOnly\n",
    )
    .unwrap();
    fs::write(
        root.join("hub.kt"),
        "package neutral.fixpoint2\n\
         \n\
         interface Speaker {\n\
         \x20   fun speak(): String\n\
         }\n\
         \n\
         @KotlinOnly\n\
         class Parrot : Speaker {\n\
         \x20   override fun speak(): String = \"squawk\"\n\
         }\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let speaker_java = fs::read_to_string(root.join("Speaker.java")).unwrap_or_default();
    let parrot_java = fs::read_to_string(root.join("Parrot.java")).unwrap_or_default();
    let annotation_java = fs::read_to_string(root.join("KotlinOnly.java")).unwrap_or_default();
    assert!(
        !speaker_java.is_empty()
            && !parrot_java.is_empty()
            && annotation_java.contains("@interface KotlinOnly"),
        "Java-representable annotation family must translate.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("remain Kotlin"),
        "no retention diagnostic expected:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn retained_delta_rechecks_only_reverse_dependency_files() {
    let root = Path::new("tests/tmp_scratch_fixpoint_dependencies");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    let hub = root.join("hub.kt");
    let retained = root.join("retained.kt");
    let child = root.join("child.kt");
    let unrelated = root.join("unrelated.kt");
    fs::write(&hub, "interface Hub\n").unwrap();
    fs::write(&retained, "interface Retained : Hub\n").unwrap();
    fs::write(&child, "class Child : Retained\n").unwrap();
    fs::write(&unrelated, "class Unrelated\n").unwrap();

    let index = notlin::workspace::SourceIndex::discover(root).unwrap();
    let delta = HashSet::from(["Retained".to_string()]);
    assert!(index.retained_delta_can_affect(&hub, &delta));
    assert!(index.retained_delta_can_affect(&retained, &delta));
    assert!(index.retained_delta_can_affect(&child, &delta));
    assert!(!index.retained_delta_can_affect(&unrelated, &delta));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn warm_retention_replanning_matches_cold_after_overlay_shrinks_seed() {
    use notlin::workspace::{SourceIndex, SourceLanguage, SourceOverlay};
    use notlin::{cli::Cli, transpiler::fixpoint};
    let root = Path::new("tests/tmp_scratch_warm_retention");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    let file = root.join("Types.kt");
    let original = "@Deprecated(\"keep\")\nclass Kept\nclass Free\n";
    fs::write(&file, original).unwrap();
    let base = SourceIndex::discover(root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", file.to_str().unwrap()]);
    let cold = fixpoint::plan_workspace_state(
        &[(file.clone(), original.into())],
        &cli,
        &base,
        &[root.to_path_buf()],
        16,
        false,
    )
    .unwrap();
    let updated = "class Kept\nclass Free\n";
    let changed = base
        .with_overlays(&[SourceOverlay::Replace {
            path: file.clone(),
            language: SourceLanguage::Kotlin,
            source: updated.into(),
        }])
        .unwrap();
    let files = [(file.clone(), updated.into())];
    let warm = fixpoint::plan_workspace_warm(
        &files,
        &cli,
        &changed,
        &[root.to_path_buf()],
        16,
        false,
        &cold.retained,
    )
    .unwrap();
    let fresh =
        fixpoint::plan_workspace_state(&files, &cli, &changed, &[root.to_path_buf()], 16, false)
            .unwrap();
    assert_eq!(warm.retained, fresh.retained);
    assert!(warm.retained.is_empty());
    assert_eq!(
        warm.plans
            .iter()
            .map(|p| &p.coverage.translated)
            .collect::<Vec<_>>(),
        fresh
            .plans
            .iter()
            .map(|p| &p.coverage.translated)
            .collect::<Vec<_>>()
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn warm_retention_replanning_adds_names_missing_from_seed() {
    use notlin::workspace::SourceIndex;
    use notlin::{cli::Cli, transpiler::fixpoint};
    let root = Path::new("tests/tmp_scratch_warm_incomplete");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    let file = root.join("Types.kt");
    let source = "@Deprecated(\"keep\")\nclass Kept\nclass Free\n";
    fs::write(&file, source).unwrap();
    let index = SourceIndex::discover(root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", file.to_str().unwrap()]);
    let files = [(file.clone(), source.into())];
    let seed = HashSet::from(["UnrelatedStaleName".to_string()]);
    let warm = fixpoint::plan_workspace_warm(
        &files,
        &cli,
        &index,
        &[root.to_path_buf()],
        16,
        false,
        &seed,
    )
    .unwrap();
    let fresh =
        fixpoint::plan_workspace_state(&files, &cli, &index, &[root.to_path_buf()], 16, false)
            .unwrap();
    assert_eq!(warm.retained, fresh.retained);
    assert!(!warm.retained.contains("UnrelatedStaleName"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn retention_pass_limit_error_recommends_cli_override() {
    let root = Path::new("tests/tmp_scratch_fixpoint_limit");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("Kept.kt"), "value class Kept(val raw: Int)\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args([
            "--root",
            root.to_str().unwrap(),
            "--in-place",
            "--max-retention-passes",
            "1",
        ])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "pass limit should fail:\n{stderr}"
    );
    assert!(
        stderr.contains("increase --max-retention-passes above 1"),
        "failure must recommend the exact CLI override:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn deep_valid_retention_chain_uses_configurable_budget() {
    use notlin::workspace::SourceIndex;
    use notlin::{cli::Cli, transpiler::fixpoint};

    let scratch = Path::new("tests/tmp_scratch_fixpoint_deep_chain");
    let _ = fs::remove_dir_all(scratch);
    fs::create_dir_all(scratch).unwrap();
    for depth in 0..7 {
        let supertype = if depth == 0 {
            String::new()
        } else {
            format!(" : Chain{}", depth - 1)
        };
        fs::write(
            scratch.join(format!("Chain{depth}.kt")),
            format!("package neutral.deep\ninterface Chain{depth}{supertype}\n"),
        )
        .unwrap();
    }
    fs::write(
        scratch.join("Kept.kt"),
        "package neutral.deep\nvalue class Kept(val raw: Int) : Chain6\n",
    )
    .unwrap();

    let root = fs::canonicalize(scratch).unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", root.to_str().unwrap()]);
    let mut files = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .map(|path| {
            let source = fs::read_to_string(&path).unwrap();
            (path, source)
        })
        .collect::<Vec<_>>();
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let error =
        fixpoint::plan_workspace_state(&files, &cli, &index, std::slice::from_ref(&root), 4, false)
            .err()
            .expect("four passes must be insufficient for this valid chain");
    assert!(error.contains("increase --max-retention-passes above 4"));

    let planned = fixpoint::plan_workspace_state(
        &files,
        &cli,
        &index,
        std::slice::from_ref(&root),
        16,
        false,
    )
    .expect("the same valid chain must converge with a larger budget");
    assert_eq!(planned.retained.len(), 8);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn silent_fixpoint_jobs_profile_reports_requested_worker_count() {
    let root = Path::new("tests/tmp_scratch_fixpoint_jobs");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("Type.kt"), "class Type\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.as_os_str())
        .env("NOTLIN_JOBS", "4")
        .env("NOTLIN_PROFILE", "1")
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        stderr.contains("fixpoint jobs=4"),
        "missing jobs profile line:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn retained_comments_name_direct_declarations_and_root_markers() {
    let root = Path::new("tests/tmp_scratch_precise_retention_comments");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Chain0.kt"),
        "package neutral.comments\ninterface Chain0\n",
    )
    .unwrap();
    fs::write(
        root.join("Chain1.kt"),
        "package neutral.comments\ninterface Chain1 : Chain0\n",
    )
    .unwrap();
    fs::write(
        root.join("Kept.kt"),
        "package neutral.comments\nvalue class Kept(val raw: Int) : Chain1\n",
    )
    .unwrap();

    let run = || {
        Command::new(env!("CARGO_BIN_EXE_notlin"))
            .args([
                "--root",
                root.to_str().unwrap(),
                "--in-place",
                "--max-retention-passes",
                "16",
            ])
            .arg(root.as_os_str())
            .output()
            .expect("run notlin")
    };
    let first = run();
    assert!(
        first.status.success(),
        "notlin failed:\n{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let chain0 = fs::read_to_string(root.join("Chain0.kt")).unwrap();
    let chain1 = fs::read_to_string(root.join("Chain1.kt")).unwrap();
    assert!(
        chain0.contains("retained interface Chain0")
            && chain0.contains("blocked by interface Chain1 at Chain1.kt")
            && chain0
                .contains("class Kept at Kept.kt [NF7FA]: class modifier not supported: value"),
        "{chain0}"
    );
    assert!(
        chain1.contains("retained interface Chain1")
            && chain1.contains("blocked by class Kept at Kept.kt")
            && !chain1.contains(&root.to_string_lossy().replace('\\', "/")),
        "{chain1}"
    );

    let before = [
        fs::read_to_string(root.join("Chain0.kt")).unwrap(),
        fs::read_to_string(root.join("Chain1.kt")).unwrap(),
        fs::read_to_string(root.join("Kept.kt")).unwrap(),
    ];
    let second = run();
    assert!(
        second.status.success(),
        "second notlin run failed:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let after = [
        fs::read_to_string(root.join("Chain0.kt")).unwrap(),
        fs::read_to_string(root.join("Chain1.kt")).unwrap(),
        fs::read_to_string(root.join("Kept.kt")).unwrap(),
    ];
    assert_eq!(after, before, "precise comments must be byte-idempotent");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn final_retention_markers_exclude_names_released_from_the_seed() {
    use notlin::workspace::SourceIndex;
    use notlin::{cli::Cli, transpiler::fixpoint};

    let root = Path::new("tests/tmp_scratch_released_marker");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    let file = root.join("Types.kt");
    let source = "interface Hub\n\
                  enum class Free : Hub { A }\n\
                  class Kept : Hub {\n\
                  \x20   fun unsupported() = mapOf(\"a\" to 1).plus(emptyMap())\n\
                  }\n";
    fs::write(&file, source).unwrap();
    let index = SourceIndex::discover(root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", file.to_str().unwrap()]);
    let files = [(file.clone(), source.into())];
    let seed = HashSet::from(["Free".to_string()]);

    let planned =
        fixpoint::plan_workspace_warm(&files, &cli, &index, &[root.to_path_buf()], 16, true, &seed)
            .unwrap();

    assert!(!planned.retained.contains("Free"));
    let blockers = planned.plans[0]
        .coverage
        .blockers
        .iter()
        .map(|(_, marker)| marker.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !blockers.contains("enum Free"),
        "released declarations must not survive in final provenance:\n{blockers}"
    );
    let _ = fs::remove_dir_all(root);
}
