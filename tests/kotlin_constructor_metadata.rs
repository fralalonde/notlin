use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider};
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
            "notlin-kotlin-constructor-metadata-{}-{id}",
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
            "SettingsFamily.kt",
            r#"package metadata
interface SettingsFamily {
    companion object {
        suspend fun retainedHook() {}
    }
}
"#,
        ),
        (
            "Options.kt",
            r#"package metadata
data class Options @JvmOverloads constructor(
    val label: String = "default"
) : SettingsFamily
"#,
        ),
        (
            "ReflectiveBuilder.kt",
            r#"package metadata
import kotlin.reflect.full.primaryConstructor

class ReflectiveBuilder<T : Any>(private val type: Class<T>) {
    fun build(values: Map<String, Any?>): T {
        val constructor = type.kotlin.primaryConstructor!!
        return constructor.callBy(
            constructor.parameters
                .filter { it.name != null && values.containsKey(it.name) }
                .associateWith { values[it.name!!] }
        )
    }

    companion object {
        fun <U : SettingsFamily> create(type: Class<U>): ReflectiveBuilder<U> =
            ReflectiveBuilder(type)
    }
}
"#,
        ),
        (
            "Unrelated.kt",
            "package metadata\ndata class Unrelated @JvmOverloads constructor(val value: String = \"other\")\n",
        ),
        (
            "Runner.kt",
            r#"package metadata
fun main() {
    suspend fun retainedHarness() {}
    val options = ReflectiveBuilder.create(Options::class.java).build(mapOf("label" to "supplied"))
    println(options.label)
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

fn provider(root: &Path) -> SyntaxSemanticProvider {
    SyntaxSemanticProvider::new(
        fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
            .map(|path| {
                let source = fs::read_to_string(&path).unwrap();
                let mut parser = tree_sitter::Parser::new();
                parser
                    .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
                    .unwrap();
                assert!(
                    !parser.parse(&source, None).unwrap().root_node().has_error(),
                    "{}",
                    path.display()
                );
                (path, source)
            }),
    )
}

fn needs_metadata(provider: &SyntaxSemanticProvider, package: &str, name: &str) -> bool {
    let symbol = provider
        .symbols_named(name)
        .into_iter()
        .find(|symbol| symbol.id.package == package && symbol.id.kind == "class")
        .expect("indexed fixture class");
    provider
        .kotlin_constructor_metadata_dependency(&symbol.id)
        .is_some()
}

#[test]
fn metadata_reader_can_use_an_explicit_this_property_declared_in_the_body() {
    let scratch = Scratch::new();
    write_fixture(&scratch.0);
    let file = scratch.0.join("ReflectiveBuilder.kt");
    let source = fs::read_to_string(&file)
        .unwrap()
        .replace(
            "(private val type: Class<T>) {",
            "(sourceType: Class<T>) {\n    private val type: Class<T> = sourceType",
        )
        .replace(
            "type.kotlin.primaryConstructor",
            "this.type.kotlin.primaryConstructor",
        );
    fs::write(file, source).unwrap();
    assert!(needs_metadata(&provider(&scratch.0), "metadata", "Options"));
}

#[test]
fn comments_and_strings_do_not_establish_constructor_reflection() {
    let scratch = Scratch::new();
    write_fixture(&scratch.0);
    fs::write(scratch.0.join("ReflectiveBuilder.kt"), r#"package metadata
class ReflectiveBuilder<T : Any>(private val type: Class<T>) {
    // type.kotlin.primaryConstructor; constructor.callBy(values)
    fun description(): String = "type.kotlin.constructors and constructor.callBy"
    companion object {
        fun <U : SettingsFamily> create(type: Class<U>): ReflectiveBuilder<U> = ReflectiveBuilder(type)
    }
}
"#).unwrap();
    assert!(!needs_metadata(
        &provider(&scratch.0),
        "metadata",
        "Options"
    ));
}

#[test]
fn a_factory_returning_another_package_builder_does_not_capture_the_family() {
    let scratch = Scratch::new();
    write_fixture(&scratch.0);
    let file = scratch.0.join("ReflectiveBuilder.kt");
    let source = fs::read_to_string(&file).unwrap().replace(
        "): ReflectiveBuilder<U> =\n            ReflectiveBuilder(type)",
        "): other.ReflectiveBuilder<U> =\n            other.ReflectiveBuilder(type)",
    );
    assert!(source.contains("): other.ReflectiveBuilder<U>"));
    fs::write(file, source).unwrap();
    fs::write(
        scratch.0.join("Other.kt"),
        "package other\nclass ReflectiveBuilder<T>(val type: Class<T>)\n",
    )
    .unwrap();
    assert!(!needs_metadata(
        &provider(&scratch.0),
        "metadata",
        "Options"
    ));
}

#[test]
fn a_nested_metadata_dependent_class_retains_its_whole_owner() {
    let scratch = Scratch::new();
    write_fixture(&scratch.0);
    fs::write(
        scratch.0.join("Wrapper.kt"),
        r#"package metadata
class Wrapper {
    data class Nested @JvmOverloads constructor(val label: String = "nested") : SettingsFamily
}
"#,
    )
    .unwrap();
    assert!(needs_metadata(&provider(&scratch.0), "metadata", "Nested"));
    translate(&scratch.0);
    assert!(scratch.0.join("Wrapper.kt").is_file());
    assert!(!scratch.0.join("Wrapper.java").exists());
}

#[test]
fn constructor_metadata_loss_is_explicitly_selectable_as_an_approximation() {
    let scratch = Scratch::new();
    write_fixture(&scratch.0);
    let provider = provider(&scratch.0);
    let file = scratch.0.join("Options.kt");
    let source = fs::read_to_string(&file).unwrap();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(&source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(&source, tree.root_node(), &file);
    plan.plan_constructor_metadata_losses(&file, &provider, true);
    assert!(
        plan.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "A001"
                && diagnostic.message.contains("constructor metadata"))
    );
    assert!(
        plan.declarations
            .iter()
            .all(|declaration| declaration.retention_reasons.is_empty())
    );
}

#[test]
fn reflective_constructor_consumer_retains_metadata_owner_only() {
    let scratch = Scratch::new();
    let root = scratch.0.join("sources");
    write_fixture(&root);
    translate(&root);

    assert!(
        root.join("Options.kt").is_file(),
        "Options must retain Kotlin metadata"
    );
    assert!(
        !root.join("Options.java").exists(),
        "the generic reflective builder needs Kotlin constructor metadata"
    );
    assert!(
        root.join("Unrelated.java").is_file(),
        "unrelated plain classes should still translate"
    );
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, NOTLIN_KOTLIN_STDLIB, and NOTLIN_KOTLIN_REFLECT"]
fn reflective_builder_preserves_supplied_constructor_value_at_runtime() {
    use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};

    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let reflect =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_REFLECT").expect("NOTLIN_KOTLIN_REFLECT"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    write_fixture(&original);
    write_fixture(&migrated);

    let compile = |root: &Path, output: &Path| {
        let mut config = JvmValidationConfig::new(&kotlinc, &javac, &java, output);
        config.kotlin_sources = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        config.classpath.extend([stdlib.clone(), reflect.clone()]);
        config.run_main_class = Some("metadata.RunnerKt".into());
        validate_jvm_sources(&config)
    };

    let before = compile(&original, &scratch.0.join("original classes"))
        .expect("original reflective fixture compiles");
    let before_output = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(before_output, "supplied");

    translate(&migrated);
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    for entry in fs::read_dir(&migrated).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(path),
            Some("java") => mixed.java_sources.push(path),
            _ => {}
        }
    }
    mixed.classpath.extend([stdlib, reflect]);
    mixed.run_main_class = Some("metadata.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("mixed reflective fixture compiles");
    assert_eq!(
        after.commands.last().unwrap().stdout.trim(),
        before_output,
        "the generic reflective factory must preserve the supplied constructor value"
    );
}
