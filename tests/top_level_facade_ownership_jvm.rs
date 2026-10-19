use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "notlin-top-level-facade-ownership-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SOURCES: [(&str, &str); 2] = [
    (
        "Functions.kt",
        r#"@file:JvmName("Utilities")
package sample

var count = 0
fun unrelated(): Int = 7
"#,
    ),
    (
        "Runner.kt",
        r#"package sample.runner
import sample.count
import sample.unrelated

suspend fun main() {
    count = 2
    val old = count++
    println("$old|$count|${unrelated()}")
}
"#,
    ),
];

fn populate(root: &Path) {
    populate_sources(root, &SOURCES);
}

fn populate_sources(root: &Path, sources: &[(&str, &str)]) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in sources {
        fs::write(root.join(name), source).unwrap();
    }
}

fn translate(root: &Path, lombok_enabled: bool) -> Output {
    let mut kotlin = fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect::<Vec<_>>();
    kotlin.sort();
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .arg("--root")
        .arg(root)
        .args(["--in-place", "--annotations", "none"]);
    if lombok_enabled {
        command.arg("--lombok");
    }
    command.args(kotlin);
    command.output().expect("run notlin")
}

fn jvm_config(
    root: &Path,
    output: PathBuf,
    stdlib: &Path,
    lombok_enabled: bool,
) -> JvmValidationConfig {
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output,
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config.javac_args.extend(["--release".into(), "17".into()]);
    config.classpath.push(stdlib.to_path_buf());
    if lombok_enabled {
        config.classpath.push(PathBuf::from(
            std::env::var_os("NOTLIN_LOMBOK").expect("NOTLIN_LOMBOK"),
        ));
        config.javac_args.push("-proc:full".into());
        let mut plugin = std::ffi::OsString::from("-Xplugin=");
        plugin.push(
            std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").expect("NOTLIN_KOTLIN_LOMBOK_PLUGIN"),
        );
        config.kotlinc_args.push(plugin);
    } else {
        config.javac_args.push("-proc:none".into());
    }
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "kt") {
            config.kotlin_sources.push(path);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "java")
        {
            config.java_sources.push(path);
        }
    }
    config.run_main_class = Some("sample.runner.RunnerKt".into());
    config
}

fn assert_facade_is_owned_by_retained_kotlin(root: &Path) {
    assert!(root.join("Functions.kt").is_file());
    assert!(
        !root.join("Utilities.java").exists(),
        "a generated Utilities.java would collide with the retained @file:JvmName facade"
    );
    let functions = fs::read_to_string(root.join("Functions.kt")).unwrap();
    assert!(functions.contains("@file:JvmName(\"Utilities\")"));
    assert!(functions.contains("var count = 0"));
    assert!(functions.contains("fun unrelated(): Int = 7"));
}

#[test]
fn partial_translation_does_not_split_an_annotated_file_facade() {
    for lombok_enabled in [false, true] {
        let scratch = Scratch::new();
        let migrated = scratch.0.join("migrated");
        populate(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert_facade_is_owned_by_retained_kotlin(&migrated);
    }
}

#[test]
fn annotated_function_facades_keep_retained_functions_but_translate_classes_independently() {
    const SOURCES: [(&str, &str); 1] = [(
        "Actions.kt",
        r#"@file:JvmName("Actions")
package sample.facade

fun available(): Int = 7
suspend fun retainedHook(): Int = 3

class Worker(val amount: Int) {
    fun doubled(): Int = amount * 2
}
"#,
    )];

    for lombok_enabled in [false, true] {
        let scratch = Scratch::new();
        let migrated = scratch.0.join("migrated");
        populate_sources(&migrated, &SOURCES);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );

        let actions = fs::read_to_string(migrated.join("Actions.kt")).unwrap();
        assert!(actions.contains("fun available(): Int = 7"), "{actions}");
        assert!(
            actions.contains("suspend fun retainedHook(): Int = 3"),
            "{actions}"
        );
        assert!(!actions.contains("class Worker"), "{actions}");
        assert!(
            !migrated.join("Actions.java").exists(),
            "the retained top-level functions still own the annotated facade"
        );

        let worker = fs::read_to_string(migrated.join("Worker.java"))
            .expect("the unrelated class remains independently migratable");
        assert!(worker.contains("class Worker"), "{worker}");
    }
}

#[test]
fn fully_translatable_annotated_function_facade_moves_to_one_java_owner() {
    const SOURCES: [(&str, &str); 1] = [(
        "Actions.kt",
        r#"@file:JvmName("Actions")
package sample.facade

fun first(): Int = 1
fun second(): String = "ready"
"#,
    )];

    for lombok_enabled in [false, true] {
        let scratch = Scratch::new();
        let migrated = scratch.0.join("migrated");
        populate_sources(&migrated, &SOURCES);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            !migrated.join("Actions.kt").exists(),
            "the all-translatable facade should not leave a competing Kotlin owner"
        );
        let actions = fs::read_to_string(migrated.join("Actions.java"))
            .expect("the annotated file facade is emitted as Java");
        assert!(actions.contains("first()"), "{actions}");
        assert!(actions.contains("second()"), "{actions}");
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_annotated_file_facade_ownership() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&jvm_config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        false,
    ))
    .expect("original Kotlin/JVM fixture compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(expected, "2|3|7");

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert_facade_is_owned_by_retained_kotlin(&migrated);
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin/Java file-facade fixture compiles and runs");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
