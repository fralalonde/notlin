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
            "notlin-jackson-default-ctor-{}-{id}",
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
        "Default.java",
        "package boundary.defaults; import java.lang.annotation.*; @Target(ElementType.CONSTRUCTOR) @Retention(RetentionPolicy.RUNTIME) public @interface Default {}\n",
    ),
    (
        "Payload.kt",
        r#"package boundary.defaults
import com.fasterxml.jackson.annotation.JsonTypeInfo
import com.fasterxml.jackson.annotation.JsonTypeName
data class IdentifierDto(val code: String)
enum class Category { BASIC, PREMIUM }
@JsonTypeInfo(use = JsonTypeInfo.Id.NAME, property = "kind")
interface PayloadContract {
    val reference: String
    val id: IdentifierDto
    val category: Category
    val note: String?
}
@JsonTypeName("PAYLOAD")
data class Payload @JvmOverloads @Default constructor(
    override val reference: String,
    override val id: IdentifierDto = IdentifierDto("${reference}-auto"),
    override val category: Category = Category.BASIC,
    override val note: String? = "default-note"
) : PayloadContract
"#,
    ),
    (
        "RoundTrip.java",
        r#"package boundary.defaults;
import java.util.HashMap;
import java.util.Map;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.module.kotlin.KotlinModule;
public final class RoundTrip {
    private static String detail(ObjectMapper mapper, Object value) {
        JsonNode node = mapper.valueToTree(value);
        return node.path("id").path("code").asText() + "/" + node.path("category").asText() + "/" + node.path("note").asText();
    }
    private static boolean rejectsRead(ObjectMapper mapper, String json) {
        try { mapper.readValue(json, Payload.class); return false; }
        catch (Exception expected) { return true; }
    }
    private static boolean rejectsConvert(ObjectMapper mapper, Map<String, Object> values) {
        try { mapper.convertValue(values, Payload.class); return false; }
        catch (IllegalArgumentException expected) { return true; }
    }
    public static String verify() throws Exception {
        ObjectMapper mapper = new ObjectMapper().registerModule(new KotlinModule.Builder().build());
        mapper.registerSubtypes(new com.fasterxml.jackson.databind.jsontype.NamedType(Payload.class, "PAYLOAD"));
        String missing = detail(mapper, mapper.readValue("{\"kind\":\"PAYLOAD\",\"reference\":\"r1\"}", Payload.class));
        String polymorphic = detail(mapper, mapper.readValue("{\"kind\":\"PAYLOAD\",\"reference\":\"r4\"}", PayloadContract.class));
        String supplied = detail(mapper, mapper.readValue("{\"kind\":\"PAYLOAD\",\"reference\":\"r2\",\"id\":{\"code\":\"given\"},\"category\":\"PREMIUM\",\"note\":\"supplied\"}", Payload.class));
        String nullableNull = detail(mapper, mapper.readValue("{\"kind\":\"PAYLOAD\",\"reference\":\"r3\",\"note\":null}", Payload.class));
        Map<String, Object> missingMap = new HashMap<>();
        missingMap.put("kind", "PAYLOAD");
        missingMap.put("reference", "r5");
        String convertedMissing = detail(mapper, mapper.convertValue(missingMap, Payload.class));
        Map<String, Object> suppliedMap = new HashMap<>();
        suppliedMap.put("kind", "PAYLOAD");
        suppliedMap.put("reference", "r6");
        suppliedMap.put("id", Map.of("code", "map-id"));
        suppliedMap.put("category", "PREMIUM");
        suppliedMap.put("note", "map-note");
        String convertedSupplied = detail(mapper, mapper.convertValue(suppliedMap, Payload.class));
        Map<String, Object> nullMap = new HashMap<>();
        nullMap.put("kind", "PAYLOAD");
        nullMap.put("reference", "r8");
        nullMap.put("id", null);
        String nullRejected = rejectsRead(mapper, "{\"kind\":\"PAYLOAD\",\"reference\":\"r7\",\"id\":null}")
            + "/" + rejectsConvert(mapper, nullMap);
        return missing + "|" + polymorphic + "|" + supplied + "|" + convertedMissing + "|" + convertedSupplied + "|" + nullableNull + "|reject-null=" + nullRejected;
    }
}
"#,
    ),
    (
        "Runner.kt",
        "package boundary.defaults\nfun main() {\n    suspend fun keepRunnerKotlin() {}\n    println(RoundTrip.verify())\n}\n",
    ),
];

fn populate(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
}

fn populate_unannotated(root: &Path) {
    populate(root);
    fs::write(
        root.join("Payload.kt"),
        r#"package boundary.defaults
import com.fasterxml.jackson.annotation.JsonCreator
import com.fasterxml.jackson.databind.annotation.JsonDeserialize
data class Tags(val values: List<String> = emptyList())
data class Payload(
    @Marker("scoped") val reference: String,
    val tags: Tags = Tags(),
    val note: String?,
    val enabled: Boolean = true,
    val label: String = "${reference}-auto"
)
"#,
    )
    .unwrap();
    fs::write(root.join("RoundTrip.java"), r#"package boundary.defaults;
import java.util.Map;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.module.kotlin.KotlinModule;
public final class RoundTrip {
    private static String detail(ObjectMapper mapper, Object value) {
        JsonNode node = mapper.valueToTree(value);
        return node.path("tags").path("values").size() + "/" + node.path("enabled").asBoolean()
            + "/" + node.path("label").asText() + "/" + node.path("note").asText();
    }
    public static String verify() throws Exception {
        ObjectMapper mapper = new ObjectMapper().registerModule(new KotlinModule.Builder().build())
            .registerModule(new com.fasterxml.jackson.databind.module.SimpleModule().addDeserializer(String.class, new ContextString()));
        String missing = detail(mapper, mapper.readValue("{\"reference\":\"r1\"}", Payload.class));
        String supplied = detail(mapper, mapper.readValue("{\"reference\":\"r2\",\"tags\":{\"values\":[\"a\"]},\"enabled\":false,\"label\":\"given\",\"note\":\"memo\"}", Payload.class));
        String converted = detail(mapper, mapper.convertValue(Map.of("reference", "r3"), Payload.class));
        String convertedSupplied = detail(mapper, mapper.convertValue(Map.of("reference", "r4", "enabled", false, "label", "mapped"), Payload.class));
        return missing + "|" + supplied + "|" + converted + "|" + convertedSupplied
            + "|tag-null=" + observeNull(mapper, "{\"reference\":\"r5\",\"tags\":null}")
            + "|boolean-null=" + observeNull(mapper, "{\"reference\":\"r6\",\"enabled\":null}");
    }
    private static String observeNull(ObjectMapper mapper, String json) {
        try { return detail(mapper, mapper.readValue(json, Payload.class)); }
        catch (Exception expected) { return "rejected"; }
    }
}
"#).unwrap();
    fs::write(root.join("Marker.java"), "package boundary.defaults; import java.lang.annotation.*; @Target(ElementType.PARAMETER) @Retention(RetentionPolicy.RUNTIME) public @interface Marker { String value(); }\n").unwrap();
    fs::write(root.join("ContextString.java"), r#"package boundary.defaults;
public final class ContextString extends com.fasterxml.jackson.databind.deser.std.StdScalarDeserializer<String> implements com.fasterxml.jackson.databind.deser.ContextualDeserializer {
    private final String prefix;
    public ContextString() { this(""); }
    private ContextString(String prefix) { super(String.class); this.prefix = prefix; }
    @Override public com.fasterxml.jackson.databind.JsonDeserializer<?> createContextual(com.fasterxml.jackson.databind.DeserializationContext context, com.fasterxml.jackson.databind.BeanProperty property) {
        Marker marker = property == null ? null : property.getAnnotation(Marker.class);
        return new ContextString(marker == null ? "" : marker.value() + ":");
    }
    @Override public String deserialize(com.fasterxml.jackson.core.JsonParser parser, com.fasterxml.jackson.databind.DeserializationContext context) throws java.io.IOException {
        return prefix + parser.getValueAsString();
    }
}
"#).unwrap();
}

fn translate(root: &Path, lombok_enabled: bool) -> std::process::Output {
    let binary = std::env::var_os("NOTLIN_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
    let mut command = Command::new(binary);
    command
        .arg("--root")
        .arg(root)
        .args(["--in-place", "--annotations", "none"]);
    if lombok_enabled {
        command.arg("--lombok");
    }
    command
        .arg(root.join("Payload.kt"))
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
        let plugin = PathBuf::from(
            std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").expect("NOTLIN_KOTLIN_LOMBOK_PLUGIN"),
        );
        config
            .kotlinc_args
            .push(format!("-Xplugin={}", plugin.display()).into());
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
    config.run_main_class = Some("boundary.defaults.RunnerKt".into());
    config
}

#[test]
fn cli_preserves_jackson_constructor_defaults_in_both_lombok_modes() {
    let scratch = Scratch::new();
    for lombok_enabled in [false, true] {
        let root = scratch.0.join(format!("cli-{lombok_enabled}"));
        populate(&root);
        let translated = translate(&root, lombok_enabled);
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        let java = fs::read_to_string(root.join("Payload.java")).unwrap_or_default();
        assert!(
            java.contains("ValueInstantiators.Base"),
            "Jackson defaults should use the generated presence-aware creator: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(java.contains("new IdentifierDto"), "{java}");
    }
}

#[test]
fn unannotated_data_defaults_use_a_presence_aware_jackson_instantiator() {
    let scratch = Scratch::new();
    populate_unannotated(&scratch.0);
    let translated = translate(&scratch.0, true);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    let java =
        fs::read_to_string(scratch.0.join("Payload.java")).expect("unannotated DTO translates");
    assert!(java.contains("ValueInstantiators.Base"), "{java}");
    assert!(
        java.contains("new Tags()") && java.contains("buffer.hasParameter"),
        "{java}"
    );
    assert!(
        java.contains("? (Boolean)") && java.contains(": true"),
        "{java}"
    );
    assert!(
        java.contains("boolean enabled"),
        "ordinary constructor keeps primitive ABI: {java}"
    );
}

#[test]
fn another_modules_jackson_dependency_does_not_change_plain_data_classes() {
    let scratch = Scratch::new();
    populate(&scratch.0);
    fs::write(
        scratch.0.join("Payload.kt"),
        "package boundary.defaults\ndata class Payload(val enabled: Boolean = true)\n",
    )
    .unwrap();
    let other = scratch.0.join("other/src/main/java");
    fs::create_dir_all(&other).unwrap();
    fs::write(
        other.join("Hint.java"),
        "import com.fasterxml.jackson.annotation.JsonCreator; class Hint {}\n",
    )
    .unwrap();
    let translated = translate(&scratch.0, true);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    let java = fs::read_to_string(scratch.0.join("Payload.java")).unwrap();
    assert!(!java.contains("com.fasterxml.jackson"), "{java}");
}

#[test]
#[ignore = "requires JVM tools, Kotlin reflect, Lombok, and Jackson jars in NOTLIN_* environment variables"]
fn unannotated_jackson_dto_preserves_omitted_and_supplied_defaults() {
    let classpath = [
        "NOTLIN_KOTLIN_STDLIB",
        "NOTLIN_KOTLIN_REFLECT",
        "NOTLIN_LOMBOK",
        "NOTLIN_JACKSON_CORE",
        "NOTLIN_JACKSON_ANNOTATIONS",
        "NOTLIN_JACKSON_DATABIND",
        "NOTLIN_JACKSON_KOTLIN",
    ]
    .map(|name| PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name}"))));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate_unannotated(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &classpath,
        false,
    ))
    .expect("original default DTO runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert!(expected.starts_with("0/true/scoped:r1-auto/null|1/false/given/memo|0/true/scoped:r3-auto/null|0/false/mapped/null|tag-null=rejected|boolean-null="), "{expected}");
    let migrated = scratch.0.join("migrated");
    populate_unannotated(&migrated);
    let translated = translate(&migrated, true);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(
        migrated.join("Payload.java").is_file(),
        "must exercise converted DTO"
    );
    let after = validate_jvm_sources(&config(
        &migrated,
        scratch.0.join("mixed-classes"),
        &classpath,
        true,
    ))
    .expect("translated default DTO runs");
    assert_eq!(after.commands.last().unwrap().stdout.trim(), expected);
}

#[test]
#[ignore = "requires JVM tools, Kotlin reflect, Lombok, and Jackson jars in NOTLIN_* environment variables"]
fn jackson_uses_defaults_for_missing_constructor_properties_in_both_lombok_modes() {
    let classpath = [
        "NOTLIN_KOTLIN_STDLIB",
        "NOTLIN_KOTLIN_REFLECT",
        "NOTLIN_LOMBOK",
        "NOTLIN_JACKSON_CORE",
        "NOTLIN_JACKSON_ANNOTATIONS",
        "NOTLIN_JACKSON_DATABIND",
        "NOTLIN_JACKSON_KOTLIN",
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
    .expect("original Kotlin/Jackson default fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "r1-auto/BASIC/default-note|r4-auto/BASIC/default-note|given/PREMIUM/supplied|r5-auto/BASIC/default-note|map-id/PREMIUM/map-note|r3-auto/BASIC/null|reject-null=true/true"
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
        assert!(migrated.join("Runner.kt").is_file());
        assert!(
            migrated.join("Payload.java").exists(),
            "presence-aware defaults should migrate this DTO"
        );
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &classpath,
            lombok_enabled,
        ))
        .expect("mixed Kotlin/Java Jackson default fixture compiles and runs");
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .trim()
                .replace("\r\n", "\n"),
            expected,
            "Lombok mode {lombok_enabled} must preserve Jackson defaults, supplied values, and explicit-null behavior"
        );
    }
}
