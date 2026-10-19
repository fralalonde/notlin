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
            "notlin-object-string-jvm-{}-{id}",
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

fn config(
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
        let plugin =
            std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").expect("NOTLIN_KOTLIN_LOMBOK_PLUGIN");
        let mut plugin_argument = std::ffi::OsString::from("-Xplugin=");
        plugin_argument.push(plugin);
        config.kotlinc_args.push(plugin_argument);
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
    config.run_main_class = Some("boundary.RunnerKt".into());
    config
}

fn translate(root: &Path, lombok_enabled: bool) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .arg("--root")
        .arg(root)
        .args(["--annotations", "none", "--in-place"]);
    if lombok_enabled {
        command.arg("--lombok");
    }
    command.arg(root).output().unwrap()
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_object_string_semantics() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();

    let sources = [
        (
            "AbsenceApi.kt",
            "package boundary\nfun absent(): Nothing? = null\nclass AbsenceApi {\n    fun render(): String? = absent()\n}\n",
        ),
        (
            "Label.kt",
            r#"package boundary
interface Label {
    suspend fun retainedHook()
}
"#,
        ),
        (
            "DynamicLabel.kt",
            r#"package boundary
class DynamicLabel(private val value: String) : Label {
    override fun toString(): String = value
    override suspend fun retainedHook() {}
}
"#,
        ),
        (
            "Display.kt",
            r#"package boundary
class Display private constructor(private val text: String) {
    constructor(label: Label) : this(label.toString())
    fun render(): String = text
}
"#,
        ),
        (
            "ChainDisplay.kt",
            r#"package boundary
class ChainDisplay private constructor(private val text: String) {
    constructor(label: Label) : this(label.toString().lowercase())
    fun render(): String = text
}
"#,
        ),
        (
            "PropertyDisplay.kt",
            r#"package boundary
class PropertyDisplay(val label: Label) {
    fun render(): String = label.toString()
}
"#,
        ),
        (
            "NullableView.kt",
            r#"package boundary
class NullableView(private val value: String?) {
    fun render(): String = value.toString()
}
"#,
        ),
        (
            "FailureApi.kt",
            r#"package boundary
fun stop(): Nothing = throw IllegalStateException("custom bottom")
class FailureApi {
    fun render(): String = stop()
}
"#,
        ),
        (
            "Runner.kt",
            r#"package boundary
private fun capture(block: () -> Any?): String = try {
    block().toString()
} catch (error: Throwable) {
    error::class.java.name + ":" + error.message
}

fun main() {
    suspend fun retainedHarness() {}
    val label: Label = DynamicLabel("MiXeD")
    println(listOf(
        Display(label).render(),
        ChainDisplay(label).render(),
        PropertyDisplay(label).render(),
        NullableView(null).render(),
        NullableView("present").render(),
        capture { FailureApi().render() },
        capture { AbsenceApi().render() }
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
        false,
    ))
    .expect("original Kotlin object and nullable string conversions compile and run");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "MiXeD|mixed|MiXeD|null|present|java.lang.IllegalStateException:custom bottom|null"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        fs::create_dir_all(&migrated).unwrap();
        for (name, source) in sources {
            fs::write(migrated.join(name), source).unwrap();
        }
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            migrated.join("Display.java").is_file(),
            "the secondary-constructor class must translate in Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Runner.kt").is_file());
        assert!(migrated.join("PropertyDisplay.java").is_file());
        assert!(
            migrated.join("NullableView.kt").is_file(),
            "the retained nullable caller must remain Kotlin"
        );
        assert!(
            migrated.join("FailureApi.kt").is_file(),
            "the caller of a custom Nothing-returning function must remain Kotlin"
        );
        assert!(!migrated.join("FailureApi.java").exists());
        assert!(migrated.join("AbsenceApi.kt").is_file());
        assert!(!migrated.join("AbsenceApi.java").exists());

        assert!(
            migrated.join("ChainDisplay.java").is_file(),
            "the established String return must allow a chained call"
        );

        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("translated object string conversion compiles and runs");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
