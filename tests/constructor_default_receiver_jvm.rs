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
            "notlin-constructor-default-receiver-{}-{id}",
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
        "Token.java",
        r#"package sample;
public interface Token { String label(); }
"#,
    ),
    (
        "JavaToken.java",
        r#"package sample;
public final class JavaToken implements Token {
    private static int calls;
    private final String value;
    public JavaToken(String value) { this.value = value; }
    @Override public String label() { calls++; return value; }
    public static int calls() { return calls; }
    public static void reset() { calls = 0; }
}
"#,
    ),
    (
        "Key.java",
        r#"package sample;
public final class Key {
    private final String value;
    public Key(String value) { this.value = value; }
    public String value() { return value; }
}
"#,
    ),
    (
        "Api.kt",
        r#"package sample
interface Api { val token: Token }
"#,
    ),
    (
        "Product.kt",
        r#"package sample
data class Product @JvmOverloads constructor(
    override val token: Token,
    val reference: String,
    val id: Key = Key("${token.label()}/$reference")
) : Api
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    suspend fun retainedHarness() {}
    JavaToken.reset()
    val omitted = Product(JavaToken("origin"), "path")
    println("omitted=${omitted.id.value()}|calls=${JavaToken.calls()}")

    val explicit = Product(JavaToken("unused"), "other", Key("provided"))
    println("explicit=${explicit.id.value()}|calls=${JavaToken.calls()}")
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
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

#[test]
fn constructor_default_reads_the_parameter_not_the_instance_getter() {
    let scratch = Scratch::new();
    let root = scratch.0.join("cli");
    populate(&root);
    let translated = translate(&root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(root.join("Api.java").is_file());
    assert!(root.join("Product.java").is_file());
    assert!(root.join("Runner.kt").is_file());
    let java = fs::read_to_string(root.join("Product.java")).unwrap();
    assert!(
        java.contains("token.label()"),
        "default overload should read the constructor parameter:\n{java}"
    );
    assert!(
        !java.contains("this.getToken().label()"),
        "constructor delegation cannot call an instance getter before this is initialized:\n{java}"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_constructor_default_receiver_behavior() {
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
    .expect("original Kotlin constructor-default fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "omitted=origin/path|calls=1\nexplicit=provided|calls=1"
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
        assert!(migrated.join("Api.java").is_file());
        assert!(migrated.join("Product.java").is_file());
        assert!(migrated.join("Runner.kt").is_file());
        let java = fs::read_to_string(migrated.join("Product.java")).unwrap();
        assert!(
            java.contains("token.label()"),
            "missing parameter receiver:\n{java}"
        );
        assert!(
            !java.contains("this.getToken().label()"),
            "invalid receiver:\n{java}"
        );
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin and Java constructor-default fixture compiles and runs");
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
