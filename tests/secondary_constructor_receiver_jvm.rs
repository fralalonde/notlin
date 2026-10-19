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
            "notlin-secondary-constructor-receiver-{}-{id}",
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
        let lombok = std::env::var_os("NOTLIN_LOMBOK").expect("NOTLIN_LOMBOK");
        config.classpath.push(PathBuf::from(lombok));
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
    config.run_main_class = Some("boundary.RunnerKt".into());
    config
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_secondary_constructor_property_receiver() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();

    let sources = [
        (
            "Token.kt",
            r#"package boundary
import java.util.UUID

class Token {
    var count: Int = 0
    fun read(): UUID {
        count += 1
        return UUID(0L, count.toLong())
    }
    suspend fun retainedHook() {}
}
"#,
        ),
        (
            "Position.kt",
            r#"package boundary
interface Position {
    val token: Token
}
"#,
        ),
        (
            "PositionImpl.kt",
            r#"package boundary
class PositionImpl(override val token: Token) : Position
"#,
        ),
        (
            "Snapshot.kt",
            r#"package boundary
import java.util.UUID

class Snapshot private constructor(val first: UUID, val second: UUID) {
    var marker: Int = 0
    constructor(position: Position) : this(copy(position.token.read()), copy(position.token.read())) {
        this.marker = 7
    }

    companion object {
        fun copy(value: UUID): UUID = value
    }
}
"#,
        ),
        (
            "Runner.kt",
            r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val token = Token()
    val snapshot = Snapshot(PositionImpl(token))
    println(snapshot.first.toString() + "|" + snapshot.second.toString() + "|" + token.count + "|" + snapshot.marker)
}
"#,
        ),
    ];
    for (name, source) in sources {
        fs::write(original.join(name), source).unwrap();
    }

    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original classes"),
        &stdlib,
        false,
    ))
    .expect("original Kotlin secondary constructor compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "00000000-0000-0000-0000-000000000001|00000000-0000-0000-0000-000000000002|2|7"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        fs::create_dir_all(&migrated).unwrap();
        for (name, source) in sources {
            fs::write(migrated.join(name), source).unwrap();
        }
        let mut translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
        translation
            .arg("--root")
            .arg(&migrated)
            .args(["--annotations", "none", "--in-place"]);
        if lombok_enabled {
            translation.arg("--lombok");
        }
        let translated = translation.arg(&migrated).output().unwrap();
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            migrated.join("Snapshot.java").is_file(),
            "Snapshot must translate in Lombok mode {lombok_enabled}: {}\nSnapshot.kt:\n{}",
            String::from_utf8_lossy(&translated.stderr),
            fs::read_to_string(migrated.join("Snapshot.kt")).unwrap_or_default()
        );
        assert!(migrated.join("Token.kt").is_file());
        assert!(migrated.join("Runner.kt").is_file());

        let snapshot_java = fs::read_to_string(migrated.join("Snapshot.java")).unwrap();
        assert!(
            snapshot_java.contains("position.getToken().read()"),
            "secondary constructor must resolve the property receiver and method return type:\n{snapshot_java}"
        );
        assert!(
            snapshot_java.contains("copy("),
            "the companion helper should be available to the generated constructor:\n{snapshot_java}"
        );
        assert!(
            snapshot_java
                .lines()
                .any(|line| line.contains("static") && line.contains("copy(")),
            "the ordinary-class companion helper should be emitted as a static method:\n{snapshot_java}"
        );

        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("translated Java constructor and retained Kotlin token compile and run");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_retains_secondary_constructor_with_non_getter_property_abi() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let cases = [
        (
            "renamed-getter",
            r#"package boundary
import kotlin.jvm.JvmName

class Location(@get:JvmName("getRenamedToken") val token: Token) {
    suspend fun retainedHook() {}
}
"#,
        ),
        (
            "public-field",
            r#"package boundary
class Location(@JvmField val token: Token) {
    suspend fun retainedHook() {}
}
"#,
        ),
    ];

    for (case_name, location_source) in cases {
        let scratch = Scratch::new();
        let original = scratch.0.join("original");
        fs::create_dir_all(&original).unwrap();
        let sources = [
            (
                "Token.kt",
                r#"package boundary
import java.util.UUID

class Token {
    var count: Int = 0
    fun read(): UUID {
        count += 1
        return UUID(0L, count.toLong())
    }
    suspend fun retainedHook() {}
}
"#,
            ),
            ("Location.kt", location_source),
            (
                "Snapshot.kt",
                r#"package boundary
import java.util.UUID

class Snapshot private constructor(val first: UUID, val second: UUID) {
    var marker: Int = 0
    constructor(position: Location) : this(position.token.read(), position.token.read()) {
        this.marker = 7
    }
}
"#,
            ),
            (
                "Runner.kt",
                r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val token = Token()
    val snapshot = Snapshot(Location(token))
    println(snapshot.first.toString() + "|" + snapshot.second.toString() + "|" + token.count + "|" + snapshot.marker)
}
"#,
            ),
        ];
        for (name, source) in sources {
            fs::write(original.join(name), source).unwrap();
        }

        let before = validate_jvm_sources(&config(
            &original,
            scratch.0.join(format!("original classes-{case_name}")),
            &stdlib,
            false,
        ))
        .unwrap_or_else(|error| panic!("original {case_name} Kotlin fixture failed: {error}"));
        let expected = before.commands.last().unwrap().stdout.trim().to_owned();
        assert_eq!(
            expected,
            "00000000-0000-0000-0000-000000000001|00000000-0000-0000-0000-000000000002|2|7",
            "original Kotlin behavior for {case_name}"
        );

        for lombok_enabled in [false, true] {
            let migrated = scratch
                .0
                .join(format!("migrated-{case_name}-{lombok_enabled}"));
            fs::create_dir_all(&migrated).unwrap();
            for (name, source) in sources {
                fs::write(migrated.join(name), source).unwrap();
            }
            let mut translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
            translation
                .arg("--root")
                .arg(&migrated)
                .args(["--annotations", "none", "--in-place"]);
            if lombok_enabled {
                translation.arg("--lombok");
            }
            let translated = translation.arg(&migrated).output().unwrap();
            assert!(
                translated.status.success(),
                "{case_name}, Lombok mode {lombok_enabled}: {}",
                String::from_utf8_lossy(&translated.stderr)
            );
            assert!(
                migrated.join("Snapshot.kt").is_file() && !migrated.join("Snapshot.java").exists(),
                "Snapshot must remain Kotlin for {case_name}, Lombok mode {lombok_enabled}: {}\nSnapshot.kt:\n{}",
                String::from_utf8_lossy(&translated.stderr),
                fs::read_to_string(migrated.join("Snapshot.kt")).unwrap_or_default()
            );

            let after = validate_jvm_sources(&config(
                &migrated,
                scratch
                    .0
                    .join(format!("mixed classes-{case_name}-{lombok_enabled}")),
                &stdlib,
                lombok_enabled,
            ))
            .unwrap_or_else(|error| {
                panic!("mixed {case_name} fixture failed in Lombok mode {lombok_enabled}: {error}")
            });
            assert_eq!(
                after.commands.last().unwrap().stdout.trim(),
                expected,
                "mixed runtime output for {case_name} in Lombok mode {lombok_enabled}"
            );
        }
    }
}
