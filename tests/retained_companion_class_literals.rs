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
            "notlin-retained-companion-class-literals-{}-{id}",
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

const SOURCES: &[(&str, &str)] = &[
    (
        "Factory.kt",
        r#"package boundary
import kotlin.reflect.KClass

class Factory {
    companion object {
        suspend fun retainedMarker() {}
        fun of(type: Class<out Enum<*>>): String = "one:${type.simpleName}"
        fun of(type: Class<out Enum<*>>, defaultValue: Any?): String = "two:${type.simpleName}:$defaultValue"
        fun note(value: String): String = value
        fun accept(type: KClass<*>): String = type.toString()
        fun acceptClass(type: Class<*>): String = type.name
        fun acceptBlock(block: () -> KClass<*>): String = block().toString()
    }
}
"#,
    ),
    (
        "Choice.kt",
        r#"package boundary
enum class Choice(val description: String, val detail: String) {
    FIRST(Factory.of(Choice::class.java), Factory.note("first ::class value")),
    SECOND(Factory.of(Choice::class.java, "fallback"), Factory.note(/* ::class */ "second"))
}
"#,
    ),
    (
        "BareChoice.kt",
        "package boundary\nenum class BareChoice { ONLY }\n",
    ),
    (
        "BareCaller.kt",
        r#"package boundary
class BareCaller {
    fun send(): String = Factory.accept(BareChoice::class)
}
"#,
    ),
    (
        "Runner.kt",
        r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    println(Choice.FIRST.description + ":" + Choice.FIRST.detail + "|" + Choice.SECOND.description + ":" + Choice.SECOND.detail)
}
"#,
    ),
];

fn write_sources(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
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

fn config(root: &Path, output: PathBuf, stdlib: &Path) -> JvmValidationConfig {
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output,
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config
        .javac_args
        .extend(["--release".into(), "17".into(), "-proc:none".into()]);
    config.classpath.push(stdlib.to_path_buf());
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
fn class_literals_reach_retained_companion_class_overloads() {
    let scratch = Scratch::new();
    let root = scratch.0.join("sources");
    write_sources(&root);

    let translated = translate(&root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(
        root.join("Choice.java").is_file(),
        "{}\n{}",
        String::from_utf8_lossy(&translated.stderr),
        fs::read_to_string(root.join("Choice.kt")).unwrap_or_default()
    );
    assert!(root.join("Factory.kt").is_file());
    assert!(!root.join("Factory.java").exists());
    assert!(root.join("BareCaller.kt").is_file());
    assert!(!root.join("BareCaller.java").exists());

    let caller = fs::read_to_string(root.join("Choice.java")).unwrap();
    assert!(
        caller.contains("Factory.Companion.of(Choice.class)"),
        "{caller}"
    );
    assert!(
        caller.contains("Factory.Companion.of(Choice.class, \"fallback\")"),
        "{caller}"
    );
    assert!(
        caller.contains("Factory.Companion.note(\"first ::class value\")"),
        "string contents mentioning ::class must not alter overload routing: {caller}"
    );
}

#[test]
fn uncertain_class_literals_retain_callers_of_kotlin_companion_apis() {
    type FixtureSource = (&'static str, &'static str);
    type RetentionCase = (&'static str, &'static str, &'static [FixtureSource]);
    let cases: &[RetentionCase] = &[
        (
            "primitive-class",
            "package boundary\nclass Caller { fun run(): String = Factory.acceptClass(Int::class.java) }\n",
            &[],
        ),
        (
            "value-class",
            "package boundary\nclass Caller { fun run(value: Any): String = Factory.acceptClass(value::class.java) }\n",
            &[],
        ),
        (
            "uppercase-parameter-shadow",
            "package boundary\nclass Caller { fun run(Choice: Any): String = Factory.acceptClass(Choice::class.java) }\n",
            &[],
        ),
        (
            "reified-class-parameter",
            "package boundary\nclass Caller { inline fun <reified Choice : Any> run(): String = Factory.acceptClass(Choice::class.java) }\n",
            &[],
        ),
        (
            "type-alias",
            "package boundary\ntypealias Alias = Choice\nclass Caller { fun run(): String = Factory.acceptClass(Alias::class.java) }\n",
            &[],
        ),
        (
            "java-import-alias",
            "package boundary\nimport java.time.DayOfWeek as DayAlias\nclass Caller { fun run(): String = Factory.acceptClass(DayAlias::class.java) }\n",
            &[],
        ),
        (
            "source-kclass-java-extension",
            "package boundary\nimport boundary.custom.java\nclass Caller { fun run(): String = Factory.accept(Choice::class.java) }\n",
            &[(
                "ClassBridge.kt",
                "package boundary.custom\nimport kotlin.reflect.KClass\nval KClass<*>.java: KClass<*> get() = this\n",
            )],
        ),
        (
            "kclass-trailing-lambda",
            "package boundary\nclass Caller { fun run(): String = Factory.acceptBlock { Choice::class } }\n",
            &[],
        ),
    ];

    for &(name, caller, extra_sources) in cases {
        let scratch = Scratch::new();
        let root = scratch.0.join(name);
        write_sources(&root);
        fs::write(root.join("Caller.kt"), caller).unwrap();
        for &(source_name, source) in extra_sources {
            fs::write(root.join(source_name), source).unwrap();
        }

        let translated = translate(&root, false);
        assert!(
            translated.status.success(),
            "case {name}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            root.join("Caller.kt").is_file(),
            "case {name}: caller must remain Kotlin\n{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            !root.join("Caller.java").exists(),
            "case {name}: caller must not be emitted as Java\n{}",
            String::from_utf8_lossy(&translated.stderr)
        );
    }
}

#[test]
fn renamed_methods_and_named_companion_holders_retain_their_callers() {
    let factory = SOURCES[0].1;
    let variants = [
        factory.replacen("companion object {", "companion object Named {", 1),
        factory.replacen(
            "fun of(type: Class<out Enum<*>>): String",
            "@kotlin.jvm.JvmName(\"renamed\") fun of(type: Class<out Enum<*>>): String",
            1,
        ),
    ];
    for factory in variants {
        let scratch = Scratch::new();
        let root = scratch.0.join("sources");
        write_sources(&root);
        fs::write(root.join("Factory.kt"), factory).unwrap();
        fs::write(
            root.join("Caller.kt"),
            "package boundary\nclass Caller { fun run(): String = Factory.of(Choice::class.java) }\n",
        ).unwrap();
        let translated = translate(&root, false);
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            root.join("Caller.kt").is_file(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(!root.join("Caller.java").exists());
    }
}

#[test]
#[ignore = "requires JVM tools and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_retained_companion_class_literal_calls() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    write_sources(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original classes"),
        &stdlib,
    ))
    .expect("original Kotlin source compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "one:Choice:first ::class value|two:Choice:fallback:second"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        write_sources(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            migrated.join("Choice.java").is_file(),
            "the class-literal caller must translate in Lombok mode {lombok_enabled}"
        );
        assert!(migrated.join("Factory.kt").is_file());
        assert!(migrated.join("BareCaller.kt").is_file());
        assert!(!migrated.join("BareCaller.java").exists());
        let caller = fs::read_to_string(migrated.join("Choice.java")).unwrap();
        assert!(
            caller.contains("Factory.Companion.of(Choice.class)"),
            "{caller}"
        );
        assert!(
            caller.contains("Factory.Companion.of(Choice.class, \"fallback\")"),
            "{caller}"
        );
        assert!(
            caller.contains("Factory.Companion.note(\"first ::class value\")"),
            "string contents mentioning ::class must not alter overload routing: {caller}"
        );

        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed classes-{lombok_enabled}")),
            &stdlib,
        ))
        .expect("translated Java caller and retained Kotlin companion compile and run");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
