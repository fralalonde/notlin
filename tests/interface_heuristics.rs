use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-temp")
            .join(format!(
                "interface-heuristics-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn write(&self, name: &str, text: &str) {
        fs::write(self.0.join(name), text).unwrap();
    }
    fn migrate(&self) {
        let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
            .current_dir(&self.0)
            .args(["--in-place", "--lombok", "--annotations", "none", "."])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.0.join(name)).unwrap_or_default()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const ENUMERATION: &str = r#"package heuristics
import java.util.Locale
import com.fasterxml.jackson.databind.annotation.JsonDeserialize
import com.fasterxml.jackson.databind.annotation.JsonSerialize
@JsonDeserialize(using = EnumerationDeserializer::class /* Disabled::class { -> } */)
@JsonSerialize(using = EnumerationSerializer::class)
interface Enumeration {
    val baseAlias: String
    val alias: String
    fun getKey(): String {
        if (alias == "") return baseAlias
        return "$baseAlias/$alias".lowercase(Locale.getDefault())
    }
}
"#;

const INFERRED: &str = "package heuristics\ninterface Inferred {\n val prefix get() = \"base\"\n val alias: String\n}\n";
const MUTABLE: &str = "package heuristics\ninterface Mutable {\n var raw: String\n var alias: String\n  get() = raw\n  set(next) { raw = next + \"!\" }\n}\n";
const BOOLEAN: &str = "package heuristics\ninterface Flag {\n var isReady: Boolean\n fun read(): Boolean = isReady\n}\n";
const COMPANION: &str = "package heuristics\ninterface Defaults {\n val alias: String\n companion object {\n val DEFAULT: String = \"base\"\n }\n}\n";
const METHODS: &str = "package heuristics\ninterface Methods {\n fun describe(prefix: String): String\n}\ninterface Namespace {\n companion object {\n val DEFAULT: String = \"namespace\"\n }\n}\n";
const DEFAULT_CHAIN: &str = "package heuristics\ninterface ParentDefault: WideContract {\n override fun getId(): MidId = MidValue()\n}\ninterface ChildDefault: ParentDefault {\n override fun getId(): NarrowId = NarrowValue()\n}\ninterface RetainedProvider: ChildDefault {\n override fun getId(): NarrowId = RetainedValue()\n suspend fun retain() {}\n}\nclass RetainedId: RetainedProvider {\n suspend fun anchor() {}\n}\n";
const IMPLEMENTERS: &str = r#"package heuristics
class Entry(override val baseAlias: String, override val alias: String): Enumeration {
 suspend fun anchor() {}
}
class InferredEntry(override val alias: String): Inferred {
 suspend fun anchor() {}
}
class MutableEntry(override var raw: String): Mutable {
 suspend fun anchor() {}
}
class FlagEntry(override var isReady: Boolean): Flag {
 suspend fun anchor() {}
}
enum class FlagEnum(override var isReady: Boolean): Flag { OFF(false), ON(true) }
class DefaultsEntry(override val alias: String): Defaults {
 suspend fun anchor() {}
}
interface DerivedMutable: Mutable {
 override var alias: String
  get() = raw
  set(next) { raw = next + "?" }
 suspend fun anchor()
}
class DerivedEntry(override var raw: String): DerivedMutable {
 override suspend fun anchor() {}
}
class MethodEntry: Methods, Namespace {
 override fun describe(prefix: String): String = prefix + "method"
 suspend fun anchor() {}
}
"#;

fn populate(f: &Fixture) {
    f.write("AliasSource.java", "package heuristics; public class AliasSource { public static int calls; public String getBaseAlias() { calls++; return \"ALIAS\"; } }");
    f.write("LaterEnum.kt", "package heuristics\nenum class LaterEnum {\n ONE;\n fun getBaseAlias(): String = resourceClass.getBaseAlias().lowercase()\n val resourceClass: AliasSource get() = AliasSource()\n}\n");
    f.write("Variants.java", "package heuristics; @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME) public @interface Variants { Kind[] value(); @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME) public @interface Kind { Class<?> value(); String name(); } }");
    f.write("VariantContract.kt", "package heuristics\nimport covarianttypes.CovValue\n@Variants(\n Variants.Kind(value = CovValue::class, name = \"//keep/*text*/\"),\n // Variants.Kind(value = Disabled::class) { -> }\n)\ninterface VariantContract {\n val code: String\n}\n");
    f.write(
        "CovWideRoot.java",
        "package covarianttypes; public interface CovWideRoot { CovWide getCategory(); }",
    );
    f.write(
        "CovNarrowRoot.java",
        "package covarianttypes; public interface CovNarrowRoot { CovNarrow getCategory(); }",
    );
    f.write("CovVariant.java", "package heuristics; public class CovVariant implements CombinedVariant { public static int calls; public covarianttypes.CovNarrow getCategory() { calls++; return new covarianttypes.CovValue(); } }");
    f.write("GetterConsumer.kt", "package heuristics\nimport covarianttypes.*\ninterface CombinedVariant: CovWideRoot, CovNarrowRoot\nclass GetterConsumer(@JvmField val variant: CombinedVariant) {\n fun getVariant(): CombinedVariant = variant\n fun read(): String = getVariant().getCategory().label()\n}\n");
    f.write(
        "CovWide.java",
        "package covarianttypes; public interface CovWide { String label(); }",
    );
    f.write(
        "CovNarrow.java",
        "package covarianttypes; public interface CovNarrow extends CovWide {}",
    );
    f.write("CovValue.java", "package covarianttypes; public class CovValue implements CovNarrow { public String label() { return \"wild\"; } }");
    f.write(
        "CovRoot.java",
        "package covarianttypes; public interface CovRoot { CovWide getId(); }",
    );
    f.write("WildcardChain.kt", "package heuristics\nimport covarianttypes.*\ninterface WildcardProvider: CovRoot {\n override fun getId(): CovNarrow = CovValue()\n}\nclass WildcardChild: WildcardProvider {\n suspend fun anchor() {}\n}\n");
    f.write("CrossSource.kt", "package wildcardsource\ninterface CrossApi {\n val request: String\n}\ninterface CrossSource: CrossApi {\n override val request: String\n  get() = \"cross\"\n suspend fun anchor() {}\n}\nclass CrossEntry: CrossSource {\n suspend fun keep() {}\n}\n");
    f.write("CrossCaller.kt", "package heuristics\nimport wildcardsource.*\nclass CrossCaller(val work: CrossSource) {\n fun read(): String = work.request\n suspend fun keep() {}\n}\n");
    f.write(
        "Collections.kt",
        r#"package heuristics
interface ItemBag { val items: List<WideId> }
enum class CollectionEvent(val source: ListSource): ItemBag {
 ONE(ListSource());
 override val items: List<WideId> get() = source.items
 fun expression(): List<WideId> = source.items
 fun block(): List<WideId> { return source.items }
}
"#,
    );
    f.write("ListSource.java", "package heuristics; public class ListSource { private final java.util.List<NarrowId> items = new java.util.ArrayList<>(java.util.List.of(new NarrowValue())); public java.util.List<NarrowId> getItems() { return items; } }");
    for (name, source) in [
        ("Enumeration.kt", ENUMERATION),
        ("Inferred.kt", INFERRED),
        ("Mutable.kt", MUTABLE),
        ("Flag.kt", BOOLEAN),
        ("Defaults.kt", COMPANION),
        ("Methods.kt", METHODS),
        ("DefaultChain.kt", DEFAULT_CHAIN),
        ("Implementers.kt", IMPLEMENTERS),
        (
            "JsonDeserialize.java",
            "package com.fasterxml.jackson.databind.annotation; public @interface JsonDeserialize { Class<?> using(); }",
        ),
        (
            "JsonSerialize.java",
            "package com.fasterxml.jackson.databind.annotation; public @interface JsonSerialize { Class<?> using(); }",
        ),
        (
            "EnumerationDeserializer.java",
            "package heuristics; public class EnumerationDeserializer {}",
        ),
        (
            "EnumerationSerializer.java",
            "package heuristics; public class EnumerationSerializer {}",
        ),
        (
            "WideId.java",
            "package heuristics; public interface WideId { String label(); }",
        ),
        (
            "MidId.java",
            "package heuristics; public interface MidId extends WideId {}",
        ),
        (
            "NarrowId.java",
            "package heuristics; public interface NarrowId extends MidId {}",
        ),
        (
            "WideContract.java",
            "package heuristics; public interface WideContract { WideId getId(); }",
        ),
        (
            "MidValue.java",
            "package heuristics; public class MidValue implements MidId { public String label() { return \"mid\"; } }",
        ),
        (
            "NarrowValue.java",
            "package heuristics; public class NarrowValue implements NarrowId { public String label() { return \"narrow\"; } }",
        ),
    ] {
        f.write(name, source);
    }
    f.write("RetainedValue.java", "package heuristics; public class RetainedValue implements NarrowId { public String label() { return \"retained\"; } }");
}

#[test]
fn properties_defaults_companions_and_implementers_use_one_accessor_model() {
    let f = Fixture::new();
    populate(&f);
    f.migrate();
    for name in [
        "Enumeration",
        "Inferred",
        "Mutable",
        "Flag",
        "Defaults",
        "Methods",
        "Namespace",
        "ParentDefault",
        "ChildDefault",
        "WildcardProvider",
        "GetterConsumer",
        "VariantContract",
        "LaterEnum",
    ] {
        assert!(
            f.0.join(format!("{name}.java")).exists(),
            "{name} retained: {}",
            f.read(&format!("{name}.kt"))
        );
    }
    let java = f.read("Enumeration.java");
    assert!(
        java.contains("String getAlias()") && java.contains("default String getKey()"),
        "{java}"
    );
    assert!(
        java.contains("using = EnumerationDeserializer.class"),
        "{java}"
    );
    assert!(
        java.contains("using = EnumerationSerializer.class"),
        "{java}"
    );
    let flag = f.read("Flag.java");
    assert!(
        flag.contains("boolean isReady()") && flag.contains("void setReady(boolean value)"),
        "{flag}"
    );
    let mutable = f.read("Mutable.java");
    assert!(
        mutable.contains("default void setAlias(String next)"),
        "{mutable}"
    );
    let defaults = f.read("Defaults.java");
    assert!(
        defaults.contains("class Companion") && defaults.contains("getDEFAULT()"),
        "{defaults}"
    );
    let kotlin = f.read("Implementers.kt");
    assert!(
        f.read("CrossCaller.kt").contains("work.getRequest()"),
        "{}",
        f.read("CrossCaller.kt")
    );
    assert!(
        kotlin.contains("override fun getAlias()") && kotlin.contains("override fun setRaw("),
        "{kotlin}"
    );
    assert!(
        kotlin.contains("override fun isReady()") && kotlin.contains("override fun setReady("),
        "{kotlin}"
    );
    assert!(
        kotlin.contains("override fun setAlias(next: String)"),
        "{kotlin}"
    );
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, NOTLIN_KOTLIN_STDLIB, NOTLIN_LOMBOK and JAVA_HOME"]
fn original_and_migrated_interfaces_agree_with_kotlin_and_java_callers() {
    use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
    let f = Fixture::new();
    populate(&f);
    f.write("Caller.kt", r#"package heuristics
fun kotlinCheck(): String {
 suspend fun anchor() {}
 val mutable: Mutable = MutableEntry("first")
 mutable.alias = "next"
 val flag: Flag = FlagEntry(false)
 flag.isReady = true
 val derived: Mutable = DerivedEntry("start")
 derived.alias = "derived"
 return Entry("BASE", "").getKey() + "|" + Entry("BASE", "AbC").getKey() + "|" +
     mutable.alias + "|" + flag.isReady + "|" + flag.read() + "|" + InferredEntry("x").prefix + "|" +
     Defaults.DEFAULT + "|" + Defaults.Companion.DEFAULT + "|" + derived.alias + "|" +
     MethodEntry().describe("prefix-") + "|" + Namespace.Companion.DEFAULT + "|" + RetainedId().getId().label()
}
"#);
    f.write(
        "Runner.java",
        r#"package heuristics;
public class Runner {
 public static void main(String[] args) {
  Flag flag = new FlagEntry(false);
  flag.setReady(true);
  Flag enumFlag = FlagEnum.OFF;
  enumFlag.setReady(true);
  Mutable mutable = new MutableEntry("zero");
  mutable.setAlias("java");
  CollectionEvent event = CollectionEvent.ONE;
  java.util.List<WideId> items = event.getItems();
  boolean same = (Object) items == event.getSource().getItems() && items == event.expression() && items == event.block();
  items.add(new NarrowValue());
  String category = new GetterConsumer(new CovVariant()).read();
  Variants.Kind[] kinds = VariantContract.class.getAnnotation(Variants.class).value();
  String annotation = kinds.length + "/" + kinds[0].value().getSimpleName() + "/" + kinds[0].name();
  String later = LaterEnum.ONE.getBaseAlias() + "/" + AliasSource.calls;
  System.out.println(CallerKt.kotlinCheck() + "|" + flag.isReady() + "|" + mutable.getAlias() + "|" + enumFlag.isReady() + "|" + same + "|" + event.getSource().getItems().size() + "|" + items.get(0).label() + "|" + new CrossCaller(new wildcardsource.CrossEntry()).read() + "|" + new WildcardChild().getId().label() + "|" + category + "/" + CovVariant.calls + "|" + annotation + "|" + later);
 }
}
"#,
    );
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").unwrap();
    let javac = std::env::var_os("NOTLIN_JAVAC").unwrap();
    let java = std::env::var_os("NOTLIN_JAVA").unwrap();
    let stdlib = PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap());
    let lombok = PathBuf::from(std::env::var_os("NOTLIN_LOMBOK").unwrap());
    let check = |output: &str, mode: &str| {
        let mut config = JvmValidationConfig::new(&kotlinc, &javac, &java, f.0.join(output));
        config.kotlinc_args.extend([
            "-jvm-target".into(),
            "17".into(),
            format!("-jvm-default={mode}").into(),
        ]);
        config.javac_args.extend(["--release".into(), "17".into()]);
        config.javac_args.extend([
            "-processor".into(),
            "lombok.launch.AnnotationProcessorHider$AnnotationProcessor".into(),
        ]);
        for entry in fs::read_dir(&f.0).unwrap().flatten() {
            let p = entry.path();
            match p.extension().and_then(|e| e.to_str()) {
                Some("kt") => config.kotlin_sources.push(p),
                Some("java") => config.java_sources.push(p),
                _ => {}
            }
        }
        config.classpath.push(stdlib.clone());
        config.classpath.push(lombok.clone());
        config.run_main_class = Some("heuristics.Runner".into());
        validate_jvm_sources(&config)
            .unwrap_or_else(|e| panic!("{output}: {e}"))
            .commands
            .last()
            .unwrap()
            .stdout
            .trim()
            .to_owned()
    };
    let modes = ["disable", "enable", "no-compatibility"];
    let before = modes.map(|mode| check(&format!("original-{mode}"), mode));
    for value in &before {
        assert_eq!(
            value,
            "BASE|base/abc|next!|true|true|base|base|base|derived?|prefix-method|namespace|retained|true|java!|true|true|2|narrow|cross|wild|wild/1|1/CovValue///keep/*text*/|alias/1"
        );
    }
    f.migrate();
    for name in [
        "Enumeration",
        "Inferred",
        "Mutable",
        "Flag",
        "Defaults",
        "Methods",
        "Namespace",
        "ParentDefault",
        "ChildDefault",
        "WildcardProvider",
        "GetterConsumer",
        "VariantContract",
        "LaterEnum",
    ] {
        assert!(
            f.0.join(format!("{name}.java")).exists(),
            "{name} did not migrate"
        );
    }
    assert!(f.read("DefaultChain.kt").contains("RetainedProvider"));
    for (mode, expected) in modes.into_iter().zip(before) {
        assert_eq!(check(&format!("migrated-{mode}"), mode), expected);
    }
}
