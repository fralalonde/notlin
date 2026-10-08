use clap::Parser;
use notlin::jvm_validation::load_validation_config;
use notlin::migration_pipeline::{
    GeneratedJava, WorkspaceMigrationPlan, validate_workspace_plan, verify_original_snapshots,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        Self::new_under(&std::env::temp_dir())
    }
    fn new_under(base: &Path) -> Self {
        let p = base.join(format!(
            "notlin-validation-gate-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fake_tool(dir: &Path, name: &str, log: &Path, code: i32) -> PathBuf {
    #[cfg(windows)]
    let (path, body) = (
        dir.join(format!("{name}.cmd")),
        format!("@echo %* > \"{}\"\r\n@exit /b {code}\r\n", log.display()),
    );
    #[cfg(not(windows))]
    let (path, body) = (
        dir.join(name),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nexit {code}\n",
            log.display().to_string().replace('\'', "'\\''")
        ),
    );
    fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
    }
    path
}

fn plan(
    kt: &Path,
    old_kt: &str,
    generated_path: &Path,
    generated_java: &str,
) -> WorkspaceMigrationPlan {
    WorkspaceMigrationPlan {
        original_sources: vec![(kt.to_path_buf(), old_kt.to_owned())],
        original_snapshots: vec![(kt.to_path_buf(), Some(old_kt.as_bytes().to_vec()))],
        final_kotlin_sources: HashMap::from([(
            kt.to_path_buf(),
            "class PlannedKotlin {}\n".to_owned(),
        )]),
        generated_java: vec![GeneratedJava {
            path: generated_path.to_path_buf(),
            origin: kt.to_path_buf(),
            name: "Generated.java".into(),
            source: generated_java.to_owned(),
        }],
        final_plans: Vec::new(),
        translation_plans: HashMap::new(),
        migration_proposals: HashMap::new(),
        source_edits: Vec::new(),
        rounds: 1,
    }
}

fn write_config(
    path: &Path,
    kotlinc: &Path,
    javac: &Path,
    kt: &Path,
    java: &Path,
    extra_kt: &Path,
    extra_java: &Path,
) {
    let value = serde_json::json!({
        "version": 1,
        "kotlinc": kotlinc.to_string_lossy(),
        "javac": javac.to_string_lossy(),
        "java": "unused-java",
        "kotlin_sources": [kt.file_name().unwrap().to_string_lossy(), extra_kt.file_name().unwrap().to_string_lossy()],
        "java_sources": [java.file_name().unwrap().to_string_lossy(), extra_java.file_name().unwrap().to_string_lossy()],
        "classpath": [],
        "kotlinc_args": ["-jvm-target", "17"],
        "javac_args": ["--release", "17"]
    });
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

#[test]
fn validation_failure_stages_complete_mixed_sources_and_leaves_originals_unchanged() {
    let scratch = Scratch::new();
    let cfg_dir = scratch.0.join("module");
    let tools = scratch.0.join("tools");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::create_dir_all(&tools).unwrap();
    let kt = cfg_dir.join("Input.kt");
    let existing_java = cfg_dir.join("Existing.java");
    let extra_kt = cfg_dir.join("Support.kt");
    let extra_java = cfg_dir.join("Support.java");
    fs::write(&kt, "class Input\n").unwrap();
    fs::write(&existing_java, "class Existing {}\n").unwrap();
    fs::write(&extra_kt, "class Support\n").unwrap();
    fs::write(&extra_java, "class SupportJava {}\n").unwrap();
    let log = scratch.0.join("compiler-args.txt");
    let kotlinc = fake_tool(&tools, "kotlinc", &log, 2);
    let javac = fake_tool(&tools, "javac", &scratch.0.join("javac-args.txt"), 0);
    let cfg = cfg_dir.join("validation.json");
    let relative_kotlinc = PathBuf::from("../tools").join(kotlinc.file_name().unwrap());
    let relative_javac = PathBuf::from("../tools").join(javac.file_name().unwrap());
    write_config(
        &cfg,
        &relative_kotlinc,
        &relative_javac,
        &kt,
        &existing_java,
        &extra_kt,
        &extra_java,
    );
    // Tool paths and source paths in JSON are resolved from the config directory.
    let parsed = load_validation_config(&cfg, scratch.0.join("classes")).unwrap();
    assert_eq!(parsed.kotlinc, cfg_dir.join(relative_kotlinc));
    assert_eq!(parsed.kotlin_sources[0], cfg_dir.join("Input.kt"));
    assert_eq!(
        parsed.kotlinc_args,
        vec![std::ffi::OsString::from("-jvm-target"), "17".into()]
    );
    assert_eq!(
        parsed.javac_args,
        vec![std::ffi::OsString::from("--release"), "17".into()]
    );

    let generated = cfg_dir.join("Generated.java");
    let mut migration = plan(&kt, "class Input\n", &generated, "class Generated {}\n");
    migration.original_snapshots.push((
        existing_java.clone(),
        Some(fs::read(&existing_java).unwrap()),
    ));
    let before_kt = fs::read(&kt).unwrap();
    let before_java = fs::read(&existing_java).unwrap();
    let error =
        validate_workspace_plan(&migration, &[], std::slice::from_ref(&existing_java), &cfg)
            .unwrap_err();
    assert!(error.contains("Kotlin compilation failed"), "{error}");
    let staged_args = fs::read_to_string(log).unwrap();
    assert!(staged_args.contains("Input.kt"), "{staged_args}");
    assert!(staged_args.contains("Generated.java"), "{staged_args}");
    assert!(staged_args.contains("Existing.java"), "{staged_args}");
    assert!(staged_args.contains("Support.kt"), "{staged_args}");
    assert!(staged_args.contains("Support.java"), "{staged_args}");
    assert_eq!(fs::read(&kt).unwrap(), before_kt);
    assert_eq!(fs::read(&existing_java).unwrap(), before_java);
    assert!(!generated.exists());
    let _ = kotlinc;
    let _ = javac;
}

#[test]
fn stale_original_snapshot_is_rejected_before_application() {
    let scratch = Scratch::new();
    let kt = scratch.0.join("Input.kt");
    fs::write(&kt, "class Before\n").unwrap();
    let migration = plan(
        &kt,
        "class Before\n",
        &scratch.0.join("Generated.java"),
        "class Generated {}\n",
    );
    fs::write(&kt, "class ConcurrentEdit\n").unwrap();
    let error = verify_original_snapshots(&migration).unwrap_err();
    assert!(error.contains("source changed since planning"));
}

#[test]
fn overlapping_checked_source_edits_are_rejected() {
    let scratch = Scratch::new();
    let kt = scratch.0.join("Input.kt");
    let source = "class Before\n";
    fs::write(&kt, source).unwrap();
    let mut migration = plan(
        &kt,
        source,
        &scratch.0.join("Generated.java"),
        "class Generated {}\n",
    );
    let hash = *blake3::hash(source.as_bytes()).as_bytes();
    for (start, end) in [(0, 8), (4, 12)] {
        migration
            .source_edits
            .push(notlin::translation_plan::PlannedSourceEdit {
                location: notlin::semantics::SourceLocation {
                    file: kt.clone(),
                    snapshot_hash: hash,
                    start_byte: start,
                    end_byte: end,
                },
                replacement: String::new(),
                speculative: false,
            });
    }
    assert!(
        verify_original_snapshots(&migration)
            .unwrap_err()
            .contains("overlapping planned source edits")
    );
}

#[test]
fn snapshot_edits_resolve_through_a_path_alias() {
    let scratch = Scratch::new();
    let kt = scratch.0.join("Input.kt");
    let source = "class Before\n";
    fs::write(&kt, source).unwrap();
    let mut migration = plan(
        &kt,
        source,
        &scratch.0.join("Generated.java"),
        "class Generated {}\n",
    );
    let alias_parent = scratch.0.join("alias");
    fs::create_dir_all(&alias_parent).unwrap();
    let alias = alias_parent.join("..").join("Input.kt");
    migration
        .source_edits
        .push(notlin::translation_plan::PlannedSourceEdit {
            location: notlin::semantics::SourceLocation {
                file: alias,
                snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
                start_byte: 0,
                end_byte: 5,
            },
            replacement: "After".into(),
            speculative: false,
        });

    verify_original_snapshots(&migration).unwrap();
}

#[test]
fn validation_success_compiles_without_running_java() {
    let scratch = Scratch::new();
    let cfg_dir = scratch.0.join("module");
    let tools = scratch.0.join("tools");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::create_dir_all(&tools).unwrap();
    let kt = cfg_dir.join("Input.kt");
    fs::write(&kt, "class Input\n").unwrap();
    let java = cfg_dir.join("Existing.java");
    fs::write(&java, "class Existing {}\n").unwrap();
    let kotlinc = fake_tool(&tools, "kotlinc", &scratch.0.join("k.log"), 0);
    let javac = fake_tool(&tools, "javac", &scratch.0.join("j.log"), 0);
    let extra_kt = cfg_dir.join("Support.kt");
    let extra_java = cfg_dir.join("Support.java");
    fs::write(&extra_kt, "class Support\n").unwrap();
    fs::write(&extra_java, "class SupportJava {}\n").unwrap();
    let cfg = cfg_dir.join("validation.json");
    write_config(&cfg, &kotlinc, &javac, &kt, &java, &extra_kt, &extra_java);
    let mut migration = plan(
        &kt,
        "class Input\n",
        &cfg_dir.join("Generated.java"),
        "class Generated {}\n",
    );
    migration
        .original_snapshots
        .push((java.clone(), Some(fs::read(&java).unwrap())));
    validate_workspace_plan(&migration, &[], &[java], &cfg).unwrap();
    assert!(scratch.0.join("j.log").is_file());
    assert!(!scratch.0.join("classes").exists());
}

#[test]
fn successful_cli_validation_allows_planned_java_and_kotlin_writes() {
    let scratch = Scratch::new();
    let root = scratch.0.join("workspace");
    let tools = root.join("tools");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&tools).unwrap();
    let kt = root.join("Simple.kt");
    fs::write(&kt, "package gate\nclass Simple\n").unwrap();
    let kotlinc = fake_tool(&tools, "kotlinc", &scratch.0.join("k.log"), 0);
    let javac = fake_tool(&tools, "javac", &scratch.0.join("j.log"), 0);
    let cfg = root.join("validation.json");
    fs::write(&cfg, serde_json::to_vec(&serde_json::json!({
        "version": 1, "kotlinc": format!("tools/{}", kotlinc.file_name().unwrap().to_string_lossy()), "javac": format!("tools/{}", javac.file_name().unwrap().to_string_lossy()), "java": "unused-java",
        "kotlin_sources": ["Simple.kt"], "java_sources": []
    })).unwrap()).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place", "--validation-config"])
        .arg(&cfg)
        .arg(&kt)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(root.join("Simple.java").is_file());
    assert!(
        !kt.exists(),
        "a fully translated Kotlin source is removed after successful validation"
    );
    assert!(scratch.0.join("j.log").is_file());
}

#[test]
fn failed_cli_validation_keeps_kotlin_and_java_files_unchanged() {
    let scratch = Scratch::new();
    let root = scratch.0.join("workspace");
    let tools = root.join("tools");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&tools).unwrap();
    let kt = root.join("Simple.kt");
    let java = root.join("Existing.java");
    let before_kt = b"package gate\nclass Simple\nsuspend fun keep() {}\n";
    let before_java = b"package gate; class Existing {}\n";
    fs::write(&kt, before_kt).unwrap();
    fs::write(&java, before_java).unwrap();
    let kotlinc = fake_tool(&tools, "kotlinc", &scratch.0.join("k.log"), 3);
    let javac = fake_tool(&tools, "javac", &scratch.0.join("j.log"), 0);
    let cfg = root.join("validation.json");
    fs::write(&cfg, serde_json::to_vec(&serde_json::json!({
        "version": 1, "kotlinc": format!("tools/{}", kotlinc.file_name().unwrap().to_string_lossy()), "javac": format!("tools/{}", javac.file_name().unwrap().to_string_lossy()), "java": "unused-java",
        "kotlin_sources": ["Simple.kt"], "java_sources": ["Existing.java"]
    })).unwrap()).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place", "--validation-config"])
        .arg(&cfg)
        .arg(&kt)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("Kotlin compilation failed"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(&kt).unwrap(), before_kt);
    assert_eq!(fs::read(&java).unwrap(), before_java);
    assert!(!root.join("Simple.java").exists());
    assert!(
        !scratch.0.join("j.log").exists(),
        "javac must not run after Kotlin failure"
    );
}

#[test]
fn strict_mode_warning_keeps_original_source_and_suppresses_generated_java() {
    let scratch = Scratch::new();
    let root = scratch.0.join("workspace");
    fs::create_dir_all(&root).unwrap();
    let kt = root.join("Uncertain.kt");
    let original = b"fun uncertain() { val value = unknownApi(); println(value) }\n";
    fs::write(&kt, original).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args([
            "--annotations",
            "none",
            "--in-place",
            "--untranslatable",
            "error",
            "--allow-approximations",
        ])
        .arg(&kt)
        .output()
        .unwrap();
    assert_eq!(
        fs::read(&kt).unwrap(),
        original,
        "strict warning must not delete or rewrite the Kotlin source"
    );
    assert!(
        !root.join("Uncertain.java").exists(),
        "strict warning must suppress generated Java output"
    );
    let _ = result;
}

#[test]
fn opaque_top_level_parse_region_is_not_deleted_with_translated_declarations() {
    let scratch = Scratch::new();
    let root = scratch.0.join("workspace");
    fs::create_dir_all(&root).unwrap();
    let kt = root.join("Features.kt");
    let source = "package gate\ndata class Measurement(val amount: Int, val label: String = \"unit\")\nclass Device(val id: Int) { companion object { fun create(id: Int): Device = Device(id) } }\n";
    fs::write(&kt, source).unwrap();
    let cli =
        notlin::cli::Cli::parse_from(["notlin", "--annotations", "none", kt.to_str().unwrap()]);
    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let plan = notlin::migration_pipeline::plan_workspace_migration(
        vec![(kt.clone(), source.to_owned())],
        &cli,
        &index,
        &[root],
        |_| {},
    )
    .unwrap();
    assert!(
        plan.generated_java
            .iter()
            .any(|generated| generated.name == "Measurement.java")
    );
    let remaining = plan
        .final_kotlin_sources
        .get(&kt)
        .expect("opaque ERROR declaration must remain");
    assert!(remaining.contains("class Device"), "{remaining}");
}
