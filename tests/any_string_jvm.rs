use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("notlin-any-string-jvm-{}-{id}", std::process::id()));
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

const SOURCES: [(&str, &str); 12] = [
    (
        "Renderer.kt",
        r#"package sample.render

fun Any.toString(): String = "extension"

class Renderer {
    fun direct(value: Any): String = value.toString()
    fun composed(value: Any): String = "[" + value.toString() + "]"
}
"#,
    ),
    (
        "NullableRenderer.kt",
        r#"package sample.render

class NullableRenderer {
    fun render(value: Any?): String? = value.toString()
}
"#,
    ),
    (
        "ObjectRenderer.kt",
        r#"package sample.render

class ObjectRenderer {
    fun direct(value: java.lang.Object): String = value.toString()
}
"#,
    ),
    (
        "RefinedRenderer.kt",
        r#"package sample.render

class RefinedRenderer {
    fun direct(value: Any): String {
        if (value is NullLabel) return value.toString()
        return value.toString()
    }
}
"#,
    ),
    (
        "Tracked.kt",
        r#"package sample.render

class Tracked(private val text: String) {
    override fun toString(): String {
        Trace.tick()
        return text
    }

    suspend fun retainedHook() {}
}
"#,
    ),
    (
        "RetainedCaller.kt",
        r#"package sample.render

suspend fun retainedCall(renderer: Renderer, value: Any): String = renderer.direct(value)
"#,
    ),
    (
        "ShadowAny.kt",
        r#"package sample.shadow

class Any(private val text: String) {
    override fun toString(): String = "shadow:$text"
    suspend fun retainedHook() {}
}
"#,
    ),
    (
        "ShadowRenderer.kt",
        r#"package sample.shadow

class ShadowRenderer {
    fun render(value: Any): String = value.toString()
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample.runner

import sample.render.JavaCaller
import sample.render.NullLabel
import sample.render.NullableRenderer
import sample.render.ObjectRenderer
import sample.render.RefinedRenderer
import sample.render.Renderer
import sample.render.Trace
import sample.render.Tracked
import sample.render.retainedCall
import sample.shadow.Any as ShadowAny
import sample.shadow.ShadowRenderer

suspend fun main() {
    val renderer = Renderer()
    val tracked = Tracked("dynamic")
    Trace.reset()
    val direct = renderer.direct(tracked)
    val composed = renderer.composed(tracked)
    val retained = retainedCall(renderer, tracked)
    val fromJava = JavaCaller.call(renderer, tracked)
    val objectRenderer = ObjectRenderer()
    val objectValue = JavaCaller.objectCall(objectRenderer, tracked)
    val objectNull = try {
        JavaCaller.objectCall(objectRenderer, NullLabel()) ?: "null"
    } catch (error: Throwable) {
        error.javaClass.name
    }
    val refinedNull = try {
        JavaCaller.refinedCall(RefinedRenderer(), NullLabel()) ?: "null"
    } catch (error: Throwable) {
        error.javaClass.name
    }
    println("$direct|$composed|$retained|$fromJava|$objectValue|${Trace.calls()}|null=${renderer.direct(NullLabel())}|objectNull=$objectNull|refinedNull=$refinedNull|nullable=${NullableRenderer().render(null)}|${ShadowRenderer().render(ShadowAny("value"))}")
}
"#,
    ),
    (
        "Trace.java",
        r#"package sample.render;
public final class Trace {
    private static int calls;
    private Trace() {}
    public static void tick() { calls++; }
    public static int calls() { return calls; }
    public static void reset() { calls = 0; }
}
"#,
    ),
    (
        "JavaCaller.java",
        r#"package sample.render;
public final class JavaCaller {
    private JavaCaller() {}
    public static String call(Renderer renderer, Object value) {
        return renderer.direct(value);
    }
    public static String objectCall(ObjectRenderer renderer, Object value) {
        return renderer.direct(value);
    }
    public static String refinedCall(RefinedRenderer renderer, Object value) {
        return renderer.direct(value);
    }
}
"#,
    ),
    (
        "NullLabel.java",
        r#"package sample.render;
public final class NullLabel {
    @Override public String toString() { return null; }
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
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .arg("--root")
        .arg(root)
        .args(["--annotations", "none", "--in-place"]);
    if lombok_enabled {
        command.arg("--lombok");
    }
    command.arg(root).output().expect("run notlin")
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
    config.run_main_class = Some("sample.runner.RunnerKt".into());
    config
}

#[test]
fn canonical_any_string_calls_preserve_nullable_calls_and_custom_type_dispatch() {
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
        assert!(migrated.join("Renderer.java").is_file());
        assert!(migrated.join("ObjectRenderer.kt").is_file());
        assert!(!migrated.join("ObjectRenderer.java").exists());
        assert!(migrated.join("RefinedRenderer.kt").is_file());
        assert!(!migrated.join("RefinedRenderer.java").exists());
        assert!(migrated.join("NullableRenderer.kt").is_file());
        assert!(!migrated.join("NullableRenderer.java").exists());
        assert!(migrated.join("ShadowAny.kt").is_file());
        assert!(migrated.join("ShadowRenderer.java").is_file());
        assert!(migrated.join("Tracked.kt").is_file());
        assert!(migrated.join("RetainedCaller.kt").is_file());
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_canonical_any_string_dispatch() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        false,
    ))
    .expect("original Kotlin/JVM Any.toString fixture compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "dynamic|[dynamic]|dynamic|dynamic|dynamic|5|null=null|objectNull=java.lang.NullPointerException|refinedNull=java.lang.NullPointerException|nullable=null|shadow:value"
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
        assert!(migrated.join("Renderer.java").is_file());
        assert!(migrated.join("ObjectRenderer.kt").is_file());
        assert!(migrated.join("RefinedRenderer.kt").is_file());
        assert!(migrated.join("Runner.kt").is_file());
        assert!(migrated.join("NullableRenderer.kt").is_file());
        assert!(migrated.join("ShadowRenderer.java").is_file());
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin/Java Any.toString fixture compiles and runs");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
