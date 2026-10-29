use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "notlin-property-annotation-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, source: &str) {
        fs::write(self.0.join(name), source).unwrap();
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.0.join(name)).unwrap_or_default()
    }

    fn translate(&self, lombok: bool) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
        for (key, _) in
            std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with("NOTLIN_"))
        {
            command.env_remove(key);
        }
        command
            .current_dir(&self.0)
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
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const BIN: &str = r#"package probe
import com.fasterxml.jackson.annotation.JsonProperty
import com.fasterxml.jackson.annotation.JsonManagedReference
import com.fasterxml.jackson.annotation.JsonBackReference
class Response {
    class Bin {
        @JsonProperty("bin_mode")
        lateinit var binMode: String
        private lateinit var secret: String
        fun readSecret(): String = secret
        fun writeSecret(value: String) { secret = value }
    }
    class Batch {
        @JsonManagedReference
        var lines: List<Line> = emptyList()
    }
    class Line {
        var referenceNo: String = "line"
        @JsonBackReference
        lateinit var parent: Batch
    }
}
"#;

#[test]
fn instance_lateinit_uses_checked_accessors_including_private_reads() {
    for lombok in [false, true] {
        let f = Fixture::new();
        f.write("Response.kt", BIN);
        f.translate(lombok);
        let java = f.read("Response.java");
        assert!(
            f.read("Response.kt").is_empty(),
            "{}",
            f.read("Response.kt")
        );
        assert!(java.contains("String value = this.binMode;"), "{java}");
        assert!(java.contains("String value = this.secret;"), "{java}");
        assert!(java.contains("return this.getSecret();"), "{java}");
        assert!(java.contains("Objects.requireNonNull(binMode"), "{java}");
        assert!(java.contains("Objects.requireNonNull(secret"), "{java}");
        assert!(java.contains("has not been initialized"), "{java}");
    }
}

#[test]
fn external_annotation_classes_resolve_explicit_imports_nested_names_and_aliases() {
    let f = Fixture::new();
    f.write(
        "Types.kt",
        r#"package probe
import dependency.Handlers
import dependency.Handlers.Nested as Alias
import dependency.Standalone
@Mapping(using = Handlers.Nested::class)
class Nested
@Mapping(using = Alias::class)
class Aliased
@Mapping(using = Standalone::class)
class Direct
"#,
    );
    f.translate(true);
    for (name, literal) in [
        ("Nested", "Handlers.Nested.class"),
        ("Aliased", "dependency.Handlers.Nested.class"),
        ("Direct", "Standalone.class"),
    ] {
        let java = f.read(&format!("{name}.java"));
        assert!(
            java.contains(&format!("@Mapping(using = {literal})")),
            "{java}"
        );
        assert!(!java.contains(" as Alias"), "{java}");
    }
    assert!(f.read("Types.kt").is_empty(), "{}", f.read("Types.kt"));
}

#[test]
fn unresolved_or_ambiguous_annotation_classes_keep_their_owners() {
    for imports in [
        "",
        "import first.Handler\nimport second.Handler\n",
        "import dependency.*\n",
    ] {
        let f = Fixture::new();
        f.write("Unrelated.kt", "package unrelated\nclass Handler\n");
        f.write(
            "Owner.kt",
            &format!("package probe\n{imports}@Mapping(using = Handler::class)\nclass Owner\n"),
        );
        f.translate(true);
        assert!(!f.0.join("Owner.java").exists());
        assert!(f.read("Owner.kt").contains("class Owner"));
    }
}

#[test]
fn imported_kotlin_type_alias_is_not_mistaken_for_a_dependency_class() {
    let f = Fixture::new();
    f.write(
        "Alias.kt",
        "package dependency\ntypealias Handler = String\n",
    );
    f.write(
        "Owner.kt",
        "package probe\nimport dependency.Handler\n@Mapping(using = Handler::class)\nclass Owner\n",
    );
    f.translate(true);
    assert!(!f.0.join("Owner.java").exists());
    assert!(f.read("Owner.kt").contains("class Owner"));
}

#[test]
fn static_lateinit_stays_retained_while_callers_can_bypass_its_getter() {
    let f = Fixture::new();
    f.write(
        "Registry.kt",
        "package probe\nobject Registry { lateinit var name: String }\n",
    );
    f.translate(true);
    assert!(!f.0.join("Registry.java").exists());
    assert!(f.read("Registry.kt").contains("static lateinit"));
}

fn required(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).expect(name));
    assert!(path.is_file(), "{name}: {path:?}");
    path
}

fn validate(root: &Path, output: &str, lombok: bool) -> String {
    use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
    let mut config = JvmValidationConfig::new(
        required("NOTLIN_KOTLINC"),
        required("NOTLIN_JAVAC"),
        required("NOTLIN_JAVA"),
        root.join(output),
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config.javac_args.extend(["--release".into(), "17".into()]);
    for name in [
        "NOTLIN_KOTLIN_STDLIB",
        "NOTLIN_KOTLIN_REFLECT",
        "NOTLIN_JACKSON_CORE",
        "NOTLIN_JACKSON_ANNOTATIONS",
        "NOTLIN_JACKSON_DATABIND",
        "NOTLIN_JACKSON_KOTLIN",
    ] {
        config.classpath.push(required(name));
    }
    if lombok {
        let jar = required("NOTLIN_LOMBOK");
        config.classpath.push(jar.clone());
        config.javac_args.extend([
            "-processorpath".into(),
            jar.into_os_string(),
            "-processor".into(),
            "lombok.launch.AnnotationProcessorHider$AnnotationProcessor".into(),
        ]);
        config.kotlinc_args.push(
            format!(
                "-Xplugin={}",
                required("NOTLIN_KOTLIN_LOMBOK_PLUGIN").display()
            )
            .into(),
        );
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
    config.run_main_class = Some("probe.RunnerKt".into());
    validate_jvm_sources(&config)
        .unwrap_or_else(|error| panic!("{output}: {error}"))
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned()
}

#[test]
#[ignore = "requires the native Kotlin/Java, Lombok and Jackson tools in NOTLIN_* variables"]
fn jackson_and_java_callers_agree_before_and_after_translation() {
    for lombok in [false, true] {
        let f = Fixture::new();
        f.write("Response.kt", BIN);
        f.write(
            "Location.kt",
            r#"package probe
import com.fasterxml.jackson.databind.PropertyNamingStrategies
import com.fasterxml.jackson.databind.annotation.JsonNaming
@JsonNaming(PropertyNamingStrategies.UpperCamelCaseStrategy::class)
data class Location(val aisle: String, val rack: String)
"#,
        );
        f.write(
            "Check.java",
            r#"package probe;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.module.kotlin.KotlinModule;
public final class Check {
    static boolean fails(Runnable action) {
        try { action.run(); return false; }
        catch (RuntimeException expected) { return true; }
    }
    public static String verify() throws Exception {
        Response.Bin bin = new Response.Bin();
        boolean unreadable = fails(bin::getBinMode) && fails(bin::readSecret);
        bin.setBinMode("manual");
        boolean rejectsNull = fails(() -> bin.setBinMode(null));
        bin.writeSecret("secret");
        ObjectMapper mapper = new ObjectMapper().registerModule(new KotlinModule.Builder().build());
        Response.Bin restored = mapper.readValue("{\"bin_mode\":\"automatic\"}", Response.Bin.class);
        var tree = mapper.valueToTree(new Location("A", "R"));
        Location location = mapper.readValue("{\"Aisle\":\"A2\",\"Rack\":\"R2\"}", Location.class);
        Response.Batch batch = mapper.readValue("{\"lines\":[{}]}", Response.Batch.class);
        boolean parentRestored = batch.getLines().get(0).getParent() == batch;
        boolean parentSerialized = mapper.valueToTree(batch).path("lines").get(0).has("parent");
        return unreadable + "/" + rejectsNull + "/" + bin.getBinMode() + "/" + bin.readSecret()
            + "/" + restored.getBinMode() + "/" + tree.path("Aisle").asText()
            + "/" + tree.path("Rack").asText() + "/" + tree.has("aisle")
            + "/" + location.getAisle() + "/" + location.getRack()
            + "/" + parentRestored + "/" + parentSerialized;
    }
}
"#,
        );
        f.write(
            "Runner.kt",
            "package probe\nfun main() {\n suspend fun anchor() {}\n println(Check.verify())\n}\n",
        );
        let before = validate(&f.0, "original", lombok);
        assert_eq!(
            before,
            "true/true/manual/secret/automatic/A/R/false/A2/R2/true/false"
        );
        f.translate(lombok);
        assert!(f.0.join("Response.java").is_file());
        assert!(f.0.join("Location.java").is_file());
        assert!(!f.0.join("Response.kt").is_file());
        assert!(!f.0.join("Location.kt").is_file());
        assert_eq!(validate(&f.0, "migrated", lombok), before);
    }
}
