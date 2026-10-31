//! Compare metadata-bound member access against independently compiled Kotlin,
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
        "Api.java",
        r#"package sample;
import com.fasterxml.jackson.annotation.*;
import org.jetbrains.annotations.NotNull;
import java.lang.String;
@JsonTypeInfo(use = JsonTypeInfo.Id.NAME, property = "kind")
@JsonSubTypes(@JsonSubTypes.Type(value = Bean.class, name = "bean"))
public interface Api {
    @JsonIgnore int getCount();
    @JsonProperty(value = "label", access = JsonProperty.Access.READ_ONLY)
    @NotNull String getLabel();
}
"#,
    ),
    (
        "MetadataChild.java",
        r#"package sample;
import com.fasterxml.jackson.annotation.JsonIgnoreProperties;
@JsonIgnoreProperties({"unused"})
public interface MetadataChild extends Api {}
"#,
    ),
    (
        "Bean.java",
        r#"package sample;
import com.fasterxml.jackson.annotation.JsonTypeName;
@JsonTypeName("bean")
public final class Bean implements MetadataChild {
    private static int calls;
    @Override public int getCount() { calls++; return 7; }
    @Override public String getLabel() { calls++; return " ready "; }
    public static int calls() { return calls; }
}
"#,
    ),
    ("Other.java", "package domain; public interface Other {}"),
    (
        "Marker.java",
        r#"package sample;
import com.fasterxml.jackson.annotation.*;
import domain.*;
@JsonIgnoreType
public interface Marker extends java.io.Serializable {}
"#,
    ),
    (
        "CallableMarker.java",
        r#"package sample;
import com.fasterxml.jackson.annotation.JsonIgnoreType;
@JsonIgnoreType
public interface CallableMarker {
    kotlin.jvm.functions.Function0<String> TODO = () -> "custom";
}
"#,
    ),
    (
        "Reader.kt",
        r#"package sample
class Reader {
    fun count(api: Api): Int = api.count
    fun inherited(api: MetadataChild): Int = api.count
    fun label(api: Api): String = api.label
    fun trimmed(api: Api): String = api.label.trim()
}
"#,
    ),
    (
        "Failures.kt",
        r#"package sample
class Failures : Marker {
    fun plain(): String = TODO()
    fun reason(): String = TODO("next")
}
"#,
    ),
    (
        "Custom.kt",
        r#"package sample
class Custom : CallableMarker {
    fun call(): String = CallableMarker.TODO.invoke()
}
"#,
    ),
    (
        "JsonProbe.java",
        r#"package sample;
import com.fasterxml.jackson.databind.ObjectMapper;
public final class JsonProbe {
    public static String inspect(Api value) throws Exception {
        ObjectMapper mapper = new ObjectMapper();
        String json = mapper.writeValueAsString(value);
        Api copy = mapper.readValue(json, Api.class);
        return json + "|" + copy.getLabel().trim();
    }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    suspend fun keepRunner() {}
    val reader = Reader()
    val api = Bean()
    println("${reader.count(api)}|${reader.inherited(api)}|${reader.label(api)}|${reader.trimmed(api)}")
    val failures = Failures()
    try {
        failures.plain()
    } catch (error: kotlin.NotImplementedError) {
        println("${error.javaClass.name}:${error.message}")
    }
    try {
        failures.reason()
    } catch (error: kotlin.NotImplementedError) {
        println("${error.javaClass.name}:${error.message}")
    }
    println(Custom().call())
    println(JsonProbe.inspect(api))
    println(Bean.calls())
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
    for variable in [
        "NOTLIN_JACKSON_CORE",
        "NOTLIN_JACKSON_ANNOTATIONS",
        "NOTLIN_JACKSON_DATABIND",
    ] {
        config
            .classpath
            .push(std::env::var_os(variable).expect(variable).into());
    }
    config
        .classpath
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"));
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

fn scratch() -> Scratch {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Scratch(std::env::temp_dir().join(format!(
        "notlin-java-metadata-{}-{nonce}",
        std::process::id()
    )))
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
    for name in ["Reader", "Failures"] {
        assert!(
            root.join(format!("{name}.java")).exists(),
            "{name} must translate: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(root.join(format!("{name}.kt"))).unwrap_or_default()
        );
    }
    assert!(
        root.join("Custom.kt").exists(),
        "the inherited callable field must keep its binding"
    );
    assert!(root.join("Runner.kt").exists());
}

#[test]
fn metadata_allows_declared_getters_and_stdlib_todo_without_rewriting_callable_fields() {
    use notlin::semantics::{ReferenceReturnNullability, SemanticProvider, SyntaxSemanticProvider};
    let provider = SyntaxSemanticProvider::new(
        SOURCES
            .iter()
            .map(|(name, source)| (PathBuf::from(name), (*source).to_owned())),
    );
    let api = &provider.symbols_named("Api")[0].id;
    let label = provider
        .java_property_read(api, "label")
        .unwrap_or_else(|| {
            panic!(
                "metadata-bound String getter; field lookup: {:?}; getter symbols: {:?}",
                provider.java_field_named(api, "TODO"),
                provider.symbols_named("getLabel")
            )
        });
    assert_eq!(label.nullability, ReferenceReturnNullability::NonNull);
    assert!(!label.result_type.nullable);
    let scratch = scratch();
    populate(&scratch.0);
    translate(&scratch.0, false);
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Jackson, Lombok and its Kotlin plugin"]
fn runtime_metadata_getter_effects_and_todo_exceptions_match_original_kotlin() {
    let scratch = scratch();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        false,
    ))
    .expect("original Kotlin and Java compile and run");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "7|7| ready |ready\nkotlin.NotImplementedError:An operation is not implemented.\nkotlin.NotImplementedError:An operation is not implemented: next\ncustom\n{\"kind\":\"bean\",\"label\":\" ready \"}|ready\n6"
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
        .expect("converted readers, throws and retained callable-field users compile and run");
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
