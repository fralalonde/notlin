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
            "notlin-reified-class-call-jvm-{}-{id}",
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

const SOURCES: [(&str, &str); 8] = [
    (
        "Holder.kt",
        r#"package sample
class Holder(private val delegate: Store) {
    private var readCount = 0
    val store: Store get() { readCount++; return delegate }
    fun reads(): Int = readCount
    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Context.java",
        r#"package sample;
public interface Context {
    String label();
}
"#,
    ),
    (
        "Store.kt",
        r#"package sample

class Store(private val value: Context?) {
    private var calls = 0
    private var lastType = ""

    fun calls(): Int = calls
    fun lastType(): String = lastType

    fun <Value : Context> find(type: Class<Value>): Value? {
        calls++
        lastType = type.simpleName
        return if (value == null) null else type.cast(value)
    }

    inline fun <reified Result : Context> find(): Result? = find(Result::class.java)

    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Entry.kt",
        r#"package sample

class Entry(private val text: String) : Context {
    override fun label(): String = text
}
"#,
    ),
    (
        "Caller.kt",
        r#"package sample

class Caller {
    fun read(store: Store): Entry? = store.find<Entry>()
    fun inferred(store: Store) = store.find<Entry>()
    fun projected(holder: Holder): Entry? = holder.store.find<Entry>()
}
"#,
    ),
    (
        "Negative.kt",
        r#"package sample

typealias EntryAlias = Entry

class NegativeCaller {
    fun nullable(store: Store?): Entry? = store?.find<Entry>()
    fun aliased(store: Store): EntryAlias? = store.find<EntryAlias>()
    fun wrapped(store: Store): Entry? = Inputs.wrap(store).find<Entry>()
    fun arbitrary(box: Box): Entry? = box.find<Entry>()
}

class Box {
    inline fun <reified T> find(): T? = null
    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample.runner

import sample.Box
import sample.Caller
import sample.Entry
import sample.NegativeCaller
import sample.Store
import sample.Inputs
import sample.Holder

suspend fun main() {
    val caller = Caller()
    val presentStore = Store(Entry("payload"))
    val present = caller.read(presentStore)
    val presentCalls = presentStore.calls()
    val presentType = presentStore.lastType()
    val emptyStore = Store(null)
    val missing = caller.read(emptyStore)
    val emptyCalls = emptyStore.calls()
    val emptyType = emptyStore.lastType()
    val inferredStore = Store(Entry("inferred"))
    val inferred = caller.inferred(inferredStore)
    val projectedStore = Store(Entry("projected"))
    val holder = Holder(projectedStore)
    val projected = caller.projected(holder)
    val negative = NegativeCaller()
    val alias = negative.aliased(presentStore)
    val nullable = negative.nullable(null)
    val arbitrary = negative.arbitrary(Box())
    val wrappedStore = Store(Entry("wrapped"))
    val wrapped = negative.wrapped(wrappedStore)
    println(
        "${present?.label()}|$presentCalls|$presentType|${missing == null}|" +
            "$emptyCalls|$emptyType|${alias?.label()}|${nullable == null}|" +
            "${arbitrary == null}|${wrapped?.label()}|${wrappedStore.calls()}|${Inputs.calls()}|${inferred?.label()}|${inferredStore.calls()}|${projected?.label()}|${holder.reads()}|${projectedStore.calls()}"
    )
}
"#,
    ),
    (
        "Inputs.java",
        r#"package sample;
public final class Inputs {
    private static int calls;
    private Inputs() {}
    public static Store wrap(Store store) { calls++; return store; }
    public static int calls() { return calls; }
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command.arg("--root").arg(root).arg("--in-place");
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
    config
        .classpath
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"));
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
fn reified_class_argument_rewrites_only_the_proven_bounded_call() {
    for lombok_enabled in [false, true] {
        let scratch = Scratch::new();
        populate(&scratch.0);
        let translated = translate(&scratch.0, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(scratch.0.join("Caller.java").is_file());
        let caller = fs::read_to_string(scratch.0.join("Caller.java")).unwrap();
        assert!(
            caller.contains("((sample.Store) store).find(sample.Entry.class)"),
            "{caller}"
        );
        assert!(
            caller.contains("@Nullable sample.Entry inferred("),
            "{caller}"
        );
        assert!(scratch.0.join("Store.kt").is_file());
        assert!(scratch.0.join("Negative.kt").is_file());
        assert!(!scratch.0.join("Negative.java").exists());
        assert!(scratch.0.join("Runner.kt").is_file());
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn reified_class_argument_preserves_mixed_jvm_behavior() {
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
    .expect("original Kotlin/JVM reified-call fixture compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "payload|1|Entry|true|1|Entry|payload|true|true|wrapped|1|1|inferred|1|projected|1|1"
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
        assert!(migrated.join("Caller.java").is_file());
        assert!(migrated.join("Store.kt").is_file());
        assert!(migrated.join("Negative.kt").is_file());
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin/Java reified-call fixture compiles and runs");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
