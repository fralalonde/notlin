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
            std::env::temp_dir().join(format!("notlin-enum-when-{}-{id}", std::process::id()));
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

fn compile_future_phase(kotlinc: &std::ffi::OsStr, stdlib: &Path, root: &Path) -> PathBuf {
    fs::create_dir_all(root).unwrap();
    let source = root.join("PhaseV2.kt");
    fs::write(
        &source,
        r#"package boundary
enum class Phase {
    ALPHA, BETA, GAMMA, DELTA;
    suspend fun retainedHook() {}
}
"#,
    )
    .unwrap();
    let output_dir = root.join("future phase classes");
    fs::create_dir_all(&output_dir).unwrap();
    let compiled = std::process::Command::new(kotlinc)
        .args(["-jvm-target", "17", "-classpath"])
        .arg(stdlib)
        .arg("-d")
        .arg(&output_dir)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "future enum compilation failed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    output_dir
}

fn assert_binary_skew_behavior(
    api_classes: &Path,
    future_phase_classes: &Path,
    stdlib: &Path,
    javac: &std::ffi::OsStr,
    java: &std::ffi::OsStr,
    probe_root: &Path,
) {
    fs::create_dir_all(probe_root).unwrap();
    let source = probe_root.join("Probe.java");
    fs::write(
        &source,
        r#"import boundary.Dispatcher;
import boundary.Phase;

public final class Probe {
    public static void main(String[] args) {
        try {
            System.out.println(new Dispatcher().exhaustive(Phase.valueOf("DELTA")));
        } catch (Throwable error) {
            System.out.println(error.getClass().getName());
        }
    }
}
"#,
    )
    .unwrap();
    let probe_classes = probe_root.join("probe classes");
    fs::create_dir_all(&probe_classes).unwrap();
    let compile_classpath = std::env::join_paths([api_classes, stdlib]).unwrap();
    let compiled = std::process::Command::new(javac)
        .args(["--release", "17", "-proc:none", "-classpath"])
        .arg(compile_classpath)
        .arg("-d")
        .arg(&probe_classes)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "binary-skew probe compilation failed against {}:\n{}",
        api_classes.display(),
        String::from_utf8_lossy(&compiled.stderr)
    );

    let runtime_classpath = std::env::join_paths([
        future_phase_classes,
        api_classes,
        probe_classes.as_path(),
        stdlib,
    ])
    .unwrap();
    let result = std::process::Command::new(java)
        .arg("-classpath")
        .arg(runtime_classpath)
        .arg("Probe")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "binary-skew probe failed at runtime:\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&result.stdout).trim(),
        "kotlin.NoWhenBranchMatchedException",
        "new enum entry must follow Kotlin's exhaustive-when behavior"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_enum_when_behavior_in_mixed_sources() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();

    let sources = [
        (
            "Phase.kt",
            r#"package boundary
enum class Phase {
    ALPHA, BETA, GAMMA;
    suspend fun retainedHook() {}
}
"#,
        ),
        (
            "PhaseSource.kt",
            r#"package boundary
class PhaseSource(private val phase: Phase) {
    var evaluations: Int = 0
    val events: StringBuilder = StringBuilder()

    fun nextPhase(): Phase {
        evaluations += 1
        events.append("subject;")
        return phase
    }

    fun selected(name: String): String {
        events.append(name).append(";")
        return name
    }

    suspend fun retainedHook() {}
}
"#,
        ),
        (
            "Dispatcher.kt",
            r#"package boundary
import java.lang.IllegalArgumentException

class Dispatcher {
    fun grouped(phase: Phase): String = when (phase) {
        Phase.ALPHA, Phase.BETA -> "early"
        Phase.GAMMA -> "late"
    }

    fun fallback(phase: Phase): String = when (phase) {
        Phase.ALPHA -> "alpha"
        else -> "fallback"
    }

    fun exhaustive(phase: Phase): String = when (phase) {
        Phase.ALPHA -> "A"
        Phase.BETA -> "B"
        Phase.GAMMA -> "G"
    }

    fun numeric(phase: Phase): Any = when (phase) {
        Phase.ALPHA -> 1
        Phase.BETA -> 2.0
        Phase.GAMMA -> 3L
    }

    fun selected(source: PhaseSource): String = when (source.nextPhase()) {
        Phase.ALPHA -> source.selected("alpha")
        Phase.BETA -> source.selected("beta")
        Phase.GAMMA -> source.selected("gamma")
    }

    fun failing(phase: Phase): String = when (phase) {
        Phase.ALPHA -> "ok"
        Phase.BETA -> throw IllegalArgumentException("bad phase")
        Phase.GAMMA -> "ok"
    }
}
"#,
        ),
        (
            "NullableDispatcher.kt",
            r#"package boundary
class NullableDispatcher {
    fun value(phase: Phase?): String = when (phase) {
        Phase.ALPHA -> "alpha"
        else -> "fallback"
    }
}
"#,
        ),
        (
            "Runner.kt",
            r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val dispatcher = Dispatcher()
    val alphaSource = PhaseSource(Phase.ALPHA)
    val betaSource = PhaseSource(Phase.BETA)
    val gammaSource = PhaseSource(Phase.GAMMA)
    val alpha = dispatcher.selected(alphaSource) + ":" + alphaSource.evaluations + ":" + alphaSource.events
    val beta = dispatcher.selected(betaSource) + ":" + betaSource.evaluations + ":" + betaSource.events
    val gamma = dispatcher.selected(gammaSource) + ":" + gammaSource.evaluations + ":" + gammaSource.events
    val thrown = try {
        dispatcher.failing(Phase.BETA)
        "none"
    } catch (error: IllegalArgumentException) {
        error::class.java.simpleName
    }
    val nullable = NullableDispatcher().value(null)
    println(listOf(
        dispatcher.grouped(Phase.ALPHA), dispatcher.grouped(Phase.BETA), dispatcher.grouped(Phase.GAMMA),
        dispatcher.fallback(Phase.GAMMA), dispatcher.exhaustive(Phase.ALPHA),
        dispatcher.exhaustive(Phase.BETA), dispatcher.exhaustive(Phase.GAMMA),
        dispatcher.numeric(Phase.ALPHA).javaClass.simpleName,
        dispatcher.numeric(Phase.BETA).javaClass.simpleName,
        dispatcher.numeric(Phase.GAMMA).javaClass.simpleName,
        alpha, beta, gamma, thrown, nullable
    ).joinToString("|"))
}
"#,
        ),
    ];
    for (name, source) in sources {
        fs::write(original.join(name), source).unwrap();
    }

    let original_classes = scratch.0.join("original classes");
    let before = validate_jvm_sources(&config(&original, original_classes.clone(), &stdlib, false))
        .expect("original Kotlin enum when fixture compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "early|early|late|fallback|A|B|G|Integer|Double|Long|alpha:1:subject;alpha;|beta:1:subject;beta;|gamma:1:subject;gamma;|IllegalArgumentException|fallback"
    );

    let future_phase_classes = compile_future_phase(
        &std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        &stdlib,
        &scratch.0.join("future enum"),
    );
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    assert_binary_skew_behavior(
        &original_classes,
        &future_phase_classes,
        &stdlib,
        &javac,
        &java,
        &scratch.0.join("original skew probe"),
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
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            migrated.join("Dispatcher.java").is_file(),
            "Dispatcher must translate in Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(migrated.join("Phase.kt").is_file());
        assert!(migrated.join("PhaseSource.kt").is_file());
        assert!(migrated.join("Runner.kt").is_file());
        assert!(migrated.join("NullableDispatcher.kt").is_file());
        assert!(!migrated.join("NullableDispatcher.java").exists());

        let dispatcher_java = fs::read_to_string(migrated.join("Dispatcher.java")).unwrap();
        assert!(
            dispatcher_java.contains("switch ("),
            "enum when should emit Java switch expressions:\n{dispatcher_java}"
        );
        assert!(
            dispatcher_java.contains("NoWhenBranchMatchedException"),
            "an exhaustive enum when should preserve Kotlin's unmatched-branch exception:\n{dispatcher_java}"
        );

        let mixed_classes = scratch.0.join(format!("mixed classes-{lombok_enabled}"));
        let after = validate_jvm_sources(&config(
            &migrated,
            mixed_classes.clone(),
            &stdlib,
            lombok_enabled,
        ))
        .expect("translated dispatcher and retained Kotlin enum compile and run");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
        assert_binary_skew_behavior(
            &mixed_classes,
            &future_phase_classes,
            &stdlib,
            &javac,
            &java,
            &scratch.0.join(format!("mixed skew probe-{lombok_enabled}")),
        );
    }
}
