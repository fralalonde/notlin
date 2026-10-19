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
            "notlin-parallel-builtin-getters-jvm-{}-{id}",
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

const SOURCES: [(&str, &str); 6] = [
    (
        "KindApi.kt",
        r#"package boundary
interface KindApi {
    suspend fun retainedHook()
}
"#,
    ),
    (
        "Kind.kt",
        r#"package boundary
enum class Kind : KindApi {
    FIRST,
    SECOND;

    override suspend fun retainedHook() {}
}
"#,
    ),
    (
        "IdView.kt",
        r#"package boundary
interface IdView {
    val id: String
    val count: Int
    val enabled: Boolean
}
"#,
    ),
    (
        "EntityView.kt",
        r#"package boundary
interface EntityView {
    val id: String
    val count: Int
    val enabled: Boolean
    val type: KindApi
}
"#,
    ),
    (
        "EntityModel.kt",
        r#"package boundary
data class EntityModel(override val id: String, override val count: Int, override val enabled: Boolean) : IdView, EntityView {
    override val type: Kind
        get() = Kind.SECOND

    suspend fun retainedHook() {}
    fun update(): EntityModel = this
}
"#,
    ),
    (
        "Runner.kt",
        r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val model = EntityModel("shared", 7, true).update()
    val idView: IdView = model
    val entityView: EntityView = model
    println("${idView.id}|${entityView.id}|${idView.count}|${entityView.count}|${idView.enabled}|${entityView.enabled}|${(entityView.type as Kind).name}|${model.type.name}")
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

fn assert_translated_shape(root: &Path) {
    assert!(root.join("IdView.java").is_file());
    assert!(root.join("EntityView.java").is_file());
    assert!(root.join("EntityModel.kt").is_file());
    assert!(!root.join("EntityModel.java").exists());

    let id_view = fs::read_to_string(root.join("IdView.java")).unwrap();
    let entity_view = fs::read_to_string(root.join("EntityView.java")).unwrap();
    assert_eq!(id_view.matches("getId()").count(), 1, "{id_view}");
    assert_eq!(entity_view.matches("getId()").count(), 1, "{entity_view}");

    let model = fs::read_to_string(root.join("EntityModel.kt")).unwrap();
    assert!(model.contains("@JvmField val id: String"), "{model}");
    assert!(model.contains("@JvmField val count: Int"), "{model}");
    assert!(model.contains("@JvmField val enabled: Boolean"), "{model}");
    assert_eq!(
        model.matches("override fun getEnabled()").count(),
        1,
        "{model}"
    );
    assert!(
        model.contains("fun update(): EntityModel = this"),
        "{model}"
    );
    assert_eq!(model.matches("override fun getId()").count(), 1, "{model}");
    assert_eq!(
        model.matches("override fun getCount()").count(),
        1,
        "{model}"
    );
    assert!(!model.contains("val type: Kind"), "{model}");
    assert!(
        model.contains("override fun getType(): Kind = Kind.SECOND"),
        "{model}"
    );
    assert_eq!(
        model.matches("override fun getType()").count(),
        1,
        "{model}"
    );
}

#[test]
fn cli_migrates_parallel_builtin_getters_without_duplicate_id_declarations() {
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
fn real_toolchain_preserves_parallel_builtin_getter_dispatch() {
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
    .expect("original Kotlin parallel property contracts compile and run");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(expected, "shared|shared|7|7|true|true|SECOND|SECOND");

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
        .expect("mixed Java/Kotlin parallel property contracts compile and run");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
