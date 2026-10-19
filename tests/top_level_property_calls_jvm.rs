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
            "notlin-top-level-property-jvm-{}-{id}",
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

const SOURCES: [(&str, &str); 6] = [
    (
        "State.kt",
        r#"package sample.state
var count = 0
val label = "state"
suspend fun sameFileStep(): String {
    val starting = count
    val old = count++
    count += StepSource.amount(2)
    return "$starting:$old:${count}"
}
"#,
    ),
    (
        "Peer.kt",
        r#"package sample.state
suspend fun samePackageStep(): String {
    val starting = count
    val old = count++
    count += StepSource.amount(2)
    return "$starting:$old:${count}"
}
"#,
    ),
    (
        "AliasUse.kt",
        r#"package sample.alias
import sample.state.StepSource
import sample.state.count as total
import sample.state.label as caption
suspend fun aliasedStep(): String {
    val starting = total
    val old = total++
    total += StepSource.amount(3)
    return "${caption}:$starting:$old:${total}"
}
"#,
    ),
    (
        "QualifiedUse.kt",
        r#"package sample.qualified
suspend fun qualifiedStep(): String {
    val starting = sample.state.count
    val old = sample.state.count++
    sample.state.count += sample.state.StepSource.amount(4)
    return "$starting:$old:${sample.state.count}"
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample.runner
import sample.alias.aliasedStep
import sample.qualified.qualifiedStep
import sample.state.StepSource
import sample.state.count as finalCount
import sample.state.label as caption
import sample.state.sameFileStep
import sample.state.samePackageStep
suspend fun main() {
    println("${caption}|${sameFileStep()}|${samePackageStep()}|${aliasedStep()}|${qualifiedStep()}|${finalCount}|${StepSource.calls()}")
}
"#,
    ),
    (
        "StepSource.java",
        r#"package sample.state;
public final class StepSource {
    private static int calls;
    public static int amount(int value) { calls++; return value; }
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
    config.run_main_class = Some("sample.runner.RunnerKt".into());
    config
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_migrated_top_level_property_accesses() {
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
    .expect("original Kotlin/JVM top-level property fixture compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(expected, "state|0:0:3|3:3:6|state:6:6:10|10:10:15|15|4");

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        let java = fs::read_to_string(migrated.join("State.java"))
            .expect("plain top-level fields move into a Java facade");
        assert!(java.contains("public static int count"), "{java}");
        assert!(java.contains("public static final String label"), "{java}");
        for retained in [
            "State.kt",
            "Peer.kt",
            "AliasUse.kt",
            "QualifiedUse.kt",
            "Runner.kt",
        ] {
            assert!(
                migrated.join(retained).is_file(),
                "{retained} should remain Kotlin"
            );
        }
        let same_file = fs::read_to_string(migrated.join("State.kt")).unwrap();
        assert!(
            same_file.contains("State.count++") && same_file.contains("State.count +="),
            "same-file suspend helper should be repaired to the Java static field:\n{same_file}"
        );
        let peer = fs::read_to_string(migrated.join("Peer.kt")).unwrap();
        assert!(
            peer.contains("sample.state.State.count++")
                && peer.contains("sample.state.State.count +="),
            "same-package bare accesses should be repaired to the Java static field:\n{peer}"
        );
        let alias = fs::read_to_string(migrated.join("AliasUse.kt")).unwrap();
        assert!(
            alias.contains("import sample.state.State.count as total"),
            "the explicit property alias should target the Java facade:\n{alias}"
        );
        let qualified = fs::read_to_string(migrated.join("QualifiedUse.kt")).unwrap();
        assert!(
            qualified.contains("sample.state.State.count"),
            "fully-qualified property operations should target the Java facade:\n{qualified}"
        );
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin/Java top-level property fixture compiles and runs");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
