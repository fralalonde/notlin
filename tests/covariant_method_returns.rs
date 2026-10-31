//! Compare covariant method dispatch against independently compiled Kotlin,
//! including retained Kotlin ancestors and callers across Java intermediates.
use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SOURCES: &[(&str, &str)] = &[
    (
        "Types.kt",
        r#"package sample
interface Wide
enum class Narrow : Wide { VALUE }
"#,
    ),
    (
        "Roots.kt",
        r#"package sample
interface Root {
    fun result(): Wide
    fun find(index: Int): Wide
    fun find(name: String): String
    companion object {
        suspend fun keepRoot() {}
    }
}
"#,
    ),
    (
        "GenericRoot.kt",
        r#"package contracts
interface GenericRoot<T> {
    fun item(): T
    companion object {
        suspend fun keepRoot() {}
    }
}
"#,
    ),
    (
        "Providers.kt",
        r#"package sample
open class Provider : JavaMiddle {
    override fun result(): Narrow {
        Trace.mark()
        return Narrow.VALUE
    }
    override fun find(index: Int): Narrow = Narrow.VALUE
    override fun find(name: String): String = name
}
enum class EnumProvider : JavaMiddle {
    ONLY;
    override fun result(): Narrow = Narrow.VALUE
    override fun find(index: Int): Narrow = Narrow.VALUE
    override fun find(name: String): String = name
}
interface DefaultProvider : JavaMiddle {
    override fun result(): Narrow = Narrow.VALUE
    override fun find(index: Int): Narrow = Narrow.VALUE
    override fun find(name: String): String = name
}
class UsesDefault : DefaultProvider
class GenericProvider : JavaGenericMiddle {
    override fun item(): Narrow = Narrow.VALUE
}
"#,
    ),
    (
        "Retained.kt",
        r#"package sample
class Kept : DefaultProvider {
    suspend fun keepChild() {}
}
"#,
    ),
    (
        "JavaMiddle.java",
        r#"package sample;
public interface JavaMiddle extends Root {
    @Override Wide result();
}
"#,
    ),
    (
        "JavaGenericMiddle.java",
        r#"package sample;
public interface JavaGenericMiddle extends contracts.GenericRoot<Wide> {}
"#,
    ),
    (
        "Trace.java",
        r#"package sample;
public final class Trace {
    private static int calls;
    public static void mark() { calls++; }
    public static int calls() { return calls; }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    suspend fun keepRunner() {}
    val direct = Provider()
    val child = Kept()
    val roots: List<Root> = listOf(direct, child, EnumProvider.ONLY, UsesDefault())
    for (root in roots) {
        println("${root.result() === Narrow.VALUE}|${root.find(3) === Narrow.VALUE}|${root.find("text")}")
    }
    val generic: contracts.GenericRoot<Wide> = GenericProvider()
    println(generic.item() === Narrow.VALUE)
    println(direct.result() === Narrow.VALUE)
    println(Trace.calls())
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

fn config(root: &Path, output: PathBuf, lombok: bool) -> JvmValidationConfig {
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
    config.classpath.push(
        std::env::var_os("NOTLIN_KOTLIN_STDLIB")
            .expect("NOTLIN_KOTLIN_STDLIB")
            .into(),
    );
    if lombok {
        config.classpath.push(
            std::env::var_os("NOTLIN_LOMBOK")
                .expect("NOTLIN_LOMBOK")
                .into(),
        );
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
        let file = entry.unwrap().path();
        match file.extension().and_then(|ext| ext.to_str()) {
            Some("kt") => config.kotlin_sources.push(file),
            Some("java") => config.java_sources.push(file),
            _ => {}
        }
    }
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

fn translate(root: &Path, lombok: bool) {
    let binary = std::env::var_os("NOTLIN_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
    let mut command = Command::new(binary);
    command
        .current_dir(root)
        .args(["--in-place", "--annotations", "none"]);
    if lombok {
        command.arg("--lombok");
    }
    let output = command.arg(".").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in [
        "Provider",
        "EnumProvider",
        "DefaultProvider",
        "GenericProvider",
        "UsesDefault",
    ] {
        assert!(
            root.join(format!("{name}.java")).exists(),
            "{name} must translate: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(root.join("Providers.kt")).unwrap_or_default()
        );
    }
    for name in ["Roots", "GenericRoot", "Retained", "Runner"] {
        assert!(
            root.join(format!("{name}.kt")).exists(),
            "{name} must exercise the retained Kotlin boundary"
        );
    }
}

fn scratch() -> Scratch {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Scratch(std::env::temp_dir().join(format!(
        "notlin-covariant-methods-{}-{nonce}",
        std::process::id()
    )))
}

#[test]
fn source_reference_returns_translate_beside_retained_kotlin_method_contracts() {
    let scratch = scratch();
    populate(&scratch.0);
    translate(&scratch.0, false);
}

#[test]
fn primitive_and_value_class_returns_do_not_enter_reference_covariance() {
    for (types, inherited, implementation, expression) in [
        ("", "Any", "Int", "7"),
        (
            "interface Wide\n@JvmInline value class Narrow(val value: Int) : Wide\n",
            "Wide",
            "Narrow",
            "Narrow(7)",
        ),
    ] {
        let scratch = scratch();
        fs::create_dir_all(&scratch.0).unwrap();
        fs::write(scratch.0.join("Root.kt"), format!(
            "package sample\n{types}interface Root {{\n fun result(): {inherited}\n companion object {{\n  suspend fun keepRoot() {{}}\n }}\n}}\n"
        )).unwrap();
        fs::write(
            scratch.0.join("JavaMiddle.java"),
            "package sample;\npublic interface JavaMiddle extends Root {}\n",
        )
        .unwrap();
        fs::write(scratch.0.join("Provider.kt"), format!(
            "package sample\nclass Provider : JavaMiddle {{\n override fun result(): {implementation} = {expression}\n}}\n"
        )).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
            .current_dir(&scratch.0)
            .args(["--in-place", "--annotations", "none", "."])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!scratch.0.join("Provider.java").exists());
        let retained = fs::read_to_string(scratch.0.join("Provider.kt")).unwrap();
        assert!(retained.contains("NOTLIN NC68A"), "{retained}");
    }
}

#[test]
fn method_type_parameter_does_not_bind_to_a_same_named_source_enum() {
    let scratch = scratch();
    fs::create_dir_all(&scratch.0).unwrap();
    fs::write(scratch.0.join("Root.kt"), "package sample\ninterface Wide\nenum class Narrow : Wide { VALUE }\ninterface Root {\n fun <Narrow : Wide> result(value: Narrow): Wide\n companion object {\n  suspend fun keepRoot() {}\n }\n}\n").unwrap();
    fs::write(
        scratch.0.join("JavaMiddle.java"),
        "package sample;\npublic interface JavaMiddle extends Root {}\n",
    )
    .unwrap();
    fs::write(scratch.0.join("Provider.kt"), "package sample\nclass Provider : JavaMiddle {\n override fun <Narrow : Wide> result(value: Narrow): Narrow = value\n}\n").unwrap();
    let index = notlin::workspace::SourceIndex::discover(&scratch.0).unwrap();
    let provider = index
        .declarations()
        .find(|declaration| declaration.name == "Provider")
        .unwrap();
    assert_eq!(provider.members[0].type_parameters, ["Narrow"]);
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&scratch.0)
        .args(["--in-place", "--annotations", "none", "."])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!scratch.0.join("Provider.java").exists());
    let retained = fs::read_to_string(scratch.0.join("Provider.kt")).unwrap();
    assert!(retained.contains("NOTLIN NC68A"), "{retained}");
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin plugin"]
fn retained_kotlin_callers_dispatch_through_covariant_jvm_bridges() {
    let scratch = scratch();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        false,
    ))
    .expect("original Kotlin fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "true|true|text\ntrue|true|text\ntrue|true|text\ntrue|true|text\ntrue\ntrue\n2"
    );
    for lombok in [false, true] {
        let root = scratch.0.join(format!("candidate-{lombok}"));
        populate(&root);
        translate(&root, lombok);
        let after = validate_jvm_sources(&config(
            &root,
            scratch.0.join(format!("mixed-{lombok}")),
            lombok,
        ))
        .expect("mixed Kotlin and Java compile and dispatch the same values");
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .trim()
                .replace("\r\n", "\n"),
            expected,
            "Lombok={lombok}"
        );
    }
}
