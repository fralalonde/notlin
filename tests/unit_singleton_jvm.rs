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
        let path =
            std::env::temp_dir().join(format!("notlin-unit-singleton-{}-{id}", std::process::id()));
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
        "Command.kt",
        r#"package sample.unitvalue
import kotlin.Unit
import kotlin.Unit as Empty
data class Command(
    val unused: Unit = Unit,
    val alias: Empty = Empty,
    val maybe: Unit? = null,
    val values: List<Unit> = listOf(Unit)
) {
    fun signal(): Unit {}
    fun signalExpr(): Unit = Unit
    fun signalReturn(): Unit { return Unit }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample.unitvalue
fun main() {
    suspend fun keepRunnerKotlin() {}
    val value = Command()
    val fields = Command::class.java.declaredFields.associateBy { it.name }
    val field = fields.getValue("unused")
    field.isAccessible = true
    val signature = Command::class.java.getDeclaredMethod("signal").returnType.name
    val expressionSignature = Command::class.java.getDeclaredMethod("signalExpr").returnType.name
    val returnedSignature = Command::class.java.getDeclaredMethod("signalReturn").returnType.name
    value.signalExpr()
    value.signalReturn()
    println("default=${value.unused === Unit}|alias=${value.alias === Unit}|nullable=${value.maybe == null}|generic=${value.values.single() === Unit}|return=$signature/$expressionSignature/$returnedSignature|field=${field.type.name}")
}
"#,
    ),
];

fn populate(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
}

fn translate(root: &Path, lombok_enabled: bool) -> std::process::Output {
    let mut kotlin = fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.file_name().is_some_and(|name| name == "Command.kt"))
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
    command.args(kotlin).output().expect("run notlin")
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
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => config.kotlin_sources.push(path),
            Some("java") => config.java_sources.push(path),
            _ => {}
        }
    }
    config.run_main_class = Some("sample.unitvalue.RunnerKt".into());
    config
}

#[test]
fn cli_keeps_unit_values_reference_typed_and_runner_retained() {
    let scratch = Scratch::new();
    let root = scratch.0.join("cli");
    populate(&root);
    let translated = translate(&root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    let java = fs::read_to_string(root.join("Command.java")).unwrap();
    assert!(java.contains("kotlin.Unit unused"), "{java}");
    assert!(java.contains("kotlin.Unit alias"), "{java}");
    assert!(java.contains("List<kotlin.Unit> values"), "{java}");
    assert!(java.contains("void signal()"), "{java}");
    assert!(java.contains("void signalExpr()"), "{java}");
    assert!(java.contains("void signalReturn()"), "{java}");
    assert!(root.join("Runner.kt").is_file());
}

#[test]
#[ignore = "requires JVM tools and Kotlin stdlib; Lombok modes also need Lombok and its Kotlin compiler plugin"]
fn original_and_mixed_jvms_preserve_unit_value_and_return_behavior() {
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
    .expect("original Kotlin Unit fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "default=true|alias=true|nullable=true|generic=true|return=void/void/void|field=kotlin.Unit"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Command.java").is_file());
        assert!(migrated.join("Runner.kt").is_file());
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin and Java Unit fixture compiles and runs");
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .trim()
                .replace("\r\n", "\n"),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
