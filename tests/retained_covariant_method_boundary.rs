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
            "notlin-retained-covariant-method-boundary-{}-{id}",
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

fn write_fixture(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in [
        (
            "Root.kt",
            r#"package boundary
interface Identifier
interface NarrowId : Identifier {
    fun label(): String
}
data class AssetTag(private val value: String) : NarrowId {
    override fun label(): String = value
}
interface Root {
    val assetType: Identifier
    suspend fun keepRoot()
}
"#,
        ),
        (
            "Info.kt",
            "package boundary\ninterface Info {\n    val assetType: NarrowId\n}\n",
        ),
        (
            "Entity.kt",
            r#"package boundary
open class EntityBase(open val assetType: NarrowId) {
    suspend fun keepBase() {}
}
class Entity : EntityBase(AssetTag("entity")), Root {
    override suspend fun keepRoot() {}
}
"#,
        ),
        (
            "Model.kt",
            "package boundary\nclass Model(override val assetType: NarrowId) : Root, Info {\n    override suspend fun keepRoot() {}\n}\n",
        ),
        (
            "Runner.kt",
            r#"package boundary
fun main() {
    suspend fun keepRunner() {}
    println("${Entity().assetType.label()}|${Model(AssetTag("model")).assetType.label()}")
}
"#,
        ),
    ] {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        assert!(
            !parser.parse(source, None).unwrap().root_node().has_error(),
            "invalid fixture {name}: {source}"
        );
        fs::write(root.join(name), source).unwrap();
    }
}

fn translate(root: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(root)
        .args(["--annotations", "none", "--in-place"])
        .arg(root)
        .output()
        .expect("run notlin");
    assert!(
        output.status.success(),
        "notlin failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn parallel_property_interface_with_retained_descendant_stays_kotlin() {
    let scratch = Scratch::new();
    let root = scratch.0.join("sources");
    write_fixture(&root);
    translate(&root);

    assert!(root.join("Info.kt").is_file(), "Info must remain Kotlin");
    assert!(
        !root.join("Info.java").exists(),
        "translating Info would turn its property into a Java getter that the retained Model property cannot implement"
    );
    assert!(root.join("Root.kt").is_file(), "Root must remain Kotlin");
    assert!(
        root.join("Entity.kt").is_file(),
        "Entity must remain Kotlin"
    );
    assert!(root.join("Model.kt").is_file(), "Model must remain Kotlin");
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB"]
fn parallel_property_retention_preserves_runtime_behavior() {
    use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};

    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let root = scratch.0.join("sources");
    write_fixture(&root);

    let compile = |suffix: &str| {
        let mut config = JvmValidationConfig::new(
            &kotlinc,
            &javac,
            &java,
            scratch.0.join(format!("classes-{suffix}")),
        );
        for entry in fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path();
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("kt") => config.kotlin_sources.push(path),
                Some("java") => config.java_sources.push(path),
                _ => {}
            }
        }
        config.classpath.push(stdlib.clone());
        config.run_main_class = Some("boundary.RunnerKt".into());
        validate_jvm_sources(&config)
    };

    let before = compile("before").expect("original Kotlin fixture compiles");
    let before_output = before.commands.last().unwrap().stdout.trim().to_string();
    translate(&root);
    let after = compile("after").expect("translated fixture compiles");
    let after_output = after.commands.last().unwrap().stdout.trim();
    assert_eq!(after_output, before_output);
}
