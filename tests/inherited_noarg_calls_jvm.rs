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
            "notlin-inherited-noarg-calls-{}-{id}",
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

const SOURCES: [(&str, &str); 7] = [
    (
        "Counter.java",
        r#"package sample;
public final class Counter {
    private static final StringBuilder CALLS = new StringBuilder();
    public static String record(String value) {
        if (CALLS.length() > 0) CALLS.append(',');
        CALLS.append(value);
        return value;
    }
    public static String calls() { return CALLS.toString(); }
    public static void reset() { CALLS.setLength(0); }
}
"#,
    ),
    (
        "Root.kt",
        r#"package sample
open class Root {
    open fun getId(): String = Counter.record("root")
    suspend fun retainRoot() {}
}
"#,
    ),
    (
        "Base.kt",
        r#"package sample
open class Base : Root() {
    override fun getId(): String = Counter.record("base")
    suspend fun retainBase() {}
}
"#,
    ),
    (
        "Middle.kt",
        r#"package sample
open class Middle : Base() {
    fun <T> getId(clazz: Class<T>): String = Counter.record("generic")
    suspend fun retainMiddle() {}
}
"#,
    ),
    (
        "Child.kt",
        r#"package sample
open class Child : Middle() {
    fun read(): String = getId()
}
"#,
    ),
    (
        "JavaChild.java",
        r#"package sample;
public final class JavaChild extends Child {
    @Override public String getId() { return Counter.record("java"); }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    Counter.reset()
    println("kotlin=${Child().read()}")
    println("java=${JavaChild().read()}")
    println("calls=${Counter.calls()}")
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
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "kt")
                && path.file_name().is_none_or(|name| name != "Runner.kt")
        })
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
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

fn assert_translation(root: &Path, lombok_enabled: bool) {
    let translated = translate(root, lombok_enabled);
    assert!(
        translated.status.success(),
        "Lombok mode {lombok_enabled}: {}",
        String::from_utf8_lossy(&translated.stderr)
    );
    for retained in ["Root", "Base", "Middle"] {
        assert!(
            root.join(format!("{retained}.kt")).is_file(),
            "{retained} should remain Kotlin"
        );
        assert!(
            !root.join(format!("{retained}.java")).exists(),
            "{retained} should not be emitted as Java"
        );
    }
    assert!(root.join("Child.java").is_file(), "Child should translate");
    assert!(
        root.join("Runner.kt").is_file(),
        "the comparison driver stays Kotlin"
    );
    assert!(!root.join("Child.kt").exists());
    let java = fs::read_to_string(root.join("Child.java")).unwrap();
    assert!(
        java.contains("return getId();"),
        "the bare call should resolve to the inherited no-argument method:\n{java}"
    );
    assert!(
        !java.contains("getId(null)") && !java.contains("getId((Class"),
        "translation must not emit an unsafe generic-overload fallback:\n{java}"
    );
}

#[test]
fn inherited_bare_noarg_call_translates_past_generic_overload() {
    let scratch = Scratch::new();
    let root = scratch.0.join("cli");
    populate(&root);
    assert_translation(&root, false);
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_inherited_bare_noarg_dispatch() {
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
    .expect("original Kotlin fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(expected, "kotlin=base\njava=java\ncalls=base,java");

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        assert_translation(&migrated, lombok_enabled);
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("retained Kotlin parents, translated Child, and Java subclass compile and run");
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
