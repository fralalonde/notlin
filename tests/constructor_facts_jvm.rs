//! Real-toolchain differential for helper-backed constructors.
//!
//! One original Kotlin program uses three constructor shapes the pre-helper
//! table did not declare (`StringBuilder(String)`, `StringBuilder(Int)`,
//! `BigDecimal(String, MathContext)`) plus two it did
//! (`IllegalArgumentException(String)`, `IllegalStateException(String)`).
//!
//! * the original Kotlin must compile with the real compiler and produce the
//!   reference stdout;
//! * the helper-off migration must keep the three sourced constructors in Kotlin
//!   (the pre-helper floor) and still compile and run;
//! * the **slim** Track A helper must translate every declaration, emit
//!   canonical owners (`new java.lang.StringBuilder(...)`,
//!   `new java.math.BigDecimal(...)`), compile with javac and print the *same*
//!   bytes;
//! * the full helper must behave the same, so the change is not Track B in
//!   disguise.
//!
//! Ignored by default: it needs a JDK, the Kotlin compiler, the Kotlin stdlib and
//! both helper jars. Run it with the compiler/toolchain paths and both jars
//! exported:
//!
//! ```sh
//! NOTLIN_KOTLINC=/path/to/kotlinc NOTLIN_JAVAC=/path/to/javac \
//! NOTLIN_JAVA=/path/to/java NOTLIN_KOTLIN_STDLIB=/path/to/kotlin-stdlib.jar \
//! NOTLIN_JVM_FACTS_JAR=/path/to/notlin-jvm-facts.jar \
//! NOTLIN_JVM_FACTS_FULL_JAR=/path/to/notlin-jvm-facts-full.jar \
//! cargo test --locked --test constructor_facts_jvm -- --ignored --nocapture
//! ```
//!
//! `NOTLIN_JVM_FACTS_JAR` must be the slim (Track A) jar and
//! `NOTLIN_JVM_FACTS_FULL_JAR` the full (Track A plus Track B) jar; the test
//! exercises both, so neither may be omitted.

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
            "notlin-constructor-facts-jvm-{}-{id}",
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

const SOURCE: &str = r#"package probe

import java.math.BigDecimal
import java.math.MathContext

fun main() {
    println(seeded())
    println(sized())
    println(scaled(MathContext.DECIMAL64))
    println(failure())
    println(state())
}

fun seeded(): String = StringBuilder("seed").toString()

fun sized(): String = StringBuilder(16).toString()

fun scaled(context: MathContext): String = BigDecimal("1", context).toPlainString()

fun failure(): IllegalArgumentException = IllegalArgumentException("bad")

fun state(): IllegalStateException = IllegalStateException("bad")
"#;

fn config(root: &Path, output: PathBuf, stdlib: &Path, main_class: &str) -> JvmValidationConfig {
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
    config.run_main_class = Some(main_class.to_string());
    config
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the slim/full helper jars"]
fn slim_track_a_sources_the_same_constructors_the_full_helper_does() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let slim = PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_JAR").expect("slim helper jar"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();

    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("ConstructorsNative.kt"), SOURCE).unwrap();
    let reference = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        "probe.ConstructorsNativeKt",
    ))
    .expect("the original Kotlin must compile and run");
    let expected = reference.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "seed\n\n1\njava.lang.IllegalArgumentException: bad\njava.lang.IllegalStateException: bad"
    );

    for (mode, jar, should_translate) in [
        ("off", None, false),
        ("slim", Some(&slim), true),
        ("full", Some(&full), true),
    ] {
        let migrated = scratch.0.join(format!("migrated-{mode}"));
        fs::create_dir_all(&migrated).unwrap();
        fs::write(migrated.join("ConstructorsNative.kt"), SOURCE).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
        command
            .arg("--root")
            .arg(&migrated)
            .args(["--annotations", "none", "--in-place"])
            .arg("--jvm-facts")
            .arg(if should_translate { "on" } else { "off" })
            .arg(&migrated);
        command
            .env_remove("NOTLIN_KOTLIN_SOURCES")
            .env_remove("NOTLIN_CLASSPATH")
            .env_remove("NOTLIN_JVM_FACTS_KOTLIN");
        match jar {
            Some(jar) => {
                command.env("NOTLIN_JVM_FACTS_JAR", jar);
            }
            None => {
                command.env_remove("NOTLIN_JVM_FACTS_JAR");
            }
        }
        let translated = command.output().unwrap();
        assert!(
            translated.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );

        let java = fs::read_to_string(migrated.join("ConstructorsNative.java")).ok();
        let kotlin = fs::read_to_string(migrated.join("ConstructorsNative.kt")).ok();
        if should_translate {
            let java = java.unwrap_or_else(|| panic!("{mode}: expected Java output"));
            for expected_call in [
                "new java.lang.StringBuilder(\"seed\")",
                "new java.lang.StringBuilder(16)",
                "new java.math.BigDecimal(\"1\", context)",
                "new java.lang.IllegalArgumentException(\"bad\")",
                "new java.lang.IllegalStateException(\"bad\")",
            ] {
                assert!(
                    java.contains(expected_call),
                    "{mode}: missing `{expected_call}`:\n{java}"
                );
            }
            assert!(
                kotlin.is_none(),
                "{mode}: every declaration should have migrated: {kotlin:?}"
            );
        } else {
            let retained = kotlin
                .as_deref()
                .unwrap_or_else(|| panic!("{mode}: expected retained Kotlin"));
            assert!(java.as_deref().is_none_or(|java| {
                !java.contains("new java.lang.StringBuilder(\"seed\")")
                    && !java.contains("new java.math.BigDecimal(\"1\", context)")
            }));
            for retained_call in [
                "StringBuilder(\"seed\")",
                "StringBuilder(16)",
                "BigDecimal(\"1\", context)",
            ] {
                assert!(
                    retained.contains(retained_call),
                    "{mode}: `{retained_call}` must stay Kotlin without facts:\n{retained}"
                );
            }
        }

        // Whatever the mix, the migrated tree must compile and print the same
        // bytes as the original Kotlin.
        let main_class = if kotlin.is_some() {
            "probe.ConstructorsNativeKt"
        } else {
            "probe.ConstructorsNative"
        };
        let report = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("migrated-classes-{mode}")),
            &stdlib,
            main_class,
        ))
        .unwrap_or_else(|error| panic!("{mode}: migrated tree must compile and run: {error}"));
        assert_eq!(
            report.commands.last().unwrap().stdout.trim(),
            expected,
            "{mode}: translated program must print the original bytes"
        );
    }
}
