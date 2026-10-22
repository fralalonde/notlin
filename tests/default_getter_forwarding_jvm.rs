//! A migrated Java class may keep a Kotlin interface's default getter body
//! when that body is a proven accessor chain rooted at `this`.
//!
//! The Kotlin provider itself stays Kotlin in every assertion here: the point
//! of this slice is that notlin re-creates the default's *source* body instead
//! of delegating to `Provider.super`, which only exists when the provider's
//! Java method semantics are known.

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
            "notlin-default-getter-forwarding-{}-{id}",
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

const SOURCES: [(&str, &str); 9] = [
    (
        "MarkerView.kt",
        r#"package sample

interface MarkerView {
    val text: String
}
"#,
    ),
    (
        "Marker.kt",
        r#"package sample

class Marker(override val text: String) : MarkerView
"#,
    ),
    (
        "Envelope.kt",
        r#"package sample

class Envelope(private val stored: Marker) {
    val marker: Marker
        get() {
            Reads.markerRead()
            return stored
        }
}
"#,
    ),
    (
        "Reads.java",
        r#"package sample;

public final class Reads {
    private static int envelopeReads;
    private static int markerReads;

    private Reads() {}

    public static int envelopeRead() { return ++envelopeReads; }
    public static int markerRead() { return ++markerReads; }
    public static int envelopeReads() { return envelopeReads; }
    public static int markerReads() { return markerReads; }
    public static void reset() { envelopeReads = 0; markerReads = 0; }
}
"#,
    ),
    (
        "WideSource.kt",
        r#"package sample

interface WideSource {
    fun getMarker(): MarkerView
}
"#,
    ),
    (
        // Retained in Kotlin for a reason that adds no JVM member obligation:
        // the annotation metadata cannot be reproduced on a Java interface.
        "DefaultSource.kt",
        r#"package sample

@Deprecated("legacy provider")
interface DefaultSource : WideSource {
    val envelope: Envelope

    override fun getMarker(): Marker = envelope.marker
}
"#,
    ),
    (
        "Subject.kt",
        r#"package sample

class Subject(private val storage: Envelope) : DefaultSource {
    override val envelope: Envelope
        get() {
            Reads.envelopeRead()
            return storage
        }
}
"#,
    ),
    (
        // A retained Kotlin implementor of the same provider, so the JVM also
        // proves that the migrated Java child and retained Kotlin coexist.
        "LegacySubject.kt",
        r#"package sample

class LegacySubject(private val storage: Envelope) : DefaultSource {
    override val envelope: Envelope
        get() {
            Reads.envelopeRead()
            return storage
        }

    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample

private fun inspect(label: String, value: DefaultSource) {
    val wide: WideSource = value
    val narrow: DefaultSource = value
    val fromWide = wide.getMarker()
    val fromNarrow = narrow.getMarker()
    println("$label=${fromWide === fromNarrow},${fromWide.text},${fromNarrow.text}")
}

fun main() {
    Reads.reset()
    val subject = Subject(Envelope(Marker("kept")))
    inspect("subject", subject)
    inspect("legacy", LegacySubject(Envelope(Marker("kept"))))
    println("reflected=${subject::class.java.getMethod("getMarker").returnType.name}")
    println("reads=${Reads.envelopeReads()},${Reads.markerReads()}")
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

fn overload_fixture(root: &Path) {
    populate(root);
    let provider = fs::read_to_string(root.join("DefaultSource.kt"))
        .unwrap()
        .replace(
            "val envelope: Envelope",
            "val envelope: Envelope\n    fun extra(): String = \"base\"",
        );
    fs::write(root.join("DefaultSource.kt"), provider).unwrap();
    let child = fs::read_to_string(root.join("Subject.kt"))
        .unwrap()
        .replace(
            "override val envelope",
            "fun extra(value: Int): Int = value\n    override val envelope",
        );
    fs::write(root.join("Subject.kt"), child).unwrap();
}

fn inherited_obligation_fixture(root: &Path) {
    populate(root);
    let provider = fs::read_to_string(root.join("DefaultSource.kt"))
        .unwrap()
        .replace(
            "@Deprecated",
            "interface Ancestor { fun extra(): String = \"base\" }\n\n@Deprecated",
        )
        .replace(
            "DefaultSource : WideSource",
            "DefaultSource : WideSource, Ancestor",
        );
    fs::write(root.join("DefaultSource.kt"), provider).unwrap();
}

fn translate(root: &Path) -> std::process::Output {
    let mut kotlin = fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect::<Vec<_>>();
    kotlin.sort();
    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(root)
        .args(["--in-place", "--annotations", "none"])
        .args(kotlin)
        .output()
        .expect("run notlin")
}

fn tool(variable: &str, fallback: &str) -> PathBuf {
    if let Some(value) = std::env::var_os(variable) {
        return PathBuf::from(value);
    }
    let home = std::env::var_os("HOME").expect("HOME");
    let path = PathBuf::from(home).join(fallback);
    assert!(
        path.exists(),
        "{variable} is unset and the fallback tool {path:?} does not exist"
    );
    path
}

fn jvm_config(root: &Path, output: PathBuf, default_mode: &str) -> JvmValidationConfig {
    let mut config = JvmValidationConfig::new(
        tool("NOTLIN_KOTLINC", ".rsdk/tools/kotlin/2.4.21/bin/kotlinc"),
        tool("NOTLIN_JAVAC", ".rsdk/tools/java/25.0.4-tem/bin/javac"),
        tool("NOTLIN_JAVA", ".rsdk/tools/java/25.0.4-tem/bin/java"),
        output,
    );
    config.kotlinc_args.extend([
        "-jvm-target".into(),
        "17".into(),
        format!("-jvm-default={default_mode}").into(),
    ]);
    config.javac_args.extend(["--release".into(), "17".into()]);
    config.classpath.push(tool(
        "NOTLIN_KOTLIN_STDLIB",
        ".rsdk/tools/kotlin/2.4.21/lib/kotlin-stdlib.jar",
    ));
    let mut entries = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => config.kotlin_sources.push(path),
            Some("java") => config.java_sources.push(path),
            _ => {}
        }
    }
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

fn expected_output() -> String {
    [
        "subject=true,kept,kept",
        "legacy=true,kept,kept",
        "reflected=sample.Marker",
        "reads=4,4",
    ]
    .join("\n")
}

#[test]
fn the_cli_forwarding_bridge_replaces_java_super_delegation() {
    let scratch = Scratch::new();
    let root = scratch.0.join("cli");
    populate(&root);
    let translated = translate(&root);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(
        root.join("Subject.java").is_file(),
        "the child with a proven forwarding body must migrate"
    );
    assert!(
        root.join("DefaultSource.kt").is_file() && !root.join("DefaultSource.java").exists(),
        "the default provider itself must stay Kotlin in this slice"
    );
    assert!(
        root.join("LegacySubject.kt").is_file() && !root.join("LegacySubject.java").exists(),
        "a retained Kotlin implementor must stay Kotlin"
    );
    let subject = fs::read_to_string(root.join("Subject.java")).unwrap();
    assert!(
        subject.contains("public sample.Marker getMarker()")
            && subject.contains("return this.getEnvelope().getMarker();"),
        "the bridge must re-create the proven accessor chain:\n{subject}"
    );
    assert!(
        !subject.contains("super.getMarker()"),
        "a Kotlin-retained provider has no Java default to delegate to:\n{subject}"
    );
    let provider = fs::read_to_string(root.join("DefaultSource.kt")).unwrap();
    assert!(
        provider.contains("override fun getMarker(): Marker = envelope.marker"),
        "the retained provider keeps its Kotlin default:\n{provider}"
    );
}

#[test]
fn unsupported_unicode_forwarding_accessors_are_retained_not_panicked() {
    let scratch = Scratch::new();
    let root = scratch.0.join("unicode-accessor");
    populate(&root);
    for name in ["DefaultSource.kt", "Subject.kt", "LegacySubject.kt"] {
        let source = fs::read_to_string(root.join(name))
            .unwrap()
            .replace("val envelope:", "val réceptacle:")
            .replace("= envelope.marker", "= réceptacle.marker");
        fs::write(root.join(name), source).unwrap();
    }
    let translated = translate(&root);
    assert!(
        translated.status.success(),
        "unsupported accessor spellings must retain safely: {}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(
        !root.join("Subject.java").exists(),
        "the current ASCII-only forwarding emitter cannot accept this accessor"
    );
}

#[test]
fn a_same_named_overload_does_not_cover_a_retained_default_method() {
    let scratch = Scratch::new();
    let root = scratch.0.join("overload");
    overload_fixture(&root);
    let translated = translate(&root);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(
        !root.join("Subject.java").exists(),
        "an overload does not implement the retained interface's zero-argument default method"
    );
}

#[test]
fn inherited_retained_default_obligations_are_not_ignored() {
    let scratch = Scratch::new();
    let root = scratch.0.join("inherited-obligation");
    inherited_obligation_fixture(&root);
    let translated = translate(&root);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(
        !root.join("Subject.java").exists(),
        "inherited defaults also require a proven JVM implementation"
    );
}

#[test]
#[ignore = "requires the Kotlin compiler, JDK, and Kotlin stdlib on this machine"]
fn real_toolchain_matches_kotlin_across_every_jvm_default_mode() {
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    let migrated = scratch.0.join("migrated");
    populate(&migrated);
    let translated = translate(&migrated);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(migrated.join("Subject.java").is_file());
    assert!(migrated.join("DefaultSource.kt").is_file());

    for mode in ["disable", "enable", "no-compatibility"] {
        let expected = validate_jvm_sources(&jvm_config(
            &original,
            scratch.0.join(format!("original-{mode}")),
            mode,
        ))
        .unwrap_or_else(|error| {
            panic!("original Kotlin fixture must compile and run in -jvm-default={mode}: {error:?}")
        });
        let expected = expected
            .commands
            .last()
            .expect("runtime command")
            .stdout
            .trim()
            .replace("\r\n", "\n");
        assert_eq!(expected, expected_output(), "-jvm-default={mode} baseline");

        let actual = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("migrated-{mode}")),
            mode,
        ))
        .unwrap_or_else(|error| {
            panic!(
                "the migrated Java child must compile and run against the retained Kotlin provider in -jvm-default={mode}: {error:?}"
            )
        });
        let actual = actual
            .commands
            .last()
            .expect("runtime command")
            .stdout
            .trim()
            .replace("\r\n", "\n");
        assert_eq!(
            actual, expected,
            "dispatch, most-specific reflected return and accessor evaluation \
             counts must match the all-Kotlin fixture in -jvm-default={mode}"
        );
    }
}

/// A declined forwarding proof must still produce a buildable mixed
/// Kotlin/Java workspace: the retained child stays Kotlin, and the same
/// dispatch, reflection and accessor-count output holds in every
/// `-jvm-default` mode. This is the safety half of the positive differential
/// above; it runs only where the Kotlin toolchain exists.
#[test]
#[ignore = "requires the Kotlin compiler, JDK, and Kotlin stdlib on this machine"]
fn real_toolchain_retains_unsound_forwarding_fixtures_in_every_mode() {
    type Prepare = fn(&Path);
    let fixtures: [(&str, Prepare); 2] = [
        ("overload", overload_fixture),
        ("inherited-obligation", inherited_obligation_fixture),
    ];
    for (label, prepare) in fixtures {
        let scratch = Scratch::new();
        let original = scratch.0.join("original");
        prepare(&original);
        let migrated = scratch.0.join("migrated");
        prepare(&migrated);

        let translated = translate(&migrated);
        assert!(
            translated.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            !migrated.join("Subject.java").exists(),
            "{label}: an unsound forwarding proof must retain the child"
        );
        assert!(
            migrated.join("Subject.kt").is_file(),
            "{label}: the retained child must stay Kotlin"
        );

        for mode in ["disable", "enable", "no-compatibility"] {
            let expected = validate_jvm_sources(&jvm_config(
                &original,
                scratch.0.join(format!("{label}-original-{mode}")),
                mode,
            ))
            .unwrap_or_else(|error| {
                panic!("{label}: original Kotlin fixture must compile and run in -jvm-default={mode}: {error:?}")
            });
            let expected = expected
                .commands
                .last()
                .expect("runtime command")
                .stdout
                .trim()
                .replace("\r\n", "\n");
            assert_eq!(
                expected,
                expected_output(),
                "{label}: -jvm-default={mode} baseline"
            );

            let actual = validate_jvm_sources(&jvm_config(
                &migrated,
                scratch.0.join(format!("{label}-migrated-{mode}")),
                mode,
            ))
            .unwrap_or_else(|error| {
                panic!(
                    "{label}: the retained Kotlin child must still compile and run beside any migrated Java in -jvm-default={mode}: {error:?}"
                )
            });
            let actual = actual
                .commands
                .last()
                .expect("runtime command")
                .stdout
                .trim()
                .replace("\r\n", "\n");
            assert_eq!(actual, expected, "{label}: -jvm-default={mode}");
        }
    }
}
