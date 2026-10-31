//! Compare map merges against independently compiled Kotlin, including callers
//! that remain Kotlin after the same CLI conversion used for applications.
use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SOURCES: &[(&str, &str)] = &[
    (
        "Defaults.kt",
        r#"package sample
interface Defaults {
    companion object {
        val BASE: Map<String, Int> = mapOf("first" to 1, "same" to 2)
    }
}
"#,
    ),
    (
        "Merger.kt",
        r#"package sample
class Merger(private val values: Map<String, Int>) {
    fun merge(other: Merger): Map<String, Int> = this.values + other.values
    operator fun plus(other: Merger): Merger = Merger(this.values + other.values)
    fun entries(): Map<String, Int> = values
    fun chained(): Map<String, Int> = Defaults.BASE + JavaDefaults.defaults + values + mapOf("last" to 9)
    fun explicit(left: Map<String?, Int?>, right: Map<String?, Int?>): Map<String?, Int?> = left.plus(right)
    fun local(left: Map<String?, Int?>, right: Map<String?, Int?>): Map<String?, Int?> {
        val selected: Map<String?, Int?> = left
        return selected + right
    }
    fun <T> generic(left: Map<String, T>, right: Map<String, T>): Map<String, T> = left + right
    fun evaluated(): Map<String, Int> = left() + right()
    private fun left(): Map<String, Int> {
        Trace.mark("L")
        return values
    }
    private fun right(): Map<String, Int> {
        Trace.mark("R")
        return Defaults.BASE
    }
}
"#,
    ),
    (
        "Trace.java",
        r#"package sample;
public final class Trace {
    private static final StringBuilder calls = new StringBuilder();
    public static void mark(String value) { calls.append(value); }
    public static String calls() { return calls.toString(); }
}
"#,
    ),
    (
        "JavaDefaults.java",
        r#"package sample;
import java.util.*;
public interface JavaDefaults {
    Map<String, Integer> defaults = Collections.singletonMap("java", 6);
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    suspend fun retainedHarness() {}
    val left = linkedMapOf<String?, Int?>("a" to 1, null to 2, "same" to 3)
    val right = linkedMapOf<String?, Int?>("same" to 8, null to null, "last" to 9)
    val merger = Merger(mapOf("same" to 7, "middle" to 4))
    val explicit = merger.explicit(left, right)
    println(explicit)
    println(merger.local(left, right))
    left["after-left"] = 10
    right["after-right"] = 11
    println(explicit)
    println(merger.merge(Merger(mapOf("same" to 5, "other" to 6))))
    println((merger + Merger(mapOf("same" to 5, "other" to 6))).entries())
    println(merger.chained())
    println(merger.generic(mapOf("x" to "left"), mapOf("x" to "right", "y" to "new")))
    println(merger.evaluated())
    println(Trace.calls())
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

fn config(root: &Path, output: PathBuf, lombok: bool) -> JvmValidationConfig {
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
    config.classpath.push(
        std::env::var_os("NOTLIN_KOTLIN_STDLIB")
            .expect("NOTLIN_KOTLIN_STDLIB")
            .into(),
    );
    if lombok {
        config.classpath.push(
            std::env::var_os("NOTLIN_LOMBOK")
                .expect("NOTLIN_LOMBOK")
                .into(),
        );
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
        let file = entry.unwrap().path();
        match file.extension().and_then(|ext| ext.to_str()) {
            Some("kt") => config.kotlin_sources.push(file),
            Some("java") => config.java_sources.push(file),
            _ => {}
        }
    }
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin plugin"]
fn map_merges_preserve_order_nulls_duplicates_copying_and_operand_evaluation() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let scratch = Scratch(
        std::env::temp_dir().join(format!("notlin-map-algebra-{}-{nonce}", std::process::id())),
    );
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        false,
    ))
    .expect("original Kotlin compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "{a=1, null=null, same=8, last=9}\n{a=1, null=null, same=8, last=9}\n{a=1, null=null, same=8, last=9}\n{same=5, middle=4, other=6}\n{same=5, middle=4, other=6}\n{first=1, same=7, java=6, middle=4, last=9}\n{x=right, y=new}\n{same=2, middle=4, first=1}\nLR"
    );
    for lombok in [false, true] {
        let root = scratch.0.join(format!("candidate-{lombok}"));
        populate(&root);
        let binary = std::env::var_os("NOTLIN_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
        let mut command = Command::new(binary);
        command
            .current_dir(&root)
            .args(["--in-place", "--annotations", "none"]);
        if lombok {
            command.arg("--lombok");
        }
        let result = command.arg(".").output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let java = fs::read_to_string(root.join("Merger.java")).expect("map owner must translate");
        assert!(
            java.matches("kotlin.collections.MapsKt.plus(").count() >= 8,
            "{java}"
        );
        assert!(
            root.join("Runner.kt").exists(),
            "retained caller must exercise the migrated methods"
        );
        let after = validate_jvm_sources(&config(
            &root,
            scratch.0.join(format!("mixed-classes-{lombok}")),
            lombok,
        ))
        .expect("mixed Kotlin and Java compile and run");
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .trim()
                .replace("\r\n", "\n"),
            expected,
            "Lombok={lombok}"
        );
    }
}
