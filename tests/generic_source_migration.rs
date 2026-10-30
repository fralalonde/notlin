use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "notlin-generic-source-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
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

const SOURCES: &[(&str, &str)] = &[
    (
        "Mode.kt",
        r#"package sample
enum class Mode {
    FIRST, SECOND;
    companion object {
        val DEFAULTS: Map<Mode, String?> = mapOf(FIRST to "old", SECOND to null, FIRST to "new")
        var selected: Mode = FIRST
        fun choose(mode: Mode): String? { return DEFAULTS[mode] }
    }
}
"#,
    ),
    (
        "CompanionReader.java",
        r#"package sample;
public class CompanionReader {
    public static java.util.Map<Mode, String> defaults() { return Mode.Companion.getDEFAULTS(); }
    public static void select(Mode mode) { Mode.Companion.setSelected(mode); }
    public static String choose(Mode mode) { return Mode.Companion.choose(mode); }
}
"#,
    ),
    (
        "TextOps.kt",
        r#"package sample
interface TextOps {
    fun with(suffix: String): String
    fun let(suffix: String): String
}
data class Text(val text: String) : TextOps {
    override fun with(suffix: String): String { return text + suffix }
    override fun let(suffix: String): String { return suffix + text }
}
class Wrapped(val value: TextOps) {
    fun render(): String { return value.with("!") + value.let("?") }
}
"#,
    ),
    (
        "Factory.kt",
        r#"package sample
class Factory {
    companion object {
        fun with(value: String): String { return "f:" + value }
        fun run(): String { return "zero" }
        suspend fun keepKotlin() {}
    }
}
class FactoryCaller {
    fun read(): String { return Factory.with("x") }
    fun zero(): String { return Factory.run() }
}
"#,
    ),
    (
        "ParentId.java",
        r#"package sample;
public interface ParentId {
    default String getExternalId() { return "id" + Counter.next(); }
}
"#,
    ),
    (
        "ChildId.java",
        "package sample; public interface ChildId extends ParentId {}\n",
    ),
    (
        "Job.java",
        "package sample; public final class Job implements ChildId {}\n",
    ),
    (
        "Counter.java",
        "package sample; public class Counter { private static int count; public static int next() { return ++count; } }\n",
    ),
    (
        "Activity.kt",
        r#"package sample
class Activity(val first: String, val second: String) {
    constructor(job: Job) : this(job.getExternalId(), job.getExternalId())
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    suspend fun keepHarness() {}
    val activity = Activity(Job())
    val defaults = Mode.DEFAULTS
    CompanionReader.select(Mode.SECOND)
    println("${defaults[Mode.FIRST]}|${defaults[Mode.SECOND]}|${defaults.keys.joinToString(",")}|${defaults === CompanionReader.defaults()}|${Mode.selected}|${CompanionReader.choose(Mode.FIRST)}|${Wrapped(Text("x")).render()}|${activity.first}|${activity.second}|${FactoryCaller().read()}${FactoryCaller().zero()}")
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

fn translate(root: &Path, lombok: bool) -> std::process::Output {
    let binary = std::env::var_os("NOTLIN_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
    let mut cmd = std::process::Command::new(binary);
    cmd.args([
        "--jvm-facts=off",
        "--annotations",
        "none",
        "--in-place",
        "--root",
    ])
    .arg(root);
    if lombok {
        cmd.arg("--lombok");
    }
    cmd.arg(root).output().unwrap()
}

fn assert_migrated(root: &Path, output: &std::process::Output) {
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{diagnostics}");
    for name in ["Mode", "TextOps", "Text", "Wrapped", "Activity"] {
        assert!(
            root.join(format!("{name}.java")).is_file(),
            "{name} retained:\n{diagnostics}"
        );
    }
    let activity = fs::read_to_string(root.join("Activity.java")).unwrap();
    assert!(
        activity.contains("this(job.getExternalId(), job.getExternalId());"),
        "{activity}"
    );
    let wrapped = fs::read_to_string(root.join("Wrapped.java")).unwrap();
    assert!(
        wrapped.contains(".with(\"!\")") && wrapped.contains(".let(\"?\")"),
        "{wrapped}"
    );
    let mode = fs::read_to_string(root.join("Mode.java")).unwrap();
    assert!(
        mode.contains("class Companion") && mode.contains("getDEFAULTS()"),
        "{mode}"
    );
    assert!(mode.contains("setSelected("), "{mode}");
    assert!(root.join("Factory.kt").is_file());
    assert!(
        fs::read_to_string(root.join("Factory.kt"))
            .unwrap()
            .contains("class FactoryCaller"),
        "companion calls need their holder mapping and must not become invalid Owner.with(...) Java calls"
    );
    assert!(!root.join("FactoryCaller.java").exists());
}

#[test]
fn translates_enum_maps_user_builtin_spellings_and_inherited_java_constructor_calls() {
    let scratch = Scratch::new();
    for lombok in [false, true] {
        let root = scratch.0.join(format!("source-{lombok}"));
        populate(&root);
        assert_migrated(&root, &translate(&root, lombok));
    }
}

#[test]
fn builtin_named_methods_with_mangled_parameter_abis_remain_kotlin() {
    let scratch = Scratch::new();
    fs::write(
        scratch.0.join("Token.kt"),
        "package sample\n@JvmInline value class Token(val value: String)\n",
    )
    .unwrap();
    fs::write(
        scratch.0.join("Source.kt"),
        "package sample\nclass Source { fun with(token: Token): String = token.value; suspend fun stay() {} }\n",
    )
    .unwrap();
    fs::write(scratch.0.join("Caller.kt"), "package sample\nclass Caller { fun read(source: Source, token: Token): String = source.with(token) }\n").unwrap();
    let output = translate(&scratch.0, false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        scratch.0.join("Caller.kt").is_file(),
        "a mangled retained method must not be emitted as .with(token)"
    );
}

fn config(root: &Path, output: PathBuf, lombok: bool) -> JvmValidationConfig {
    let mut cfg = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output,
    );
    cfg.classpath.push(PathBuf::from(
        std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"),
    ));
    cfg.kotlinc_args.extend(["-jvm-target".into(), "17".into()]);
    cfg.javac_args.extend(["--release".into(), "17".into()]);
    if lombok {
        cfg.classpath.push(PathBuf::from(
            std::env::var_os("NOTLIN_LOMBOK").expect("NOTLIN_LOMBOK"),
        ));
        cfg.javac_args.push("-proc:full".into());
        let mut plugin = std::ffi::OsString::from("-Xplugin=");
        plugin.push(std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").expect("Kotlin Lombok plugin"));
        cfg.kotlinc_args.push(plugin);
    } else {
        cfg.javac_args.push("-proc:none".into());
    }
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|e| e.to_str()) {
            Some("kt") => cfg.kotlin_sources.push(path),
            Some("java") => cfg.java_sources.push(path),
            _ => {}
        }
    }
    cfg.run_main_class = Some("sample.RunnerKt".into());
    cfg
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn runtime_preserves_map_contents_order_identity_user_dispatch_and_constructor_evaluation() {
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        false,
    ))
    .unwrap();
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "new|null|FIRST,SECOND|true|SECOND|new|x!?x|id1|id2|f:xzero"
    );
    for lombok in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok}"));
        populate(&migrated);
        assert_migrated(&migrated, &translate(&migrated, lombok));
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("classes-{lombok}")),
            lombok,
        ))
        .unwrap();
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "Lombok mode {lombok}"
        );
    }
}
