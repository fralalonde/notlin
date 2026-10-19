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
            "notlin-exact-overloads-jvm-{}-{id}",
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

const SOURCES: [(&str, &str); 6] = [
    (
        "JavaFactory.java",
        r#"package sample;
import java.math.BigDecimal;
public final class JavaFactory {
    private static int stringCalls;
    public static String create(String value) {
        stringCalls++;
        return "null".equals(value) ? null : "string:" + value;
    }
    public static String create(BigDecimal value) { return "decimal:" + value; }
    public static String create(long value) { return "long:" + value; }
    public static String create(int value) { return "int:" + value; }
    public static int stringCalls() { return stringCalls; }
}
"#,
    ),
    (
        "Caller.kt",
        r#"package sample
fun literal(): String = JavaFactory.create("literal")
fun anyToString(value: Any): String = JavaFactory.create(value.toString())
fun strictString(): String = JavaFactory.create("null")
fun nullableString(): String? = JavaFactory.create("null")
fun inferredString() = JavaFactory.create("null")
"#,
    ),
    (
        "Widen.kt",
        r#"package sample
fun javaIntOverloadsRemainKotlin(value: Int) = JavaFactory.create(value)
"#,
    ),
    (
        "KotlinFactory.kt",
        r#"package sample
class KotlinFactory {
    companion object {
        fun create(value: String): String = "kotlin:$value"
        fun create(value: Long): String = "long:$value"
    }

    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "KotlinCaller.kt",
        r#"package sample
fun retainedFactoryText(value: String): String = KotlinFactory.create(value)
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
class Counter {
    var calls: Int = 0
    suspend fun retainedCounter() {}
}
class Probe(private val counter: Counter) {
    suspend fun retainedProbe() {}
    override fun toString(): String {
        counter.calls++
        return "probe"
    }
}

fun main() {
    suspend fun retainedHarness() {}
    val counter = Counter()
    val once = anyToString(Probe(counter))
    val strict = try { strictString(); "no-exception" } catch (failure: Throwable) { failure.javaClass.name }
    println("${literal()}|${anyToString("plain")}|$once|${counter.calls}|$strict|${nullableString()}|${inferredString()}|${javaIntOverloadsRemainKotlin(7)}|${retainedFactoryText("typed")}|${JavaFactory.stringCalls()}")
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
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

fn translate(root: &Path, lombok_enabled: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .arg("--root")
        .arg(root)
        .args(["--annotations", "none", "--in-place"]);
    if lombok_enabled {
        command.arg("--lombok");
    }
    command.arg(root).output().expect("run notlin")
}

#[test]
fn exact_overloads_migrate_only_proven_callers_and_plan_result_checks() {
    for lombok_enabled in [false, true] {
        let scratch = Scratch::new();
        populate(&scratch.0);
        let translated = translate(&scratch.0, lombok_enabled);
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        let caller = fs::read_to_string(scratch.0.join("Caller.java")).unwrap();
        assert!(caller.contains("JavaFactory.create(\"literal\")"));
        assert!(caller.contains("JavaFactory.create(value.toString())"));
        assert!(caller.contains("java.util.Objects.requireNonNull"));
        assert!(scratch.0.join("Widen.kt").is_file());
        assert!(!scratch.0.join("Widen.java").exists());
        assert!(scratch.0.join("KotlinFactory.kt").is_file());
        assert!(scratch.0.join("KotlinCaller.java").is_file());
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn exact_overloads_preserve_mixed_jvm_behavior() {
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
    .expect("original Kotlin/Java overload fixture compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "string:literal|string:plain|string:probe|1|java.lang.NullPointerException|null|null|int:7|kotlin:typed|6"
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

        let caller_java = migrated.join("Caller.java");
        assert!(
            caller_java.is_file(),
            "exact String overload callers should migrate"
        );
        let caller = fs::read_to_string(caller_java).unwrap();
        assert!(caller.contains("JavaFactory.create(\"literal\")"));
        assert!(caller.contains("JavaFactory.create(value.toString())"));
        assert!(migrated.join("Widen.kt").is_file() && !migrated.join("Widen.java").exists());
        assert!(
            migrated.join("KotlinFactory.kt").is_file()
                && !migrated.join("KotlinFactory.java").exists(),
            "suspend factory should retain its Kotlin owner"
        );
        assert!(
            migrated.join("KotlinCaller.java").is_file(),
            "exact typed String call should bind the retained Kotlin factory overload"
        );

        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin/Java overload fixture compiles and runs");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
