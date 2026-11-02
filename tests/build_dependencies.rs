use clap::Parser;
use notlin::{build_dependencies::plan_jspecify_dependencies, cli::Cli};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "notlin-build-dependencies-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn source(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
const JAVA: &str = "import org.jspecify.annotations.NonNull; public interface Model { @NonNull String getText(); }";

#[test]
fn nearest_gradle_projects_keep_dialects_newlines_and_existing_content() {
    let scratch = Scratch::new();
    let root = scratch.source("build.gradle");
    fs::write(&root, "// root untouched\n").unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", "."]);
    for script in ["library/build.gradle", "application/build.gradle.kts"] {
        let build = scratch.source(script);
        let before = "// existing settings\r\n";
        fs::write(&build, before).unwrap();
        let source = build.parent().unwrap().join("src/main/java/Model.java");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        let plan =
            plan_jspecify_dependencies(&cli, [(source.as_path(), JAVA)], &scratch.0).unwrap();
        assert_eq!(
            fs::read_to_string(&build).unwrap(),
            before,
            "planning must not write"
        );
        plan.apply().unwrap();
        let after = fs::read_to_string(&build).unwrap();
        assert!(after.starts_with(before), "{after}");
        assert!(!after.replace("\r\n", "").contains('\n'), "{after}");
        assert!(
            after.contains(if script.ends_with("kts") {
                "val notlinJspecifyConfiguration"
            } else {
                "def notlinJspecifyConfiguration"
            }),
            "{after}"
        );
        assert!(
            after.contains("configurations.findByName(\"api\")"),
            "{after}"
        );
        let repeat =
            plan_jspecify_dependencies(&cli, [(source.as_path(), JAVA)], &scratch.0).unwrap();
        repeat.apply().unwrap();
        assert_eq!(
            fs::read_to_string(&build).unwrap(),
            after,
            "repeated runs must be idempotent"
        );
    }
    assert_eq!(fs::read_to_string(root).unwrap(), "// root untouched\n");
}

#[test]
fn literals_output_directories_and_ordinary_transpilation_do_not_edit_builds() {
    let scratch = Scratch::new();
    let build = scratch.source("build.gradle");
    fs::write(&build, "// untouched\n").unwrap();
    let source = scratch.source("Model.java");
    for (args, java) in [
        (vec!["notlin", "Model.kt"], JAVA),
        (
            vec!["notlin", "--in-place", "--out-dir", "elsewhere", "."],
            JAVA,
        ),
        (
            vec!["notlin", "--in-place", "."],
            "public class Model { String label = \"org.jspecify.annotations.NonNull\"; }",
        ),
    ] {
        let cli = Cli::parse_from(args);
        plan_jspecify_dependencies(&cli, [(source.as_path(), java)], &scratch.0)
            .unwrap()
            .apply()
            .unwrap();
        assert_eq!(fs::read_to_string(&build).unwrap(), "// untouched\n");
    }
}

#[test]
fn changed_build_snapshots_are_rejected_before_any_metadata_write() {
    let scratch = Scratch::new();
    let cli = Cli::parse_from(["notlin", "--in-place", "."]);
    let first = scratch.source("a/build.gradle");
    let second = scratch.source("b/build.gradle");
    for path in [&first, &second] {
        fs::write(path, "// original\n").unwrap();
    }
    let paths = [
        scratch.source("a/Model.java"),
        scratch.source("b/Model.java"),
    ];
    let plan =
        plan_jspecify_dependencies(&cli, paths.iter().map(|p| (p.as_path(), JAVA)), &scratch.0)
            .unwrap();
    fs::write(second, "// user edit\n").unwrap();
    assert!(
        plan.apply()
            .unwrap_err()
            .contains("changed after dependency planning")
    );
    assert_eq!(fs::read_to_string(first).unwrap(), "// original\n");
}

#[test]
fn existing_java_and_qualified_annotations_can_repair_missing_dependencies() {
    let scratch = Scratch::new();
    let build = scratch.source("build.gradle.kts");
    fs::write(&build, "").unwrap();
    let existing = scratch.source("src/main/java/Existing.java");
    let cli = Cli::parse_from(["notlin", "--in-place", "."]);
    let java =
        "public interface Existing {java.util.@org.jspecify.annotations.NonNull UUID getId();}";
    plan_jspecify_dependencies(&cli, [(existing.as_path(), java)], &scratch.0)
        .unwrap()
        .apply()
        .unwrap();
    assert!(
        fs::read_to_string(build)
            .unwrap()
            .contains("org.jspecify:jspecify:1.0.0")
    );
}

#[test]
fn dependency_names_follow_import_syntax_instead_of_formatting() {
    let scratch = Scratch::new();
    let build = scratch.source("build.gradle");
    fs::write(&build, "").unwrap();
    let existing = scratch.source("Existing.java");
    let cli = Cli::parse_from(["notlin", "--in-place", "."]);
    let java = "import\n org /* annotation library */ . jspecify.annotations.NonNull; public interface Existing { @NonNull String getText(); }";
    plan_jspecify_dependencies(&cli, [(existing.as_path(), java)], &scratch.0)
        .unwrap()
        .apply()
        .unwrap();
    assert!(
        fs::read_to_string(build)
            .unwrap()
            .contains("org.jspecify:jspecify:1.0.0")
    );
}

#[test]
fn relative_single_file_migrations_resolve_the_current_project_directory() {
    for input in ["Model.kt", "./Model.kt"] {
        let scratch = Scratch::new();
        fs::write(scratch.source("build.gradle"), "plugins { id 'java' }\n").unwrap();
        fs::write(
            scratch.source("Model.kt"),
            "interface Example {fun text():String?}\n",
        )
        .unwrap();
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
            .current_dir(&scratch.0)
            .args(["--in-place", "--jvm-facts", "off", input])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{input}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!scratch.0.join("Model.kt").exists());
        assert!(scratch.0.join("Example.java").exists());
        assert!(
            fs::read_to_string(scratch.0.join("build.gradle"))
                .unwrap()
                .contains("org.jspecify:jspecify:1.0.0")
        );
    }
}

#[test]
#[ignore = "requires NOTLIN_GRADLE and JDK, with JSpecify cached for offline Gradle"]
fn real_gradle_builds_both_dialects_and_exports_annotations_from_libraries() {
    use std::process::Command;
    const MODEL: &str = "package sample\ninterface Base {fun text():String?}\ninterface Child:Base {override fun text():String=\"value\"}\n";
    const OBSERVE: &str = r#"
tasks.register("annotationDependency") {
    doLast {
        println("NOTLIN-RUNTIME: " + configurations.getByName("runtimeClasspath").files.any { it.name.startsWith("jspecify-") })
        println("NOTLIN-DIRECT: " + configurations.getByName("implementation").dependencies.any { it.group == "org.jspecify" && it.name == "jspecify" })
    }
}
"#;
    for (kotlin, library, both, aggregate) in [
        (false, false, false, false),
        (true, false, false, false),
        (false, true, false, false),
        (true, true, false, false),
        (false, false, true, false),
        (false, true, false, true),
        (true, true, false, true),
    ] {
        let scratch = Scratch::new();
        let extension = if kotlin { "gradle.kts" } else { "gradle" };
        let plugin = if library { "java-library" } else { "java" };
        let settings = if library {
            "rootProject.name = \"annotation-test\"\ninclude(\"library\", \"consumer\")\n"
        } else {
            "rootProject.name = \"annotation-test\"\n"
        };
        fs::write(scratch.source(&format!("settings.{extension}")), settings).unwrap();
        let repositories = "repositories { mavenCentral() }\n";
        let (model, build) = if aggregate {
            let build = scratch.source(&format!("build.{extension}"));
            fs::write(&build, format!("subprojects {{ pluginManager.apply(\"java\"); {repositories} }}\nproject(\":library\") {{ pluginManager.apply(\"java-library\") }}\nproject(\":consumer\") {{ dependencies {{ add(\"implementation\", project(\":library\")) }}\n{OBSERVE} }}\n")).unwrap();
            (scratch.source("library/src/main/java/Model.kt"), build)
        } else if library {
            fs::write(
                scratch.source(&format!("build.{extension}")),
                format!("allprojects {{ {repositories} }}\n"),
            )
            .unwrap();
            let library_build = scratch.source(&format!("library/build.{extension}"));
            fs::write(&library_build, format!("plugins {{ id(\"{plugin}\") }}\n")).unwrap();
            let consumer = scratch.source(&format!("consumer/build.{extension}"));
            fs::write(consumer,format!("plugins {{ id(\"java\") }}\ndependencies {{ add(\"implementation\", project(\":library\")) }}\n{OBSERVE}")).unwrap();
            (
                scratch.source("library/src/main/java/Model.kt"),
                library_build,
            )
        } else {
            let build = scratch.source(&format!("build.{extension}"));
            let original = format!("plugins {{ id(\"{plugin}\") }}\n{repositories}{OBSERVE}");
            fs::write(&build, &original).unwrap();
            if both {
                fs::write(scratch.source("build.gradle.kts"), &original).unwrap();
            }
            (scratch.source("src/main/java/Model.kt"), build)
        };
        fs::write(&model, MODEL).unwrap();
        let output = Command::new(
            std::env::var_os("NOTLIN_TEST_BINARY")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into()),
        )
        .current_dir(&scratch.0)
        .args(["--in-place", "--jvm-facts", "off", "."])
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !model.exists(),
            "the original interface definitions should translate"
        );
        assert!(
            fs::read_to_string(&build)
                .unwrap()
                .contains("org.jspecify:jspecify:1.0.0")
        );
        if library {
            // Add this caller after migration. It must obtain the annotation
            // through the library's public API, without a consumer dependency.
            fs::write(scratch.source("consumer/src/main/java/sample/Consumer.java"),
                "package sample; public class Consumer { public @org.jspecify.annotations.NonNull String read(){return new Child(){}.text();} }\n").unwrap();
        }
        let task = if library {
            ":consumer:annotationDependency"
        } else {
            "annotationDependency"
        };
        let output = Command::new(std::env::var_os("NOTLIN_GRADLE").expect("NOTLIN_GRADLE"))
            .current_dir(&scratch.0)
            .args([
                "--offline",
                "--no-daemon",
                "--no-configuration-cache",
                "--console=plain",
                "build",
                task,
            ])
            .output()
            .unwrap();
        let log = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "Kotlin DSL {kotlin}, library {library}, both {both}, aggregate {aggregate}: {log}"
        );
        assert!(log.contains("NOTLIN-RUNTIME: true"), "{log}");
        assert!(
            log.contains(if library && !aggregate {
                "NOTLIN-DIRECT: false"
            } else {
                "NOTLIN-DIRECT: true"
            }),
            "{log}"
        );
    }
}
