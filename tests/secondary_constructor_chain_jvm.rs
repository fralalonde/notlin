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
            "notlin-secondary-constructor-chain-jvm-{}-{id}",
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

const SITE_ID: &str = r#"package sample;
public final class SiteId {
    private final String value;
    public SiteId(String value) { this.value = value; }
    public String getValue() { return value; }
}
"#;

const SOURCES: [(&str, &str); 5] = [
    (
        "Kind.kt",
        "package sample\nenum class Kind { FIRST, SECOND }\n",
    ),
    (
        "Group.kt",
        r#"package sample
import support.*
data class Group(
    val site: SiteId,
    val type: Kind,
    val reference: String,
    val name: String
) {
    constructor(site: SiteId, type: Kind, reference: String) : this(site, type, reference, reference)
    constructor(site: SiteId, type: Kind) : this(site, type, type.name)
    val lookup: String
        get() {
            if (reference == type.name) {
                return "${site.value}/${type}"
            }
            return "${site.value}/${type}/$reference"
        }
}
"#,
    ),
    (
        "Provider.kt",
        r#"package sample
class Provider {
    var calls: String = ""
    var count: Int = 0

    fun read(): String {
        count += 1
        val value = count.toString()
        calls += value
        return value
    }

    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Deferred.kt",
        r#"package sample
import support.*
class Deferred(val first: String, val second: String, val third: String) : Parent() {
    constructor(first: String, second: String) : this(first, second, second)
    constructor(provider: Provider) : this(provider.read(), provider.read())
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    suspend fun retainedHarness() {}
    val site = SiteId("site")
    val primary = Group(site, Kind.FIRST, "ref", "label")
    val secondary = Group(site, Kind.SECOND, "middle")
    val chained = Group(site, Kind.FIRST)
    val provider = Provider()
    val deferred = Deferred(provider)
    println("${primary.name}|${secondary.name}|${chained.name}|${primary == Group(site, Kind.FIRST, "ref", "label")}|${secondary == Group(site, Kind.SECOND, "middle", "middle")}|${chained == Group(site, Kind.FIRST, "FIRST", "FIRST")}|${deferred.first}|${deferred.second}|${deferred.third}|${provider.calls}|${provider.count}|${primary.lookup}|${chained.lookup}")
}
"#,
    ),
];

fn populate(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("Parent.java"), "package sample; public class Parent { public Provider getProvider() { throw new AssertionError(\"parameter must shadow inherited property\"); } }\n").unwrap();
    fs::write(
        root.join("Marker.java"),
        "package support; public class Marker {}\n",
    )
    .unwrap();
    fs::write(root.join("SiteId.java"), SITE_ID).unwrap();
    for (name, source) in SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
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
    config.run_main_class = Some("sample.RunnerKt".into());
    config
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

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_data_class_secondary_constructor_chain() {
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
    .expect("original Kotlin constructor chain compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected,
        "label|middle|FIRST|true|true|true|1|2|2|12|2|site/FIRST/ref|site/FIRST"
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
        assert!(
            migrated.join("Group.java").is_file(),
            "Group should migrate in Lombok mode {lombok_enabled}: {}\nGroup.kt:\n{}",
            String::from_utf8_lossy(&translated.stderr),
            fs::read_to_string(migrated.join("Group.kt")).unwrap_or_default()
        );
        assert!(migrated.join("Runner.kt").is_file());
        assert!(migrated.join("Provider.kt").is_file());
        assert!(
            migrated.join("Deferred.java").is_file(),
            "effectful constructor chain must migrate: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(
            fs::read_to_string(migrated.join("Deferred.java"))
                .unwrap()
                .contains("this(provider.read(), provider.read());"),
            "delegation must read the parameter rather than the inherited property"
        );
        let group_java = fs::read_to_string(migrated.join("Group.java")).unwrap();
        assert!(
            group_java.contains("this(site, type, reference, reference);")
                && group_java.contains("this(site, type, type.name());"),
            "Java constructors must retain the unique secondary-delegation chain:\n{group_java}"
        );
        assert!(migrated.join("Deferred.java").is_file());
        assert!(migrated.join("Provider.kt").is_file());
        let deferred_java = fs::read_to_string(migrated.join("Deferred.java")).unwrap();
        assert!(
            deferred_java.contains("this(provider.read(), provider.read());")
                && deferred_java.contains("this(first, second, second);"),
            "deferred arguments must preserve the two-step constructor chain:\n{deferred_java}"
        );

        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Java/Kotlin data class chain compiles and runs");
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
