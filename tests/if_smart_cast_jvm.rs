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
            "notlin-if-smart-cast-jvm-{}-{id}",
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

const SOURCES: [(&str, &str); 4] = [
    (
        "View.kt",
        r#"package boundary
interface View {
    val id: String

    val key: String
        get() = if (this is Detailed) this.detail else id

    val inverse: String
        get() = if (this !is Detailed) id else this.detail

    val nested: String
        get() = if (this is Detailed) (if (id == "d") detail else id) else id

    fun localKey(): String {
        val current: View = this
        return if (current is Detailed) current.detail else current.id
    }

    fun unrelated(current: View, fallback: String): String =
        if (current is Detailed) fallback else current.id

    fun afterCheck(): String {
        if (this is Detailed) return this.detail
        return this.id
    }
}
"#,
    ),
    (
        "Detailed.kt",
        r#"package boundary
class Detailed(override val id: String, val detail: String) : View
"#,
    ),
    (
        "Plain.kt",
        r#"package boundary
class Plain(override val id: String) : View
"#,
    ),
    (
        "Runner.kt",
        r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val detailed: View = Detailed("d", "detail")
    val plain: View = Plain("p")
    println(listOf(
        detailed.key,
        detailed.inverse,
        detailed.nested,
        detailed.localKey(),
        detailed.unrelated(detailed, "outside"),
        detailed.afterCheck(),
        plain.key,
        plain.inverse,
        plain.nested,
        plain.localKey(),
        plain.afterCheck()
    ).joinToString("|"))
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

fn assert_translated_shape(root: &Path) {
    assert!(
        root.join("View.java").is_file(),
        "{}",
        fs::read_to_string(root.join("View.kt")).unwrap_or_default()
    );
    assert!(root.join("Detailed.java").is_file());
    assert!(root.join("Plain.java").is_file());
    assert!(root.join("Runner.kt").is_file());

    let generated = fs::read_to_string(root.join("View.java")).unwrap();
    let compact_body = |signature: &str| {
        let signature_start = generated
            .find(signature)
            .unwrap_or_else(|| panic!("missing method {signature}:\n{generated}"));
        let body_start = signature_start
            + generated[signature_start..]
                .find('{')
                .unwrap_or_else(|| panic!("missing body for {signature}:\n{generated}"));
        let mut depth = 0usize;
        let mut body_end = None;
        for (offset, character) in generated[body_start..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        body_end = Some(body_start + offset + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &generated[body_start..body_end.expect("balanced Java method body")];
        body.split_whitespace().collect::<String>()
    };

    let key = compact_body("getKey()");
    assert!(
        key.contains("thisinstanceofDetailed") && key.contains("((Detailed)this).getDetail()"),
        "the checked branch must cast this before reading detail:\n{generated}"
    );
    assert!(
        !key.contains("((Detailed)this).getId()"),
        "the unrelated key branch must not be cast:\n{generated}"
    );

    let inverse = compact_body("getInverse()");
    assert!(
        inverse.contains("((Detailed)this).getDetail()"),
        "the negated check must narrow its else branch:\n{generated}"
    );
    assert!(
        !inverse.contains("((Detailed)this).getId()"),
        "the unrelated inverse branch must not be cast:\n{generated}"
    );

    let nested = compact_body("getNested()");
    assert!(
        nested.contains("thisinstanceofDetailed")
            && nested.contains("((Detailed)this).getDetail()"),
        "nested conditions must preserve the outer narrowing:\n{generated}"
    );

    let local_key = compact_body("localKey()");
    assert!(
        local_key.contains("((Detailed)current).getDetail()"),
        "a checked local identifier must be narrowed:\n{generated}"
    );
    assert!(
        !local_key.contains("((Detailed)current).getId()"),
        "the local identifier's unrelated branch must not inherit narrowing:\n{generated}"
    );

    let unrelated = compact_body("unrelated(");
    assert!(
        unrelated.contains("fallback") && !unrelated.contains("((Detailed)fallback)"),
        "the unrelated branch identifier must not receive a cast:\n{generated}"
    );

    let after_check = compact_body("afterCheck()");
    assert!(
        after_check.contains("returnthis.getId();")
            && !after_check.contains("((Detailed)this).getId()"),
        "the check's narrowing must not escape into the following return:\n{generated}"
    );
}

#[test]
fn cli_emits_if_smart_casts_for_checked_this_and_local_subjects() {
    let scratch = Scratch::new();
    let root = scratch.0.join("sources");
    populate(&root);

    let translated = translate(&root, false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert_translated_shape(&root);
}

#[test]
fn distinct_nested_smart_casts_retain_without_intersection_proof() {
    use clap::Parser;

    let source = r#"interface Left { val left: String }
interface Right
fun read(value: Any): String =
    if (value is Left) (if (value is Right) value.left else "") else ""
fun readSame(value: Any): String =
    if (value is Left) (if (value is Left) value.left else "") else ""
"#;
    let cli = notlin::cli::Cli::parse_from(["notlin", "Intersection.kt"]);
    let (files, errors, _, coverage) =
        notlin::transpiler::transpile(source, Path::new("Intersection.kt"), &cli);
    assert_eq!(errors, 0);
    assert!(coverage.untranslated.iter().any(|name| name == "read"));
    assert!(!coverage.untranslated.iter().any(|name| name == "readSame"));
    let java = files
        .into_iter()
        .map(|(_, source)| source)
        .collect::<String>();
    assert!(!java.contains("String read("), "{java}");
    assert!(java.contains("String readSame("), "{java}");
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_if_smart_cast_behavior() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);

    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        false,
    ))
    .expect("original Kotlin if smart casts compile and run");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "detail|detail|detail|detail|outside|detail|p|p|p|p|p"
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
        assert_translated_shape(&migrated);

        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Java/Kotlin if smart casts compile and run");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
