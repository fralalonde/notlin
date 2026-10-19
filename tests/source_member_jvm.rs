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
            "notlin-source-member-jvm-{}-{id}",
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

const SOURCES: [(&str, &str); 9] = [
    (
        "Trace.kt",
        r#"package origin
class Trace {
    var calls: String = ""

    fun record(value: String) {
        calls += value
    }
}
"#,
    ),
    (
        "Payload.kt",
        r#"package origin
class Payload(private val trace: Trace) {
    fun getWorkflow(): String {
        trace.record("W")
        return "flow"
    }

    fun markEvent(): Payload {
        trace.record("E")
        return this
    }

    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Provider.kt",
        r#"package origin
class Provider(private val trace: Trace, private val payload: Payload) {
    fun getPayload(): Payload {
        trace.record("P")
        return payload
    }

    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Event.kt",
        r#"package origin
interface Event<T : Payload> {
    fun getPayload(): T

    val workflow: String
        get() = getPayload().getWorkflow()
}
"#,
    ),
    (
        "Created.kt",
        r#"package origin
data class Created(val content: Payload) : Event<Payload> {
    override fun getPayload(): Payload = content.markEvent()
}
"#,
    ),
    (
        "Consumer.kt",
        r#"package client
import origin.Provider
interface Consumer {
    val provider: Provider

    val workflow: String
        get() = provider.getPayload().getWorkflow()

    fun render(): String = provider.getPayload().getWorkflow()
}
"#,
    ),
    (
        "ConsumerImpl.kt",
        r#"package client
import origin.Provider
class ConsumerImpl(override val provider: Provider) : Consumer {
    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "PayloadDecoy.kt",
        r#"package client
class Payload {
    fun getWorkflow(): String = "decoy"
}
"#,
    ),
    (
        "Runner.kt",
        r#"package client
import origin.Payload
import origin.Provider
import origin.Trace
import origin.Created
import origin.Event
fun main() {
    suspend fun retainedHarness() {}
    val trace = Trace()
    val payload = Payload(trace)
    val provider = Provider(trace, payload)
    val consumer: Consumer = ConsumerImpl(provider)
    val created = Created(payload)
    val event: Event<Payload> = created
    println("${consumer.workflow}|${consumer.render()}|${event.workflow}|${created == Created(payload)}|${trace.calls}|${trace.calls.length}")
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

fn add_checked_throws(root: &Path, aliased: bool) {
    let path = root.join("Payload.kt");
    let source = fs::read_to_string(&path).unwrap();
    let (source, annotation) = if aliased {
        (
            source.replace(
                "package origin\n",
                "package origin\nimport kotlin.jvm.Throws as Checked\n",
            ),
            "@Checked(java.io.IOException::class)",
        )
    } else {
        (source, "@kotlin.jvm.Throws(java.io.IOException::class)")
    };
    fs::write(
        path,
        source.replace(
            "    fun getWorkflow()",
            &format!("    {annotation}\n    fun getWorkflow()"),
        ),
    )
    .unwrap();
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

fn assert_translated_shape(root: &Path) {
    assert!(root.join("Consumer.java").is_file());
    assert!(root.join("Event.java").is_file());
    assert!(root.join("Created.java").is_file());
    assert!(root.join("ConsumerImpl.kt").is_file());
    assert!(root.join("Provider.kt").is_file());
    assert!(root.join("Payload.kt").is_file());
    assert!(!root.join("Provider.java").exists());
    // The client-package decoy may translate to Payload.java in this flat
    // fixture. The actual producer's origin.Payload must remain Kotlin.
    if let Ok(payload) = fs::read_to_string(root.join("Payload.java")) {
        assert!(payload.contains("package client;"), "{payload}");
    }

    let event = fs::read_to_string(root.join("Event.java")).unwrap();
    assert!(
        event.contains("getPayload().getWorkflow()"),
        "the generic event getter should preserve the typed source-member chain:\n{event}"
    );

    let consumer = fs::read_to_string(root.join("Consumer.java")).unwrap();
    assert!(
        consumer.contains("getPayload().getWorkflow()"),
        "the migrated default getter and method should retain the typed Kotlin call chain:\n{consumer}"
    );
}

#[test]
fn cli_translates_source_member_callers_but_keeps_kotlin_providers() {
    let scratch = Scratch::new();
    let root = scratch.0.join("sources");
    populate(&root);

    let translated = translate(&root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert_translated_shape(&root);
}

#[test]
fn checked_exception_attributes_keep_callers_kotlin_without_a_java_exception_plan() {
    let scratch = Scratch::new();
    for aliased in [false, true] {
        for lombok in [false, true] {
            let root = scratch.0.join(format!("checked-{aliased}-{lombok}"));
            populate(&root);
            add_checked_throws(&root, aliased);
            let translated = translate(&root, lombok);
            assert!(
                translated.status.success(),
                "{}",
                String::from_utf8_lossy(&translated.stderr)
            );
            assert!(root.join("Consumer.kt").is_file());
            assert!(!root.join("Consumer.java").exists());
            assert!(root.join("Event.kt").is_file());
            assert!(!root.join("Event.java").exists());
        }
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
    config.run_main_class = Some("client.RunnerKt".into());
    config
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_source_member_call_chain() {
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
    .expect("original Kotlin source member calls compile and run");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(expected, "flow|flow|flow|true|PWPWEW|6");

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert_translated_shape(&migrated);

        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Java/Kotlin source member calls compile and run");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("checked-{lombok_enabled}"));
        populate(&migrated);
        add_checked_throws(&migrated, false);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Consumer.kt").is_file());
        assert!(!migrated.join("Consumer.java").exists());
        assert!(migrated.join("Event.kt").is_file());
        assert!(!migrated.join("Event.java").exists());
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("checked-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("checked-exception caller stays Kotlin and the mixed workspace compiles");
        assert_eq!(after.commands.last().unwrap().stdout.trim(), expected);
    }
}
