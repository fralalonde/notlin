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
            "notlin-returned-receiver-string-jvm-{}-{id}",
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

const NOT_NULL: &str = r#"package org.jetbrains.annotations;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
@Retention(RetentionPolicy.CLASS)
@Target({ElementType.METHOD, ElementType.PARAMETER, ElementType.FIELD, ElementType.TYPE_USE})
public @interface NotNull {}
"#;

const NULLABLE: &str = r#"package org.jetbrains.annotations;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
@Retention(RetentionPolicy.CLASS)
@Target({ElementType.METHOD, ElementType.PARAMETER, ElementType.FIELD, ElementType.TYPE_USE})
public @interface Nullable {}
"#;

const POSITIVE: [(&str, &str); 4] = [
    (
        "Provider.java",
        r#"package sample;
import java.util.UUID;
import org.jetbrains.annotations.NotNull;
public interface Provider {
    @NotNull UUID readId();
}
"#,
    ),
    (
        "Trace.kt",
        r#"package sample
class Trace {
    var calls: Int = 0
}
"#,
    ),
    (
        "ProviderImpl.kt",
        r#"package sample
class ProviderImpl(private val trace: Trace) : Provider {
    override fun readId(): java.util.UUID {
        trace.calls += 1
        return java.util.UUID(0L, trace.calls.toLong())
    }

    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Snapshot.kt",
        r#"package sample
data class Snapshot(val value: String) {
    constructor(provider: Provider) : this(provider.readId().toString())
}
"#,
    ),
];

const RUNNER: &str = r#"package sample
fun main() {
    suspend fun retainedHarness() {}
    val trace = Trace()
    val snapshot = Snapshot(ProviderImpl(trace))
    println("${snapshot.value}|${trace.calls}")
}
"#;

fn populate_positive(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("NotNull.java"), NOT_NULL).unwrap();
    for (name, source) in POSITIVE {
        fs::write(root.join(name), source).unwrap();
    }
    fs::write(root.join("Runner.kt"), RUNNER).unwrap();
}

const NULL_SOURCES: [(&str, &str); 7] = [
    (
        "Provider.java",
        r#"package sample;
import java.util.UUID;
public interface Provider {
    UUID readId();
    String readText();
    @org.jetbrains.annotations.Nullable UUID readNullableId();
    @org.jetbrains.annotations.Nullable String readNullableText();
    @org.jetbrains.annotations.NotNull UUID readNotNullId();
    @org.jetbrains.annotations.NotNull String readNotNullText();
}
"#,
    ),
    (
        "NullProvider.java",
        r#"package sample;
public final class NullProvider implements Provider {
    private final StringBuilder calls = new StringBuilder();
    public java.util.UUID readId() { calls.append('u'); return null; }
    public String readText() { calls.append('t'); return null; }
    public java.util.UUID readNullableId() { calls.append('U'); return null; }
    public String readNullableText() { calls.append('T'); return null; }
    public java.util.UUID readNotNullId() { calls.append('n'); return null; }
    public String readNotNullText() { calls.append('N'); return null; }
    public String calls() { return calls.toString(); }
}
"#,
    ),
    (
        "PlatformIdSnapshot.kt",
        r#"package sample
data class PlatformIdSnapshot(val value: String) {
    constructor(provider: Provider) : this(provider.readId().toString())
}
"#,
    ),
    (
        "PlatformTextSnapshot.kt",
        r#"package sample
data class PlatformTextSnapshot(val value: String) {
    constructor(provider: Provider) : this(provider.readText().toString())
}
"#,
    ),
    (
        "NullableSnapshot.kt",
        r#"package sample
data class NullableSnapshot(val uuid: String, val text: String) {
    constructor(provider: Provider) : this(provider.readNullableId().toString(), provider.readNullableText().toString())
}
"#,
    ),
    (
        "StrictIdSnapshot.kt",
        r#"package sample
data class StrictIdSnapshot(val value: String) {
    constructor(provider: Provider) : this(provider.readNotNullId().toString())
}
"#,
    ),
    (
        "StrictTextSnapshot.kt",
        r#"package sample
data class StrictTextSnapshot(val value: String) {
    constructor(provider: Provider) : this(provider.readNotNullText().toString())
}
"#,
    ),
];

const NULL_RUNNER: &str = r#"package sample
fun main() {
    suspend fun retainedHarness() {}
    fun observed(block: () -> String): String = try { block() } catch (failure: Throwable) { failure.javaClass.name }
    val platformId = NullProvider()
    val platformText = NullProvider()
    val nullable = NullProvider()
    val notNullId = NullProvider()
    val notNullText = NullProvider()
    val interpolation = NullProvider()
    val nullableSnapshot = NullableSnapshot(nullable)
    val interpolated = "${interpolation.readId().toString()}"
    println("${observed { PlatformIdSnapshot(platformId).value }}|${platformId.calls()}|${observed { PlatformTextSnapshot(platformText).value }}|${platformText.calls()}|${nullableSnapshot.uuid}|${nullableSnapshot.text}|${nullable.calls()}|${observed { StrictIdSnapshot(notNullId).value }}|${notNullId.calls()}|${observed { StrictTextSnapshot(notNullText).value }}|${notNullText.calls()}|$interpolated|${interpolation.calls()}")
}
"#;

fn populate_null(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("NotNull.java"), NOT_NULL).unwrap();
    for (name, source) in NULL_SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
    fs::write(root.join("Nullable.java"), NULLABLE).unwrap();
    fs::write(root.join("Runner.kt"), NULL_RUNNER).unwrap();
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
    config.run_main_class = Some("sample.RunnerKt".into());
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
fn real_toolchain_preserves_returned_receiver_string_semantics() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();

    let original = scratch.0.join("original-positive");
    populate_positive(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-positive-classes"),
        &stdlib,
        false,
    ))
    .expect("original Kotlin UUID receiver conversion compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(expected, "00000000-0000-0000-0000-000000000001|1");

    for lombok_enabled in [false, true] {
        let migrated = scratch
            .0
            .join(format!("migrated-positive-{lombok_enabled}"));
        populate_positive(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Snapshot.java").is_file());
        assert!(migrated.join("ProviderImpl.kt").is_file());
        let snapshot_java = fs::read_to_string(migrated.join("Snapshot.java")).unwrap();
        assert!(
            snapshot_java.contains("provider.readId().toString()"),
            "the non-null UUID result should lower to a direct Java chain:\n{snapshot_java}"
        );
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch
                .0
                .join(format!("mixed-positive-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Java/Kotlin non-null receiver call compiles and runs");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }

    let original_null = scratch.0.join("original-null");
    populate_null(&original_null);
    let before_null = validate_jvm_sources(&config(
        &original_null,
        scratch.0.join("original-null-classes"),
        &stdlib,
        false,
    ))
    .expect("original Kotlin platform-null receiver calls compile and run");
    let expected_null = before_null
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    assert_eq!(
        expected_null,
        "java.lang.NullPointerException|u|java.lang.NullPointerException|t|null|null|UT|java.lang.NullPointerException|n|java.lang.NullPointerException|N|null|u"
    );

    for lombok_enabled in [false, true] {
        let migrated_null = scratch.0.join(format!("migrated-null-{lombok_enabled}"));
        populate_null(&migrated_null);
        let translated = translate(&migrated_null, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            migrated_null.join("PlatformIdSnapshot.java").is_file()
                && migrated_null.join("PlatformTextSnapshot.java").is_file()
                && migrated_null.join("NullableSnapshot.java").is_file()
                && migrated_null.join("StrictIdSnapshot.java").is_file()
                && migrated_null.join("StrictTextSnapshot.java").is_file(),
            "all direct receiver-return contexts should migrate"
        );
        let platform_uuid_java =
            fs::read_to_string(migrated_null.join("PlatformIdSnapshot.java")).unwrap();
        assert!(
            platform_uuid_java.contains("provider.readId().toString()"),
            "platform UUID return must preserve the direct call's NPE behavior:\n{platform_uuid_java}"
        );
        let platform_text_java =
            fs::read_to_string(migrated_null.join("PlatformTextSnapshot.java")).unwrap();
        assert!(
            platform_text_java.contains("provider.readText().toString()"),
            "platform String return must preserve the direct call's NPE behavior:\n{platform_text_java}"
        );
        let nullable_java =
            fs::read_to_string(migrated_null.join("NullableSnapshot.java")).unwrap();
        assert!(
            nullable_java
                .contains("java.lang.String.valueOf((java.lang.Object) provider.readNullableId())")
                && nullable_java.contains(
                    "java.lang.String.valueOf((java.lang.Object) provider.readNullableText())"
                ),
            "nullable UUID and String returns should use Kotlin-compatible null-to-string conversion:\n{nullable_java}"
        );
        assert!(
            migrated_null.join("Runner.kt").is_file(),
            "interpolation stays as a Kotlin control case"
        );
        let after_null = validate_jvm_sources(&config(
            &migrated_null,
            scratch
                .0
                .join(format!("mixed-null-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin/Java null-safe receiver calls compile and run");
        assert_eq!(
            after_null.commands.last().unwrap().stdout.trim(),
            expected_null,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}

const MEMBER_PRIORITY_SOURCES: [(&str, &str); 11] = [
    (
        "Provider.java",
        r#"package sample;
import java.util.UUID;
public interface Provider {
    UUID readId();
    String readText();
    @org.jetbrains.annotations.NotNull UUID readStrictId();
    @org.jetbrains.annotations.NotNull String readStrictText();
    @org.jetbrains.annotations.Nullable UUID readNullableId();
    @org.jetbrains.annotations.Nullable String readNullableText();
}
"#,
    ),
    (
        "Values.java",
        r#"package sample;
public final class Values implements Provider {
    private final boolean present;
    private final StringBuilder calls = new StringBuilder();
    public Values(boolean present) { this.present = present; }
    public java.util.UUID readId() { calls.append('u'); return present ? new java.util.UUID(0L, 1L) : null; }
    public String readText() { calls.append('t'); return present ? "text" : null; }
    public java.util.UUID readStrictId() { calls.append('U'); return present ? new java.util.UUID(0L, 2L) : null; }
    public String readStrictText() { calls.append('T'); return present ? "strict" : null; }
    public java.util.UUID readNullableId() { calls.append('x'); return present ? new java.util.UUID(0L, 3L) : null; }
    public String readNullableText() { calls.append('y'); return present ? "maybe" : null; }
    public String calls() { return calls.toString(); }
}
"#,
    ),
    (
        "UuidWildcard.kt",
        r#"package extension.uuidwildcard
import java.util.UUID
var calls = 0
fun UUID?.toString(): String { calls++; return "uuid-wildcard" }
suspend fun retainedMarker() {}
"#,
    ),
    (
        "TextWildcard.kt",
        r#"package extension.textwildcard
var calls = 0
fun String?.toString(): String { calls++; return "text-wildcard" }
suspend fun retainedMarker() {}
"#,
    ),
    (
        "UuidExplicit.kt",
        r#"package extension.uuidexplicit
import java.util.UUID
var calls = 0
fun UUID.toString(): String { calls++; return "uuid-explicit" }
suspend fun retainedMarker() {}
"#,
    ),
    (
        "TextExplicit.kt",
        r#"package extension.textexplicit
var calls = 0
fun String.toString(): String { calls++; return "text-explicit" }
suspend fun retainedMarker() {}
"#,
    ),
    (
        "PlatformUuidSnapshot.kt",
        r#"package sample
import extension.uuidwildcard.*
data class PlatformUuidSnapshot(val value: String) {
    constructor(java: Provider) : this(java.readId().toString())
}
"#,
    ),
    (
        "PlatformTextSnapshot.kt",
        r#"package sample
import extension.textwildcard.*
data class PlatformTextSnapshot(val value: String) {
    constructor(java: Provider) : this(java.readText().toString())
}
"#,
    ),
    (
        "StrictUuidSnapshot.kt",
        r#"package sample
import extension.uuidexplicit.toString
data class StrictUuidSnapshot(val value: String) {
    constructor(java: Provider) : this(java.readStrictId().toString())
}
"#,
    ),
    (
        "StrictTextSnapshot.kt",
        r#"package sample
import extension.textexplicit.toString
data class StrictTextSnapshot(val value: String) {
    constructor(java: Provider) : this(java.readStrictText().toString())
}
"#,
    ),
    (
        "NullableSnapshot.kt",
        r#"package sample
import extension.uuidwildcard.*
import extension.textwildcard.*
data class NullableSnapshot(val uuid: String, val text: String) {
    constructor(java: Provider) : this(java.readNullableId().toString(), java.readNullableText().toString())
}
"#,
    ),
];

const MEMBER_PRIORITY_RUNNER: &str = r#"package sample
fun main() {
    suspend fun retainedHarness() {}
    fun observed(block: () -> String): String = try { block() } catch (failure: Throwable) { failure.javaClass.name }
    fun runCase(present: Boolean) {
        extension.uuidwildcard.calls = 0
        extension.textwildcard.calls = 0
        extension.uuidexplicit.calls = 0
        extension.textexplicit.calls = 0
        val provider = Values(present)
        val platformUuid = observed { PlatformUuidSnapshot(provider).value }
        val platformText = observed { PlatformTextSnapshot(provider).value }
        val strictUuid = observed { StrictUuidSnapshot(provider).value }
        val strictText = observed { StrictTextSnapshot(provider).value }
        val nullable = NullableSnapshot(provider)
        println("$present|$platformUuid|$platformText|$strictUuid|$strictText|${nullable.uuid}|${nullable.text}|${provider.calls()}|${extension.uuidwildcard.calls}|${extension.textwildcard.calls}|${extension.uuidexplicit.calls}|${extension.textexplicit.calls}")
    }
    runCase(true)
    runCase(false)
}
"#;

fn populate_member_priority(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("NotNull.java"), NOT_NULL).unwrap();
    fs::write(root.join("Nullable.java"), NULLABLE).unwrap();
    for (name, source) in MEMBER_PRIORITY_SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
    fs::write(root.join("Runner.kt"), MEMBER_PRIORITY_RUNNER).unwrap();
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_keeps_platform_string_calls_on_the_member_path() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original-member-priority");
    populate_member_priority(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-member-priority-classes"),
        &stdlib,
        false,
    ))
    .expect("original extension/member priority fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "true|00000000-0000-0000-0000-000000000001|text|00000000-0000-0000-0000-000000000002|strict|uuid-wildcard|text-wildcard|utUTxy|1|1|0|0\nfalse|java.lang.NullPointerException|java.lang.NullPointerException|java.lang.NullPointerException|java.lang.NullPointerException|uuid-wildcard|text-wildcard|utUTxy|1|1|0|0"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch
            .0
            .join(format!("migrated-member-priority-{lombok_enabled}"));
        populate_member_priority(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        for class in [
            "PlatformUuidSnapshot",
            "PlatformTextSnapshot",
            "StrictUuidSnapshot",
            "StrictTextSnapshot",
        ] {
            assert!(
                migrated.join(format!("{class}.java")).is_file(),
                "{class} should use the direct JVM member call"
            );
        }
        assert!(
            migrated.join("NullableSnapshot.kt").is_file()
                && !migrated.join("NullableSnapshot.java").exists(),
            "nullable calls with imported custom extensions must remain Kotlin"
        );
        for (class, call) in [
            ("PlatformUuidSnapshot", "java.readId().toString()"),
            ("PlatformTextSnapshot", "java.readText().toString()"),
            ("StrictUuidSnapshot", "java.readStrictId().toString()"),
            ("StrictTextSnapshot", "java.readStrictText().toString()"),
        ] {
            let java = fs::read_to_string(migrated.join(format!("{class}.java"))).unwrap();
            assert!(
                java.contains(call),
                "{class} should invoke the member exactly once without a java.lang qualifier:\n{java}"
            );
        }
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch
                .0
                .join(format!("mixed-member-priority-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed original Kotlin and migrated Java member paths compile and run");
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
