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
            "notlin-mutable-contract-inheritance-{}-{id}",
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

const SOURCES: [(&str, &str); 8] = [
    (
        "Root.kt",
        r#"package sample
interface Root {
    val id: String
}
"#,
    ),
    (
        "Mutable.kt",
        r#"package sample
interface Mutable : Root { override var id: String }
"#,
    ),
    (
        "Base.kt",
        r#"package sample
import com.fasterxml.jackson.module.kotlin.jacksonObjectMapper
import com.fasterxml.jackson.module.kotlin.readValue
@Tag("select id from table")
open class Base(
    @FieldTag("public val id; var id; select id from table")
    override var id: String,
    val defaultEcho: String = id
) : Root {
    val initializedId: String = id
    val initId: String

    init {
        initId = this.id
    }

    constructor(seed: CharSequence, suffix: Int = 0) : this("$seed:$suffix")

    suspend fun baseHook() {}
    fun baseRead(): String = id
    fun baseWrite(value: String) { id = value }

    fun jacksonRoundTrip(json: String): String {
        val mapper = jacksonObjectMapper()
        val decoded: Base = mapper.readValue(json)
        val tree = mapper.readTree(mapper.writeValueAsString(decoded))
        return "${decoded.id}|${decoded.defaultEcho}|${tree["id"].asText()}"
    }
}
"#,
    ),
    (
        "Intermediate.kt",
        r#"package sample
open class Intermediate(id: String) : Base(id) {
    suspend fun intermediateHook() {}

    fun implicitOther(other: Other): String = with(other) { id }
    fun explicitOther(other: Other): String = with(other) { this.id }
}
"#,
    ),
    (
        "Leaf.kt",
        r#"package sample
open class Leaf(id: String) : Intermediate(id), Mutable {
    suspend fun leafHook() {}
}
"#,
    ),
    (
        "LeafOwned.kt",
        r#"package sample
open class LeafOwned(override var id: String) : Intermediate(id), Mutable {
    suspend fun ownedHook() {}
}
"#,
    ),
    (
        "Tag.kt",
        r#"package sample
@Target(AnnotationTarget.CLASS)
@Retention(AnnotationRetention.RUNTIME)
annotation class Tag(val value: String)
"#,
    ),
    (
        "FieldTag.kt",
        r#"package sample
@Target(AnnotationTarget.FIELD)
@Retention(AnnotationRetention.RUNTIME)
annotation class FieldTag(val value: String)
"#,
    ),
];

const EXTERNAL_SOURCES: [(&str, &str); 4] = [
    (
        "JavaProxy.java",
        r#"package sample;
public final class JavaProxy extends Leaf {
    private static int getterCalls;
    private static int setterCalls;
    public JavaProxy(String id) { super(id); }
    @Override public String getId() { getterCalls++; return "proxy(" + super.getId() + ")"; }
    @Override public void setId(String id) { setterCalls++; super.setId("java:" + id); }
    public static int getterCalls() { return getterCalls; }
    public static int setterCalls() { return setterCalls; }
}
"#,
    ),
    (
        "OwnedProxy.java",
        r#"package sample;
public final class OwnedProxy extends LeafOwned {
    private static int getterCalls;
    private static int setterCalls;
    public OwnedProxy(String id) { super(id); }
    @Override public String getId() { getterCalls++; return "owned(" + super.getId() + ")"; }
    @Override public void setId(String id) { setterCalls++; super.setId("java:" + id); }
    public static int getterCalls() { return getterCalls; }
    public static int setterCalls() { return setterCalls; }
}
"#,
    ),
    (
        "JavaCaller.java",
        r#"package sample;
public final class JavaCaller {
    public static String exercise(Leaf leaf, Mutable mutable, Root root) {
        String before = leaf.getId() + "|" + mutable.getId() + "|" + root.getId();
        mutable.setId("java-call");
        String after = leaf.getId() + "|" + mutable.getId() + "|" + root.getId();
        return before + "->" + after;
    }
    public static String exercise(LeafOwned leaf, Mutable mutable, Root root) {
        String before = leaf.getId() + "|" + mutable.getId() + "|" + root.getId();
        mutable.setId("java-call");
        String after = leaf.getId() + "|" + mutable.getId() + "|" + root.getId();
        return before + "->" + after;
    }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
data class Other(val id: String)

fun main() {
    val proxy = JavaProxy("seed")
    val leaf: Leaf = proxy
    val mutable: Mutable = proxy
    val root: Root = proxy
    println("initial=${leaf.id}|${mutable.id}|${root.id}")
    println("construction=${leaf.initializedId}|${leaf.initId}|${leaf.defaultEcho}")
    println("lambda=${leaf.implicitOther(Other("implicit"))}|${leaf.explicitOther(Other("explicit"))}")
    mutable.id = "kotlin-call"
    leaf.baseWrite("body-call")
    println("updated=${leaf.id}|${mutable.id}|${root.id}")
    println("base=${leaf.baseRead()}")
    println("java=${JavaCaller.exercise(leaf, mutable, root)}")
    println("counts=${JavaProxy.getterCalls()}|${JavaProxy.setterCalls()}")

    val ownedProxy = OwnedProxy("seed")
    val owned: LeafOwned = ownedProxy
    val ownedMutable: Mutable = ownedProxy
    val ownedRoot: Root = ownedProxy
    println("owned-initial=${owned.id}|${ownedMutable.id}|${ownedRoot.id}")
    println("owned-construction=${owned.initializedId}|${owned.initId}|${owned.defaultEcho}")
    ownedMutable.id = "kotlin-call"
    owned.baseWrite("body-call")
    println("owned-updated=${owned.id}|${ownedMutable.id}|${ownedRoot.id}")
    println("owned-base=${owned.baseRead()}")
    println("owned-java=${JavaCaller.exercise(owned, ownedMutable, ownedRoot)}")
    println("owned-counts=${OwnedProxy.getterCalls()}|${OwnedProxy.setterCalls()}")
    println("tag=${Base::class.java.getAnnotation(Tag::class.java).value}")
    println("secondary=${Base("secondary" as CharSequence).id}")
    println("private-fields=${java.lang.reflect.Modifier.isPrivate(Base::class.java.getDeclaredField("id").modifiers)}|${java.lang.reflect.Modifier.isPrivate(LeafOwned::class.java.getDeclaredField("id").modifiers)}")
    println("field-tag=${Base::class.java.getDeclaredField("id").getAnnotation(FieldTag::class.java).value}")
    println("jackson=${Base("json-seed").jacksonRoundTrip("""{"id":"json-id","defaultEcho":"json-echo"}""")}")
}
"#,
    ),
];

fn populate(root: &Path) {
    let source_root = root.join("sources");
    let external_root = root.join("external");
    fs::create_dir_all(&source_root).unwrap();
    fs::create_dir_all(&external_root).unwrap();
    for (name, source) in SOURCES {
        fs::write(source_root.join(name), source).unwrap();
    }
    for (name, source) in EXTERNAL_SOURCES {
        fs::write(external_root.join(name), source).unwrap();
    }
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
    command.arg(root).output().expect("run notlin")
}

fn jvm_config(
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
    for name in [
        "NOTLIN_KOTLIN_REFLECT",
        "NOTLIN_JACKSON_CORE",
        "NOTLIN_JACKSON_ANNOTATIONS",
        "NOTLIN_JACKSON_DATABIND",
        "NOTLIN_JACKSON_KOTLIN",
    ] {
        config.classpath.push(PathBuf::from(
            std::env::var_os(name).unwrap_or_else(|| panic!("{name}")),
        ));
    }
    if lombok_enabled {
        config.classpath.push(PathBuf::from(
            std::env::var_os("NOTLIN_LOMBOK").expect("NOTLIN_LOMBOK"),
        ));
        config.javac_args.push("-proc:full".into());
        let mut plugin = std::ffi::OsString::from("-Xplugin=");
        plugin.push(
            std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").expect("NOTLIN_KOTLIN_LOMBOK_PLUGIN"),
        );
        config.kotlinc_args.push(plugin);
    } else {
        config.javac_args.push("-proc:none".into());
    }
    for source_root in [root.join("sources"), root.join("external")] {
        for entry in fs::read_dir(source_root).unwrap() {
            let path = entry.unwrap().path();
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("kt") => config.kotlin_sources.push(path),
                Some("java") => config.java_sources.push(path),
                _ => {}
            }
        }
    }
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

fn assert_shape(root: &Path) {
    let source_root = root.join("sources");
    assert!(!source_root.join("Root.kt").exists());
    assert!(source_root.join("Base.kt").is_file());
    assert!(source_root.join("Intermediate.kt").is_file());
    assert!(source_root.join("Leaf.kt").is_file());
    assert!(source_root.join("LeafOwned.kt").is_file());
    assert!(source_root.join("Root.java").is_file());
    assert!(!source_root.join("Base.java").exists());
    assert!(!source_root.join("Intermediate.java").exists());
    assert!(!source_root.join("Leaf.java").exists());
    assert!(!source_root.join("LeafOwned.java").exists());
    assert!(source_root.join("Mutable.java").is_file());
    assert!(!source_root.join("Mutable.kt").exists());
}

#[test]
fn mutable_interface_inherits_readonly_property_cli_shape() {
    let scratch = Scratch::new();
    let root = scratch.0.join("cli");
    populate(&root);
    let translated = translate(&root.join("sources"), false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert_shape(&root);
}

#[test]
#[ignore = "requires JVM tools, Kotlin reflect, Lombok, and Jackson jars in NOTLIN_* environment variables"]
fn real_toolchain_preserves_mutable_contract_dispatch_over_readonly_parent() {
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&jvm_config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        false,
    ))
    .expect("original Kotlin/Java mutable contract fixture compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "initial=proxy(seed)|proxy(seed)|proxy(seed)\nconstruction=proxy(seed)|proxy(seed)|seed\nlambda=implicit|explicit\nupdated=proxy(java:body-call)|proxy(java:body-call)|proxy(java:body-call)\nbase=proxy(java:body-call)\njava=proxy(java:body-call)|proxy(java:body-call)|proxy(java:body-call)->proxy(java:java-call)|proxy(java:java-call)|proxy(java:java-call)\ncounts=15|3\nowned-initial=owned(seed)|owned(seed)|owned(seed)\nowned-construction=owned(null)|owned(null)|seed\nowned-updated=owned(java:body-call)|owned(java:body-call)|owned(java:body-call)\nowned-base=owned(java:body-call)\nowned-java=owned(java:body-call)|owned(java:body-call)|owned(java:body-call)->owned(java:java-call)|owned(java:java-call)|owned(java:java-call)\nowned-counts=15|3\ntag=select id from table\nsecondary=secondary:0\nprivate-fields=true|true\nfield-tag=public val id; var id; select id from table\njackson=json-id|json-echo|json-id"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        let translated = translate(&migrated.join("sources"), lombok_enabled);
        assert!(
            translated.status.success(),
            "Lombok mode {lombok_enabled}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert_shape(&migrated);
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect("mixed Kotlin and Java mutable contract fixture compiles and runs");
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .trim()
                .replace("\r\n", "\n"),
            expected,
            "mixed runtime output in Lombok mode {lombok_enabled}"
        );
    }
}
