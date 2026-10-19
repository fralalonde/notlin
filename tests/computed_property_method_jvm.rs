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
            "notlin-computed-property-method-{}-{id}",
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

const SOURCES: [(&str, &str); 10] = [
    (
        "Entity.java",
        r#"package sample;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface Entity {}
"#,
    ),
    (
        "Identifier.java",
        r#"package sample;
public interface Identifier { String label(); }
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
    private final String label;
    public JavaKey(String label) { this.label = label; }
    @Override public String label() { return label; }
}
"#,
    ),
    (
        "JavaView.java",
        r#"package sample;
public interface JavaView { Identifier getValue(); }
"#,
    ),
    (
        "NarrowView.java",
        r#"package sample;
public interface NarrowView extends JavaView { @Override Key getValue(); }
"#,
    ),
    (
        "Counter.java",
        r#"package sample;
public final class Counter {
    private static int calls;
    public static Key next() { return new JavaKey(Integer.toString(++calls)); }
    public static int calls() { return calls; }
    public static void reset() { calls = 0; }
}
"#,
    ),
    (
        "Api.kt",
        r#"package sample
interface Api { val value: Identifier }
"#,
    ),
    (
        "Computed.kt",
        r#"package sample
class Computed : Api {
    override val value: Key get() = Counter.next()
}
"#,
    ),
    (
        "RetainedComputed.kt",
        r#"package sample
@Entity
open class RetainedComputed : Api {
    override val value: Key get() = Counter.next()
    suspend fun retainedMarker() {}
}
"#,
    ),
];

const EXTERNAL_SOURCES: [(&str, &str); 2] = [
    (
        "JavaProxy.java",
        r#"package sample;
public final class JavaProxy extends RetainedComputed implements NarrowView {
    private static int calls;
    @Override public Key getValue() { return new JavaKey("proxy-" + (++calls)); }
    public static int calls() { return calls; }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    Counter.reset()
    val computed = Computed()
    val api: Api = computed
    println("computed=${computed.value.label()},${api.value.label()}")

    val proxy = JavaProxy()
    val retained: RetainedComputed = proxy
    val retainedValue: Key = retained.value
    val retainedApi: Api = proxy
    val apiValue: Identifier = retainedApi.value
    val wide: JavaView = proxy
    val narrow: NarrowView = proxy
    println("proxy=${retainedValue.label()},${apiValue.label()},${wide.getValue().label()},${narrow.getValue().label()}|proxyCalls=${JavaProxy.calls()}|computedCalls=${Counter.calls()}")
}
"#,
    ),
];

fn populate(root: &Path) {
    let source_root = root.join("sources");
    let external_root = root.join("external");
    fs::create_dir_all(&source_root).unwrap();
    fs::create_dir_all(&external_root).unwrap();
    for (name, source) in SOURCES {
        fs::write(source_root.join(name), source).unwrap();
    }
    for (name, source) in EXTERNAL_SOURCES {
        fs::write(external_root.join(name), source).unwrap();
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

#[test]
fn storage_free_computed_property_emits_an_ordinary_override_getter() {
    let scratch = Scratch::new();
    let root = scratch.0.join("cli");
    populate(&root);
    let source_root = root.join("sources");
    let translated = translate(&source_root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(source_root.join("Api.java").is_file());
    assert!(source_root.join("Computed.java").is_file());
    assert!(source_root.join("RetainedComputed.kt").is_file());
    assert!(!source_root.join("RetainedComputed.java").exists());
    let java = fs::read_to_string(source_root.join("Computed.java")).unwrap();
    assert!(java.contains("getValue()"), "missing getter:\n{java}");
    assert!(
        java.contains("public Key getValue()"),
        "wrong getter contract:\n{java}"
    );
    assert!(
        !java.contains("notlinProperty"),
        "computed property should not need a backing alias:\n{java}"
    );
    let retained = fs::read_to_string(source_root.join("RetainedComputed.kt")).unwrap();
    assert!(
        retained.contains("override fun getValue"),
        "retained computed property should use the inherited Java getter ABI:\n{retained}"
    );
    assert!(
        !retained.contains("val value") && !retained.contains("notlinProperty"),
        "retained computed property should not keep a Kotlin property alias:\n{retained}"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_computed_property_getter_dispatch() {
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
    .expect("original mixed Kotlin/Java computed getter fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "computed=1,2\nproxy=proxy-1,proxy-2,proxy-3,proxy-4|proxyCalls=4|computedCalls=2"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        let source_root = migrated.join("sources");
        let translated = translate(&source_root, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(source_root.join("Api.java").is_file());
        assert!(source_root.join("Computed.java").is_file());
        assert!(source_root.join("RetainedComputed.kt").is_file());
        assert!(!source_root.join("RetainedComputed.java").exists());
        let java = fs::read_to_string(source_root.join("Computed.java")).unwrap();
        assert!(java.contains("getValue()"), "missing getter:\n{java}");
        assert!(
            !java.contains("notlinProperty"),
            "computed getter should not use a property alias:\n{java}"
        );
        let retained = fs::read_to_string(source_root.join("RetainedComputed.kt")).unwrap();
        assert!(
            retained.contains("override fun getValue"),
            "retained computed property should use the inherited Java getter ABI:\n{retained}"
        );
        assert!(
            !retained.contains("val value") && !retained.contains("notlinProperty"),
            "retained property should not keep a Kotlin property alias:\n{retained}"
        );
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed retained Kotlin and translated Java fixture compiles and runs");
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
