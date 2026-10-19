use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
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
            "notlin-jackson-property-annotations-{}-{id}",
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

const SOURCES: [(&str, &str); 4] = [
    (
        "AccessorBase.kt",
        r#"package boundary.annotations
open class AccessorBase {
    open val selfReference: Any get() = this
    open val displayValue: String get() = "base"
    open var status: String = "base"
}
"#,
    ),
    (
        "AccessorChild.kt",
        r#"package boundary.annotations
import com.fasterxml.jackson.annotation.JsonIgnore
import com.fasterxml.jackson.annotation.JsonProperty
class AccessorChild : AccessorBase() {
    @get:JsonIgnore
    override val selfReference: AccessorChild get() = this

    @get:JsonProperty("renamed_display")
    override val displayValue: String get() = "visible"

    @set:JsonProperty("incoming_status")
    override var status: String = "active"
}
"#,
    ),
    (
        "JacksonProbe.java",
        r#"package boundary.annotations;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
public final class JacksonProbe {
    public static String verify() throws Exception {
        ObjectMapper mapper = new ObjectMapper();
        JsonNode original = mapper.valueToTree(new AccessorChild());
        AccessorChild restored = mapper.readValue("{\"incoming_status\":\"updated\"}", AccessorChild.class);
        JsonNode restoredJson = mapper.valueToTree(restored);
        return original.has("selfReference") + "/" + original.has("renamed_display") + "/" + original.has("displayValue") + "/" + original.path("renamed_display").asText()
            + "|" + restored.getStatus() + "/" + restoredJson.has("selfReference") + "/" + restoredJson.has("renamed_display") + "/" + restoredJson.path("renamed_display").asText();
    }
}
"#,
    ),
    (
        "Runner.kt",
        "package boundary.annotations\nfun main() {\n    suspend fun keepRunnerKotlin() {}\n    println(JacksonProbe.verify())\n}\n",
    ),
];

fn populate(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
}

fn install_repaired_child(root: &Path) {
    fs::write(
        root.join("AccessorBase.java"),
        r#"package boundary.annotations;
public class AccessorBase {
    private String status = "base";
    public Object getSelfReference() { return this; }
    public String getDisplayValue() { return "base"; }
    public String getStatus() { return status; }
    public void setStatus(String value) { status = value; }
}
"#,
    )
    .unwrap();
    fs::remove_file(root.join("AccessorBase.kt")).unwrap();
    fs::write(
        root.join("AccessorChild.kt"),
        r#"package boundary.annotations
import com.fasterxml.jackson.annotation.JsonIgnore
import com.fasterxml.jackson.annotation.JsonProperty
import kotlin.jvm.JvmName
class AccessorChild : AccessorBase() {
    @get:JvmName("notlinPropertygetSelfReference")
    val selfReference: AccessorChild get() = this
    @JsonIgnore
    override fun getSelfReference(): AccessorChild = selfReference

    @get:JvmName("notlinPropertygetDisplayValue")
    val displayValue: String get() = "visible"
    @JsonProperty("renamed_display")
    override fun getDisplayValue(): String = displayValue

    @get:JvmName("notlinPropertygetStatus")
    @set:JvmName("notlinPropertysetStatus")
    var status: String = "active"
    override fun getStatus(): String = status
    @JsonProperty("incoming_status")
    override fun setStatus(value: String) { status = value }
}
"#,
    )
    .unwrap();
}

fn translate(root: &Path, lombok_enabled: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .arg("--root")
        .arg(root)
        .args(["--in-place", "--annotations", "none"]);
    if lombok_enabled {
        command.arg("--lombok");
    }
    command
        .arg(root.join("AccessorChild.kt"))
        .output()
        .expect("run notlin")
}

fn config(
    root: &Path,
    output: PathBuf,
    classpath: &[PathBuf],
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
    if lombok_enabled {
        config.javac_args.extend([
            "-processorpath".into(),
            std::env::var_os("NOTLIN_LOMBOK").expect("NOTLIN_LOMBOK"),
            "-processor".into(),
            "lombok.launch.AnnotationProcessorHider$AnnotationProcessor".into(),
        ]);
        let mut plugin = std::ffi::OsString::from("-Xplugin=");
        plugin.push(
            std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").expect("NOTLIN_KOTLIN_LOMBOK_PLUGIN"),
        );
        config.kotlinc_args.push(plugin);
    } else {
        config.javac_args.push("-proc:none".into());
    }
    config.classpath.extend_from_slice(classpath);
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => config.kotlin_sources.push(path),
            Some("java") => config.java_sources.push(path),
            _ => {}
        }
    }
    config.run_main_class = Some("boundary.annotations.RunnerKt".into());
    config
}

#[test]
#[ignore = "requires JVM tools, Lombok, and Jackson jars in NOTLIN_* environment variables"]
fn jackson_getter_and_setter_annotations_survive_mixed_property_repairs() {
    let classpath = [
        "NOTLIN_KOTLIN_STDLIB",
        "NOTLIN_LOMBOK",
        "NOTLIN_JACKSON_CORE",
        "NOTLIN_JACKSON_ANNOTATIONS",
        "NOTLIN_JACKSON_DATABIND",
    ]
    .map(|name| PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name}"))));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &classpath,
        false,
    ))
    .expect("original Kotlin/Jackson property annotation fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "false/true/false/visible|updated/false/true/visible"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        install_repaired_child(&migrated);
        let child = translate(&migrated, lombok_enabled);
        assert!(
            child.status.success(),
            "AccessorChild, Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(migrated.join("AccessorBase.java").is_file());
        assert!(
            migrated.join("AccessorChild.java").is_file(),
            "AccessorChild was not emitted: stdout={} stderr={}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr),
        );
        let java = fs::read_to_string(migrated.join("AccessorChild.java")).unwrap();
        assert!(
            java.contains("JsonIgnore") && java.contains("JsonProperty"),
            "{java}"
        );
        assert!(
            !java.contains("notlinProperty"),
            "temporary property bridges must not survive: {java}"
        );
        assert!(!migrated.join("AccessorChild.kt").exists());
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &classpath,
            lombok_enabled,
        ))
        .expect("mixed Kotlin/Java property annotation fixture compiles and runs");
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .trim()
                .replace("\r\n", "\n"),
            expected,
            "mixed ownership must preserve getter ignore/rename and setter input names in Lombok mode {lombok_enabled}"
        );
    }
}
