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
        "Bags.kt",
        r#"package sample
import com.fasterxml.jackson.databind.annotation.JsonDeserialize as Decode
interface TaggedBag : Bag { val kind: Category }
data class BagOne(@JvmField val values: Map<String, String> = BagDefaults.values()) : TaggedBag {
    override fun getValues(): Map<String, String> = values
    override val kind: Category get() = Category.BASIC
}
@Decode(using = BagDecoder::class)
data class BagTwo(@JvmField val values: Map<String, String> = BagDefaults.values()) : TaggedBag {
    override fun getValues(): Map<String, String> = values
    override val kind: Category get() = Category.PREMIUM
}
"#,
    ),
    (
        "Bag.java",
        "package sample; @com.fasterxml.jackson.databind.annotation.JsonDeserialize(using=BagDecoder.class) @com.fasterxml.jackson.databind.annotation.JsonSerialize(using=BagEncoder.class) public interface Bag { java.util.Map<String,String> getValues(); }\n",
    ),
    (
        "BagDefaults.java",
        "package sample; public class BagDefaults { public static int calls; public static java.util.Map<String,String> values() { calls++; return java.util.Map.of(\"seed\", \"base\"); } }\n",
    ),
    (
        "BagDecoder.java",
        r#"package sample;
import com.fasterxml.jackson.core.JsonParser;
import com.fasterxml.jackson.databind.*;
import com.fasterxml.jackson.databind.deser.ContextualDeserializer;
public class BagDecoder extends JsonDeserializer<Bag> implements ContextualDeserializer {
    private final Class<? extends Bag> target;
    public BagDecoder() { this(Bag.class); }
    private BagDecoder(Class<? extends Bag> target) { this.target = target; }
    @Override public JsonDeserializer<?> createContextual(DeserializationContext context, BeanProperty property) {
        JavaType type = context.getContextualType();
        if (type == null && property != null) type = property.getType();
        return new BagDecoder(type == null ? Bag.class : type.getRawClass().asSubclass(Bag.class));
    }
    @Override public Bag deserialize(JsonParser parser, DeserializationContext context) throws java.io.IOException {
        JsonNode node = parser.getCodec().readTree(parser);
        Class<? extends Bag> actual = target.isInterface() ? ("two".equals(node.path("_kind").asText()) ? BagTwo.class : BagOne.class) : target;
        try {
            Bag result = actual.getConstructor().newInstance();
            var merged = new java.util.LinkedHashMap<>(result.getValues());
            JsonNode entries = node.get("entries");
            if (entries != null && !entries.isNull()) {
                var fields = entries.properties().iterator();
                while (fields.hasNext()) { var field = fields.next(); merged.put(field.getKey(), field.getValue().isNull() ? null : field.getValue().asText()); }
            }
            return actual.getConstructor(java.util.Map.class).newInstance(merged);
        } catch (ReflectiveOperationException error) { throw new java.io.IOException(error); }
    }
}
"#,
    ),
    (
        "BagEncoder.java",
        r#"package sample;
public class BagEncoder extends com.fasterxml.jackson.databind.JsonSerializer<Bag> {
    @Override public void serialize(Bag value, com.fasterxml.jackson.core.JsonGenerator writer, com.fasterxml.jackson.databind.SerializerProvider provider) throws java.io.IOException {
        writer.writeStartObject();
        writer.writeStringField("_kind", value instanceof BagTwo ? "two" : "one");
        writer.writeObjectField("entries", value.getValues());
        writer.writeEndObject();
    }
}
"#,
    ),
    (
        "Facets.kt",
        r#"package sample
@Deprecated("retain the property owner")
interface Facet<T> {
    val value: T
    var label: String
    fun describe(prefix: String): String = prefix + label
    suspend fun keepRoot() {}
}
interface FacetMarker<T> : Facet<T> {
    companion object { const val PREFIX: String = "tag:" }
}
class FacetImpl(override val value: String, override var label: String) : FacetMarker<String> {
    suspend fun retained() {}
}
@Deprecated("retain the default getter")
interface DefaultFacet { val label: String get() = "default"; suspend fun keepRoot() {} }
interface DefaultFacetMarker : DefaultFacet
class DefaultFacetImpl : DefaultFacetMarker { suspend fun retained() {} }
"#,
    ),
    (
        "EnumDefaultRoot.java",
        "package sample; public interface EnumDefaultRoot { EnumCode getCode(); }\n",
    ),
    (
        "EnumDefaults.kt",
        r#"package sample
enum class EnumCode { FIRST, SECOND }
interface FirstDefault : EnumDefaultRoot {
    override fun getCode(): EnumCode = EnumCode.FIRST
    fun getRank(): Int = 1
    fun getLabel(): String? = null
    fun valuesByName(): Map<String, String> = mapOf()
}
interface SecondDefault : EnumDefaultRoot {
    override fun getCode(): EnumCode = EnumCode.SECOND
    fun getRank(): Int = 2
    fun getLabel(): String? = "second"
    fun valuesByName(): Map<String, String> = mapOf()
}
interface FirstHop : FirstDefault
interface SecondHop : SecondDefault
enum class FirstChoice : FirstHop, SecondHop {
    VALUE;
    suspend fun retained() {}
}
enum class SecondChoice : SecondHop, FirstHop {
    VALUE;
    suspend fun retained() {}
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
        BagDefaults.calls = 0;
        BagOne emptyBag = mapper.readerFor(BagOne.class).readValue("{}");
        BagOne providedBag = mapper.readValue("{\"entries\":{\"seed\":\"new\",\"given\":\"v\"}}", BagOne.class);
        BagTwo nullBag = mapper.readValue("{\"entries\":null}", BagTwo.class);
        Bag polyBag = mapper.readValue("{\"_kind\":\"two\",\"entries\":{\"z\":\"9\"}}", Bag.class);
        System.out.println("bags=" + emptyBag.getValues().get("seed") + "/" + providedBag.getValues().get("seed") + "/" + nullBag.getValues().get("seed") + "/" + ((TaggedBag) polyBag).getKind() + "/" + BagDefaults.calls);
        System.out.println("bag-serialized=" + mapper.valueToTree(providedBag).path("entries").path("seed").asText());
        emptyBag.getValues().put("changed", "yes");
        BagOne anotherBag = mapper.readValue("{}", BagOne.class);
        System.out.println("bag-fresh=" + anotherBag.getValues().get("seed") + "/" + anotherBag.getValues().containsKey("changed") + "/" + BagDefaults.calls);
        FacetImpl facet = new FacetImpl("v", "a");
        FacetMarker<String> facetView = facet;
        facetView.setLabel("b");
        facetView.keepRoot(null);
        System.out.println("facet=" + facetView.getValue() + "/" + facetView.getLabel() + "/" + facetView.describe("p:") + "/" + FacetMarker.PREFIX);
        System.out.println("default-facet=" + new DefaultFacetImpl().getLabel());
        EnumDefaultRoot firstChoice = FirstChoice.VALUE;
        EnumDefaultRoot secondChoice = SecondChoice.VALUE;
        System.out.println("enum-defaults=" + firstChoice.getCode() + "/" + secondChoice.getCode());
        System.out.println("enum-types=" + FirstChoice.VALUE.getRank() + "/" + SecondChoice.VALUE.getRank() + "/" + FirstChoice.VALUE.getLabel() + "/" + SecondChoice.VALUE.getLabel() + "/" + FirstChoice.VALUE.valuesByName().size());
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
    configure_fixture_build(root, None);
}

fn configure_fixture_build(root: &Path, jvm_default: Option<&str>) {
    if jvm_default == Some("all") {
        // Kotlin's no-compatibility mode already fails these implicit enum
        // defaults before translation (ICCE). Use valid explicit selections in
        // that mode; default/disable exercise the implicit-default repair.
        let path = root.join("EnumDefaults.kt");
        let mut source = fs::read_to_string(&path).unwrap();
        for (name, provider) in [("FirstChoice", "FirstHop"), ("SecondChoice", "SecondHop")] {
            let start = source.find(&format!("enum class {name}")).unwrap();
            let insertion = start + source[start..].find("VALUE;").unwrap() + "VALUE;".len();
            let overrides = ["getCode", "getRank", "getLabel", "valuesByName"]
                .into_iter()
                .map(|method| {
                    format!("\n    override fun {method}() = super<{provider}>.{method}()")
                })
                .collect::<String>();
            source.insert_str(insertion, &overrides);
        }
        fs::write(path, source).unwrap();
    }
    let setting = match jvm_default {
        Some("disable") => "kotlin { compilerOptions { jvmDefault = JvmDefaultMode.DISABLE } }",
        Some("all") => {
            "kotlin { compilerOptions { jvmDefault = JvmDefaultMode.NO_COMPATIBILITY } }"
        }
        _ => "",
    };
    fs::write(
        root.join("build.gradle.kts"),
        format!("plugins {{ kotlin(\"jvm\") version \"2.3.21\" }}\n{setting}\n"),
    )
    .unwrap();
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
    assert_migrated_with_mode(root, output, true);
}

fn assert_migrated_with_mode(root: &Path, output: &std::process::Output, modern: bool) {
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
        "BagOne",
        "BagTwo",
    ] {
        assert!(
            root.join(format!("{name}.java")).is_file(),
            "{name} remains Kotlin:\n{}\n{diagnostics}",
            fs::read_to_string(root.join(format!("{name}.kt")))
                .or_else(|_| fs::read_to_string(root.join("Payload.kt")))
                .unwrap_or_default()
        );
    }
    assert_eq!(
        root.join("FacetMarker.java").exists(),
        modern,
        "the facade requires a known modern JVM default-method mode: {diagnostics}"
    );
    let reader = fs::read_to_string(root.join("Reader.java")).unwrap();
    assert!(
        reader.contains("StringsKt.split") && reader.contains("CollectionsKt.last"),
        "{reader}"
    );
    assert!(root.join("Bridge.kt").exists(), "provider must stay Kotlin");
    assert!(
        root.join("Facets.kt").exists(),
        "facet implementations stay Kotlin"
    );
    assert!(
        !root.join("DefaultFacetMarker.java").exists(),
        "an inherited getter body still requires the existing bridge proof"
    );
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
    for name in ["BagOne", "BagTwo"] {
        let java = fs::read_to_string(root.join(format!("{name}.java"))).unwrap();
        assert!(
            !java.contains("__notlinDefaults") && !java.contains("JsonCreator"),
            "custom decoder must own construction: {java}"
        );
    }
    let enum_source = fs::read_to_string(root.join("EnumDefaults.kt")).unwrap();
    for provider in ["FirstHop", "SecondHop"] {
        assert!(
            enum_source.contains(&format!("super<sample.{provider}>.getCode()"))
                || enum_source.contains(&format!("super<{provider}>.getCode()")),
            "retained enum must select its original default: {enum_source}"
        );
    }
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
    compare_runtime(None);
}

#[test]
#[ignore = "requires JVM tools, Kotlin, Lombok, and Jackson jars in NOTLIN_* variables"]
fn runtime_preserves_abstract_property_facades_in_both_jvm_default_modes() {
    for mode in ["disable", "all"] {
        compare_runtime(Some(mode));
    }
}

fn compare_runtime(jvm_default: Option<&str>) {
    eprintln!("native property frontier JVM mode: {jvm_default:?}");
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    configure_fixture_build(&original, jvm_default);
    let configured = |root: &Path, output: PathBuf, lombok: bool| {
        let mut cfg = config(root, output, lombok);
        if let Some(mode) = jvm_default {
            cfg.kotlinc_args
                .push(format!("-Xjvm-default={mode}").into());
        }
        cfg
    };
    let before =
        validate_jvm_sources(&configured(&original, scratch.0.join("before"), false)).unwrap();
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "b//plain/./local\ntrue/kept/3\nmissing/BASIC/true/null\ngiven/PREMIUM/false/memo\nmap/BASIC/true/null\nnulls=true/true\nstrict=true\nPREMIUM\nsnake=kept\npoly=BASIC\ndeferred=[seed]/{}/note/a!/1/items,note,suffix,\nsupplied=[x, y]/{k=v}/null/given/0/\nsummaries=a[seed]{}{x=1, y=2}/b[x, y]{k=v}{x=1, y=2}\nmissing=true/\nnull-list=true/\nfresh=[seed]/true/BASIC\nrecursive=root/leaf/null/null/7\ninitialized=note?/note,derived,init,\nconstructor=direct!/items,note,suffix,\nbags=base/new/base/PREMIUM/4\nbag-serialized=new\nbag-fresh=base/false/5\nfacet=v/b/p:b/tag:\ndefault-facet=default\nenum-defaults=FIRST/SECOND\nenum-types=1/2/null/second/0"
    );
    for lombok in [false, true] {
        let root = scratch.0.join(format!("migrated-{lombok}"));
        populate(&root);
        configure_fixture_build(&root, jvm_default);
        assert_migrated_with_mode(
            &root,
            &translate(&root, lombok),
            jvm_default != Some("disable"),
        );
        let after = validate_jvm_sources(&configured(
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
