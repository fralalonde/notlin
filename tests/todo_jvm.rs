use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("notlin-todo-jvm-{}-{id}", std::process::id()));
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

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_standard_todo_exceptions() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    let sources = [
        (
            "Custom.kt",
            "package shadow\nfun TODO(): String = \"custom\"\nclass CustomApi { fun call(): String = TODO() }\n",
        ),
        (
            "PendingApi.kt",
            "package boundary\nimport java.util.*\ninterface PendingApi {\n    fun pending(): String {\n        TODO()\n    }\n}\n",
        ),
        (
            "TaskApi.kt",
            r#"package boundary
import kotlin.TODO
import java.util.*

class TaskApi : PendingApi {
    fun direct(): String = TODO()
    fun reason(): String = TODO("reason")
    fun body(): String {
        TODO("body")
    }
    fun explicitReturn(): String {
        return TODO("return")
    }
    val property: String
        get() = TODO("getter")
}
"#,
        ),
        (
            "Runner.kt",
            r#"package boundary
private fun capture(block: () -> Any?): String = try {
    block()
    "returned"
} catch (error: Throwable) {
    error::class.java.name + ":" + error.message
}

fun main() {
    suspend fun retainedHarness() {}
    val api = TaskApi()
    println(listOf(
        capture { api.direct() },
        capture { api.reason() },
        capture { api.body() },
        capture { api.explicitReturn() },
        capture { api.property },
        capture { api.pending() },
        shadow.CustomApi().call()
    ).joinToString("|"))
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
    .expect("original Kotlin TODO functions compile and run");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "kotlin.NotImplementedError:An operation is not implemented.|kotlin.NotImplementedError:An operation is not implemented: reason|kotlin.NotImplementedError:An operation is not implemented: body|kotlin.NotImplementedError:An operation is not implemented: return|kotlin.NotImplementedError:An operation is not implemented: getter|kotlin.NotImplementedError:An operation is not implemented.|custom"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        fs::create_dir_all(&migrated).unwrap();
        for (name, source) in sources {
            fs::write(migrated.join(name), source).unwrap();
        }
        let translated = translate(&migrated, lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            migrated.join("TaskApi.java").is_file(),
            "TaskApi must translate in Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Runner.kt").is_file());
        assert!(migrated.join("PendingApi.java").is_file());
        let java = fs::read_to_string(migrated.join("TaskApi.java")).unwrap();
        assert!(java.contains("kotlin.NotImplementedError"), "{java}");
        for message in [
            "An operation is not implemented.",
            "An operation is not implemented: ",
            "\"reason\"",
            "\"body\"",
            "\"return\"",
            "\"getter\"",
        ] {
            assert!(java.contains(message), "missing {message:?} in:\n{java}");
        }

        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("translated TODO methods compile and run");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}

#[test]
fn same_package_todo_shadow_is_not_lowered() {
    let scratch = Scratch::new();
    let root = scratch.0.join("custom todo");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("CustomApi.kt"),
        r#"package boundary
fun TODO(): String = "custom TODO"

class CustomApi {
    fun call(): String = TODO()
}
"#,
    )
    .unwrap();

    let translated = translate(&root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    let retained = fs::read_to_string(root.join("CustomApi.java")).unwrap();
    assert!(
        retained.contains("return boundary.CustomApiKt.TODO();"),
        "{retained}"
    );
    assert!(!retained.contains("NotImplementedError"), "{retained}");
}
