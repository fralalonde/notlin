use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "notlin-property-frontier-{}-{}",
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
        "DirectCalls.kt",
        "package sample\nobject DirectCalls { @JvmStatic fun make(): String = DeferredPayload(\"direct\").full; suspend fun retained() {} }\n",
    ),
    (
        "Constants.kt",
        "package constants\nconst val SEPARATOR: String = \".\"\nsuspend fun retained() {}\n",
    ),
    (
        "Reader.kt",
        r#"package sample
import constants.SEPARATOR as SEP
class Reader {
    fun lastPart(value: String): String = value.split(SEP).last()
    fun literal(): String = SEP
    fun shadow(SEP: String): String = SEP
}
"#,
    ),
    (
        "Bridge.kt",
        r#"package sample
interface Root<T> {
    fun getId(): T
    val aggregate: Any get() = getId() as Any
}
class Identifier(val text: String)
@Deprecated("retain the provider")
interface Provider<T> : Root<T> {
    val identifier: Identifier
    override val aggregate: Identifier get() = identifier
}
class Subject(private val stored: Identifier) : Provider<Identifier> {
    override fun getId(): Identifier = stored
    override val identifier: Identifier
        get() { Counter.read(); return stored }
}
"#,
    ),
    (
        "Counter.java",
        "package sample; public class Counter { public static int count; public static void read() { count++; } }\n",
    ),
    (
        "PolyContract.java",
        "package sample; import com.fasterxml.jackson.annotation.*; import java.util.*; @JsonIgnoreProperties(ignoreUnknown = true) @JsonTypeInfo(use = JsonTypeInfo.Id.NAME, property = \"kind\") public interface PolyContract {}\n",
    ),
    (
        "Payload.kt",
        r#"package sample
import com.fasterxml.jackson.annotation.JsonCreator
import com.fasterxml.jackson.annotation.JsonTypeName
import com.fasterxml.jackson.annotation.JsonProperty
import com.fasterxml.jackson.databind.annotation.JsonDeserialize
enum class Category { BASIC, PREMIUM }
@JsonTypeName("payload")
data class Payload(
    val id: String,
    @JsonProperty("category_name") val category: Category = Category.BASIC,
    val enabled: Boolean = true,
    val externalCode: String = "default",
    val note: String? = null
)
@JsonTypeName("poly")
data class PolyPayload(val id: String, val category: Category = Category.BASIC) : PolyContract
interface HasId { val id: String }
@JsonTypeName("deferred")
data class DeferredPayload(
    override val id: String,
    val items: List<String> = DefaultsTrace.items(),
    val options: Map<String, String> = emptyMap(),
    val note: String? = DefaultsTrace.note(),
    val full: String = id + DefaultsTrace.suffix(),
    val count: Int = items.size,
    val summary: String = id + items + options + mapOf("x" to 1, "y" to 2)
) : HasId {
    val kind: Category get() = Category.BASIC
}
@JsonTypeName("recursive")
data class RecursivePayload(val id: String, val child: RecursivePayload? = null, val amount: Int? = null)
@JsonTypeName("initialized")
data class InitializedPayload(val id: String = DefaultsTrace.note()) {
    val derived: String = DefaultsTrace.derived(id)
    init { DefaultsTrace.constructed() }
}
"#,
    ),
    (
        "DefaultsTrace.java",
        r#"package sample;
public class DefaultsTrace {
    public static String trace = "";
    public static java.util.List<String> items() { trace += "items,"; return new java.util.ArrayList<>(java.util.List.of("seed")); }
    public static String note() { trace += "note,"; return "note"; }
    public static Object suffix() { trace += "suffix,"; return new Object() { public String toString() { return "!"; } }; }
    public static String derived(String id) { trace += "derived,"; return id + "?"; }
    public static void constructed() { trace += "init,"; }
}
"#,
    ),
    (
        "Runner.java",
        r#"package sample;
import java.util.Map;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.module.kotlin.KotlinModule;
public class Runner {
    private static String describe(Payload value) {
        return value.getId() + "/" + value.getCategory() + "/" + value.getEnabled() + "/" + value.getNote();
    }
    private static boolean rejects(ObjectMapper mapper, String json) {
        try { mapper.readValue(json, Payload.class); return false; }
        catch (Exception expected) { return true; }
    }
    public static void main(String[] args) throws Exception {
        Reader reader = new Reader();
        System.out.println(reader.lastPart("a.b") + "/" + reader.lastPart("a.") + "/" + reader.lastPart("plain") + "/" + reader.literal() + "/" + reader.shadow("local"));
        Subject subject = new Subject(new Identifier("kept"));
        Root<Identifier> wide = subject;
        Provider<Identifier> narrow = subject;
        System.out.println((((Identifier) wide.getAggregate()) == narrow.getAggregate()) + "/" + narrow.getAggregate().getText() + "/" + Counter.count);
        ObjectMapper mapper = new ObjectMapper().registerModule(new KotlinModule.Builder().build());
        mapper.registerSubtypes(new com.fasterxml.jackson.databind.jsontype.NamedType(PolyPayload.class, "poly"));
        System.out.println(describe(mapper.readValue("{\"id\":\"missing\"}", Payload.class)));
        System.out.println(describe(mapper.readValue("{\"id\":\"given\",\"category_name\":\"PREMIUM\",\"enabled\":false,\"note\":\"memo\"}", Payload.class)));
        System.out.println(describe(mapper.convertValue(Map.of("id", "map"), Payload.class)));
        System.out.println("nulls=" + rejects(mapper, "{\"id\":\"x\",\"category_name\":null}") + "/" + rejects(mapper, "{\"id\":null}"));
        System.out.println("strict=" + rejects(mapper, "{\"id\":\"x\",\"unrelated\":\"must reject\"}"));
        System.out.println(mapper.valueToTree(new Payload("direct", Category.PREMIUM, false, "direct", null)).path("category_name").asText());
        ObjectMapper snake = mapper.copy().setPropertyNamingStrategy(com.fasterxml.jackson.databind.PropertyNamingStrategies.SNAKE_CASE);
        System.out.println("snake=" + snake.readValue("{\"id\":\"s\",\"external_code\":\"kept\"}", Payload.class).getExternalCode());
        System.out.println("poly=" + ((PolyPayload) mapper.readValue("{\"kind\":\"poly\",\"id\":\"p\",\"unrelated\":\"ignored\"}", PolyContract.class)).getCategory());
        DefaultsTrace.trace = "";
        DeferredPayload omitted = mapper.readValue("{\"id\":\"a\"}", DeferredPayload.class);
        System.out.println("deferred=" + omitted.getItems() + "/" + omitted.getOptions() + "/" + omitted.getNote() + "/" + omitted.getFull() + "/" + omitted.getCount() + "/" + DefaultsTrace.trace);
        DefaultsTrace.trace = "";
        DeferredPayload supplied = mapper.readValue("{\"id\":\"b\",\"items\":[\"x\",\"y\"],\"options\":{\"k\":\"v\"},\"note\":null,\"full\":\"given\",\"count\":0}", DeferredPayload.class);
        System.out.println("supplied=" + supplied.getItems() + "/" + supplied.getOptions() + "/" + supplied.getNote() + "/" + supplied.getFull() + "/" + supplied.getCount() + "/" + DefaultsTrace.trace);
        System.out.println("summaries=" + omitted.getSummary() + "/" + supplied.getSummary());
        DefaultsTrace.trace = "";
        try { mapper.readValue("{}", DeferredPayload.class); System.out.println("missing=false"); }
        catch (Exception expected) { System.out.println("missing=true/" + DefaultsTrace.trace); }
        DefaultsTrace.trace = "";
        try { mapper.readValue("{\"id\":\"x\",\"items\":null}", DeferredPayload.class); System.out.println("null-list=false"); }
        catch (Exception expected) { System.out.println("null-list=true/" + DefaultsTrace.trace); }
        DeferredPayload another = mapper.readValue("{\"id\":\"c\"}", DeferredPayload.class);
        omitted.getItems().add("changed");
        System.out.println("fresh=" + another.getItems() + "/" + (omitted.getItems() != another.getItems()) + "/" + another.getKind());
        RecursivePayload tree = mapper.readerFor(RecursivePayload.class).readValue("{\"id\":\"root\",\"child\":{\"id\":\"leaf\",\"amount\":7}}");
        System.out.println("recursive=" + tree.getId() + "/" + tree.getChild().getId() + "/" + tree.getChild().getChild() + "/" + tree.getAmount() + "/" + tree.getChild().getAmount());
        DefaultsTrace.trace = "";
        InitializedPayload initialized = mapper.readValue("{}", InitializedPayload.class);
        System.out.println("initialized=" + initialized.getDerived() + "/" + DefaultsTrace.trace);
        DefaultsTrace.trace = "";
        System.out.println("constructor=" + DirectCalls.make() + "/" + DefaultsTrace.trace);
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
            "jetbrains",
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
        "Reader",
        "Subject",
        "Payload",
        "PolyPayload",
        "DeferredPayload",
        "RecursivePayload",
        "InitializedPayload",
    ] {
        assert!(
            root.join(format!("{name}.java")).is_file(),
            "{name} remains Kotlin:\n{}\n{diagnostics}",
            fs::read_to_string(root.join(format!("{name}.kt")))
                .or_else(|_| fs::read_to_string(root.join("Payload.kt")))
                .unwrap_or_default()
        );
    }
    let reader = fs::read_to_string(root.join("Reader.java")).unwrap();
    assert!(
        reader.contains("StringsKt.split") && reader.contains("CollectionsKt.last"),
        "{reader}"
    );
    assert!(root.join("Bridge.kt").exists(), "provider must stay Kotlin");
    let subject = fs::read_to_string(root.join("Subject.java")).unwrap();
    assert!(
        subject.contains("return this.getIdentifier();"),
        "{subject}"
    );
    let payload = fs::read_to_string(root.join("Payload.java")).unwrap();
    assert!(
        payload.contains("ValueInstantiators.Base") && payload.contains("Category.BASIC"),
        "{payload}"
    );
}

#[test]
fn migrates_constants_generic_owner_forwarders_and_scalar_json_defaults() {
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
    cfg.kotlinc_args.extend(["-jvm-target".into(), "17".into()]);
    cfg.javac_args.extend(["--release".into(), "17".into()]);
    // The vendored annotation API permits declaration annotations but not type
    // uses. Casts must compile against this version as well as modern jars.
    cfg.classpath
        .push(Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"));
    for name in [
        "NOTLIN_KOTLIN_STDLIB",
        "NOTLIN_KOTLIN_REFLECT",
        "NOTLIN_JACKSON_CORE",
        "NOTLIN_JACKSON_ANNOTATIONS",
        "NOTLIN_JACKSON_DATABIND",
        "NOTLIN_JACKSON_KOTLIN",
    ] {
        cfg.classpath.push(
            std::env::var_os(name)
                .unwrap_or_else(|| panic!("{name}"))
                .into(),
        );
    }
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
#[ignore = "requires JVM tools, Kotlin, Lombok, and Jackson jars in NOTLIN_* variables"]
fn runtime_preserves_split_edges_virtual_getters_and_json_presence() {
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
        "b//plain/./local\ntrue/kept/3\nmissing/BASIC/true/null\ngiven/PREMIUM/false/memo\nmap/BASIC/true/null\nnulls=true/true\nstrict=true\nPREMIUM\nsnake=kept\npoly=BASIC\ndeferred=[seed]/{}/note/a!/1/items,note,suffix,\nsupplied=[x, y]/{k=v}/null/given/0/\nsummaries=a[seed]{}{x=1, y=2}/b[x, y]{k=v}{x=1, y=2}\nmissing=true/\nnull-list=true/\nfresh=[seed]/true/BASIC\nrecursive=root/leaf/null/null/7\ninitialized=note?/note,derived,init,\nconstructor=direct!/items,note,suffix,"
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
