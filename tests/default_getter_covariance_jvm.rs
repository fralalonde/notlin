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
            "notlin-default-getter-covariance-{}-{id}",
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

const SOURCES: [(&str, &str); 11] = [
    (
        "Identifier.java",
        r#"package sample;
public interface Identifier { String value(); }
"#,
    ),
    (
        "Key.java",
        r#"package sample;
public interface Key extends Identifier {}
"#,
    ),
    (
        "JavaKey.java",
        r#"package sample;
public final class JavaKey implements Key, Identifier {
    private final String value;
    public JavaKey(String value) { this.value = value; }
    @Override public String value() { return value; }
}
"#,
    ),
    (
        "WideView.java",
        r#"package sample;
public interface WideView { Identifier getKey(); }
"#,
    ),
    (
        "NarrowView.java",
        r#"package sample;
public interface NarrowView { Key getKey(); }
"#,
    ),
    (
        "Counter.java",
        r#"package sample;
public final class Counter {
    private static int value;
    public static Key next() { return new JavaKey(Integer.toString(++value)); }
    public static int value() { return value; }
    public static void reset() { value = 0; }
}
"#,
    ),
    (
        "JavaChild.java",
        r#"package sample;
public final class JavaChild implements CombinedView {}
"#,
    ),
    (
        "CombinedView.kt",
        r#"package sample
interface CombinedView : WideView, NarrowView {
    override fun getKey(): Key = Counter.next()
}
"#,
    ),
    (
        "KotlinChild.kt",
        r#"package sample
class KotlinChild : CombinedView
"#,
    ),
    (
        "RetainedChild.kt",
        r#"package sample
class RetainedChild : CombinedView {
    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun inspect(label: String, value: CombinedView) {
    val wide: WideView = value
    val narrow: NarrowView = value
    println("$label=${wide.getKey().value()},${narrow.getKey().value()},${value.getKey().value()}")
}
fun main() {
    suspend fun retainedHarness() {}
    Counter.reset()
    inspect("kotlin", KotlinChild())
    inspect("retained", RetainedChild())
    inspect("java", JavaChild())
    val descriptors = CombinedView::class.java.declaredMethods
        .filter { it.name == "getKey" }
        .map { "${it.returnType.simpleName}:${it.isBridge}" }
        .sorted()
        .joinToString(",")
    println("calls=${Counter.value()}|bridge=$descriptors")
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

#[test]
fn covariant_default_getter_translates_with_a_jvm_bridge() {
    let scratch = Scratch::new();
    let root = scratch.0.join("cli");
    populate(&root);
    let translated = translate(&root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(root.join("CombinedView.java").is_file());
    assert!(root.join("KotlinChild.java").is_file());
    assert!(root.join("RetainedChild.kt").is_file());
    assert!(!root.join("RetainedChild.java").exists());
    let java = fs::read_to_string(root.join("CombinedView.java")).unwrap();
    assert!(
        java.contains("default Key getKey()"),
        "the concrete narrow getter should be emitted as an interface default:\n{java}"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_covariant_default_getter_dispatch() {
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
    .expect("original mixed covariant Java/Kotlin fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "kotlin=1,2,3\nretained=4,5,6\njava=7,8,9\ncalls=9|bridge=Identifier:true,Key:false"
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
        assert!(migrated.join("CombinedView.java").is_file());
        assert!(migrated.join("KotlinChild.java").is_file());
        assert!(migrated.join("RetainedChild.kt").is_file());
        let java = fs::read_to_string(migrated.join("CombinedView.java")).unwrap();
        assert!(
            java.contains("default Key getKey()"),
            "translated default must keep the narrow descriptor:\n{java}"
        );
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed retained Kotlin and translated Java children compile and run");
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

const PROPERTY_SOURCES: [(&str, &str); 11] = [
    (
        "Identifier.kt",
        r#"package sample
interface Identifier { fun value(): String }
"#,
    ),
    (
        "Key.kt",
        r#"package sample
interface Key : Identifier
"#,
    ),
    (
        "WideView.kt",
        r#"package sample
interface WideView { val key: Identifier }
"#,
    ),
    (
        "NarrowView.kt",
        r#"package sample
interface NarrowView { val key: Key }
"#,
    ),
    (
        "CombinedView.kt",
        r#"package sample
interface CombinedView : WideView, NarrowView {
    override val key: Key get() = Counter.next()
}
"#,
    ),
    (
        "Child.kt",
        r#"package sample
class Child : CombinedView
"#,
    ),
    (
        "RetainedChild.kt",
        r#"package sample
class RetainedChild : CombinedView {
    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun inspect(label: String, value: CombinedView) {
    val wide: WideView = value
    val narrow: NarrowView = value
    println("$label=${wide.key.value()},${narrow.key.value()},${value.key.value()}")
}
fun main() {
    suspend fun retainedHarness() {}
    Counter.reset()
    inspect("child", Child())
    inspect("retained", RetainedChild())
    inspect("java", JavaChild())
    val descriptors = CombinedView::class.java.declaredMethods
        .filter { it.name == "getKey" }
        .map { "${it.returnType.simpleName}:${it.isBridge}" }
        .sorted()
        .joinToString(",")
    println("calls=${Counter.value()}|bridge=$descriptors")
}
"#,
    ),
    (
        "ValueKey.java",
        r#"package sample;
public final class ValueKey implements Key {
    private final String text;
    public ValueKey(String text) { this.text = text; }
    @Override public String value() { return text; }
}
"#,
    ),
    (
        "Counter.java",
        r#"package sample;
public final class Counter {
    private static int calls;
    public static Key next() { return new ValueKey(Integer.toString(++calls)); }
    public static int value() { return calls; }
    public static void reset() { calls = 0; }
}
"#,
    ),
    (
        "JavaChild.java",
        r#"package sample;
public final class JavaChild implements CombinedView {}
"#,
    ),
];

fn populate_property(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in PROPERTY_SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
}

#[test]
fn covariant_property_default_translates_with_a_jvm_bridge() {
    let scratch = Scratch::new();
    let root = scratch.0.join("property-cli");
    populate_property(&root);
    let translated = translate(&root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    for class in ["WideView", "NarrowView", "CombinedView", "Child"] {
        assert!(
            root.join(format!("{class}.java")).is_file(),
            "{class} should translate to Java"
        );
    }
    assert!(root.join("RetainedChild.kt").is_file());
    let java = fs::read_to_string(root.join("CombinedView.java")).unwrap();
    assert!(
        java.contains("default Key getKey()"),
        "the property getter should emit the narrow interface default:\n{java}"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_covariant_property_default_dispatch() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("property-original");
    populate_property(&original);
    let before = validate_jvm_sources(&jvm_config(
        &original,
        scratch.0.join("property-original-classes"),
        &stdlib,
        false,
    ))
    .expect("original Kotlin covariant property fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "child=1,2,3\nretained=4,5,6\njava=7,8,9\ncalls=9|bridge=Identifier:true,Key:false"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch
            .0
            .join(format!("property-migrated-{lombok_enabled}"));
        populate_property(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        for class in ["WideView", "NarrowView", "CombinedView", "Child"] {
            assert!(
                migrated.join(format!("{class}.java")).is_file(),
                "{class} should translate to Java"
            );
        }
        assert!(migrated.join("RetainedChild.kt").is_file());
        let java = fs::read_to_string(migrated.join("CombinedView.java")).unwrap();
        assert!(
            java.contains("default Key getKey()"),
            "translated property default should keep its narrow return:\n{java}"
        );
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch
                .0
                .join(format!("property-mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin and Java property-default children compile and run");
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
