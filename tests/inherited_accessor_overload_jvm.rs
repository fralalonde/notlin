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
            "notlin-inherited-accessor-overload-{}-{id}",
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

const SOURCES: [(&str, &str); 5] = [
    (
        "Token.java",
        r#"package sample;
public final class Token {
    private final String value;
    public Token(String value) { this.value = value; }
    public String value() { return value; }
}
"#,
    ),
    (
        "Api.kt",
        r#"package sample
interface Api { var id: String }
"#,
    ),
    (
        "Base.kt",
        r#"package sample
open class Base(override var id: String) : Api
"#,
    ),
    (
        "Child.kt",
        r#"package sample
open class Child(initialId: String) : Base(initialId) {
    fun <T> getId(type: Class<T>): String = id

    fun setId(value: String, revision: Int): String {
        id = value
        return "$value:$revision"
    }
}
"#,
    ),
    ("JavaLeaf.java", JAVA_DESCENDANT),
];

const RETAINED_BASE: &str = r#"package sample
open class Base(override var id: String) : Api {
    suspend fun retainedHook() {}
}
"#;

const JAVA_DESCENDANT: &str = r#"package sample;
public class JavaLeaf extends Child {
    public JavaLeaf(String id) { super(id); }
}
"#;

const RETAINED_CHILD: &str = r#"package sample
open class Child(initialId: String) : Base(initialId) {
    suspend fun retainedChildHook() {}

    fun <T> getId(type: Class<T>): String = id

    fun setId(value: String, revision: Int): String {
        id = value
        return "$value:$revision"
    }
}
"#;

const RUNNER: &str = r#"package sample
fun main() {
    val proxy = JavaProxy("seed")
    val direct: Child = proxy
    val api: Api = proxy
    println("initial=${direct.id}|${api.id}")
    direct.id = "kotlin"
    println("updated=${direct.id}|${api.id}")
    println("java=${JavaCaller.describe(proxy)}")
    println("counts=${JavaProxy.getterCalls()}|${JavaProxy.setterCalls()}")
}
"#;

const RETAINED_RUNNER: &str = r#"package sample
suspend fun retainedHook() {}
fun main() {
    val proxy = JavaProxy("seed")
    val direct: Child = proxy
    val api: Api = proxy
    println("initial=${direct.id}|${api.id}")
    direct.id = "kotlin"
    println("updated=${direct.id}|${api.id}")
    println("java=${JavaCaller.describe(proxy)}")
    println("counts=${JavaProxy.getterCalls()}|${JavaProxy.setterCalls()}")
}
"#;

const EXTERNAL_SOURCES: [(&str, &str); 2] = [
    (
        "JavaProxy.java",
        r#"package sample;
public final class JavaProxy extends JavaLeaf {
    private static int getterCalls;
    private static int setterCalls;
    public JavaProxy(String id) { super(id); }
    @Override public String getId() { getterCalls++; return "proxy(" + super.getId() + ")"; }
    @Override public void setId(String id) { setterCalls++; super.setId("java:" + id); }
    public static int getterCalls() { return getterCalls; }
    public static int setterCalls() { return setterCalls; }
}
"#,
    ),
    (
        "JavaCaller.java",
        r#"package sample;
public final class JavaCaller {
    public static String describe(Child child) {
        String direct = child.getId();
        String typed = child.getId(String.class);
        String overload = child.setId("overloaded", 9);
        child.setId("java");
        String after = child.getId();
        return direct + "|" + typed + "|" + overload + "|" + after;
    }
}
"#,
    ),
];

fn populate(root: &Path, retain_base: bool) {
    let source_root = root.join("sources");
    let external_root = root.join("external");
    fs::create_dir_all(&source_root).unwrap();
    fs::create_dir_all(&external_root).unwrap();
    for (name, source) in SOURCES {
        let source = match (name, retain_base) {
            ("Base.kt", true) => RETAINED_BASE,
            ("Child.kt", true) => RETAINED_CHILD,
            _ => source,
        };
        fs::write(source_root.join(name), source).unwrap();
    }
    if retain_base {
        fs::write(source_root.join("Runner.kt"), RETAINED_RUNNER).unwrap();
    } else {
        fs::write(external_root.join("Runner.kt"), RUNNER).unwrap();
    }
    for (name, source) in EXTERNAL_SOURCES {
        fs::write(external_root.join(name), source).unwrap();
    }
}

fn translate(root: &Path, lombok_enabled: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .arg("--root")
        .arg(root)
        .args(["--in-place", "--annotations", "none"]);
    if lombok_enabled {
        command.arg("--lombok");
    }
    command.arg(root).output().expect("run notlin")
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
    for source_root in [root.join("sources"), root.join("external")] {
        for entry in fs::read_dir(source_root).unwrap() {
            let path = entry.unwrap().path();
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("kt") => config.kotlin_sources.push(path),
                Some("java") => config.java_sources.push(path),
                _ => {}
            }
        }
    }
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

fn assert_translated_shape(root: &Path, retain_base: bool) {
    let source_root = root.join("sources");
    assert!(
        source_root.join("Api.java").is_file(),
        "retained Base={retain_base}: {}",
        fs::read_to_string(source_root.join("Api.kt")).unwrap_or_default()
    );
    assert_eq!(source_root.join("Base.java").is_file(), !retain_base);
    assert_eq!(source_root.join("Base.kt").is_file(), retain_base);
    assert_eq!(source_root.join("Child.java").is_file(), !retain_base);
    assert_eq!(source_root.join("Child.kt").is_file(), retain_base);
    assert!(source_root.join("JavaLeaf.java").is_file());
    let base = if retain_base {
        fs::read_to_string(source_root.join("Base.kt")).unwrap()
    } else {
        fs::read_to_string(source_root.join("Base.java")).unwrap()
    };
    assert!(
        base.contains("getId()") || (!retain_base && base.contains("@Getter")),
        "missing canonical getter:\n{base}"
    );
    assert!(
        base.contains("setId(") || (!retain_base && base.contains("@Setter")),
        "missing canonical setter:\n{base}"
    );
    if retain_base {
        assert!(
            !base.contains("override var id"),
            "retained base property was not repaired:\n{base}"
        );
    }
    let child = if retain_base {
        fs::read_to_string(source_root.join("Child.kt")).unwrap()
    } else {
        fs::read_to_string(source_root.join("Child.java")).unwrap()
    };
    if retain_base {
        assert!(
            child.contains("= getId()") || child.contains("= this.getId()"),
            "bare getter reference was not repaired:\n{child}"
        );
        assert!(
            child.contains("fun setId(value: String, revision: Int)")
                && (child.contains("setId(value)") || child.contains("this.setId(value)")),
            "{child}"
        );
    } else {
        assert!(
            child.contains("getId(java.lang.Class<T> type)")
                || child.contains("getId(Class<T> type)"),
            "{child}"
        );
        assert!(
            child.contains("setId(java.lang.String value, int revision)")
                || child.contains("setId(String value, int revision)"),
            "{child}"
        );
    }
    if retain_base {
        let runner = fs::read_to_string(source_root.join("Runner.kt")).unwrap();
        assert!(runner.contains("direct.getId()"), "{runner}");
        assert!(runner.contains("direct.setId(\"kotlin\")"), "{runner}");
        assert!(runner.contains("api.getId()"), "{runner}");
    }
}

#[test]
fn inherited_property_bridge_ignores_different_arity_accessor_overloads() {
    for retain_base in [false, true] {
        let scratch = Scratch::new();
        let root = scratch.0.join("cli");
        populate(&root, retain_base);
        let source_root = root.join("sources");
        let translated = translate(&source_root, false);
        assert!(
            translated.status.success(),
            "retained Base={retain_base}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert_translated_shape(&root, retain_base);
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_inherited_accessor_overload_dispatch() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    for retain_base in [false, true] {
        let original = scratch.0.join(format!("original-{retain_base}"));
        populate(&original, retain_base);
        let before = validate_jvm_sources(&jvm_config(
            &original,
            scratch.0.join(format!("original-classes-{retain_base}")),
            &stdlib,
            false,
        ))
        .expect("original Kotlin/Java inherited accessor fixture compiles and runs");
        let expected = before
            .commands
            .last()
            .unwrap()
            .stdout
            .trim()
            .replace("\r\n", "\n");
        assert_eq!(
            expected,
            "initial=proxy(seed)|proxy(seed)\nupdated=proxy(java:kotlin)|proxy(java:kotlin)\njava=proxy(java:kotlin)|proxy(java:kotlin)|overloaded:9|proxy(java:java)\ncounts=7|3"
        );

        for lombok_enabled in [false, true] {
            let migrated = scratch
                .0
                .join(format!("migrated-{retain_base}-{lombok_enabled}"));
            populate(&migrated, retain_base);
            let translated = translate(&migrated.join("sources"), lombok_enabled);
            assert!(
                translated.status.success(),
                "retained Base={retain_base}, Lombok mode {lombok_enabled}: {}",
                String::from_utf8_lossy(&translated.stderr)
            );
            assert_translated_shape(&migrated, retain_base);
            let after = validate_jvm_sources(&jvm_config(
                &migrated,
                scratch
                    .0
                    .join(format!("mixed-classes-{retain_base}-{lombok_enabled}")),
                &stdlib,
                lombok_enabled,
            ))
            .expect("mixed Kotlin and Java inherited accessor fixture compiles and runs");
            assert_eq!(
                after
                    .commands
                    .last()
                    .unwrap()
                    .stdout
                    .trim()
                    .replace("\r\n", "\n"),
                expected,
                "mixed runtime output with retained Base={retain_base} in Lombok mode {lombok_enabled}"
            );
        }
    }
}
