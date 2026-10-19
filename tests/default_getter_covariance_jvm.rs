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

const GENERIC_SOURCES: [(&str, &str); 11] = [
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
public final class JavaKey implements Key {
    private final String text;
    public JavaKey(String text) { this.text = text; }
    @Override public String value() { return text; }
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
        "Counter.java",
        r#"package sample;
public final class Counter {
    private static int reads;
    private static int created;
    public static JavaKey next(String label) { return new JavaKey(label + (++created)); }
    public static JavaKey read(JavaKey key) { reads++; return key; }
    public static int reads() { return reads; }
    public static int created() { return created; }
    public static void reset() { reads = 0; created = 0; }
}
"#,
    ),
    (
        "GenericProvider.kt",
        r#"package sample
interface GenericProvider<T : Key> : WideView {
    val current: T
    override fun getKey(): T = current
}
"#,
    ),
    (
        "Child.kt",
        r#"package sample
class Child(private val stored: JavaKey) : GenericProvider<JavaKey> {
    override val current: JavaKey get() = Counter.read(stored)
}
"#,
    ),
    (
        "RetainedChild.kt",
        r#"package sample
class RetainedChild(private val stored: JavaKey) : GenericProvider<JavaKey>, Comparable<RetainedChild> {
    override val current: JavaKey get() = Counter.read(stored)
    override fun compareTo(other: RetainedChild): Int = 0
    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Intermediate.kt",
        r#"package sample
interface Intermediate : GenericProvider<JavaKey> {
    suspend fun retainedIntermediateMarker() {}
}
"#,
    ),
    (
        "IndirectRetainedChild.kt",
        r#"package sample
class IndirectRetainedChild(private val stored: JavaKey) : Intermediate {
    override val current: JavaKey get() = Counter.read(stored)
    suspend fun retainedChildMarker() {}
}
"#,
    ),
    (
        "IndirectChild.kt",
        r#"package sample
class IndirectChild(private val stored: JavaKey) : Intermediate {
    override val current: JavaKey get() = Counter.read(stored)
}
"#,
    ),
];

const GENERIC_EXTRA_SOURCES: [(&str, &str); 2] = [
    (
        "JavaChild.java",
        r#"package sample;
public final class JavaChild implements GenericProvider<JavaKey> {
    private final JavaKey current;
    public JavaChild(JavaKey current) { this.current = current; }
    @Override public JavaKey getCurrent() { return Counter.read(current); }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
private fun inspect(label: String, value: GenericProvider<JavaKey>, expected: JavaKey?) {
    val wide: WideView = value
    val generic: GenericProvider<JavaKey> = value
    val fromWide = wide.getKey()
    val fromGeneric = generic.getKey()
    println("$label=${fromWide === expected},${fromGeneric === expected},${fromWide?.value()},${fromGeneric?.value()}")
}
fun main() {
    suspend fun retainedHarness() {}
    Counter.reset()
    val first = Counter.next("child-")
    inspect("child", Child(first), first)
    val second = Counter.next("retained-")
    inspect("retained", RetainedChild(second), second)
    val third = Counter.next("indirect-retained-")
    inspect("indirect-retained", IndirectRetainedChild(third), third)
    val fourth = Counter.next("indirect-")
    inspect("indirect", IndirectChild(fourth), fourth)
    val fifth = Counter.next("java-")
    inspect("java", JavaChild(fifth), fifth)
    inspect("java-null", JavaChild(null), null)
    val descriptors = GenericProvider::class.java.declaredMethods
        .filter { it.name == "getKey" }
        .map { "${it.returnType.simpleName}:${it.isBridge}" }
        .sorted()
        .joinToString(",")
    println("reads=${Counter.reads()}|created=${Counter.created()}|bridge=$descriptors")
}
"#,
    ),
];

fn populate_generic(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in GENERIC_SOURCES.into_iter().chain(GENERIC_EXTRA_SOURCES) {
        fs::write(root.join(name), source).unwrap();
    }
}

#[test]
fn generic_return_with_concrete_first_bound_translates_and_keeps_the_bridge() {
    let scratch = Scratch::new();
    let root = scratch.0.join("generic-cli");
    populate_generic(&root);
    let translated = translate(&root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(root.join("GenericProvider.java").is_file());
    assert!(root.join("Child.java").is_file());
    assert!(root.join("RetainedChild.kt").is_file());
    assert!(root.join("Intermediate.kt").is_file());
    assert!(!root.join("Intermediate.java").exists());
    assert!(root.join("IndirectRetainedChild.kt").is_file());
    assert!(!root.join("IndirectRetainedChild.java").exists());
    // This class has no intrinsic blocker, but the independent retained-supertype
    // rule still keeps it Kotlin. It needs the same accessor repair.
    assert!(!root.join("IndirectChild.java").exists());
    let indirect_child = fs::read_to_string(root.join("IndirectChild.kt")).unwrap();
    assert!(
        indirect_child.contains("override fun getKey(): sample.JavaKey")
            && indirect_child.contains("super<sample.Intermediate>.getKey()"),
        "the inherited retention boundary still needs its concrete getter:\n{indirect_child}"
    );
    let provider = fs::read_to_string(root.join("GenericProvider.java")).unwrap();
    assert!(
        provider.contains("T getKey()"),
        "the generic source return must remain in the interface:\n{provider}"
    );
    let child = fs::read_to_string(root.join("Child.java")).unwrap();
    assert!(
        child.contains("sample.JavaKey getKey()")
            && child.contains("sample.GenericProvider.super.getKey()")
            && !child.contains("notlinProperty"),
        "the translated child needs the concrete return bridge without a property alias:\n{child}"
    );
    let retained = fs::read_to_string(root.join("RetainedChild.kt")).unwrap();
    assert!(
        retained.contains("override fun getKey(): sample.JavaKey")
            && retained.contains("super<sample.GenericProvider>.getKey()"),
        "the retained class needs a narrow bridge to the Java interface:\n{retained}"
    );
    let intermediate = fs::read_to_string(root.join("Intermediate.kt")).unwrap();
    assert!(intermediate.contains("suspend fun retainedIntermediateMarker()"));
    let indirect = fs::read_to_string(root.join("IndirectRetainedChild.kt")).unwrap();
    assert!(
        indirect.contains("override fun getKey(): sample.JavaKey")
            && indirect.contains("super<sample.Intermediate>.getKey()")
            && !indirect.contains("notlinProperty"),
        "the retained child needs a concrete bridge through its direct interface:\n{indirect}"
    );
    let java_child = fs::read_to_string(root.join("JavaChild.java")).unwrap();
    assert!(
        !java_child.contains("getKey()"),
        "the Java implementation should use the inherited default, without an alias accessor:\n{java_child}"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_generic_bound_getter_dispatch() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("generic-original");
    populate_generic(&original);
    let before = validate_jvm_sources(&jvm_config(
        &original,
        scratch.0.join("generic-original-classes"),
        &stdlib,
        false,
    ))
    .expect("original generic Java/Kotlin fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "child=true,true,child-1,child-1\nretained=true,true,retained-2,retained-2\nindirect-retained=true,true,indirect-retained-3,indirect-retained-3\nindirect=true,true,indirect-4,indirect-4\njava=true,true,java-5,java-5\njava-null=true,true,null,null\nreads=12|created=5|bridge=Identifier:true,Key:false"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("generic-migrated-{lombok_enabled}"));
        populate_generic(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("GenericProvider.java").is_file());
        assert!(migrated.join("Child.java").is_file());
        assert!(migrated.join("RetainedChild.kt").is_file());
        assert!(migrated.join("Intermediate.kt").is_file());
        assert!(!migrated.join("Intermediate.java").exists());
        assert!(migrated.join("IndirectRetainedChild.kt").is_file());
        assert!(!migrated.join("IndirectRetainedChild.java").exists());
        assert!(!migrated.join("IndirectChild.java").exists());
        let indirect_child = fs::read_to_string(migrated.join("IndirectChild.kt")).unwrap();
        assert!(
            indirect_child.contains("super<sample.Intermediate>.getKey()"),
            "{indirect_child}"
        );
        let provider = fs::read_to_string(migrated.join("GenericProvider.java")).unwrap();
        assert!(provider.contains("T getKey()"), "{provider}");
        let retained = fs::read_to_string(migrated.join("RetainedChild.kt")).unwrap();
        assert!(
            retained.contains("override fun getKey(): sample.JavaKey")
                && retained.contains("super<sample.GenericProvider>.getKey()"),
            "{retained}"
        );
        let intermediate = fs::read_to_string(migrated.join("Intermediate.kt")).unwrap();
        assert!(intermediate.contains("suspend fun retainedIntermediateMarker()"));
        let indirect = fs::read_to_string(migrated.join("IndirectRetainedChild.kt")).unwrap();
        assert!(
            indirect.contains("override fun getKey(): sample.JavaKey")
                && indirect.contains("super<sample.Intermediate>.getKey()")
                && !indirect.contains("notlinProperty"),
            "the child through a retained generic intermediate needs a concrete bridge:\n{indirect}"
        );
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch
                .0
                .join(format!("generic-mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed generic Java/Kotlin fixture compiles and runs");
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
