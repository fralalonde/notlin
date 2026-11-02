use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
const MODEL: &str = r#"package surface
import com.fasterxml.jackson.annotation.JsonIgnore
import com.fasterxml.jackson.annotation.JsonTypeName
import com.fasterxml.jackson.annotation.JsonProperty
interface Hidden { @get:JsonIgnore val visible:String }
data class Visibility(@get:JsonIgnore(false) override val visible:String):Hidden
@JsonTypeName("flag") data class Flag(val isReady:Boolean)
@JsonTypeName("named-flag") data class NamedFlag(@get:JsonProperty("customReady") val isReady:Boolean)
data class UnmarkedFlag(val isReady:Boolean)
class Plain(val value:String, val count:Int)
class Carrier(val value:String, val count:Int) { @get:JsonIgnore val extra:Int=1 }
open class Parent(open val count:Int)
class Child(override val count:Int, val label:String):Parent(count)
data class Snapshot<T:Any>(@get:JsonIgnore(false) override val visible:String, val items:List<T>, val count:Int, @get:JsonIgnore(false) val enabled:Boolean):Hidden
data class MutableSnapshot<T:Any>(val items:List<T>, var count:Int)
data class Item(val label:String)
interface Items { val items:List<Item> }
data class Exact(override val items:List<Item>):Items
interface Shape
class Square(val label:String):Shape
interface Shapes { val shapes:List<Shape> }
data class Squares(override val shapes:List<Square>):Shapes
"#;
const PROBE: &str = r#"package surface;
import java.util.*;
import com.fasterxml.jackson.databind.ObjectMapper;
public class Probe {
    private static final ObjectMapper MAPPER = new ObjectMapper().registerModule(new com.fasterxml.jackson.module.kotlin.KotlinModule.Builder().build());
    private static List<Item> exact(Items value) { return value.getItems(); }
    public static void main(String[] args) throws Exception {
        List<Item> items = new ArrayList<>(List.of(new Item("one")));
        Snapshot<Item> original = new Snapshot<>("before", items, 1, true);
        Snapshot<Item> copied = original.copy("after", original.getItems(), 2, false);
        if (!copied.component1().equals("after") || copied.component2()!=items || copied.component3()!=2 || copied.component4()) throw new AssertionError("copy/components");
        if (!original.getVisible().equals("before") || original.getCount()!=1) throw new AssertionError("original mutated");
        if (copied.getItems()!=original.getItems()) throw new AssertionError("copy must remain shallow");
        items.add(new Item("two"));
        if (copied.getItems().size()!=2) throw new AssertionError("collection identity changed");
        MutableSnapshot<Item> mutable = new MutableSnapshot<>(items, 3);
        MutableSnapshot<Item> other = mutable.copy(mutable.getItems(), 4);
        other.setCount(5);
        if (mutable.component2()!=3 || other.getCount()!=5 || other.component1()!=items) throw new AssertionError("mutable copy");
        var json = MAPPER.valueToTree(copied);
        if (!json.has("visible") || !json.get("visible").asText().equals("after") || !json.has("enabled")) throw new AssertionError("getter annotations");
        if (!MAPPER.valueToTree(new Visibility("exposed")).has("visible")) throw new AssertionError("Lombok getter annotation");
        var flagJson = MAPPER.valueToTree(new Flag(true));
        if (!flagJson.has("isReady") || flagJson.has("ready") || !MAPPER.treeToValue(flagJson,Flag.class).isReady()) throw new AssertionError("boolean Kotlin name");
        var namedFlagJson = MAPPER.valueToTree(new NamedFlag(true));
        if (!namedFlagJson.has("customReady") || namedFlagJson.has("ready") || !MAPPER.treeToValue(namedFlagJson,NamedFlag.class).isReady()) throw new AssertionError("explicit boolean JSON name");
        Plain plain = MAPPER.treeToValue(MAPPER.valueToTree(new Plain("plain",7)),Plain.class);
        if (!plain.getValue().equals("plain") || plain.getCount()!=7) throw new AssertionError("generated property constructor names");
        Carrier carrier = MAPPER.treeToValue(MAPPER.valueToTree(new Carrier("carrier",8)),Carrier.class);
        if (!carrier.getValue().equals("carrier") || carrier.getCount()!=8 || carrier.getExtra()!=1) throw new AssertionError("explicit property constructor names");
        Child child = MAPPER.treeToValue(MAPPER.valueToTree(new Child(9,"child")),Child.class);
        if (child.getCount()!=9 || !child.getLabel().equals("child")) throw new AssertionError("delegating property constructor names");
        if (exact(new Exact(items))!=items) throw new AssertionError("exact collection return");
        Squares squares = new Squares(List.of(new Square("square")));
        Shapes shapes = squares;
        if (shapes.getShapes().get(0)!=squares.getShapes().get(0) || !squares.getShapes().get(0).getLabel().equals("square")) throw new AssertionError("covariant collection return");
        List<Square> narrow = squares.getShapes();
        Snapshot<Shape> wide = new Snapshot<>("wide", narrow, 0, false);
        if (wide.copy("narrow", narrow, 1, true).getItems().get(0)!=narrow.get(0)) throw new AssertionError("copy input variance");
        System.out.println("surface-ok");
    }
}
"#;

fn fixture() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "notlin-java-data-surface-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    assert!(!root.exists());
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Model.kt"), MODEL).unwrap();
    fs::write(root.join("Probe.java"), PROBE).unwrap();
    root
}
fn translate(root: &Path, lombok: bool) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command.current_dir(root).args([
        "--root",
        ".",
        "--in-place",
        "--annotations",
        "none",
        "--jvm-facts",
        "off",
    ]);
    if lombok {
        command.arg("--lombok");
    }
    let result = command.arg("Model.kt").output().unwrap();
    assert!(
        result.status.success(),
        "{root:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !root.join("Model.kt").exists(),
        "{root:?}: {}",
        fs::read_to_string(root.join("Model.kt")).unwrap_or_default()
    );
    let exact = fs::read_to_string(root.join("Items.java")).unwrap();
    assert!(exact.contains("List<Item> getItems()"), "{exact}");
    let narrowed = fs::read_to_string(root.join("Shapes.java")).unwrap();
    assert!(
        narrowed.contains("List<? extends Shape> getShapes()"),
        "{narrowed}"
    );
    let snapshot = fs::read_to_string(root.join("Snapshot.java")).unwrap();
    assert!(
        snapshot.contains("Snapshot<T> copy(") && snapshot.contains("component4()"),
        "{snapshot}"
    );
    for name in ["Plain", "Carrier", "Child"] {
        let java = fs::read_to_string(root.join(format!("{name}.java"))).unwrap();
        assert!(
            java.contains("@java.beans.ConstructorProperties("),
            "{java}"
        );
    }
    if lombok {
        let visibility = fs::read_to_string(root.join("Visibility.java")).unwrap();
        assert!(
            visibility.contains("@lombok.Getter(onMethod_ = {@JsonIgnore(false)})"),
            "{visibility}"
        );
        let flag = fs::read_to_string(root.join("Flag.java")).unwrap();
        assert!(flag.contains("JsonProperty(\"isReady\")"), "{flag}");
        let unmarked = fs::read_to_string(root.join("UnmarkedFlag.java")).unwrap();
        assert!(
            !unmarked.contains("JsonProperty("),
            "unmarked class acquired a Jackson annotation: {unmarked}"
        );
    }
}

#[test]
fn lombok_preserves_data_methods_getter_annotations_and_exact_collection_contracts() {
    let root = fixture();
    translate(&root, true);
    assert!(root.starts_with(std::env::temp_dir()));
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Jackson and Lombok in NOTLIN_* variables"]
fn java_callers_match_original_copy_mutation_json_and_collection_behavior() {
    for (translated, lombok) in [(false, false), (true, false), (true, true)] {
        let root = fixture();
        if translated {
            translate(&root, lombok);
        }
        let mut config = JvmValidationConfig::new(
            std::env::var_os("NOTLIN_KOTLINC").unwrap(),
            std::env::var_os("NOTLIN_JAVAC").unwrap(),
            std::env::var_os("NOTLIN_JAVA").unwrap(),
            root.join("classes"),
        );
        config
            .kotlinc_args
            .extend(["-jvm-target".into(), "17".into()]);
        config.javac_args.extend(["--release".into(), "17".into()]);
        if lombok {
            config.javac_args.extend([
                "-processorpath".into(),
                std::env::var_os("NOTLIN_LOMBOK").unwrap(),
                "-processor".into(),
                "lombok.launch.AnnotationProcessorHider$AnnotationProcessor".into(),
            ]);
            let mut plugin = std::ffi::OsString::from("-Xplugin=");
            plugin.push(std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").unwrap());
            config.kotlinc_args.push(plugin);
        } else {
            config.javac_args.push("-proc:none".into());
        }
        config.classpath.extend(
            [
                "NOTLIN_KOTLIN_STDLIB",
                "NOTLIN_KOTLIN_REFLECT",
                "NOTLIN_JACKSON_KOTLIN",
                "NOTLIN_LOMBOK",
                "NOTLIN_JACKSON_CORE",
                "NOTLIN_JACKSON_ANNOTATIONS",
                "NOTLIN_JACKSON_DATABIND",
            ]
            .map(|key| PathBuf::from(std::env::var_os(key).unwrap())),
        );
        for entry in fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path();
            match path.extension().and_then(|s| s.to_str()) {
                Some("kt") => config.kotlin_sources.push(path),
                Some("java") => config.java_sources.push(path),
                _ => {}
            }
        }
        config.run_main_class = Some("surface.Probe".into());
        let result = validate_jvm_sources(&config).unwrap_or_else(|error| {
            panic!("{root:?}, translated={translated}, lombok={lombok}: {error}")
        });
        assert_eq!(result.commands.last().unwrap().stdout.trim(), "surface-ok");
        assert!(root.starts_with(std::env::temp_dir()));
        fs::remove_dir_all(root).unwrap();
    }
}
