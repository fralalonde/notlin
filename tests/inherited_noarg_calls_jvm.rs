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
            "notlin-inherited-noarg-calls-{}-{id}",
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

const SOURCES: [(&str, &str); 7] = [
    (
        "Counter.java",
        r#"package sample;
public final class Counter {
    private static final StringBuilder CALLS = new StringBuilder();
    public static String record(String value) {
        if (CALLS.length() > 0) CALLS.append(',');
        CALLS.append(value);
        return value;
    }
    public static String calls() { return CALLS.toString(); }
    public static void reset() { CALLS.setLength(0); }
}
"#,
    ),
    (
        "Root.kt",
        r#"package sample
open class Root {
    open fun getId(): String = Counter.record("root")
    suspend fun retainRoot() {}
}
"#,
    ),
    (
        "Base.kt",
        r#"package sample
open class Base : Root() {
    override fun getId(): String = Counter.record("base")
    suspend fun retainBase() {}
}
"#,
    ),
    (
        "Middle.kt",
        r#"package sample
open class Middle : Base() {
    fun <T> getId(clazz: Class<T>): String = Counter.record("generic")
    suspend fun retainMiddle() {}
}
"#,
    ),
    (
        "Child.kt",
        r#"package sample
open class Child : Middle() {
    fun read(): String = getId()
}
"#,
    ),
    (
        "JavaChild.java",
        r#"package sample;
public final class JavaChild extends Child {
    @Override public String getId() { return Counter.record("java"); }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    Counter.reset()
    println("kotlin=${Child().read()}")
    println("java=${JavaChild().read()}")
    println("calls=${Counter.calls()}")
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

fn populate_qualified_super(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in [
        (
            "Root.java",
            "package sample; public interface Root {String getId();}",
        ),
        (
            "Left.java",
            "package sample; public interface Left extends Root {@org.jetbrains.annotations.NotNull default String getId(){return \"left\";}}",
        ),
        (
            "Branch.java",
            "package sample; public interface Branch extends Left,Root {}",
        ),
        (
            "Child.kt",
            "package sample\nclass Child:Branch {override fun getId():String=super<Branch>.getId()}\n",
        ),
        (
            "Runner.kt",
            "package sample\nfun main(){println(Child().getId())}\n",
        ),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
}

fn populate_inherited_default_join(root: &Path, retained_root: bool) {
    fs::create_dir_all(root).unwrap();
    let companion = if retained_root {
        "companion object {suspend fun remainKotlin() {}}"
    } else {
        ""
    };
    for (name, source) in [
        ("Root.kt", format!("package sample\ninterface Root {{fun number():Int\nfun value():Int=number()\n{companion}\n}}\n")),
        ("Middle.kt", "package sample\ninterface Middle:Root\n".into()),
        ("Special.kt", "package sample\ninterface Special:Root {override fun value():Int=2}\n".into()),
        ("Child.kt", "package sample\ninterface Child:Middle,Special\n".into()),
        ("Runner.kt", "package sample\nclass Impl:Child {override fun number():Int=7}\nfun main(){val x=Impl(); val root:Root=x;println(\"${x.value()}|${root.value()}\")}\n".into()),
    ] {fs::write(root.join(name), source).unwrap();}
}

fn assert_inherited_default_join(root: &Path, retained_root: bool, lombok: bool) {
    let result = translate(root, lombok);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        root.join("Special.java").exists(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        root.join("Child.java").exists(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    if retained_root {
        assert!(root.join("Middle.kt").exists());
        assert!(!root.join("Middle.java").exists());
    } else {
        let middle = fs::read_to_string(root.join("Middle.java")).unwrap();
        assert!(
            !middle.contains("value("),
            "the forwarding interface must not copy a default: {middle}"
        );
    }
}

#[test]
fn inherited_default_copies_do_not_create_unrelated_defaults_at_a_join() {
    let scratch = Scratch::new();
    for retained in [false, true] {
        for lombok in [false, true] {
            let root = scratch.0.join(format!("join-{retained}-{lombok}"));
            populate_inherited_default_join(&root, retained);
            assert_inherited_default_join(&root, retained, lombok);
        }
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_inherited_default_joins_in_every_mode() {
    let stdlib = PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap());
    let scratch = Scratch::new();
    for mode in ["disable", "enable", "no-compatibility"] {
        for retained in [false, true] {
            let run = |root: &Path, output, lombok| {
                let mut config = jvm_config(root, output, &stdlib, lombok);
                config
                    .kotlinc_args
                    .push(format!("-jvm-default={mode}").into());
                let result = validate_jvm_sources(&config).unwrap();
                assert_eq!(result.commands.last().unwrap().stdout.trim(), "2|2");
            };
            let original = scratch.0.join(format!("join-original-{mode}-{retained}"));
            populate_inherited_default_join(&original, retained);
            run(
                &original,
                scratch.0.join(format!("join-before-{mode}-{retained}")),
                false,
            );
            for lombok in [false, true] {
                let root = scratch
                    .0
                    .join(format!("join-mixed-{mode}-{retained}-{lombok}"));
                populate_inherited_default_join(&root, retained);
                assert_inherited_default_join(&root, retained, lombok);
                run(
                    &root,
                    scratch
                        .0
                        .join(format!("join-after-{mode}-{retained}-{lombok}")),
                    lombok,
                );
            }
        }
    }
}

fn populate_bypassed_super(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in [
        ("Kind.java", "package sample; public enum Kind {ROOT,RIGHT}"),
        (
            "Root.java",
            "package sample; public interface Root {default Kind choice(){return Kind.ROOT;} int number(); default int value(){return this.number();} default int arithmetic(){return 6+1;} default String opaque(){return \"root\"+System.nanoTime();}}",
        ),
        (
            "Left.java",
            "package sample; public interface Left extends Root {}",
        ),
        (
            "Right.java",
            "package sample; public interface Right extends Root {default Kind choice(){return Kind.RIGHT;} default int value(){return 99;} default String opaque(){return \"right\";}}",
        ),
        (
            "Child.kt",
            "package sample\ninterface Child:Left,Right {override fun choice():Kind=super<Left>.choice()\noverride fun value():Int=super<Left>.value()}\n",
        ),
        (
            "Unsafe.kt",
            "package sample\ninterface Unsafe:Left,Right {override fun opaque():String=super<Left>.opaque()}\n",
        ),
        (
            "MarkerChild.kt",
            "package sample\ninterface MarkerChild:Root,java.io.Serializable {override fun arithmetic():Int=super<Root>.arithmetic()}\n",
        ),
        (
            "Runner.kt",
            "package sample\nclass Impl:Child {var calls=0;override fun number():Int {calls++;return 7}}\nclass MarkerImpl:MarkerChild {override fun number():Int=7}\nfun main(){val i=Impl();println(\"${i.choice()}|${i.value()}|${i.calls}|${MarkerImpl().arithmetic()}\")}\n",
        ),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
}

#[test]
fn bypassed_super_uses_the_selected_body_and_retains_unproven_bodies() {
    let scratch = Scratch::new();
    for lombok in [false, true] {
        let root = scratch.0.join(format!("bypassed-{lombok}"));
        populate_bypassed_super(&root);
        let result = translate(&root, lombok);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let java = fs::read_to_string(root.join("Child.java"))
            .expect("selected source body can translate");
        assert!(java.contains("sample.Kind.ROOT"), "{java}");
        assert!(java.contains("this.number()"), "{java}");
        assert!(!java.contains("Left.super"), "{java}");
        assert!(root.join("MarkerChild.java").exists());
        assert!(!root.join("MarkerChild.kt").exists());
        assert!(root.join("Unsafe.kt").exists());
        assert!(!root.join("Unsafe.java").exists());
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_bypassed_super_body_and_virtual_dispatch() {
    let stdlib = PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap());
    let scratch = Scratch::new();
    for mode in ["disable", "enable", "no-compatibility"] {
        let run = |root: &Path, output: PathBuf, lombok| {
            let mut config = jvm_config(root, output, &stdlib, lombok);
            config
                .kotlinc_args
                .push(format!("-jvm-default={mode}").into());
            let result = validate_jvm_sources(&config).unwrap();
            assert_eq!(result.commands.last().unwrap().stdout.trim(), "ROOT|7|1|7");
        };
        let original = scratch.0.join(format!("original-bypass-{mode}"));
        populate_bypassed_super(&original);
        run(
            &original,
            scratch.0.join(format!("before-bypass-{mode}")),
            false,
        );
        for lombok in [false, true] {
            let root = scratch.0.join(format!("after-bypass-{mode}-{lombok}"));
            populate_bypassed_super(&root);
            let result = translate(&root, lombok);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(root.join("Child.java").exists());
            run(
                &root,
                scratch.0.join(format!("classes-bypass-{mode}-{lombok}")),
                lombok,
            );
        }
    }
}

#[test]
fn qualified_super_call_translates_past_an_overridden_ancestor() {
    let scratch = Scratch::new();
    for lombok in [false, true] {
        let root = scratch.0.join(format!("qualified-{lombok}"));
        populate_qualified_super(&root);
        let output = translate(&root, lombok);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            root.join("Child.java").exists(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!root.join("Child.kt").exists());
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_qualified_super_dispatch_in_every_mode() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    for mode in ["disable", "enable", "no-compatibility"] {
        let configured = |root: &Path, output: PathBuf, lombok| {
            let mut config = jvm_config(root, output, &stdlib, lombok);
            config
                .kotlinc_args
                .push(format!("-jvm-default={mode}").into());
            config.classpath.extend([
                Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"),
                Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/jspecify-1.0.0.jar"),
            ]);
            config
        };
        let original = scratch.0.join(format!("original-{mode}"));
        populate_qualified_super(&original);
        let before = validate_jvm_sources(&configured(
            &original,
            scratch.0.join(format!("before-{mode}")),
            false,
        ))
        .unwrap();
        assert_eq!(before.commands.last().unwrap().stdout.trim(), "left");
        for lombok in [false, true] {
            let root = scratch.0.join(format!("mixed-{mode}-{lombok}"));
            populate_qualified_super(&root);
            let translated = translate(&root, lombok);
            assert!(
                translated.status.success(),
                "{}",
                String::from_utf8_lossy(&translated.stderr)
            );
            assert!(
                root.join("Child.java").exists(),
                "{}",
                String::from_utf8_lossy(&translated.stderr)
            );
            let after = validate_jvm_sources(&configured(
                &root,
                scratch.0.join(format!("after-{mode}-{lombok}")),
                lombok,
            ))
            .unwrap();
            assert_eq!(after.commands.last().unwrap().stdout.trim(), "left");
        }
    }
}

fn translate(root: &Path, lombok_enabled: bool) -> std::process::Output {
    let mut kotlin = fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "kt")
                && path.file_name().is_none_or(|name| name != "Runner.kt")
        })
        .collect::<Vec<_>>();
    kotlin.sort();
    let mut command = Command::new(
        std::env::var_os("NOTLIN_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into()),
    );
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
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

fn assert_translation(root: &Path, lombok_enabled: bool) {
    let translated = translate(root, lombok_enabled);
    assert!(
        translated.status.success(),
        "Lombok mode {lombok_enabled}: {}",
        String::from_utf8_lossy(&translated.stderr)
    );
    for retained in ["Root", "Base", "Middle"] {
        assert!(
            root.join(format!("{retained}.kt")).is_file(),
            "{retained} should remain Kotlin"
        );
        assert!(
            !root.join(format!("{retained}.java")).exists(),
            "{retained} should not be emitted as Java"
        );
    }
    assert!(root.join("Child.java").is_file(), "Child should translate");
    assert!(
        root.join("Runner.kt").is_file(),
        "the comparison driver stays Kotlin"
    );
    assert!(!root.join("Child.kt").exists());
    let java = fs::read_to_string(root.join("Child.java")).unwrap();
    assert!(
        java.contains("return getId();"),
        "the bare call should resolve to the inherited no-argument method:\n{java}"
    );
    assert!(
        !java.contains("getId(null)") && !java.contains("getId((Class"),
        "translation must not emit an unsafe generic-overload fallback:\n{java}"
    );
}

#[test]
fn inherited_bare_noarg_call_translates_past_generic_overload() {
    let scratch = Scratch::new();
    let root = scratch.0.join("cli");
    populate(&root);
    assert_translation(&root, false);
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_inherited_bare_noarg_dispatch() {
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
    .expect("original Kotlin fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(expected, "kotlin=base\njava=java\ncalls=base,java");

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        assert_translation(&migrated, lombok_enabled);
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("retained Kotlin parents, translated Child, and Java subclass compile and run");
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
