use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "notlin-platform-constructor-{}-{id}",
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

fn config(root: &Path, output: PathBuf, stdlib: &Path) -> JvmValidationConfig {
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
    config.run_main_class = Some("boundary.RunnerKt".into());
    config
}

#[test]
#[ignore = "requires JVM tools and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_platform_constructor_values_in_mixed_sources() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();

    let sources = [
        (
            "Converter.kt",
            r#"package boundary
import java.math.BigDecimal

class Converter {
    fun fromDouble(): String = BigDecimal(0.1).toPlainString()
    fun fromText(): String = BigDecimal("12.3400").toPlainString()
    fun fromInt(): String = BigDecimal(23).toPlainString()
    fun fromLong(): String = BigDecimal(5_000_000_000L).toPlainString()
    fun fromNegativeDouble(): String = BigDecimal(-0.1).toPlainString()
    fun fromQualifiedText(): String = java.math.BigDecimal("-7.50").toPlainString()
    fun fromLocal(): String {
        val amount = BigDecimal("1.25")
        return amount.toPlainString()
    }
    fun localDouble(): Double {
        val value = 0.1
        return value
    }
    fun localLong(): Long {
        val value = 5_000_000_000L
        return value
    }
    fun overflowDouble(java: Int): Double = 1e400
    fun underflowDouble(): Double = -1e-500
    fun overflowFloat(): Float = 1e50f
    fun underflowFloat(): Float = -1e-50f
    fun add(): String = BigDecimal("1.20").add(BigDecimal("2.30")).toPlainString()
    fun builder(): String = StringBuilder().append("text:").append(42).toString()
}
"#,
        ),
        (
            "Runner.kt",
            r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val converter = Converter()
    println(listOf(
        converter.fromDouble(), converter.fromText(), converter.fromInt(),
        converter.fromLong(), converter.fromNegativeDouble(), converter.fromQualifiedText(),
        converter.fromLocal(), converter.add(), converter.builder(), converter.localDouble(), converter.localLong(),
        converter.overflowDouble(0), converter.underflowDouble(), converter.overflowFloat(), converter.underflowFloat()
    ).joinToString("|"))
}
"#,
        ),
    ];
    for (name, source) in sources {
        fs::write(original.join(name), source).unwrap();
    }

    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original classes"),
        &stdlib,
    ))
    .expect("original Kotlin constructors compile and run");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "0.1000000000000000055511151231257827021181583404541015625|12.3400|23|5000000000|-0.1000000000000000055511151231257827021181583404541015625|-7.50|1.25|3.50|text:42|0.1|5000000000|Infinity|-0.0|Infinity|-0.0"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        fs::create_dir_all(&migrated).unwrap();
        for (name, source) in sources {
            fs::write(migrated.join(name), source).unwrap();
        }
        let mut translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
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
            migrated.join("Converter.java").is_file(),
            "Converter must translate in Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Runner.kt").is_file());
        let java = fs::read_to_string(migrated.join("Converter.java")).unwrap();
        assert!(java.contains("new java.math.BigDecimal(0.1)"), "{java}");
        assert!(!java.contains("BigDecimal.valueOf(0.1)"), "{java}");
        assert!(java.contains("new java.lang.StringBuilder()"), "{java}");

        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed classes-{lombok_enabled}")),
            &stdlib,
        ))
        .expect("translated Java and retained Kotlin compile and run");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
