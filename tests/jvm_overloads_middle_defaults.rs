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
            "notlin-jvm-overloads-middle-defaults-{}-{id}",
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

const SOURCES: [(&str, &str); 5] = [
    (
        "Token.java",
        r#"package sample;
public final class Token {
    private final String label;
    public Token(String label) { this.label = label; }
    public String label() { return label; }
}
"#,
    ),
    (
        "Defaults.java",
        r#"package sample;
public final class Defaults {
    private static int calls;
    public static Token first(String owner) { calls++; return new Token(owner + "/first"); }
    public static Token second(Token first) { calls++; return new Token(first.label() + "/second"); }
    public static int calls() { return calls; }
    public static void reset() { calls = 0; }
}
"#,
    ),
    (
        "JavaCaller.java",
        r#"package sample;
public final class JavaCaller {
    public static void run() {
        Defaults.reset();
        Message both = new Message("owner-a", "payload-a");
        System.out.println("both=" + both.getOwner() + "," + both.getFirst().label() + "," + both.getSecond().label() + "," + both.getPayload() + "|calls=" + Defaults.calls());

        Defaults.reset();
        Token supplied = new Token("supplied");
        Message one = new Message("owner-b", supplied, "payload-b");
        System.out.println("one=" + one.getOwner() + "," + one.getFirst().label() + "," + one.getSecond().label() + "," + one.getPayload() + "|calls=" + Defaults.calls());

        Defaults.reset();
        Message none = new Message("owner-c", supplied, new Token("second-supplied"), "payload-c");
        System.out.println("none=" + none.getOwner() + "," + none.getFirst().label() + "," + none.getSecond().label() + "," + none.getPayload() + "|calls=" + Defaults.calls());
    }
}
"#,
    ),
    (
        "Message.kt",
        r#"package sample
class Message @JvmOverloads constructor(
    val owner: String,
    val first: Token = Defaults.first(owner),
    val second: Token = Defaults.second(first),
    val payload: String
)
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    suspend fun retainedHarness() {}
    JavaCaller.run()
}
"#,
    ),
];

const COMPETING_SECONDARY_SOURCES: [(&str, &str); 3] = [
    (
        "Defaults.java",
        r#"package sample;
public final class Defaults {
    private static int calls;
    public static String value() { calls++; return "default"; }
    public static int calls() { return calls; }
    public static void reset() { calls = 0; }
}
"#,
    ),
    (
        "Box.kt",
        r#"package sample
class Box<T : CharSequence> @JvmOverloads constructor(
    val value: CharSequence = Defaults.value(),
    var route: String = "primary"
) {
    constructor(value: String) : this(value, "secondary") {
        this.route = "secondary"
    }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    suspend fun retainedHarness() {}
    Defaults.reset()
    val box = Box<CharSequence>()
    println("value=${box.value}|route=${box.route}|calls=${Defaults.calls()}")
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

fn populate_competing_secondary(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in COMPETING_SECONDARY_SOURCES {
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
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

fn assert_overloads(java: &str) {
    let compact = java
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .replace("final", "");
    for signature in [
        "Message(Stringowner,Stringpayload)",
        "Message(Stringowner,Tokenfirst,Stringpayload)",
    ] {
        assert!(
            compact.contains(signature),
            "missing Java overload `{signature}`:\n{java}"
        );
    }
    assert!(
        compact.contains("Message(Stringowner,Tokenfirst,Tokensecond,Stringpayload)")
            || java.contains("@AllArgsConstructor"),
        "missing canonical constructor:\n{java}"
    );
}

#[test]
fn jvm_overloads_include_middle_defaults_before_required_suffix() {
    let scratch = Scratch::new();
    let root = scratch.0.join("cli");
    populate(&root);
    let translated = translate(&root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(root.join("Message.java").is_file());
    assert!(root.join("Runner.kt").is_file());
    let java = fs::read_to_string(root.join("Message.java")).unwrap();
    assert_overloads(&java);
    assert!(
        java.contains("Defaults.first("),
        "missing Java helper call:\n{java}"
    );
    assert!(
        java.contains("Defaults.second("),
        "missing Java helper call:\n{java}"
    );
    assert!(
        !java.contains("stream().findFirst"),
        "the Java helper named `first` was rewritten as a collection operation:\n{java}"
    );
}

#[test]
fn narrower_default_with_competing_secondary_keeps_primary_constructor_target() {
    let scratch = Scratch::new();
    let root = scratch.0.join("cli-competing-secondary");
    populate_competing_secondary(&root);
    let translated = translate(&root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    let java = fs::read_to_string(root.join("Box.java")).unwrap();
    let compact = java
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    assert!(
        compact.contains("Box(){this((CharSequence)(Defaults.value()));}"),
        "missing cast that anchors the primary constructor rung:\n{java}"
    );
    assert!(root.join("Runner.kt").is_file());
}

#[test]
#[ignore = "requires JVM tools and Kotlin stdlib"]
fn narrower_default_competing_secondary_preserves_runtime_target_and_evaluation() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original-competing-secondary");
    populate_competing_secondary(&original);
    let before = validate_jvm_sources(&jvm_config(
        &original,
        scratch.0.join("original-competing-secondary-classes"),
        &stdlib,
        false,
    ))
    .expect("original competing-secondary fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(expected, "value=default|route=primary|calls=1");

    for lombok_enabled in [false, true] {
        let migrated = scratch
            .0
            .join(format!("migrated-competing-secondary-{lombok_enabled}"));
        populate_competing_secondary(&migrated);
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Box.java").is_file());
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!(
                "migrated-competing-secondary-classes-{lombok_enabled}"
            )),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed competing-secondary fixture compiles and runs");
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

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_middle_default_overload_behavior() {
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
    .expect("original Kotlin overload fixture compiles Java callers and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "both=owner-a,owner-a/first,owner-a/first/second,payload-a|calls=2\none=owner-b,supplied,supplied/second,payload-b|calls=1\nnone=owner-c,supplied,second-supplied,payload-c|calls=0"
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
        assert!(migrated.join("Message.java").is_file());
        let java = fs::read_to_string(migrated.join("Message.java")).unwrap();
        assert_overloads(&java);
        assert!(
            java.contains("Defaults.first("),
            "missing Java helper call:\n{java}"
        );
        assert!(
            java.contains("Defaults.second("),
            "missing Java helper call:\n{java}"
        );
        assert!(
            !java.contains("stream().findFirst"),
            "wrong mapped member:\n{java}"
        );
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin and Java overload fixture compiles and runs");
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
