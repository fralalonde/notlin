use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn config(root: &Path, output: PathBuf, classpath: &[PathBuf]) -> JvmValidationConfig {
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output,
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config
        .javac_args
        .extend(["--release".into(), "17".into(), "-proc:none".into()]);
    config.classpath.extend_from_slice(classpath);
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
#[ignore = "requires JVM tools, Kotlin reflect, Lombok, and Jackson jars in NOTLIN_* environment variables"]
fn jackson_preserves_explicit_data_constructor_object_roundtrips() {
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
    let scratch = Scratch(
        std::env::temp_dir().join(format!("notlin-jackson-constructor-{}", std::process::id())),
    );
    fs::create_dir_all(&scratch.0).unwrap();
    let original = scratch.0.join("original");
    let sources = [
        (
            "Default.java",
            "package boundary; import java.lang.annotation.*; @Target(ElementType.CONSTRUCTOR) @Retention(RetentionPolicy.RUNTIME) public @interface Default {}\n",
        ),
        (
            "Token.kt",
            r#"package boundary
import java.util.UUID
data class Token @Default constructor(@JvmField val objectId: UUID, @JvmField val lookupId: String) {
    constructor(lookupId: String) : this(UUID.randomUUID(), lookupId)
    fun getObjectId(): UUID = objectId
    fun getLookupId(): String = lookupId
    override fun toString(): String = lookupId
}
"#,
        ),
        (
            "RoundTrip.java",
            r#"package boundary;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.module.kotlin.KotlinModule;
public class RoundTrip {
    public static String verify() throws Exception {
        ObjectMapper mapper = new ObjectMapper().registerModule(new KotlinModule.Builder().build());
        Token value = mapper.readValue("{\"objectId\":\"00000000-0000-0000-0000-000000000012\",\"lookupId\":\"supplied\"}", Token.class);
        Token copy = mapper.readValue(mapper.writeValueAsString(value), Token.class);
        return copy.getObjectId() + "|" + copy.getLookupId();
    }
}
"#,
        ),
        (
            "Runner.kt",
            "package boundary\nfun main() {\n    suspend fun retainedHarness() {}\n    println(RoundTrip.verify())\n}\n",
        ),
    ];
    fs::create_dir_all(&original).unwrap();
    for (name, source) in sources {
        fs::write(original.join(name), source).unwrap();
    }
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original classes"),
        &classpath,
    ))
    .expect("original Kotlin JSON roundtrip");
    let expected = before.commands.last().unwrap().stdout.trim();
    assert_eq!(expected, "00000000-0000-0000-0000-000000000012|supplied");
    for lombok in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok}"));
        fs::create_dir_all(&migrated).unwrap();
        for (name, source) in sources {
            fs::write(migrated.join(name), source).unwrap();
        }
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
        command
            .arg("--root")
            .arg(&migrated)
            .args(["--annotations", "none", "--in-place"]);
        if lombok {
            command.arg("--lombok");
        }
        let translated = command.arg(&migrated).output().unwrap();
        assert!(translated.status.success(), "{translated:?}");
        let java = fs::read_to_string(migrated.join("Token.java")).unwrap_or_default();
        assert!(java.contains("class Token"), "{java}\n{translated:?}");
        assert_eq!(
            java.matches("@java.beans.ConstructorProperties(").count(),
            1,
            "{java}"
        );
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed classes-{lombok}")),
            &classpath,
        ))
        .expect("translated Java JSON roundtrip");
        assert_eq!(after.commands.last().unwrap().stdout.trim(), expected);
    }
}
