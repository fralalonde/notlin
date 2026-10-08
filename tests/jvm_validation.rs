use notlin::jvm_validation::{
    JvmValidationConfig, JvmValidationError, JvmValidationStage, validate_jvm_sources,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        Self::new_under(&std::env::temp_dir(), label)
    }

    fn new_under(base: &Path, label: &str) -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!(
            "notlin jvm validation {label} {} {id}",
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

fn fake_tool(dir: &Path, name: &str, stdout: &str, stderr: &str, exit_code: i32) -> PathBuf {
    #[cfg(windows)]
    let path = dir.join(format!("{name}.cmd"));
    #[cfg(not(windows))]
    let path = dir.join(name);

    #[cfg(windows)]
    let contents = format!(
        "@echo off\r\n@echo {stdout}\r\n@echo args=%*\r
         @echo {stderr} 1>&2\r\n@exit /b {exit_code}\r\n"
    );
    #[cfg(not(windows))]
    let contents = format!(
        "#!/bin/sh\nprintf '%s\\n' '{}'\nprintf 'args:%s\\n' \"$*\"\nprintf '%s\\n' '{}' >&2\nexit {}\n",
        stdout.replace('\'', "'\\''"),
        stderr.replace('\'', "'\\''"),
        exit_code
    );
    fs::write(&path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }
    path
}

fn fixture_sources(dir: &Path) -> (PathBuf, PathBuf) {
    let kt = dir.join("Kotlin source.kt");
    let java = dir.join("Java source.java");
    fs::write(&kt, "class KotlinSource").unwrap();
    fs::write(&java, "class JavaSource {}").unwrap();
    (kt, java)
}

fn target_java_17(config: &mut JvmValidationConfig) {
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config.javac_args.extend(["--release".into(), "17".into()]);
}

fn enable_lombok_processing(config: &mut JvmValidationConfig) {
    let jar = std::env::var_os("NOTLIN_LOMBOK")
        .expect("set NOTLIN_LOMBOK to the Lombok jar for generated Lombok classes");
    config.classpath.push(PathBuf::from(jar));
    config.javac_args.push("-proc:full".into());
    let plugin = std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN")
        .expect("set NOTLIN_KOTLIN_LOMBOK_PLUGIN to the matching Kotlin Lombok compiler plugin");
    let mut argument = std::ffi::OsString::from("-Xplugin=");
    argument.push(plugin);
    config.kotlinc_args.push(argument);
}

#[test]
fn compiles_kotlin_then_java_and_preserves_paths_with_spaces() {
    let scratch = Scratch::new("ordered compile");
    let (kt, java) = fixture_sources(&scratch.0);
    let tools_dir = scratch.0.join("fake tools");
    fs::create_dir_all(&tools_dir).unwrap();
    let kotlinc = fake_tool(&tools_dir, "kotlinc", "kotlinc ran", "", 0);
    let javac = fake_tool(&tools_dir, "javac", "javac ran", "", 0);
    let runtime = fake_tool(&tools_dir, "java", "runtime ran", "", 0);
    let dependency = scratch.0.join("dependency classes");
    let mut config =
        JvmValidationConfig::new(kotlinc, javac, runtime, scratch.0.join("compiled classes"));
    config.kotlin_sources.push(kt);
    config.java_sources.push(java);
    config.classpath.push(dependency);
    config.run_main_class = Some("sample.Main".into());

    let report = validate_jvm_sources(&config).unwrap();
    assert_eq!(
        report.commands.iter().map(|c| c.stage).collect::<Vec<_>>(),
        vec![
            JvmValidationStage::KotlinCompile,
            JvmValidationStage::JavaCompile,
            JvmValidationStage::Runtime,
        ]
    );
    assert!(report.commands[0].stdout.contains("kotlinc ran"));
    assert!(report.commands[1].stdout.contains("javac ran"));
    assert!(report.commands[2].stdout.contains("runtime ran"));
    let cp = std::env::join_paths([config.output_dir.as_path(), config.classpath[0].as_path()])
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let kotlin_args = &report.commands[0].stdout;
    assert!(kotlin_args.contains("Kotlin source.kt"));
    assert!(kotlin_args.contains("Java source.java"));
    assert!(kotlin_args.contains(&cp));
    let java_args = &report.commands[1].stdout;
    assert!(java_args.contains("Java source.java"));
    assert!(java_args.contains(&cp));
    assert!(report.commands[2].stdout.contains("sample.Main"));
}

#[test]
fn returns_compiler_diagnostics_and_stops_at_first_failed_stage() {
    let scratch = Scratch::new("compiler failure");
    let (kt, java) = fixture_sources(&scratch.0);
    let tools_dir = scratch.0.join("fake tools");
    fs::create_dir_all(&tools_dir).unwrap();
    let kotlinc = fake_tool(
        &tools_dir,
        "kotlinc",
        "",
        "unresolved reference: Missing",
        3,
    );
    let javac = tools_dir.join(if cfg!(windows) {
        "missing-javac.cmd"
    } else {
        "missing-javac"
    });
    let runtime = tools_dir.join(if cfg!(windows) {
        "missing-java.cmd"
    } else {
        "missing-java"
    });
    let mut config = JvmValidationConfig::new(kotlinc, javac, runtime, scratch.0.join("out"));
    config.kotlin_sources.push(kt);
    config.java_sources.push(java);

    let error = validate_jvm_sources(&config).unwrap_err();
    match error {
        JvmValidationError::Failed(result) => {
            assert_eq!(result.stage, JvmValidationStage::KotlinCompile);
            assert_eq!(result.status_code, Some(3));
            assert!(
                result
                    .diagnostics()
                    .contains("unresolved reference: Missing")
            );
        }
        other => panic!("expected compiler failure, got {other}"),
    }
}

#[test]
fn rejects_empty_source_sets() {
    let scratch = Scratch::new("empty");
    let config = JvmValidationConfig::new("kotlinc", "javac", "java", scratch.0.join("out"));
    assert!(matches!(
        validate_jvm_sources(&config),
        Err(JvmValidationError::InvalidConfig(_))
    ));
}

#[test]
fn rejects_stale_classes_before_launching_a_compiler() {
    let scratch = Scratch::new("stale output");
    let output = scratch.0.join("classes");
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("Old.class"), b"stale bytecode").unwrap();
    let mut config =
        JvmValidationConfig::new("missing-kotlinc", "missing-javac", "missing-java", output);
    config.java_sources.push(scratch.0.join("New.java"));
    let error = validate_jvm_sources(&config).unwrap_err();
    assert!(
        matches!(error, JvmValidationError::InvalidConfig(ref message) if message.contains("stale classes"))
    );
}

#[test]
fn rejects_invalid_runtime_request_before_creating_output() {
    let scratch = Scratch::new("invalid runtime");
    let output = scratch.0.join("classes");
    let mut config =
        JvmValidationConfig::new("missing-kotlinc", "missing-javac", "missing-java", &output);
    config.java_sources.push(scratch.0.join("New.java"));
    config.run_main_class = Some(" ".into());
    assert!(matches!(
        validate_jvm_sources(&config),
        Err(JvmValidationError::InvalidConfig(_))
    ));
    assert!(!output.exists());
}

/// Set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB to
/// exercise the retained Kotlin plus transpiler-generated Java boundary.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_compiles_transpiled_java_with_retained_kotlin() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    // The transpiler runs under the repository sandbox, so keep this isolated
    // integration fixture inside the workspace rather than the system temp dir.
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "real toolchain",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let kt = root.join("Greeter.kt");
    let retained = root.join("Retained.kt");
    fs::write(
        &kt,
        "package boundary\nclass Greeter {\n    fun fromRetained(): String = Retained().label()\n}\n",
    )
    .unwrap();
    fs::write(
        &retained,
        "package boundary\nclass Retained {\n    fun label(): String = \"from-kotlin\"\n    suspend fun suspendedLabel(): String = label()\n    fun generatedReference(): String = Greeter().fromRetained()\n}\nfun main() {\n    println(Greeter().fromRetained())\n}\n",
    )
    .unwrap();

    // Record the original Kotlin behavior before migration. This compile uses
    // both source files directly, so the same retained class and entry point
    // become the reference behavior for the mixed Kotlin/Java run below.
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline
        .kotlin_sources
        .extend([kt.clone(), retained.clone()]);
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RetainedKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    assert_eq!(baseline_report.commands.len(), 2);
    let baseline_stdout = baseline_report.commands[1].stdout.trim().to_owned();

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none"])
        .arg("--in-place")
        .arg(&kt)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let generated_java = root.join("Greeter.java");
    assert!(
        generated_java.is_file(),
        "transpiler did not emit Greeter.java; outputs: {:?}; stderr: {}",
        fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>(),
        String::from_utf8_lossy(&transpile.stderr)
    );
    let mut config =
        JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut config);
    config.kotlin_sources.push(retained);
    config.java_sources.push(generated_java);
    config.classpath.push(PathBuf::from(stdlib));
    config.run_main_class = Some("boundary.RetainedKt".into());
    let report = validate_jvm_sources(&config).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "transpiled Java plus retained Kotlin must match the original Kotlin runtime output"
    );
}

/// Exercise Java/Kotlin ownership seams together: synthesized properties,
/// default constructors, companion dispatch, data-class equality, JVM argument
/// evaluation order, and a translated subclass of a retained Kotlin base.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_mixed_runtime_boundaries() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "mixed runtime boundaries",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let features = root.join("Features.kt");
    let runner = root.join("Runner.kt");
    fs::write(
        &features,
        "package boundary\n\
         data class Measurement(val amount: Int, val label: String = \"unit\")\n\
         class Device(val id: Int) {\n\
             companion object {\n\
                 fun create(id: Int): Device = Device(id)\n\
             }\n\
         }\n\
         class MutableCounter(var count: Int) {\n\
             fun increment() { count += 1 }\n\
         }\n\
         fun combine(first: Int, second: Int): Int = first * 10 + second\n\
         class GeneratedChild(seed: Int) : RetainedBase(seed) {\n\
             fun childValue(): Int = seed\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &runner,
        "package boundary\n\
         open class RetainedBase(val seed: Int) {\n\
             suspend fun retainedBoundary() {}\n\
         }\n\
         private var evaluationOrder = \"\"\n\
         private fun first(): Int { evaluationOrder += \"1\"; return 3 }\n\
         private fun second(): Int { evaluationOrder += \"2\"; return 4 }\n\
         fun main() {\n\
         \x20\x20\x20\x20val defaulted = Measurement(7)\n\
         \x20\x20\x20\x20val equal = Measurement(7, \"unit\") == defaulted\n\
         \x20\x20\x20\x20val device = Device.Companion.create(9)\n\
         \x20\x20\x20\x20val counter = MutableCounter(2); counter.increment()\n\
         \x20\x20\x20\x20val combined = combine(first(), second())\n\
         \x20\x20\x20\x20val child = GeneratedChild(5)\n\
         \x20\x20\x20\x20println(\"${defaulted.amount}:${defaulted.label}:$equal:${device.id}:${counter.count}:$evaluationOrder:$combined:${child.childValue()}:${child.seed}\")\n\
         }\n",
    ).unwrap();

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline
        .kotlin_sources
        .extend([features.clone(), runner.clone()]);
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    assert_eq!(baseline_stdout, "7:unit:true:9:3:12:34:5:5");

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "{}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let kotlin_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "kt"))
        .collect::<Vec<_>>();
    let java_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "java"))
        .collect::<Vec<_>>();
    assert!(
        !java_sources.is_empty(),
        "translation must emit Java sources"
    );
    for expected in [
        "Measurement.java",
        "Device.java",
        "MutableCounter.java",
        "GeneratedChild.java",
        "Features.java",
    ] {
        assert!(
            java_sources
                .iter()
                .any(|path| path.file_name().is_some_and(|name| name == expected)),
            "missing generated runtime boundary {expected}; outputs: {java_sources:?}; Kotlin: {:?}; stderr: {}",
            kotlin_sources
                .iter()
                .map(|path| (path.clone(), fs::read_to_string(path).unwrap_or_default()))
                .collect::<Vec<_>>(),
            String::from_utf8_lossy(&transpile.stderr)
        );
    }
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = kotlin_sources;
    mixed.java_sources = java_sources;
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some(if runner.exists() {
        "boundary.RunnerKt".into()
    } else {
        "boundary.Runner".into()
    });
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "mixed translated Java and retained Kotlin must match the original Kotlin execution"
    );
}

/// Lombok mode must preserve Kotlin initialization semantics when a data
/// class has initialized body properties or init blocks. These cases use the
/// explicit Java fallback and therefore need no Lombok annotation processor.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_initialized_data_class_behavior_in_lombok_mode() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "lombok initialized data class",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let model = root.join("Initialized.kt");
    let runner = root.join("Runner.kt");
    fs::write(
        &model,
        "package boundary\n\
         data class Initialized(val base: Int, var mutable: Int) {\n\
             val first = base + 1\n\
             init { mutable += first }\n\
             var second = mutable + first\n\
             init { second += mutable }\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &runner,
        "package boundary\n\
         fun main() {\n\
             val value = Initialized(4, 3)\n\
             println(\"${value.base}:${value.mutable}:${value.first}:${value.second}\")\n\
         }\n",
    )
    .unwrap();

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline
        .kotlin_sources
        .extend([model.clone(), runner.clone()]);
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    assert_eq!(baseline_stdout, "4:8:5:21");

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "{}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let kotlin_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect::<Vec<_>>();
    let java_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect::<Vec<_>>();
    assert!(
        java_sources.iter().any(|path| path
            .file_name()
            .is_some_and(|name| name == "Initialized.java")),
        "missing generated Initialized.java; outputs: {java_sources:?}; stderr: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = kotlin_sources;
    mixed.java_sources = java_sources;
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some(if runner.exists() {
        "boundary.RunnerKt".into()
    } else {
        "boundary.Runner".into()
    });
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(
        report.commands.len(),
        2 + usize::from(!mixed.kotlin_sources.is_empty())
    );
    assert_eq!(
        report.commands.last().unwrap().stdout.trim(),
        baseline_stdout,
        "Lombok-mode Java must preserve initialized data class field values and init ordering"
    );
}

/// Keep a retained interface's body-backed property getter callable after a
/// parallel generated interface's same-named property is migrated to Java.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_parallel_interface_property_accessors() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "parallel interface properties",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let generated_api = root.join("GeneratedApi.kt");
    let retained_api = root.join("RetainedApi.kt");
    let implementation = root.join("Implementation.kt");
    let callers = root.join("Callers.kt");
    let runner = root.join("Runner.kt");
    fs::write(
        &generated_api,
        "package boundary\ninterface GeneratedApi { val category: String }\n",
    )
    .unwrap();
    fs::write(
        &retained_api,
        "package boundary\ninterface Api { val category: String; val alias: Int get() = category.length; suspend fun retainedMarker(): Unit }\n",
    )
    .unwrap();
    fs::write(
        &implementation,
        "package boundary\ninterface ApiChild : Api, GeneratedApi {}\nclass Impl(override val category: String) : ApiChild { override suspend fun retainedMarker(): Unit {} }\n",
    )
    .unwrap();
    fs::write(
        &callers,
        "package boundary\nfun readCategory(value: Api): String = value.category\nfun readAlias(value: Api): Int = value.alias\n",
    )
    .unwrap();
    fs::write(
        &runner,
        "package boundary\nfun main() { val value = Impl(\"alpha\"); println(\"${readCategory(value)}:${readAlias(value)}\") }\n",
    )
    .unwrap();

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources.extend([
        generated_api.clone(),
        retained_api.clone(),
        implementation.clone(),
        callers.clone(),
        runner.clone(),
    ]);
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    assert_eq!(baseline_stdout, "alpha:5");

    // Model the generated interface ABI directly, then exercise the public
    // property repair pipeline on the retained Kotlin sources.
    let generated_java = root.join("GeneratedApi.java");
    let java_ir = notlin::java_ir::parse_java(
        "package boundary; public interface GeneratedApi { String getCategory(); }",
    )
    .unwrap();
    fs::write(&generated_java, notlin::java_ir::render(&java_ir)).unwrap();
    fs::remove_file(&generated_api).unwrap();

    let preindex = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let generated_java_paths = [fs::canonicalize(&generated_java).unwrap()]
        .into_iter()
        .collect();
    let contracts =
        notlin::property_abi::repaired_callsite_contracts(&preindex, &generated_java_paths);
    let mut repaired_sources = preindex
        .kotlin_files()
        .map(|source_file| {
            (
                source_file.path.clone(),
                source_file.source_text().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    notlin::property_abi::repair_virtual_sources(
        &preindex,
        &mut repaired_sources,
        &generated_java_paths,
    );
    for (path, source) in &repaired_sources {
        fs::write(path, source).unwrap();
    }
    let postindex = notlin::workspace::SourceIndex::discover(&root).unwrap();
    for (path, source) in &repaired_sources {
        let (rewritten, _) =
            notlin::property_callsite::rewrite_file(&postindex, path, source, &contracts);
        fs::write(path, rewritten).unwrap();
    }

    let kotlin_sources = repaired_sources
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    let java_sources = vec![generated_java];
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = kotlin_sources;
    mixed.java_sources = java_sources;
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some(if runner.exists() {
        "boundary.RunnerKt".into()
    } else {
        "boundary.Runner".into()
    });
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "mixed Kotlin and Java must preserve both same-named property accessors"
    );
}

/// Compare Kotlin event dispatch with the mixed runtime after an event
/// interface crosses the covariant-property ABI repair boundary.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_covariant_event_property_dispatch() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "covariant event properties",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let root_event = root.join("RootEvent.kt");
    let identifiers = root.join("Identifiers.kt");
    let broad_event = root.join("BroadEvent.kt");
    let implementation = root.join("Implementation.kt");
    let callers = root.join("Callers.kt");
    let runner = root.join("Runner.kt");
    fs::write(
        &root_event,
        "package boundary\n\
         interface ScalarRoot {\n\
             val label: String\n\
         }\n\
         interface ScalarLeft : ScalarRoot {}\n\
         interface ScalarRight : ScalarRoot {}\n\
         interface RootEvent : ScalarLeft, ScalarRight {\n\
             val id: ChildId\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &identifiers,
        "package boundary\ninterface Id {\n    fun text(): String\n}\n\
         class ChildId : Id {\n    override fun text(): String = \"item-7\"\n}\n",
    )
    .unwrap();
    fs::write(
        &broad_event,
        "package boundary\n\
         interface BroadEvent<T : Id> {\n\
             val id: T\n\
         val display: String get() = \"event:\" + id.text()\n\
             suspend fun retainedMarker(): Unit\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &implementation,
        "package boundary\ninterface EventChild : RootEvent, BroadEvent<Id> {}\n\
         class EventImpl(override val id: ChildId, override val label: String) : EventChild {\n\
             override suspend fun retainedMarker(): Unit {}\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &callers,
        "package boundary\nfun readRootId(value: RootEvent): String = value.id.text()\nfun readBroadId(value: BroadEvent<Id>): String = value.id.text()\nfun readDisplay(value: BroadEvent<Id>): String = value.display\n",
    )
    .unwrap();
    fs::write(
        &runner,
        "package boundary\nfun main() { val value = EventImpl(ChildId(), \"record\"); println(\"${readRootId(value)}:${readBroadId(value)}:${readDisplay(value)}:${value.label}\") }\n",
    )
    .unwrap();

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources.extend([
        root_event.clone(),
        identifiers.clone(),
        broad_event.clone(),
        implementation.clone(),
        callers.clone(),
        runner.clone(),
    ]);
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    assert_eq!(baseline_stdout, "item-7:item-7:event:item-7:record");

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        root.join("RootEvent.java").is_file(),
        "RootEvent should migrate to Java; stderr: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        broad_event.is_file() && !root.join("BroadEvent.java").exists(),
        "the body-backed broad interface should remain Kotlin"
    );

    let kotlin_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect::<Vec<_>>();
    let java_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect::<Vec<_>>();
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = kotlin_sources;
    mixed.java_sources = java_sources;
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some(if runner.exists() {
        "boundary.RunnerKt".into()
    } else {
        "boundary.Runner".into()
    });
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "Lombok-mode mixed Kotlin and Java must preserve covariant event property dispatch"
    );
}

/// Compare Kotlin's literal `split(...).last()` behavior with the generated
/// Java default methods across a Java-defined lookup contract.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_uuid_identifier_and_literal_split_behavior() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "UUID identifier split behavior",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("LookupKey.java"),
            "package boundary; public interface LookupKey { String getLookupKey(); }\n",
        )
        .unwrap();
        fs::write(
            root.join("Identifier.kt"),
            "package boundary\n\
             import java.util.*\n\
             private const val KEY_DELIMITER: String = \"/\"\n\
             interface Identifier : LookupKey {\n\
                 val objectId: UUID\n\
                 fun text(): String { return objectId.toString() }\n\
                 fun separatorText(): String { return \"before:$KEY_DELIMITER:${KEY_DELIMITER}\" }\n\
                 val externalKey: String\n\
                     get() { return getLookupKey().split(KEY_DELIMITER).last() }\n\
                 fun lastPiece(value: String, delimiter: String): String {\n\
                     return value.split(delimiter).last()\n\
                 }\n\
                 fun nullableLast(values: List<String?>): String? { return values.last() }\n\
             }\n",
        )
        .unwrap();
        fs::write(
            root.join("Implementation.kt"),
            "package boundary\n\
             import java.util.UUID\n\
             class Implementation(override val objectId: UUID, private val key: String) : Identifier {\n\
                 override fun getLookupKey(): String { return key }\n\
                 suspend fun retainedBoundary(): Unit {}\n\
             }\n",
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            "package boundary\n\
             import java.util.UUID\n\
             fun encoded(value: String): String = \"${value.length}:$value\"\n\
             fun main() {\n\
                 val value = Implementation(UUID.fromString(\"123e4567-e89b-12d3-a456-426614174000\"), \"scheme/object/\")\n\
                 val nullable = value.nullableLast(listOf(\"present\", null)) ?: \"<null>\"\n\
                 val emptyFailure = try {\n\
                     value.nullableLast(emptyList())\n\
                     \"no-exception\"\n\
                 } catch (failure: NoSuchElementException) { failure.javaClass.name }\n\
                 println(listOf(value.text(), encoded(value.externalKey), encoded(value.separatorText()), encoded(value.lastPiece(\"a.b.\", \".\")), encoded(value.lastPiece(\"a|b||\", \"|\")), encoded(value.lastPiece(\"abc\", \"\")), nullable, emptyFailure).joinToString(\";\"))\n\
             }\n",
        )
        .unwrap();
    }

    let baseline_kotlin = [
        baseline_root.join("Identifier.kt"),
        baseline_root.join("Implementation.kt"),
        baseline_root.join("Runner.kt"),
    ];
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = baseline_kotlin.to_vec();
    baseline
        .java_sources
        .push(baseline_root.join("LookupKey.java"));
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&mixed_root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        mixed_root.join("Identifier.java").is_file(),
        "Identifier should migrate; stderr: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );

    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "mixed Kotlin and Java must preserve UUID lookup and literal split/last behavior"
    );
}

/// A UUID interface can cross to Java while a retained Kotlin superinterface
/// still owns a generic `id` property; repairing the UUID accessor must leave
/// the unrelated Kotlin property override intact.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_parallel_generic_id_during_uuid_bridge() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "parallel generic id during UUID bridge",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Identity.kt"),
            "package boundary\n\
             import java.util.UUID\n\
             interface BaseId {\n    fun text(): String\n}\n\
             data class ExampleId(private val value: String) : BaseId {\n\
                 override fun text(): String = value\n\
             }\n\
             interface UuidIdentity {\n    val objectId: UUID\n}\n\
             interface IdView<T : BaseId> {\n\
                 val id: T\n\
                 suspend fun retainedMarker(): Unit\n\
             }\n\
             interface Identifier<T : BaseId> : UuidIdentity, IdView<T>\n\
             class ExampleInfo(override val objectId: UUID, override val id: ExampleId) : Identifier<ExampleId> {\n\
                 override suspend fun retainedMarker(): Unit {}\n\
             }\n\
             fun main() {\n\
                 val value = ExampleInfo(UUID.fromString(\"123e4567-e89b-12d3-a456-426614174000\"), ExampleId(\"value-7\"))\n\
                 println(\"${value.objectId}:${value.id.text()}\")\n\
             }\n",
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline
        .kotlin_sources
        .push(baseline_root.join("Identity.kt"));
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.IdentityKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&mixed_root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        mixed_root.join("UuidIdentity.java").is_file(),
        "UUID interface should migrate; stderr: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        mixed_root.join("Identity.kt").is_file(),
        "generic id branch and implementation should stay Kotlin"
    );
    let mixed_source = fs::read_to_string(mixed_root.join("Identity.kt")).unwrap();
    assert!(
        mixed_source.contains("override val id: ExampleId"),
        "the unrelated generic property override should retain Kotlin property syntax:\n{mixed_source}"
    );
    assert!(
        !mixed_source.contains("@JvmField val id"),
        "the UUID bridge must not replace the parallel id property with a field:\n{mixed_source}"
    );

    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.IdentityKt".into());
    enable_lombok_processing(&mut mixed);
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "mixed Kotlin and Java must preserve both UUID and generic id behavior"
    );
}

/// A Kotlin child can extend a retained abstract JPA superclass from Java only
/// when its positional super call exactly forwards a matching constructor
/// parameter and the superclass emits the omitted-default overload.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_constructor_call_to_retained_mapped_superclass() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "retained mapped superclass constructor",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root.join("jakarta/persistence")).unwrap();
        fs::write(
            root.join("jakarta/persistence/MappedSuperclass.java"),
            "package jakarta.persistence;\npublic @interface MappedSuperclass {}\n",
        )
        .unwrap();
        fs::write(
            root.join("Parent.kt"),
            r#"package boundary
import jakarta.persistence.MappedSuperclass

@MappedSuperclass
abstract class Parent @JvmOverloads constructor(
    open val id: String,
    open val version: Int = 7
) {
    suspend fun retainedBoundary(): Unit {}
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("Child.kt"),
            r#"package boundary
class Child(id: String, val token: String) : Parent(id)

class AmountUpdate(val amount: String, val unit: String)

class Entry(var amount: String, var unit: String) {
    fun setAmount(value: AmountUpdate) {
        this.amount = value.amount
        this.unit = value.unit
    }
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package boundary
fun main() {
    val child = Child("id-9", "token-4")
    val entry = Entry("amount-before", "unit-before")
    entry.setAmount(AmountUpdate("amount-updated", "unit-updated"))
    entry.amount = "direct-updated"
    println("${child.id}:${child.version}:${child.token}:${entry.amount}:${entry.unit}")
}
"#,
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [
        baseline_root.join("Parent.kt"),
        baseline_root.join("Child.kt"),
        baseline_root.join("Runner.kt"),
    ]
    .into();
    baseline
        .java_sources
        .push(baseline_root.join("jakarta/persistence/MappedSuperclass.java"));
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&mixed_root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let child_java = mixed_root.join("Child.java");
    assert!(
        child_java.is_file(),
        "the exact constructor proof should migrate Child; stderr: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        fs::read_to_string(mixed_root.join("Parent.kt"))
            .unwrap()
            .contains("abstract class Parent"),
        "the suspend boundary should keep Parent in Kotlin"
    );
    assert!(
        fs::read_to_string(child_java)
            .unwrap()
            .contains("extends Parent")
    );
    let entry_java = fs::read_to_string(mixed_root.join("Entry.java"))
        .expect("the mutable constructor-property fixture should migrate");
    assert!(
        entry_java.contains("this.amount = value.getAmount();"),
        "{entry_java}"
    );
    assert!(
        entry_java.contains("this.setUnit(value.getUnit());"),
        "{entry_java}"
    );

    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed
        .java_sources
        .push(mixed_root.join("jakarta/persistence/MappedSuperclass.java"));
    enable_lombok_processing(&mut mixed);
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some(if mixed_root.join("Runner.kt").exists() {
        "boundary.RunnerKt".into()
    } else {
        "boundary.Runner".into()
    });
    let mixed_report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(mixed_report.commands.len(), 3);
    assert_eq!(
        mixed_report.commands[2].stdout.trim(),
        baseline_stdout,
        "the Kotlin-to-Java constructor boundary must preserve inherited field values"
    );
}

/// A Kotlin override may narrow a nullable inherited getter return. The
/// generated Java declaration must carry matching nullability metadata so a
/// retained Kotlin caller sees the narrowed type after migration.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_narrowed_nullable_getter_contract() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "narrowed nullable getter contract",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root.join("org/jetbrains/annotations")).unwrap();
        fs::write(
            root.join("NullableBase.kt"),
            r#"package boundary
import java.math.BigDecimal

interface NullableBase {
    val measure: BigDecimal?
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("NarrowItem.kt"),
            r#"package boundary
import java.math.BigDecimal

interface NarrowItem : NullableBase {
    override val measure: BigDecimal
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("Caller.kt"),
            r#"package boundary

suspend fun itemScale(item: NarrowItem): Int = item.measure.scale()
"#,
        )
        .unwrap();
        fs::write(
            root.join("org/jetbrains/annotations/NotNull.java"),
            "package org.jetbrains.annotations;\n\
             @java.lang.annotation.Target({java.lang.annotation.ElementType.METHOD, java.lang.annotation.ElementType.PARAMETER, java.lang.annotation.ElementType.FIELD, java.lang.annotation.ElementType.TYPE_USE})\n\
             @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)\n\
             public @interface NotNull {}\n",
        )
        .unwrap();
        fs::write(
            root.join("org/jetbrains/annotations/Nullable.java"),
            "package org.jetbrains.annotations;\n\
             @java.lang.annotation.Target({java.lang.annotation.ElementType.METHOD, java.lang.annotation.ElementType.PARAMETER, java.lang.annotation.ElementType.FIELD, java.lang.annotation.ElementType.TYPE_USE})\n\
             @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)\n\
             public @interface Nullable {}\n",
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [
        baseline_root.join("NullableBase.kt"),
        baseline_root.join("NarrowItem.kt"),
        baseline_root.join("Caller.kt"),
    ]
    .into();
    baseline.java_sources = [
        baseline_root.join("org/jetbrains/annotations/NotNull.java"),
        baseline_root.join("org/jetbrains/annotations/Nullable.java"),
    ]
    .into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    validate_jvm_sources(&baseline).expect("original Kotlin hierarchy and caller compile");

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "jetbrains", "--in-place", "--lombok"])
        .arg(&mixed_root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let narrow_java = mixed_root.join("NarrowItem.java");
    assert!(narrow_java.is_file(), "narrow interface should migrate");
    let generated = fs::read_to_string(&narrow_java).unwrap();
    assert!(
        generated.contains("@org.jetbrains.annotations.NotNull"),
        "the overriding Java getter needs non-null metadata for Kotlin callers:\n{generated}"
    );
    assert!(mixed_root.join("NullableBase.java").is_file());
    assert!(mixed_root.join("Caller.kt").is_file());

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = [mixed_root.join("Caller.kt")].into();
    mixed.java_sources = [
        narrow_java,
        mixed_root.join("NullableBase.java"),
        mixed_root.join("org/jetbrains/annotations/NotNull.java"),
        mixed_root.join("org/jetbrains/annotations/Nullable.java"),
    ]
    .into();
    mixed.classpath.push(PathBuf::from(stdlib));
    validate_jvm_sources(&mixed)
        .expect("retained Kotlin caller compiles against migrated narrowed Java getter");
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_keeps_narrow_child_property_when_parent_is_intrinsically_retained() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "retained narrow property parent",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let variants = root.join("Types.kt");
    fs::write(
        &variants,
        "package boundary\ninterface BaseVariant {\n    fun baseLabel(): String\n}\ninterface NarrowVariant : BaseVariant {\n    fun narrowLabel(): String\n}\ninterface HolderContract {\n    val variant: BaseVariant\n    suspend fun checkpoint()\n}\n",
    )
    .unwrap();
    let detail_holder = root.join("DetailHolder.kt");
    fs::write(
        &detail_holder,
        "package boundary\ninterface DetailHolder : HolderContract {\n    override val variant: NarrowVariant\n}\n",
    )
    .unwrap();
    let caller = root.join("Caller.kt");
    fs::write(
        &caller,
        "package boundary\nfun narrowLabel(info: DetailHolder): String = info.variant.narrowLabel()\n",
    )
    .unwrap();

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [variants.clone(), detail_holder.clone(), caller.clone()].into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    validate_jvm_sources(&baseline)
        .expect("original Kotlin override keeps the narrow property type visible");

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .expect("run notlin on all selected sources");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(detail_holder.is_file());
    assert!(
        !root.join("DetailHolder.java").exists(),
        "the child must remain Kotlin while its intrinsically retained parent stays Kotlin"
    );
    let migrated = fs::read_to_string(&detail_holder).unwrap();
    assert!(
        migrated.contains("override val variant: NarrowVariant"),
        "the retained child must preserve its narrowed property declaration:\n{migrated}"
    );

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    validate_jvm_sources(&mixed)
        .expect("retained Kotlin child and caller preserve the narrow property type");
}

/// A memberless interface over a retained Kotlin property diamond must stay
/// Kotlin: inserting a Java interface there can leave Kotlin's inherited
/// getter fakeoverride without a real implementation during IR lowering.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_generic_default_property_through_java_bridge() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "generic default property through Java bridge",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("ContextParent.kt"),
            r#"package boundary

interface ContextRoot<T> {
    val contextKind: T
}

interface AbstractContextPath<T> : ContextRoot<T>

interface DefaultContextPath<T> : ContextRoot<T> {
    override val contextKind: T
        get() = defaultContextValue()

    fun defaultContextValue(): T
}

interface RetainedContext<T> : AbstractContextPath<T>, DefaultContextPath<T>
"#,
        )
        .unwrap();
        fs::write(
            root.join("Bridge.kt"),
            r#"package boundary

interface Bridge<T> : RetainedContext<T> {
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("ContextImpl.kt"),
            r#"package boundary

class ContextImpl : Bridge<String> {
    override fun defaultContextValue(): String = "context"
}

fun main() {
    val value = ContextImpl()
    println(value.contextKind)
}
"#,
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [
        baseline_root.join("ContextParent.kt"),
        baseline_root.join("Bridge.kt"),
        baseline_root.join("ContextImpl.kt"),
    ]
    .into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.ContextImplKt".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("the original all-Kotlin generic diamond compiles");
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();

    let bridge_source = mixed_root.join("Bridge.kt");
    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&bridge_source)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let bridge_java = mixed_root.join("Bridge.java");
    assert!(
        !bridge_java.exists(),
        "a memberless bridge over a retained Kotlin property diamond must stay Kotlin"
    );
    assert!(
        bridge_source.is_file(),
        "retained bridge source should remain"
    );
    assert!(mixed_root.join("ContextParent.kt").is_file());
    assert!(mixed_root.join("ContextImpl.kt").is_file());

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.ContextImplKt".into());
    let mixed_report = validate_jvm_sources(&mixed)
        .expect("the retained Kotlin bridge preserves its inherited property provider");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_stdout,
        "the mixed hierarchy must preserve the inherited property value"
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_super_property_on_recovered_generic_interface() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "recovered generic property interface",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let types = root.join("Types.kt");
    fs::write(
        &types,
        "package boundary\nimport kotlin.reflect.KClass\n\
         @Target(AnnotationTarget.CLASS)\n@Retention(AnnotationRetention.RUNTIME)\n\
         annotation class Read(val using: KClass<*>)\n\
         @Target(AnnotationTarget.CLASS)\n@Retention(AnnotationRetention.RUNTIME)\n\
         annotation class Write(val using: KClass<*>)\n\
         class Reader\nclass Writer\n",
    )
    .unwrap();
    let getter_tag = root.join("GetterTag.java");
    fs::write(&getter_tag, "package boundary;\n@java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD)\npublic @interface GetterTag {}\n").unwrap();
    let parent = root.join("Parent.kt");
    fs::write(
        &parent,
        "package boundary\n\
         @Read(using = Reader::class)\n\
         @Write(using = Writer::class)\n\
         interface Parent<T> {\n\
             @get:GetterTag\n\
             val contextKind: T\n\
             suspend fun parentCheckpoint()\n\
         }\n",
    )
    .unwrap();
    let java_bridge = root.join("JavaBridge.java");
    fs::write(
        &java_bridge,
        "package boundary;\n\
         public interface JavaBridge<T> extends Parent<T> {\n\
             @Override default T getContextKind() { return defaultKind(); }\n\
             T defaultKind();\n\
         }\n",
    )
    .unwrap();
    let child = root.join("Child.kt");
    fs::write(
        &child,
        "package boundary\n\
         interface Child<T> : JavaBridge<T> {\n\
             override val contextKind: T\n\
                 get() = super<JavaBridge<T>>.contextKind\n\
             suspend fun checkpoint() {}\n\
         }\n",
    )
    .unwrap();
    let implementation = root.join("Implementation.kt");
    fs::write(
        &implementation,
        "package boundary\nclass Implementation : Child<String> {\n\
             override fun defaultKind(): String = \"root\"\n\
             override suspend fun parentCheckpoint() {}\n\
         }\n",
    )
    .unwrap();
    let runner = root.join("Runner.kt");
    fs::write(
        &runner,
        "package boundary\nobject Runner {\n\
             @JvmStatic fun main(args: Array<String>) { println(Implementation().contextKind) }\n\
         }\n",
    )
    .unwrap();

    let source_files = [
        types.clone(),
        parent.clone(),
        child.clone(),
        implementation.clone(),
        runner.clone(),
    ];
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = source_files.clone().into();
    baseline.java_sources = vec![java_bridge.clone(), getter_tag];
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.Runner".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("baseline Kotlin hierarchy compiles");
    let baseline_stdout = baseline_report.commands.last().unwrap().stdout.trim();

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .expect("run notlin on all selected generic hierarchy sources");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(java_bridge.is_file(), "the Java bridge remains available");
    let java_bridge_source = fs::read_to_string(&java_bridge).unwrap();
    assert!(
        java_bridge_source.contains("getContextKind()") && java_bridge_source.contains("default"),
        "the Java bridge must retain its default getter:\n{java_bridge_source}"
    );
    assert!(parent.is_file(), "the annotated generic root stays Kotlin");
    let child_source = fs::read_to_string(&child).unwrap();
    assert!(
        !root.join("Child.java").exists(),
        "the suspend descendant must remain a Kotlin property override"
    );
    assert!(
        child_source.contains("override val contextKind: T")
            && child_source.contains("super<JavaBridge<T>>.contextKind")
            && !child_source.contains("fun getContextKind"),
        "the retained override must preserve its Kotlin super-property expression:\n{child_source}"
    );

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.Runner".into());
    let mixed_report = validate_jvm_sources(&mixed)
        .expect("retained Kotlin descendant compiles against generated Java default getter");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_stdout,
        "mixed runtime must preserve the super-property value"
    );
}

/// Keep three Java/Kotlin boundaries in one executable fixture: a retained
/// imported object referenced from a private serialization hook, an object
/// implementing a repaired property getter, and an overload whose nullable
/// primary-constructor default must coexist with a same-arity secondary.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_object_property_and_nullable_constructor_boundaries() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "object property and nullable constructor boundaries",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root.join("provider")).unwrap();
        fs::write(
            root.join("KindApi.kt"),
            "package boundary\ninterface KindApi<T> { val kind: T }\n",
        )
        .unwrap();
        fs::write(
            root.join("LocalMarker.kt"),
            "package boundary\nimport boundary.provider.ExternalSingleton\n\
             @java.lang.Deprecated\nobject LocalMarker : KindApi<String> {\n\
                 private fun readResolve(): Any = ExternalSingleton\n\
                 override val kind: String\n                     get() = \"marker\"\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("provider/ExternalSingleton.kt"),
            "package boundary.provider\nobject ExternalSingleton\n",
        )
        .unwrap();
        fs::write(
            root.join("Choice.kt"),
            "package boundary\nclass Context(val reason: String)\n\
             class Choice(val id: String, val context: Context?) {\n\
                 constructor(id: String) : this(id, null)\n\
                 constructor(id: String, reason: String) : this(id, Context(reason))\n\
             }\n",
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            "package boundary\nfun main() {\n\
             val marker = Class.forName(\"boundary.LocalMarker\").getField(\"INSTANCE\").get(null) as KindApi<String>\n\
             println(marker.kind)\n\
             val absent = Choice(\"item\")\n\
             val present = Choice(\"item\", \"reason\")\n\
             println(\"${absent.id}:${absent.context == null}:${present.context?.reason}\")\n}\n",
        )
        .unwrap();
    }

    let baseline = {
        let mut config =
            JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
        target_java_17(&mut config);
        config.kotlin_sources = fs::read_dir(&baseline_root)
            .unwrap()
            .flatten()
            .chain(
                fs::read_dir(baseline_root.join("provider"))
                    .unwrap()
                    .flatten(),
            )
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
            .collect();
        config.classpath.push(PathBuf::from(&stdlib));
        config.run_main_class = Some("boundary.RunnerKt".into());
        validate_jvm_sources(&config).expect("the original Kotlin fixture compiles")
    };
    let baseline_stdout = baseline.commands.last().unwrap().stdout.trim().to_owned();

    let selections = [
        mixed_root.join("KindApi.kt"),
        mixed_root.join("LocalMarker.kt"),
        mixed_root.join("Choice.kt"),
    ];
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"]);
    command.args(&selections);
    let transpile = command
        .output()
        .expect("run notlin on selected declarations");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(mixed_root.join("KindApi.java").is_file());
    assert!(mixed_root.join("LocalMarker.java").is_file());
    assert!(mixed_root.join("Choice.java").is_file());
    assert!(mixed_root.join("Context.java").is_file());
    assert!(mixed_root.join("provider/ExternalSingleton.kt").is_file());
    let marker_java = fs::read_to_string(mixed_root.join("LocalMarker.java")).unwrap();
    assert!(
        marker_java.contains("String getKind()")
            && !marker_java.contains("static String getKind()"),
        "repaired object property getter must be an instance override:\n{marker_java}"
    );
    let reader_java = fs::read_to_string(mixed_root.join("LocalMarker.java")).unwrap();
    assert!(
        reader_java.contains("ExternalSingleton.INSTANCE"),
        "retained imported Kotlin object must be read through its singleton field:\n{reader_java}"
    );

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .chain(fs::read_dir(mixed_root.join("provider")).unwrap().flatten())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .chain(fs::read_dir(mixed_root.join("provider")).unwrap().flatten())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report = validate_jvm_sources(&mixed)
        .expect("the mixed object/getter/constructor boundaries compile");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_stdout,
        "the mixed runtime must preserve object-property and constructor behavior"
    );
}

/// A selected interface property must remain in Kotlin when retained class
/// descendants override that property. Converting only the interface would
/// turn the Kotlin property contract into a Java getter the Kotlin overrides
/// cannot satisfy.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_retains_selected_interface_above_retained_class_property_overrides() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "retained interface above class property overrides",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Identified.kt"),
            r#"package boundary

interface Identified {
    var id: String
}


"#,
        )
        .unwrap();
        fs::write(
            root.join("JavaRoot.kt"),
            r#"package boundary

interface JavaRoot : Identified
"#,
        )
        .unwrap();
        fs::write(
            root.join("ParentRecord.kt"),
            r#"package boundary

abstract class ParentRecord(override open var id: String) : JavaRoot {
    suspend fun checkpoint() {}
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("ChildRecord.kt"),
            r#"package boundary

class ChildRecord(override var id: String) : ParentRecord(id), Identified {
    suspend fun childCheckpoint() {}
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package boundary

fun main() {
    val record: Identified = ChildRecord("value-7")
    println(record.id)
}
"#,
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("original Kotlin property hierarchy compiles");

    let selected = [
        mixed_root.join("Identified.kt"),
        mixed_root.join("JavaRoot.kt"),
        mixed_root.join("ParentRecord.kt"),
        mixed_root.join("ChildRecord.kt"),
    ];
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .args(selected);
    let transpile = command
        .output()
        .expect("run notlin on the selected hierarchy");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        mixed_root.join("JavaRoot.kt").is_file() && !mixed_root.join("JavaRoot.java").exists(),
        "the selected JavaRoot interface must stay Kotlin above retained class property overrides"
    );
    assert!(mixed_root.join("ParentRecord.kt").is_file());
    assert!(mixed_root.join("ChildRecord.kt").is_file());

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report =
        validate_jvm_sources(&mixed).expect("retained Kotlin property hierarchy compiles");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "the mixed hierarchy must preserve the inherited property value"
    );
}

/// Kotlin's read-only List parameter is covariant even at a Java constructor
/// boundary, so emitted constructor properties need a wildcard signature.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_readonly_list_covariance_in_constructor_properties() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "readonly list covariance in constructor properties",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("BaseRecord.kt"),
            "package boundary\ninterface BaseRecord { fun label(): String }\n",
        )
        .unwrap();
        fs::write(
            root.join("SpecificRecord.kt"),
            "package boundary\nclass SpecificRecord : BaseRecord { override fun label(): String = \"specific\" }\n",
        )
        .unwrap();
        fs::write(
            root.join("RecordBatch.kt"),
            "package boundary\nclass RecordBatch(val records: List<BaseRecord>)\n",
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            "package boundary\nfun main() { println(RecordBatch(listOf(SpecificRecord())).records.single().label()) }\n",
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [
        baseline_root.join("BaseRecord.kt"),
        baseline_root.join("SpecificRecord.kt"),
        baseline_root.join("RecordBatch.kt"),
        baseline_root.join("Runner.kt"),
    ]
    .into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline)
        .expect("original covariant Kotlin constructor call compiles");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .arg(mixed_root.join("RecordBatch.kt"));
    let transpile = command.output().expect("run notlin on RecordBatch");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let generated = fs::read_to_string(mixed_root.join("RecordBatch.java")).unwrap();
    assert!(
        generated.contains("List<? extends BaseRecord> records"),
        "the Java constructor/property ABI must preserve Kotlin List covariance:\n{generated}"
    );

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = [
        mixed_root.join("BaseRecord.kt"),
        mixed_root.join("SpecificRecord.kt"),
        mixed_root.join("Runner.kt"),
    ]
    .into();
    mixed.java_sources = [mixed_root.join("RecordBatch.java")].into();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report =
        validate_jvm_sources(&mixed).expect("Java constructor accepts a covariant Kotlin List");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "the mixed runtime must preserve the list contents"
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_enum_with_retained_generic_collection_contract() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "enum covariant constructor input",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Identifiable.kt"),
            "package boundary\ninterface Identifiable {\n    fun label(): String\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("SpecificEntry.kt"),
            "package boundary\nclass SpecificEntry : Identifiable {\n    override fun label(): String = \"specific-entry\"\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("BatchContract.kt"),
            "package boundary\ninterface BatchContract<T> {\n    val entries: List<T>\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("EntryBatch.kt"),
            "package boundary\nenum class EntryBatch(\n    override val entries: List<Identifiable>\n) : BatchContract<Identifiable> {\n    SINGLE(listOf(SpecificEntry()))\n}\n",
        )
        .unwrap();
        fs::write(root.join("Runner.kt"), "package boundary\nfun main() { val batch: BatchContract<Identifiable> = EntryBatch.SINGLE; println(batch.entries.single().label()) }\n").unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("baseline generic enum property compiles");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .arg(mixed_root.join("EntryBatch.kt"));
    let output = command.output().expect("run notlin on the enum");
    assert!(
        output.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        mixed_root.join("EntryBatch.kt").is_file(),
        "the enum remains Kotlin when its inherited generic property contract is retained"
    );
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report =
        validate_jvm_sources(&mixed).expect("migrated enum compiles over retained Kotlin contract");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_invariant_getters_with_covariant_constructor_inputs() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "invariant getters with covariant constructor inputs",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("BaseRecord.kt"),
            "package boundary\ninterface BaseRecord { fun label(): String }\n",
        )
        .unwrap();
        fs::write(root.join("SpecificRecord.kt"), "package boundary\nclass SpecificRecord : BaseRecord { override fun label(): String = \"specific\" }\n").unwrap();
        fs::write(
            root.join("BaseMetadata.kt"),
            "package boundary\ninterface BaseMetadata { fun value(): String }\n",
        )
        .unwrap();
        fs::write(root.join("SpecificMetadata.kt"), "package boundary\nclass SpecificMetadata : BaseMetadata { override fun value(): String = \"meta\" }\n").unwrap();
        fs::write(root.join("RecordHolder.kt"), "package boundary\nclass RecordHolder(val entries: List<BaseRecord>, val metadata: Map<String, BaseMetadata>)\n").unwrap();
        fs::write(root.join("RecordBatch.kt"), "package boundary\ndata class RecordBatch(val entries: List<BaseRecord>, val metadata: Map<String, BaseMetadata>)\n").unwrap();
        fs::write(root.join("Runner.kt"), r#"package boundary
fun main() {
    val entries: List<SpecificRecord> = listOf(SpecificRecord())
    val metadata: Map<String, SpecificMetadata> = mapOf("origin" to SpecificMetadata())
    val holder = RecordHolder(entries, metadata)
    println("${holder.entries.single().label()}:${holder.metadata.getValue("origin").value()}:${JavaConsumer.verify(holder, entries, metadata)}")
    val batch = RecordBatch(entries, metadata)
    println("${batch.entries.single().label()}:${batch.metadata.getValue("origin").value()}:${JavaConsumer.verify(batch, entries, metadata)}")
}
"#).unwrap();
        fs::write(root.join("JavaConsumer.java"), r#"package boundary;
import java.util.List;
import java.util.Map;
public final class JavaConsumer {
    public static boolean verify(RecordHolder batch, List<SpecificRecord> originalEntries, Map<String, SpecificMetadata> originalMetadata) {
        List<BaseRecord> entries = batch.getEntries();
        Map<String, BaseMetadata> metadata = batch.getMetadata();
        return ((Object) entries == originalEntries) && ((Object) metadata == originalMetadata)
                && entries.get(0) == originalEntries.get(0) && metadata.get("origin") == originalMetadata.get("origin");
    }
    public static boolean verify(RecordBatch batch, List<SpecificRecord> originalEntries, Map<String, SpecificMetadata> originalMetadata) {
        List<BaseRecord> entries = batch.getEntries();
        Map<String, BaseMetadata> metadata = batch.getMetadata();
        return ((Object) entries == originalEntries) && ((Object) metadata == originalMetadata)
                && entries.get(0) == originalEntries.get(0) && metadata.get("origin") == originalMetadata.get("origin");
    }
}
"#).unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    baseline.java_sources = [baseline_root.join("JavaConsumer.java")].into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline)
        .expect("original Kotlin constructor/getter ABI compiles for Java consumer");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&mixed_root);
    let output = command.output().expect("run notlin on RecordBatch");
    assert!(
        output.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let generated = fs::read_to_string(mixed_root.join("RecordHolder.java")).unwrap();
    assert!(
        generated.contains("private final List<BaseRecord> entries;")
            && generated.contains("private final Map<String,BaseMetadata> metadata;")
            && generated.contains("@Data"),
        "getters must remain invariant:\n{generated}"
    );
    assert!(
        generated.contains("List<? extends BaseRecord> entries")
            && generated.contains("Map<String, ? extends BaseMetadata> metadata"),
        "constructor inputs must preserve Kotlin covariance:\n{generated}"
    );
    let generated_data = fs::read_to_string(mixed_root.join("RecordBatch.java")).unwrap();
    assert!(
        generated_data.contains("List<BaseRecord> getEntries()")
            && generated_data.contains("Map<String,BaseMetadata> getMetadata()"),
        "data class getters must remain invariant:\n{generated_data}"
    );
    assert!(
        generated_data.contains("List<? extends BaseRecord> entries")
            && generated_data.contains("Map<String, ? extends BaseMetadata> metadata"),
        "data class constructor inputs must preserve Kotlin covariance:\n{generated_data}"
    );
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| {
                extension == "kt"
                    && path
                        .file_name()
                        .is_some_and(|name| name != "RecordHolder.kt" && name != "RecordBatch.kt")
            })
        })
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    enable_lombok_processing(&mut mixed);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report = validate_jvm_sources(&mixed)
        .expect("migrated class preserves getter typing and constructor variance");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "constructor bridge must preserve collection identity"
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_getter_routing_annotation_on_generic_command_interface() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "getter routing annotation on command interface",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::create_dir_all(root.join("routing")).unwrap();
        fs::write(
            root.join("routing/RoutingKey.java"),
            "package routing;\nimport java.lang.annotation.ElementType;\nimport java.lang.annotation.Retention;\nimport java.lang.annotation.RetentionPolicy;\nimport java.lang.annotation.Target;\n@Retention(RetentionPolicy.RUNTIME)\n@Target(ElementType.METHOD)\npublic @interface RoutingKey {}\n",
        )
        .unwrap();
        fs::write(
            root.join("CommandKey.kt"),
            "package boundary\ninterface CommandKey {\n    fun value(): String\n}\nclass SpecificKey(private val text: String) : CommandKey {\n    override fun value(): String = text\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("BaseCommand.kt"),
            "package boundary\nimport routing.RoutingKey\ninterface BaseCommand<T : CommandKey> {\n    val key: T\n    @get:RoutingKey val aggregateIdentifier: CommandKey\n        get() = key\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("GeneratedCommand.kt"),
            "package boundary\ndata class GeneratedCommand(\n    override val key: SpecificKey\n) : BaseCommand<SpecificKey>\n",
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package boundary
import routing.RoutingKey
fun main() {
    val getter = BaseCommand::class.java.getMethod("getAggregateIdentifier")
    val command = GeneratedCommand(SpecificKey("command-key"))
    println("${getter.isAnnotationPresent(RoutingKey::class.java)}:${command.aggregateIdentifier.value()}")
}
"#,
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    baseline.java_sources = [baseline_root.join("routing/RoutingKey.java")].into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("baseline annotated Kotlin interface compiles");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .arg(mixed_root.join("BaseCommand.kt"))
        .arg(mixed_root.join("GeneratedCommand.kt"))
        .output()
        .expect("run notlin on the generic command interface");
    assert!(
        output.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        mixed_root.join("BaseCommand.java").is_file(),
        "the annotated generic command interface should translate"
    );
    assert!(
        mixed_root.join("GeneratedCommand.java").is_file(),
        "the concrete data command should translate"
    );
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "kt")
                && !path.with_extension("java").is_file()
        })
        .collect();
    mixed.java_sources = vec![mixed_root.join("routing/RoutingKey.java")];
    mixed.java_sources.extend(
        fs::read_dir(&mixed_root)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "java")
            })
            .collect::<Vec<_>>(),
    );
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report = validate_jvm_sources(&mixed)
        .expect("migrated generic command interface and data class compile");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "the runtime routing annotation and command value must be preserved"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, and MapStruct jars in NOTLIN_* environment variables"]
fn real_toolchain_preserves_most_specific_inherited_reference_getter() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "most specific inherited reference getter",
    );
    let mapstruct = std::env::var_os("NOTLIN_MAPSTRUCT").map(PathBuf::from);
    let mapstruct_processor = std::env::var_os("NOTLIN_MAPSTRUCT_PROCESSOR").map(PathBuf::from);
    let (Some(mapstruct), Some(mapstruct_processor)) = (mapstruct, mapstruct_processor) else {
        panic!(
            "set NOTLIN_MAPSTRUCT and NOTLIN_MAPSTRUCT_PROCESSOR to the MapStruct API and processor jars"
        );
    };
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("ReferenceId.kt"),
            "package boundary\ninterface Identifier {\n    fun value(): String\n}\ninterface ObjectId : Identifier\ndata class ReferenceId(private val raw: String) : ObjectId {\n    override fun value(): String = raw\n}\nobject ReferenceIds {\n    val managed = ReferenceId(\"managed\")\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("ReferenceRoot.kt"),
            "package boundary\ninterface ReferenceRoot {\n    val reference: Identifier\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("NarrowReferenceContract.kt"),
            "package boundary\ninterface NarrowReferenceContract : ReferenceRoot {\n    override val reference: ReferenceId\n        get() = ReferenceIds.managed\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("NarrowReferenceProvider.kt"),
            "package boundary\ninterface NarrowReferenceProvider : NarrowReferenceContract\n",
        )
        .unwrap();
        fs::write(
            root.join("BroadReferenceContract.kt"),
            "package boundary\ninterface BroadReferenceContract : ReferenceRoot\n",
        )
        .unwrap();
        fs::write(
            root.join("RetainedReferenceContract.kt"),
            "package boundary\ninterface RetainedReferenceContract : NarrowReferenceProvider, BroadReferenceContract {\n    suspend fun pending(): String\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("ReferenceOutput.kt"),
            "package boundary\ndata class ReferenceOutput(val marker: String) : RetainedReferenceContract {\n    val memoizedMarker by lazy { marker.lowercase() }\n    override suspend fun pending(): String = marker\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("ParentEntity.java"),
            "package boundary;\npublic class ParentEntity {}\n",
        )
        .unwrap();
        fs::write(
            root.join("ChildEntity.java"),
            "package boundary;\npublic final class ChildEntity {\n    private ParentEntity parent;\n    public ParentEntity getParent() { return parent; }\n    public void setParent(ParentEntity parent) { this.parent = parent; }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("ParentResolver.java"),
            "package boundary;\nimport org.mapstruct.TargetType;\npublic interface ParentResolver {\n    ParentEntity MANAGED = new ParentEntity();\n    default <T extends ParentEntity> T resolve(ObjectId source, @TargetType Class<T> type) { return type.cast(MANAGED); }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("ReferenceMapper.java"),
            "package boundary;\nimport org.mapstruct.Mapper;\nimport org.mapstruct.Mapping;\n@Mapper\npublic interface ReferenceMapper extends ParentResolver {\n    @Mapping(target = \"parent\", source = \"reference\")\n    ChildEntity map(ReferenceOutput source);\n}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("generated-java")).unwrap();
        let runner = r#"package boundary
import java.beans.Introspector
fun main() {
    val descriptor = Introspector.getBeanInfo(ReferenceOutput::class.java).propertyDescriptors
        .first { it.name == "reference" }
    val source = ReferenceOutput("kept")
    val mapper = Class.forName("boundary.ReferenceMapperImpl").getConstructor().newInstance()
    val mapped = mapper.javaClass.getMethod("map", ReferenceOutput::class.java).invoke(mapper, source)
    val parent = mapped.javaClass.getMethod("getParent").invoke(mapped)
    println("${descriptor.readMethod.returnType.name}:${parent === ParentResolver.MANAGED}")
}

"#;
        fs::write(root.join("Runner.kt"), runner).unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.java_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    baseline.classpath.push(mapstruct.clone());
    let processor_path = std::env::join_paths([&mapstruct_processor, &mapstruct]).unwrap();
    baseline.javac_args.extend([
        "-processorpath".into(),
        processor_path,
        "-processor".into(),
        "org.mapstruct.ap.MappingProcessor".into(),
        "-Amapstruct.suppressGeneratorTimestamp=true".into(),
        "-s".into(),
        baseline_root.join("generated-java").into_os_string(),
    ]);
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("baseline intersecting Kotlin interfaces compile");
    let baseline_mapper =
        fs::read_to_string(baseline_root.join("generated-java/boundary/ReferenceMapperImpl.java"))
            .expect("baseline MapStruct implementation is generated");
    assert!(
        baseline_mapper.contains("resolve("),
        "baseline mapping must use the generic resolver:\n{baseline_mapper}"
    );
    assert!(
        !baseline_mapper.contains("ToParentEntity("),
        "baseline must not forge an identifier-to-parent mapping:\n{baseline_mapper}"
    );
    assert_eq!(
        baseline_report.commands.last().unwrap().stdout.trim(),
        "boundary.ReferenceId:true",
        "the baseline must expose and resolve the narrow default getter"
    );

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .arg(mixed_root.join("ReferenceRoot.kt"))
        .arg(mixed_root.join("NarrowReferenceContract.kt"))
        .arg(mixed_root.join("NarrowReferenceProvider.kt"))
        .arg(mixed_root.join("BroadReferenceContract.kt"))
        .arg(mixed_root.join("RetainedReferenceContract.kt"))
        .arg(mixed_root.join("ReferenceOutput.kt"))
        .output()
        .expect("run notlin on the broad and narrow reference interfaces");
    assert!(
        output.status.success(),
        "transpiler failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(mixed_root.join("ReferenceRoot.java").is_file());
    assert!(
        !mixed_root.join("NarrowReferenceContract.java").exists(),
        "the selected covariant default provider should stay Kotlin:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!mixed_root.join("RetainedReferenceContract.java").exists());
    assert!(!mixed_root.join("ReferenceOutput.java").exists());

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "kt")
                && !path.with_extension("java").is_file()
        })
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    mixed.classpath.push(mapstruct.clone());
    let processor_path = std::env::join_paths([&mapstruct_processor, &mapstruct]).unwrap();
    mixed.javac_args.extend([
        "-processorpath".into(),
        processor_path,
        "-processor".into(),
        "org.mapstruct.ap.MappingProcessor".into(),
        "-Amapstruct.suppressGeneratorTimestamp=true".into(),
        "-s".into(),
        mixed_root.join("generated-java").into_os_string(),
    ]);
    let mixed_report =
        validate_jvm_sources(&mixed).expect("migrated broad/narrow reference bridge compiles");
    let mixed_mapper =
        fs::read_to_string(mixed_root.join("generated-java/boundary/ReferenceMapperImpl.java"))
            .expect("migrated MapStruct implementation is generated");
    assert!(
        mixed_mapper.contains("resolve("),
        "migrated mapping must use the generic resolver:\n{mixed_mapper}"
    );
    assert!(
        !mixed_mapper.contains("ToParentEntity("),
        "migrated mapping must not forge an identifier-to-parent mapping:\n{mixed_mapper}"
    );
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "reflection and any available MapStruct processor must select the same most-specific getter"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_multiple_runtime_class_annotations_on_lombok_class() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "multiple runtime class annotations",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::create_dir_all(root.join("jakarta/persistence")).unwrap();
        fs::create_dir_all(root.join("org/hibernate/annotations")).unwrap();
        fs::write(
            root.join("RecordBase.kt"),
            "package boundary\nopen class RecordBase(val id: String = \"\")\ninterface RecordRepository<T>\n",
        )
        .unwrap();
        fs::write(
            root.join("jakarta/persistence/Entity.java"),
            "package jakarta.persistence;\n@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)\n@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)\npublic @interface Entity {}\n",
        )
        .unwrap();
        fs::write(
            root.join("jakarta/persistence/Table.java"),
            "package jakarta.persistence;\n@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)\n@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)\npublic @interface Table { String name(); }\n",
        )
        .unwrap();
        fs::write(
            root.join("org/hibernate/annotations/Immutable.java"),
            "package org.hibernate.annotations;\n@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)\n@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)\npublic @interface Immutable {}\n",
        )
        .unwrap();
        fs::write(
            root.join("org/hibernate/annotations/SQLRestriction.java"),
            "package org.hibernate.annotations;\n@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)\n@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)\npublic @interface SQLRestriction { String value(); }\n",
        )
        .unwrap();
        fs::write(
            root.join("ResourceRecord.kt"),
            r#"package boundary
import jakarta.persistence.*
import org.hibernate.annotations.Immutable
import org.hibernate.annotations.SQLRestriction

@Entity
@Table(name="RESOURCE")
@Immutable
@SQLRestriction("CLASS='RESOURCE_SET'")
class ResourceRecord(
    id: String = "",
    val label: String = "record",
    val enabled: Boolean = true
) : RecordBase(id)

interface ResourceRecordRepository : RecordRepository<ResourceRecord>
"#,
        )
        .unwrap();
        fs::write(
            root.join("AnnotationProbe.java"),
            r#"package boundary;
import java.util.Arrays;
import java.util.Set;
import java.util.stream.Collectors;
public final class AnnotationProbe {
    public static void main(String[] args) {
        Class<?> type = ResourceRecord.class;
        Set<Class<?>> checked = Set.of(jakarta.persistence.Entity.class, jakarta.persistence.Table.class,
                org.hibernate.annotations.Immutable.class, org.hibernate.annotations.SQLRestriction.class);
        String values = Arrays.stream(type.getAnnotations())
                .filter(annotation -> checked.contains(annotation.annotationType()))
                .map(annotation -> annotation.annotationType().getName() + "=" + annotation.toString())
                .sorted().collect(Collectors.joining("|"));
        long count = Arrays.stream(type.getAnnotations())
                .filter(annotation -> checked.contains(annotation.annotationType())).count();
        System.out.println(count + "|" + values);
    }
}
"#,
        )
        .unwrap();
    }

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let annotated_source = fs::read_to_string(mixed_root.join("ResourceRecord.kt")).unwrap();
    let annotated_tree = parser.parse(&annotated_source, None).unwrap();
    fn annotation_chain_depth(node: tree_sitter::Node<'_>) -> usize {
        let mut depth = 0;
        if node.kind() == "annotated_expression" {
            depth = 1;
            for index in 0..node.named_child_count() {
                let child = node.named_child(index).unwrap();
                if child.kind() == "annotated_expression" {
                    depth = depth.max(1 + annotation_chain_depth(child));
                }
            }
        } else {
            for index in 0..node.named_child_count() {
                depth = depth.max(annotation_chain_depth(node.named_child(index).unwrap()));
            }
        }
        depth
    }
    assert!(
        annotation_chain_depth(annotated_tree.root_node()) >= 4,
        "fixture must reproduce four nested top-level annotated_expression wrappers:\n{}",
        annotated_tree.root_node().to_sexp()
    );

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [
        baseline_root.join("RecordBase.kt"),
        baseline_root.join("ResourceRecord.kt"),
    ]
    .into();
    baseline.java_sources = vec![
        baseline_root.join("jakarta/persistence/Entity.java"),
        baseline_root.join("jakarta/persistence/Table.java"),
        baseline_root.join("org/hibernate/annotations/Immutable.java"),
        baseline_root.join("org/hibernate/annotations/SQLRestriction.java"),
        baseline_root.join("AnnotationProbe.java"),
    ];
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.AnnotationProbe".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("baseline annotated Kotlin class compiles");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(mixed_root.join("ResourceRecord.kt"))
        .output()
        .expect("run notlin on the annotated ordinary class");
    assert!(
        output.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        mixed_root.join("ResourceRecord.java").is_file(),
        "the annotated ordinary class should be converted"
    );
    let generated = fs::read_to_string(mixed_root.join("ResourceRecord.java")).unwrap();
    assert!(
        generated.contains("@Data"),
        "the ordinary class should exercise Lombok class emission:\n{generated}"
    );
    for expected in [
        "Entity",
        "Table",
        "RESOURCE",
        "SQLRestriction",
        "CLASS='RESOURCE_SET'",
        "Immutable",
    ] {
        assert!(
            generated.contains(expected),
            "class annotation {expected} must be emitted with its value:\n{generated}"
        );
    }

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = vec![
        mixed_root.join("jakarta/persistence/Entity.java"),
        mixed_root.join("jakarta/persistence/Table.java"),
        mixed_root.join("org/hibernate/annotations/Immutable.java"),
        mixed_root.join("org/hibernate/annotations/SQLRestriction.java"),
        mixed_root.join("AnnotationProbe.java"),
    ];
    for file_name in ["ResourceRecord.java", "ResourceRecordRepository.java"] {
        let path = mixed_root.join(file_name);
        if path.is_file() {
            mixed.java_sources.push(path);
        }
    }
    mixed.classpath.push(PathBuf::from(stdlib));
    enable_lombok_processing(&mut mixed);
    mixed.run_main_class = Some("boundary.AnnotationProbe".into());
    let mixed_report =
        validate_jvm_sources(&mixed).expect("migrated annotated Lombok class compiles and runs");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "all runtime class annotations and annotation values must match"
    );
    assert!(
        mixed_report
            .commands
            .last()
            .unwrap()
            .stdout
            .starts_with("4|"),
        "all four mapped class annotations must be visible at runtime: {}",
        mixed_report.commands.last().unwrap().stdout.trim()
    );
}
