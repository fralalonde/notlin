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
            "notlin-inherited-property-receiver-{}-{id}",
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
    config.run_main_class = Some("boundary.RunnerKt".into());
    config
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_translates_inherited_contract_property_receiver() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();

    let sources = [
        (
            "Token.java",
            r#"package boundary;
public final class Token {
    private final String value;
    public Token(String value) { this.value = value; }
    public String read() { return value; }
}
"#,
        ),
        (
            "Marker.kt",
            r#"package boundary
@Target(AnnotationTarget.PROPERTY, AnnotationTarget.VALUE_PARAMETER)
@Retention(AnnotationRetention.RUNTIME)
annotation class Marker
"#,
        ),
        (
            "Contract.kt",
            r#"package boundary
interface Contract {
    val token: Token
    suspend fun kept() {}
}
"#,
        ),
        (
            "Base.kt",
            r#"package boundary
open class Base(token: Token) : Contract {
    @Marker override var token: Token = token
    suspend fun retainedHook() {}
}
"#,
        ),
        (
            "Derived.kt",
            r#"package boundary
class Derived(token: Token) : Base(token) {
    fun value(): String = token.read()
    fun localValue(other: Token): String {
        val token: Token = other
        if (true) {
            return token.read()
        }
        return "unreachable"
    }
}
"#,
        ),
        (
            "SyntheticBase.kt",
            r#"package boundary
import kotlin.jvm.JvmSynthetic

open class SyntheticBase(token: Token) : Contract {
    @get:JvmSynthetic override var token: Token = token
    suspend fun retainedHook() {}
}
"#,
        ),
        (
            "SyntheticDerived.kt",
            r#"package boundary
class SyntheticDerived(token: Token) : SyntheticBase(token) {
    fun value(): String = token.read()
}
"#,
        ),
        (
            "Runner.kt",
            r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val derived = Derived(Token("first"))
    val base: Base = derived
    val initial = derived.value()
    base.token = Token("second")
    val synthetic = SyntheticDerived(Token("third"))
    val syntheticBase: SyntheticBase = synthetic
    val syntheticInitial = synthetic.value()
    syntheticBase.token = Token("fourth")
    println(initial + "|" + derived.value() + "|" + syntheticInitial + "|" + synthetic.value() + "|" + derived.localValue(Token("local")))
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
    .expect("original Kotlin and Java sources compile and run");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(expected, "first|second|third|fourth|local");

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
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            migrated.join("Derived.java").is_file(),
            "Derived must translate in Lombok mode {lombok_enabled}: {}\n{}",
            String::from_utf8_lossy(&translated.stderr),
            fs::read_to_string(migrated.join("Derived.kt")).unwrap_or_default()
        );
        assert!(
            migrated.join("SyntheticDerived.java").is_file(),
            "SyntheticDerived must translate in Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Contract.kt").is_file());
        assert!(migrated.join("Base.kt").is_file());
        assert!(migrated.join("SyntheticBase.kt").is_file());
        assert!(migrated.join("Runner.kt").is_file());

        for name in ["Derived.java", "SyntheticDerived.java"] {
            let derived_java = fs::read_to_string(migrated.join(name)).unwrap();
            assert!(
                derived_java.contains("Contract") && derived_java.contains("getToken().read()"),
                "{name} should call the inherited contract getter:\n{derived_java}"
            );
        }

        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("translated Derived and retained Kotlin contract/base compile and run");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
