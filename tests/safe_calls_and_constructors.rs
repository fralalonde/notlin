use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "notlin-safe-frontier-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
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

const SOURCES: &[(&str, &str)] = &[
    (
        "Model.kt",
        r#"package sample
import java.util.UUID
import remote.RemoteBridge
class Node(var name: String, val child: Node?, val number: Int) {
    fun touch(argument: String): String = name + argument
    fun update(argument: String) { name = argument }
}
class Cases {
    fun upper(value: String?): String? = value?.uppercase()
    fun property(value: Node?): String? = value?.name
    fun number(value: Node?): Int? = value?.number
    fun withArgument(value: Node?): String? = value?.touch(Probe.argument())
    fun chain(value: Node?): String? = value?.child?.touch(Probe.argument())
    fun tail(value: Node?): String? = value?.touch(Probe.argument())?.uppercase()
    fun produced(): String? = Probe.next()?.touch(Probe.argument())
    fun statement() { Probe.next()?.update(Probe.argument()) }
    fun assignment() { Probe.next()?.name = Probe.argument() }
    fun listSize(value: List<String>?): Int? = value?.size
    fun fallback(): String = Probe.next()?.touch(Probe.argument()) ?: "fallback"
    fun unitValue(value: Node?): Unit? = value?.update(Probe.argument())
    fun foreign(): String? = RemoteBridge.next()?.child?.text
}
interface ObjectIdBase {
    fun getObjectId(): UUID
}
interface ObjectId : ObjectIdBase {
    override fun getObjectId(): UUID
}
interface Identity : ObjectId {
    fun getLookupId(): String
    fun getExternalId(): String?
}
class Snapshot(val id: String, val lookupId: String, val externalId: String?) {
    constructor(id: Identity) : this(id.getObjectId().toString(), id.getLookupId(), id.getExternalId())
}
class PlatformSnapshot(val id: String) {
    constructor(id: PlatformIds) : this(id.getObjectId().toString())
}
"#,
    ),
    (
        "Joins.kt",
        r#"package sample
import com.fasterxml.jackson.annotation.*
import constants.SEP
class Joined(val id: String) {
    constructor(vararg parts: String) : this(parts.joinToString(SEP))
}
class Joins {
    fun join(parts: Array<String>): String = parts.joinToString("|")
    fun defaults(vararg parts: String): String = parts.joinToString()
    fun objects(parts: Array<Any?>): String = parts.joinToString("|")
    fun integers(vararg parts: Int): String = parts.joinToString("|")
}
"#,
    ),
    (
        "Constants.kt",
        "package constants\nconst val SEP: String = \"/\"\n",
    ),
    (
        "Remote.kt",
        r#"package remote
class RemoteChild(val text: String)
class RemoteContainer(val child: RemoteChild?)
"#,
    ),
    (
        "PlatformIds.java",
        r#"package sample;
interface PlatformBase { java.util.UUID getObjectId(); }
public interface PlatformIds extends PlatformBase { java.util.UUID getObjectId(); }
"#,
    ),
    (
        "RemoteBridge.java",
        r#"package remote;
public class RemoteBridge { public static RemoteContainer next() { return new RemoteContainer(new RemoteChild("foreign")); } }
"#,
    ),
    (
        "Probe.java",
        r#"package sample;
public class Probe {
    public static int receivers, arguments;
    public static Node value;
    public static Node next() { receivers++; return value; }
    public static String argument() { arguments++; return "!"; }
    public static void reset(Node node) { value=node; receivers=0; arguments=0; }
}
"#,
    ),
    (
        "Runner.java",
        r#"package sample;
import java.util.List;
import java.util.Locale;
import java.util.UUID;
public class Runner {
    public static void main(String[] args) {
        Locale.setDefault(Locale.forLanguageTag("tr-TR"));
        Cases c = new Cases();
        Node leaf = new Node("leaf", null, 7);
        Node root = new Node("root", leaf, 8);
        System.out.println(c.upper(null) + "/" + c.upper("i"));
        System.out.println(c.property(null) + "/" + c.property(root) + "/" + c.number(root));
        Probe.reset(null);
        System.out.println(c.withArgument(null) + "/" + c.chain(null) + "/" + c.chain(leaf) + "/" + Probe.arguments);
        System.out.println(c.chain(root) + "/" + c.tail(root) + "/" + Probe.arguments);
        System.out.println(c.listSize(null) + "/" + c.listSize(List.of("a", "b")));
        Probe.reset(null);
        System.out.println(c.produced() + "/" + Probe.receivers + "/" + Probe.arguments);
        c.statement(); c.assignment();
        System.out.println(Probe.receivers + "/" + Probe.arguments);
        Probe.reset(leaf);
        System.out.println(c.produced() + "/" + Probe.receivers + "/" + Probe.arguments);
        c.statement(); c.assignment();
        System.out.println(leaf.getName() + "/" + Probe.receivers + "/" + Probe.arguments);
        Snapshot snapshot = new Snapshot(new Identity() {
            public UUID getObjectId() { return new UUID(0L, 1L); }
            public String getLookupId() { return "lookup"; }
            public String getExternalId() { return null; }
        });
        System.out.println(snapshot.getId() + "/" + snapshot.getLookupId() + "/" + snapshot.getExternalId());
        System.out.println(new PlatformSnapshot((PlatformIds) () -> new UUID(0L, 2L)).getId());
        System.out.println(new Joined("a", "", "b").getId() + "/" + new Joined(new String[0]).getId());
        System.out.println(new Joins().join(new String[]{"a", "b"}) + "/" + new Joins().defaults("a", "b"));
        System.out.println(new Joins().objects(new Object[]{1, null, "x"}) + "/" + new Joins().integers(1, 2, 3));
        Probe.reset(null);
        System.out.println(c.fallback() + "/" + Probe.receivers + "/" + Probe.arguments);
        Probe.reset(leaf);
        System.out.println(c.fallback() + "/" + Probe.receivers + "/" + Probe.arguments);
        Probe.reset(null);
        System.out.println((c.unitValue(null) == null) + "/" + (c.unitValue(leaf) != null) + "/" + Probe.arguments);
        System.out.println(c.foreign());
    }
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
fn translate(root: &Path, lombok: bool) -> std::process::Output {
    let binary = std::env::var_os("NOTLIN_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
    let mut command = std::process::Command::new(binary);
    command
        .args([
            "--jvm-facts=off",
            "--in-place",
            "--annotations",
            "none",
            "--root",
        ])
        .arg(root);
    if lombok {
        command.arg("--lombok");
    }
    command.arg(root).output().unwrap()
}
fn assert_migrated(root: &Path, output: &std::process::Output) {
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{diagnostics}");
    for name in [
        "Node",
        "Cases",
        "Snapshot",
        "PlatformSnapshot",
        "Joined",
        "Joins",
    ] {
        assert!(
            root.join(format!("{name}.java")).is_file(),
            "{name} remains Kotlin:\n{}\n{diagnostics}",
            fs::read_to_string(root.join("Model.kt")).unwrap_or_default()
        );
    }
}
#[test]
fn migrates_safe_calls_array_joins_and_constructor_chains() {
    let scratch = Scratch::new();
    for lombok in [false, true] {
        let root = scratch.0.join(format!("source-{lombok}"));
        populate(&root);
        assert_migrated(&root, &translate(&root, lombok));
    }
}

fn config(root: &Path, output: PathBuf, lombok: bool) -> JvmValidationConfig {
    let mut cfg = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output,
    );
    cfg.kotlinc_args.extend(["-jvm-target".into(), "21".into()]);
    cfg.javac_args.extend(["--release".into(), "21".into()]);
    cfg.classpath.push(
        std::env::var_os("NOTLIN_KOTLIN_STDLIB")
            .expect("NOTLIN_KOTLIN_STDLIB")
            .into(),
    );
    cfg.classpath.push(
        std::env::var_os("NOTLIN_JACKSON_ANNOTATIONS")
            .expect("NOTLIN_JACKSON_ANNOTATIONS")
            .into(),
    );
    if lombok {
        cfg.classpath.push(
            std::env::var_os("NOTLIN_LOMBOK")
                .expect("NOTLIN_LOMBOK")
                .into(),
        );
        cfg.javac_args.push("-proc:full".into());
        let mut plugin = std::ffi::OsString::from("-Xplugin=");
        plugin.push(std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").expect("Kotlin Lombok plugin"));
        cfg.kotlinc_args.push(plugin);
    } else {
        cfg.javac_args.push("-proc:none".into());
    }
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("kt") => cfg.kotlin_sources.push(path),
            Some("java") => cfg.java_sources.push(path),
            _ => {}
        }
    }
    cfg.run_main_class = Some("sample.Runner".into());
    cfg
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin compiler plugin"]
fn runtime_preserves_null_propagation_evaluation_order_and_constructors() {
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&config(&original, scratch.0.join("before"), false)).unwrap();
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "null/I\nnull/root/8\nnull/null/null/0\nleaf!/ROOT!/2\nnull/2\nnull/1/0\n3/0\nleaf!/1/1\n!/3/3\n00000000-0000-0000-0000-000000000001/lookup/null\n00000000-0000-0000-0000-000000000002\na//b/\na|b/a, b\n1|null|x/1|2|3\nfallback/1/0\n!!/1/1\ntrue/true/1\nforeign"
    );
    for lombok in [false, true] {
        let root = scratch.0.join(format!("migrated-{lombok}"));
        populate(&root);
        assert_migrated(&root, &translate(&root, lombok));
        let after = validate_jvm_sources(&config(
            &root,
            scratch.0.join(format!("after-{lombok}")),
            lombok,
        ))
        .unwrap();
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .trim()
                .replace("\r\n", "\n"),
            expected,
            "Lombok {lombok}"
        );
    }
}
