use clap::Parser;
use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "notlin-nested-enum-static-{}-{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture_sources() -> [(&'static str, &'static str); 2] {
    [
        (
            "Outer.kt",
            r#"package nested
class Outer {
    enum class Mode { FIRST, SECOND }

    fun selected(): Mode = Mode.SECOND
    fun render(mode: Mode): String = mode.name
}
"#,
        ),
        (
            "Runner.kt",
            r#"package nested
fun main() {
    suspend fun retainedHarness() {}
    val outer = Outer()
    val values = Outer.Mode.values().joinToString(",") { it.name }
    val isStatic = java.lang.reflect.Modifier.isStatic(Outer.Mode::class.java.modifiers)
    println("$values|${outer.render(outer.selected())}|$isStatic")
}
"#,
        ),
    ]
}

#[test]
fn nested_enum_is_emitted_inside_its_outer_java_class() {
    let source = r#"class Outer {
    enum class Mode { FIRST, SECOND }
    fun selected(): Mode = Mode.SECOND
    fun render(mode: Mode): String = mode.name
}"#;
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "Outer.kt"]);
    let (files, errors, warnings, _coverage) =
        notlin::transpiler::transpile(source, &PathBuf::from("Outer.kt"), &cli);

    assert_eq!(errors, 0);
    assert_eq!(warnings, 0);
    let java = files
        .iter()
        .find(|(name, _)| name == "Outer.java")
        .map(|(_, content)| content)
        .expect("nested enum should be part of Outer.java");
    assert!(java.contains("enum Mode"), "{java}");
    assert!(java.contains("Mode.SECOND"), "{java}");
    assert!(java.contains("String render(Mode mode)"), "{java}");
}

fn jvm_config(root: &Path, output: PathBuf, stdlib: &Path) -> JvmValidationConfig {
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output,
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config
        .javac_args
        .extend(["--release".into(), "17".into(), "-proc:none".into()]);
    config.classpath.push(stdlib.to_path_buf());
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => config.kotlin_sources.push(path),
            Some("java") => config.java_sources.push(path),
            _ => {}
        }
    }
    config.run_main_class = Some("nested.RunnerKt".into());
    config
}

#[test]
#[ignore = "requires JVM tools and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn nested_enum_matches_kotlin_values_and_static_modifier_in_both_lombok_modes() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    for (name, source) in fixture_sources() {
        fs::write(original.join(name), source).unwrap();
    }

    let before = validate_jvm_sources(&jvm_config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
    ))
    .expect("original Kotlin fixture compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(expected, "FIRST,SECOND|SECOND|true");

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        fs::create_dir_all(&migrated).unwrap();
        for (name, source) in fixture_sources() {
            fs::write(migrated.join(name), source).unwrap();
        }

        let mut translation = Command::new(env!("CARGO_BIN_EXE_notlin"));
        translation
            .arg("--root")
            .arg(&migrated)
            .args(["--annotations", "none", "--in-place"]);
        if lombok_enabled {
            translation.arg("--lombok");
        }
        let translated = translation.arg(&migrated).output().unwrap();
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            migrated.join("Outer.java").is_file(),
            "Outer should translate in Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Runner.kt").is_file());

        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("migrated-classes-{lombok_enabled}")),
            &stdlib,
        ))
        .expect("generated Java and retained Kotlin compile and run");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
