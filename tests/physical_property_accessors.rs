use notlin::workspace::{AccessorName, SourceIndex};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn scratch(label: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "physical-accessors-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
    assert!(!root.exists());
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn index_distinguishes_each_accessor_and_ignores_bodies_and_comments() {
    let root = scratch("index");
    fs::write(
        root.join("Model.kt"),
        r#"package sample
import kotlin.jvm.JvmName as Physical
class Box(@get:Physical("fetchKey") @set:Physical(name = "assignKey") var key: String) {
    // @get:JvmName("wrong")
    val ordinary: String = "JvmName"
    @set:Physical("assignBody") var body: String = "body"
        @Physical("fetchBody") get() = field
        set(value) { field = value }
    @get:Physical(UNKNOWN) val unsupported: String = "unknown"
    @get:Physical("read$value") val interpolated: String = "unknown"
}
"#,
    )
    .unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let owner = index.declarations_named("Box").next().unwrap();
    let access = |property| {
        &owner
            .members
            .iter()
            .find(|member| member.name == property)
            .unwrap()
            .accessors
    };
    assert_eq!(access("key").getter, AccessorName::Named("fetchKey".into()));
    assert_eq!(
        access("key").setter,
        AccessorName::Named("assignKey".into())
    );
    assert_eq!(
        access("body").getter,
        AccessorName::Named("fetchBody".into())
    );
    assert_eq!(
        access("body").setter,
        AccessorName::Named("assignBody".into())
    );
    assert!(access("ordinary").is_default());
    assert_eq!(access("unsupported").getter, AccessorName::Unknown);
    assert_eq!(access("interpolated").getter, AccessorName::Unknown);
}

#[test]
fn an_annotation_in_the_default_package_can_shadow_the_builtin_too() {
    let root = scratch("default-package");
    fs::write(
        root.join("Model.kt"),
        "class Box(@get:JvmName(\"readTitle\") val title: String)\n",
    )
    .unwrap();
    fs::write(
        root.join("JvmName.kt"),
        "annotation class JvmName(val value: String)\n",
    )
    .unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let owner = index.declarations_named("Box").next().unwrap();
    assert!(
        index
            .property_accessor_names(owner, &owner.members[0])
            .is_default()
    );
}

#[test]
fn ordinary_imports_aliases_wildcards_and_default_imports_bind_the_same_annotation() {
    for (imports, name) in [
        ("", "JvmName"),
        ("import kotlin.jvm.JvmName\n", "JvmName"),
        ("import kotlin.jvm.*\n", "JvmName"),
        ("import kotlin.jvm.JvmName as Physical\n", "Physical"),
        ("", "kotlin.jvm.JvmName"),
    ] {
        let root = scratch("import-binding");
        fs::write(
            root.join("Model.kt"),
            format!(
                "package sample\n{imports}class Box(@get:{name}(\"readTitle\") val title: String)\n"
            ),
        )
        .unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        let owner = index.declarations_named("Box").next().unwrap();
        assert_eq!(
            index
                .property_accessor_names(owner, &owner.members[0])
                .getter("title")
                .as_deref(),
            Some("readTitle"),
            "{imports} {name}"
        );
    }
}

const REMOTE: &str = r#"package sample
import kotlin.jvm.JvmName as Physical
@Suppress("INAPPLICABLE_JVM_NAME")
open class Remote(@get:Physical("fetchKey") @set:Physical("assignKey") open var key: String) {
    suspend fun keep() {}
}
class RemoteChild(key: String): Remote(key) {
    fun own(): String = key
}
"#;

#[test]
fn annotation_identity_is_scoped_and_overlay_changes_do_not_leave_stale_names() {
    use notlin::workspace::SourceOverlay;
    let root = scratch("binding");
    let model = root.join("Model.kt");
    let shadow = root.join("JvmName.kt");
    fs::write(
        &model,
        "package sample\nclass Box(@get:JvmName(\"readTitle\") val title: String)\n",
    )
    .unwrap();
    fs::write(
        &shadow,
        "package sample\nannotation class JvmName(val value: String)\n",
    )
    .unwrap();
    let names = |index: &SourceIndex| {
        let owner = index.declarations_named("Box").next().unwrap();
        let member = owner
            .members
            .iter()
            .find(|member| member.name == "title")
            .unwrap();
        index.property_accessor_names(owner, member)
    };
    let index = SourceIndex::discover(&root).unwrap();
    assert!(
        names(&index).is_default(),
        "a source annotation with the same name is not Kotlin's annotation"
    );
    let overlay = index
        .with_overlays(&[SourceOverlay::Delete { path: shadow }])
        .unwrap();
    assert_eq!(
        names(&overlay).getter("title").as_deref(),
        Some("readTitle")
    );
    assert!(
        names(&index).is_default(),
        "the original snapshot remains shadowed"
    );
    let qualified = index.with_overlays(&[SourceOverlay::Replace {
        path: model,
        source: "package sample\nclass Box(@get:kotlin.jvm.JvmName(\"readTitle\") val title: String)\n".into(),
        language: notlin::workspace::SourceLanguage::Kotlin,
    }]).unwrap();
    assert_eq!(
        names(&qualified).getter("title").as_deref(),
        Some("readTitle")
    );
}
const QUERIES: &str = r#"package sample
class Queries {
    fun read(remote: Remote): String = remote.key
    fun write(remote: Remote, key: String) { remote.key = key }
    fun maybe(remote: Remote?): String? = remote?.key
    fun ordinary(other: Ordinary): String = other.key
    fun splitValue(other: Split): String = other.value.toString()
}
class Ordinary(val key: String)
"#;
const MODEL: &str = r#"package sample
import kotlin.jvm.JvmName as Physical
class Box(@get:Physical("readTitle") val title: String) {
    fun describe(): String = title
}
data class DataBox(@get:Physical("dataTitle") val title: String)
enum class Phase(@get:Physical("phaseTitle") val title: String) { READY("ready") }
class Mutable {
    @get:Physical("readTitle") @set:Physical("assignTitle")
    var title: String = "body"
    fun own(): String = title
}
class Split(@get:Physical("storedValue") val value: CharSequence) {
    fun getValue(): String = value.toString() + "/lookup"
}
"#;
const RETAINED: &str = r#"package sample
class RetainedUse {
    fun read(box: Box): String = box.title
    fun write(box: Mutable, value: String) { box.title = value }
    fun splitValue(other: Split): String = other.value.toString() + "|" + other.getValue()
    suspend fun keep() {}
}
"#;
const RUNNER: &str = r#"package sample;
public class Runner {
    public static void main(String[] args) {
        Remote remote = new RemoteChild("old");
        Queries queries = new Queries();
        System.out.println(queries.read(remote));
        queries.write(remote, "new");
        System.out.println(queries.read(remote));
        System.out.println(queries.maybe(null));
        System.out.println(queries.maybe(remote));
        System.out.println(((RemoteChild) remote).own());
        System.out.println(queries.ordinary(new Ordinary("ordinary")));
        Box box = new Box("box");
        RetainedUse retained = new RetainedUse();
        System.out.println(box.readTitle() + ":" + box.describe() + ":" + retained.read(box));
        System.out.println(new DataBox("data").dataTitle());
        System.out.println(Phase.READY.phaseTitle());
        Mutable mutable = new Mutable();
        retained.write(mutable, "updated");
        System.out.println(mutable.readTitle() + ":" + mutable.own());
        Split split = new Split("stored");
        System.out.println(queries.splitValue(split) + ":" + split.getValue() + ":" + retained.splitValue(split));
        System.out.println(queries.splitValue(new Split(new Odd())));
    }
}
"#;

fn write_case(root: &Path) {
    for (name, source) in [
        ("Remote.kt", REMOTE),
        ("Queries.kt", QUERIES),
        ("Model.kt", MODEL),
        ("Retained.kt", RETAINED),
        ("Runner.java", RUNNER),
        (
            "Odd.java",
            r#"package sample;
public final class Odd implements CharSequence {
    public int length() { return 0; }
    public char charAt(int index) { throw new IndexOutOfBoundsException(); }
    public CharSequence subSequence(int start, int end) { return this; }
    public String toString() { return null; }
}
"#,
        ),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
}

fn translate(root: &Path, lombok: bool) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command.current_dir(root).args([
        "--root",
        ".",
        "--in-place",
        "--jvm-facts",
        "off",
        "--annotations",
        "none",
    ]);
    if lombok {
        command.arg("--lombok");
    }
    let output = command.arg(".").output().unwrap();
    fs::write(root.join("translation.stdout"), &output.stdout).unwrap();
    fs::write(root.join("translation.stderr"), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{root:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in [
        "Box",
        "DataBox",
        "Phase",
        "Mutable",
        "Queries",
        "RemoteChild",
    ] {
        assert!(
            root.join(format!("{name}.java")).is_file(),
            "missing {name} in {root:?}"
        );
    }
    let queries = fs::read_to_string(root.join("Queries.java")).unwrap();
    assert!(queries.contains(".fetchKey()"), "{queries}");
    assert!(queries.contains(".assignKey("), "{queries}");
    assert!(!queries.contains("remote.getKey()"), "{queries}");
    assert!(queries.contains("other.getKey()"), "{queries}");
    let retained = fs::read_to_string(root.join("Retained.kt")).unwrap();
    assert!(retained.contains("box.readTitle()"), "{retained}");
    assert!(retained.contains("box.assignTitle("), "{retained}");
    let remote = fs::read_to_string(root.join("Remote.kt")).unwrap();
    assert!(remote.contains("@get:Physical(\"fetchKey\")"), "{remote}");
}

#[test]
fn physical_names_are_shared_by_declarations_java_calls_and_retained_kotlin_calls() {
    for (mode, lombok) in [("plain", false), ("lombok", true)] {
        let root = scratch(mode);
        write_case(&root);
        translate(&root, lombok);
        for (owner, getter) in [
            ("Box", "readTitle"),
            ("DataBox", "dataTitle"),
            ("Phase", "phaseTitle"),
            ("Mutable", "readTitle"),
        ] {
            let java = fs::read_to_string(root.join(format!("{owner}.java"))).unwrap();
            assert!(java.contains(&format!("{getter}()")), "{java}");
            assert!(!java.contains("JvmName"), "{java}");
            assert!(!java.contains("getTitle()"), "{java}");
        }
    }
}

fn run_native(root: &Path, lombok: bool, main: &str, jackson: bool) -> String {
    use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").unwrap(),
        std::env::var_os("NOTLIN_JAVAC").unwrap(),
        std::env::var_os("NOTLIN_JAVA").unwrap(),
        root.join("classes"),
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config.javac_args.extend([
        "--release".into(),
        "17".into(),
        if lombok { "-proc:full" } else { "-proc:none" }.into(),
    ]);
    config
        .classpath
        .push(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap().into());
    config
        .classpath
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"));
    if lombok {
        config
            .classpath
            .push(std::env::var_os("NOTLIN_LOMBOK").unwrap().into());
    }
    if jackson {
        for key in [
            "NOTLIN_KOTLIN_REFLECT",
            "NOTLIN_JACKSON_CORE",
            "NOTLIN_JACKSON_ANNOTATIONS",
            "NOTLIN_JACKSON_DATABIND",
            "NOTLIN_JACKSON_KOTLIN",
        ] {
            config.classpath.push(std::env::var_os(key).unwrap().into());
        }
    }
    for entry in fs::read_dir(root).unwrap() {
        let file = entry.unwrap().path();
        match file.extension().and_then(|extension| extension.to_str()) {
            Some("java") => config.java_sources.push(file),
            Some("kt") => config.kotlin_sources.push(file),
            _ => (),
        }
    }
    config.run_main_class = Some(main.into());
    let result =
        validate_jvm_sources(&config).unwrap_or_else(|error| panic!("{root:?}: {error:?}"));
    fs::write(root.join("native-validation.log"), format!("{result:#?}")).unwrap();
    result.commands.last().unwrap().stdout.replace("\r\n", "\n")
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib and Lombok in NOTLIN_* variables"]
fn real_toolchain_preserves_renamed_property_reads_writes_and_virtual_inheritance() {
    for (mode, translated, lombok) in [
        ("original", false, false),
        ("plain", true, false),
        ("lombok", true, true),
    ] {
        let root = scratch(mode);
        write_case(&root);
        if translated {
            translate(&root, lombok);
        }
        let output = run_native(&root, lombok, "sample.Runner", false);
        assert_eq!(
            output,
            "old\nnew\nnull\nnew\nnew\nordinary\nbox:box:box\ndata\nready\nupdated:updated\nstored:stored/lookup:stored|stored/lookup\nnull\n",
            "{mode}"
        );
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin, Jackson and Lombok in NOTLIN_* variables"]
fn real_jackson_preserves_empty_bean_metadata_lookup_views_and_physical_boolean_names() {
    for (mode, translated, lombok) in [
        ("json-original", false, false),
        ("json-plain", true, false),
        ("json-lombok", true, true),
    ] {
        let root = scratch(mode);
        write_case(&root);
        fs::write(root.join("Imported.kt"), "package sample\nimport kotlin.jvm.JvmName\nclass Imported(@get:JvmName(\"readTitle\") val title: String)\n").unwrap();
        fs::write(
            root.join("Flags.kt"),
            r#"package sample
import kotlin.jvm.JvmName
import com.fasterxml.jackson.databind.annotation.JsonSerialize
@JsonSerialize
class Flag(@get:JvmName("isPhysical") val isLogical: Boolean)
@JsonSerialize
class NamedFlag(@get:JvmName("getPhysical") val isLogical: Boolean)
"#,
        )
        .unwrap();
        fs::write(root.join("JsonProbe.java"), r#"package sample;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.module.kotlin.KotlinModule;
public class JsonProbe {
 public static void main(String[] args) throws Exception {
  for(ObjectMapper mapper : new ObjectMapper[]{new ObjectMapper(),new ObjectMapper().registerModule(new KotlinModule.Builder().build())}) {
   for(Object value : new Object[]{new Box("box"),new DataBox("data"),new Mutable(),new Imported("imported"),new Split("stored")}) {
    java.beans.BeanInfo info = java.beans.Introspector.getBeanInfo(value.getClass());
    System.out.println(value.getClass().getSimpleName()+":"+mapper.writeValueAsString(value));
    System.out.println(info.getBeanDescriptor().getShortDescription()+":"+info.getDefaultPropertyIndex()+":"+info.getDefaultEventIndex()+":"+info.getPropertyDescriptors().length);
   }
  }
  ObjectMapper kotlinMapper = new ObjectMapper().registerModule(new KotlinModule.Builder().build());
  System.out.println(kotlinMapper.writeValueAsString(new Flag(true)));
  System.out.println(kotlinMapper.writeValueAsString(new NamedFlag(true)));
 }
}

"#).unwrap();
        if translated {
            translate(&root, lombok);
        }
        let output = run_native(&root, lombok, "sample.JsonProbe", true);
        let common = "Box:{}\nBox:-1:-1:1\nDataBox:{}\nDataBox:-1:-1:1\nMutable:{}\nMutable:-1:-1:1\nImported:{}\nImported:-1:-1:1\nSplit:{\"value\":\"stored/lookup\"}\nSplit:-1:-1:2\n";
        assert_eq!(
            output,
            format!("{common}{common}{{\"isPhysical\":true}}\n{{\"physical\":true}}\n"),
            "{mode}"
        );
    }
}

fn generic_forwarding_fixture(label: &str) -> PathBuf {
    let root = scratch(label);
    for (name, source) in [
        ("Key.java", "package sample; public interface Key {}\n"),
        (
            "Id.java",
            "package sample; public record Id(String text) implements Key {}\n",
        ),
        (
            "Lookup.java",
            "package sample; public interface Lookup<S extends Key> { S getId(); }\n",
        ),
        (
            "Node.kt",
            "package sample\ninterface Node<T:Lookup<S>, S:Key>:Lookup<S> { val data:T; override fun getId():S=data.id }\nclass Concrete(override val data:Lookup<Id>):Node<Lookup<Id>,Id>\n",
        ),
        (
            "Distraction.kt",
            "package sample\nclass Distraction(@get:kotlin.jvm.JvmName(\"fetchId\") val id:String)\n",
        ),
        (
            "Runner.java",
            "package sample; public class Runner { public static void main(String[] args) { Concrete node=new Concrete(() -> new Id(\"stored\")); System.out.println(node.getId().text()+\"/\"+new Distraction(\"renamed\").fetchId()); }}\n",
        ),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
    root
}

fn translate_generic_forwarding(root: &Path, lombok: bool) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command.current_dir(root).args([
        "--root",
        ".",
        "--in-place",
        "--jvm-facts",
        "off",
        "--annotations",
        "none",
    ]);
    if lombok {
        command.arg("--lombok");
    }
    let output = command.arg(".").output().unwrap();
    fs::write(root.join("translation.stdout"), &output.stdout).unwrap();
    fs::write(root.join("translation.stderr"), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        root.join("Node.java").exists(),
        "generic forwarding was tainted by an unrelated renamed property: {root:?}"
    );
    let node = fs::read_to_string(root.join("Node.java")).unwrap();
    assert!(node.contains("getData().getId()"), "{node}");
    assert!(!node.contains("fetchId"), "{node}");
}

#[test]
fn unrelated_accessor_names_do_not_taint_generic_receiver_inference() {
    for (label, lombok) in [("generic-plain", false), ("generic-lombok", true)] {
        translate_generic_forwarding(&generic_forwarding_fixture(label), lombok);
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib and Lombok in NOTLIN_* variables"]
fn real_toolchain_preserves_generic_forwarding_beside_an_unrelated_named_getter() {
    for (label, converted, lombok) in [
        ("generic-original", false, false),
        ("generic-native-plain", true, false),
        ("generic-native-lombok", true, true),
    ] {
        let root = generic_forwarding_fixture(label);
        if converted {
            translate_generic_forwarding(&root, lombok);
        }
        assert_eq!(
            run_native(&root, lombok, "sample.Runner", false),
            "stored/renamed\n",
            "{label}"
        );
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib and Lombok in NOTLIN_* variables"]
fn generic_bounds_and_numeric_updates_preserve_accessor_effects() {
    for (mode, converted, lombok) in [
        ("bounds-effects-original", false, false),
        ("bounds-effects-plain", true, false),
        ("bounds-effects-lombok", true, true),
    ] {
        let root = scratch(mode);
        let mut runner = None;
        for (name, source) in [
            (
                "Contract.kt",
                r#"@file:Suppress("INAPPLICABLE_JVM_NAME")
package sample
interface Contract {
 @get:kotlin.jvm.JvmName("readText") val text:String
}
class Stored(@get:kotlin.jvm.JvmName("readText") override val text:String):Contract {
 suspend fun retained() {}
}
"#,
            ),
            (
                "Readers.kt",
                r#"package sample
class Readers {
 fun <T:Contract> read(value:T):String=value.text
 fun <T> constrained(value:T):String where T:Contract=value.text
}
class RetainedReaders {
 fun <T:Contract> read(value:T):String=value.text
 fun <T> constrained(value:T):String where T:Contract=value.text
 fun <T:Contract> mapped(values:List<T>):String=values.map {it.text}.joinToString(",")
 suspend fun keep() {}
}
"#,
            ),
            (
                "Counter.kt",
                r#"package sample
open class BaseCode(val text:String)
interface Tag
class Code(text:String):BaseCode(text),Tag
class BoundBox<T>(val current:T) where T:Tag,T:BaseCode {fun label():String=current.text}
class Counter {
 var reads:Int=0
 var writes:Int=0
 private var stored:Int=0
 var value:Int
  get() {reads++;return stored}
  set(newValue) {writes++;stored=newValue}
 var tiny:Byte=127
 var lastRecorded:Int=0
 var delegated:Int=0
  set(incoming)=record(incoming)
 private fun record(incoming:Int) {lastRecorded=incoming}
 fun advance() {value++;++value;tiny++}
 fun locals():Int {var value=3;val before=value++;val after=++value;return before*100+after}
}
"#,
            ),
            (
                "Globals.kt",
                r#"package sample
var calls=0
fun bump() {calls++;++calls}
fun read():Int=calls
"#,
            ),
            (
                "Runner.java",
                r#"package sample;
public class Runner {
 public static void main(String[] args) {
  Stored stored=new Stored("proof");Readers readers=new Readers();RetainedReaders retained=new RetainedReaders();
  System.out.println(readers.read(stored)+"|"+readers.constrained(stored));
  System.out.println(retained.read(stored)+"|"+retained.constrained(stored)+"|"+retained.mapped(java.util.List.of(stored)));
  Counter counter=new Counter();counter.advance();counter.setDelegated(7);
  System.out.println(counter.getReads()+"|"+counter.getWrites()+"|"+counter.getValue()+"|"+counter.getTiny()+"|"+counter.locals()
   +"|"+new BoundBox<Code>(new Code("bound")).label()+"|"+counter.getLastRecorded()+"|"+counter.getDelegated());
  try {
   Class<?> globals;
   try {globals=Class.forName("sample.Globals");} catch(ClassNotFoundException original) {globals=Class.forName("sample.GlobalsKt");}
   globals.getMethod("bump").invoke(null);System.out.println(globals.getMethod("read").invoke(null));
  } catch(ReflectiveOperationException failure) {throw new AssertionError(failure);}
 }
}
"#,
            ),
        ] {
            if name == "Runner.java" {
                runner = Some(source);
            } else {
                fs::write(root.join(name), source).unwrap();
            }
        }
        if converted {
            let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
            command.current_dir(&root).args([
                "--in-place",
                "--jvm-facts",
                "off",
                "--annotations",
                "none",
            ]);
            if lombok {
                command.arg("--lombok");
            }
            let output = command.arg(".").output().unwrap();
            fs::write(root.join("translation.stdout"), &output.stdout).unwrap();
            fs::write(root.join("translation.stderr"), &output.stderr).unwrap();
            assert!(
                output.status.success(),
                "{root:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            for owner in ["Readers", "BoundBox", "Counter", "Globals"] {
                assert!(
                    root.join(format!("{owner}.java")).exists(),
                    "{owner} retained: {root:?}"
                );
            }
        }
        // This harness compares the two facade names; it is not a source
        // caller that the migration is being asked to repair.
        fs::write(root.join("Runner.java"), runner.unwrap()).unwrap();
        assert_eq!(
            run_native(&root, lombok, "sample.Runner", false),
            "proof|proof\nproof|proof|proof\n3|2|2|-128|305|bound|7|0\n2\n",
            "{mode}"
        );
    }
}
