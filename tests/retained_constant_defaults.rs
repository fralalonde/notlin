use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};

const SOURCES: &[(&str, &str)] = &[
    (
        "Category.kt",
        "package sample\ninterface CategoryView { fun getKey(): String }\nenum class Category : CategoryView {\n CONTAINER;\n override fun getKey(): String = \"container\"\n}\n",
    ),
    (
        "Contract.kt",
        "package sample\ninterface Contract {\n fun answer(): Int = 42\n val label: String get() = \"ok\"\n fun getCategory(): CategoryView = Category.CONTAINER\n companion object {\n  @JvmField val initialized: String = Effects.mark(\"contract\")\n  suspend fun remainKotlin() {}\n }\n}\n",
    ),
    (
        "Narrow.kt",
        "package sample\ninterface Narrow<T> : Contract {\n override fun getCategory(): Category = Category.CONTAINER\n companion object {\n  @JvmField val initialized: String = Effects.mark(\"narrow\")\n  suspend fun remainKotlin() {}\n }\n}\n",
    ),
    (
        "Leaf.kt",
        "package sample\ninterface Leaf : Contract {\n override fun answer(): Int = super<Contract>.answer()\n}\n",
    ),
    (
        "Children.kt",
        "package sample\nclass Concrete : Narrow<String> {\n override fun answer(): Int = super<Narrow>.answer()\n override fun getCategory(): Category = super<Narrow>.getCategory()\n}\nenum class Choice : Narrow<String> {\n ONE;\n override fun answer(): Int = super<Narrow>.answer()\n override fun getCategory(): Category = super<Narrow>.getCategory()\n}\n",
    ),
    (
        "KeptLeaf.kt",
        "package sample\nclass KeptLeaf : Leaf {\n suspend fun remainKotlin() {}\n}\n",
    ),
    (
        "Effects.java",
        r#"package sample;
public class Effects {
    public static String events = "";
    public static int receivers;
    public static String mark(String value) { events += value + "|"; return value; }
    public static Contract touch(Contract value) { receivers++; return value; }
    public static String read(Contract value) { return value.answer() + ":" + value.getCategory().getKey() + ":" + value.getLabel(); }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
 suspend fun retainedHarness() {}
 val concrete = Concrete()
 val choice = Choice.ONE
 val leaf = KeptLeaf()
 println("${concrete.answer()}|${choice.answer()}|${leaf.answer()}|${Effects.read(concrete)}|${Effects.read(choice)}|${Effects.read(leaf)}")
 println("${concrete.getCategory() === Category.CONTAINER}|${choice.getCategory() === Category.CONTAINER}|${Effects.touch(choice).answer()}|${Effects.receivers}")
 println(Effects.events)
}
"#,
    ),
];

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "notlin-retained-constants-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if let (Ok(path), Ok(temp)) = (self.0.canonicalize(), std::env::temp_dir().canonicalize())
            && path.parent() == Some(temp.as_path())
            && path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with("notlin-retained-constants-")
            })
        {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn populate(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, text) in SOURCES {
        fs::write(root.join(name), text).unwrap();
    }
}

fn populate_with_overloads(root: &Path) {
    populate(root);
    let children = fs::read_to_string(root.join("Children.kt"))
        .unwrap()
        .replace(
            "override fun answer(): Int = super<Narrow>.answer()",
            "fun answer(value: Int): Int = value",
        );
    fs::write(root.join("Children.kt"), children).unwrap();
    let runner = fs::read_to_string(root.join("Runner.kt")).unwrap().replace(
        " val leaf = KeptLeaf()",
        " val leaf = KeptLeaf()\n println(\"overloads:${concrete.answer(7)}|${choice.answer(9)}\")",
    );
    fs::write(root.join("Runner.kt"), runner).unwrap();
}

fn translate(root: &Path, lombok: bool) {
    translate_owners(root, lombok, &["Concrete", "Choice", "Leaf"]);
}

fn translate_owners(root: &Path, lombok: bool, owners: &[&str]) {
    let binary = std::env::var_os("NOTLIN_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
    let mut command = std::process::Command::new(binary);
    command.current_dir(root).arg("--in-place");
    if lombok {
        command.arg("--lombok");
    }
    let output = command.arg(".").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in owners {
        assert!(
            root.join(format!("{name}.java")).is_file(),
            "{name} must translate: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            ["Children.kt", "Leaf.kt"]
                .map(|file| fs::read_to_string(root.join(file)).unwrap_or_default())
                .join("\n")
        );
    }
    for name in ["Contract", "Narrow", "KeptLeaf", "Runner"] {
        assert!(
            root.join(format!("{name}.kt")).is_file(),
            "{name} stays Kotlin"
        );
    }
    let retained_child = fs::read_to_string(root.join("KeptLeaf.kt")).unwrap();
    for method in ["fun answer", "fun getCategory", "fun getLabel"] {
        assert!(
            !retained_child.contains(method),
            "unneeded Kotlin bridge: {retained_child}"
        );
    }
}

fn config(root: &Path, output: PathBuf, mode: &str, lombok: bool) -> JvmValidationConfig {
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output,
    );
    config.kotlinc_args.extend([
        "-jvm-target".into(),
        "21".into(),
        format!("-jvm-default={mode}").into(),
    ]);
    config.javac_args.extend(["--release".into(), "21".into()]);
    config.classpath.push(
        std::env::var_os("NOTLIN_KOTLIN_STDLIB")
            .expect("NOTLIN_KOTLIN_STDLIB")
            .into(),
    );
    config
        .classpath
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"));
    config
        .classpath
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jspecify-1.0.0.jar"));
    if lombok {
        config.classpath.push(
            std::env::var_os("NOTLIN_LOMBOK")
                .expect("NOTLIN_LOMBOK")
                .into(),
        );
        config.javac_args.push("-proc:full".into());
        let mut plugin = std::ffi::OsString::from("-Xplugin=");
        plugin.push(
            std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").expect("NOTLIN_KOTLIN_LOMBOK_PLUGIN"),
        );
        config.kotlinc_args.push(plugin);
    } else {
        config.javac_args.push("-proc:none".into());
    }
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|x| x.to_str()) {
            Some("kt") => config.kotlin_sources.push(path),
            Some("java") => config.java_sources.push(path),
            _ => {}
        }
    }
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

#[test]
fn explicit_constant_defaults_translate_classes_enums_and_interfaces() {
    let scratch = Scratch::new();
    populate(&scratch.0);
    translate(&scratch.0, false);
}

#[test]
fn inherited_constant_default_and_argument_overloads_translate_together() {
    let scratch = Scratch::new();
    populate_with_overloads(&scratch.0);
    translate(&scratch.0, false);
}

#[test]
fn sealed_retained_provider_keeps_its_direct_children_in_kotlin() {
    let scratch = Scratch::new();
    populate(&scratch.0);
    let source = fs::read_to_string(scratch.0.join("Contract.kt")).unwrap();
    fs::write(
        scratch.0.join("Contract.kt"),
        source.replace("interface Contract", "sealed interface Contract"),
    )
    .unwrap();
    translate_owners(&scratch.0, false, &["Concrete", "Choice"]);
    assert!(
        scratch.0.join("Leaf.kt").exists(),
        "a direct sealed child cannot become Java while its provider stays Kotlin"
    );
}

#[test]
#[ignore = "requires JVM tools and Kotlin stdlib"]
fn java_constant_body_control_matches_kotlin_in_every_default_mode() {
    let scratch = Scratch::new();
    for mode in ["disable", "enable", "no-compatibility"] {
        let original = scratch.0.join(format!("original-{mode}"));
        populate(&original);
        let before = validate_jvm_sources(&config(
            &original,
            scratch.0.join(format!("original-classes-{mode}")),
            mode,
            false,
        ))
        .expect("original Kotlin compiles and runs");
        let expected = before
            .commands
            .last()
            .unwrap()
            .stdout
            .trim()
            .replace('\r', "");
        assert!(
            expected.starts_with(
                "42|42|42|42:container:ok|42:container:ok|42:container:ok\ntrue|true|42|1"
            ),
            "{expected}"
        );
        // Independent Java control establishes the proposed body strategy;
        // production translation is tested separately below, without patches.
        let manual = scratch.0.join(format!("control-{mode}"));
        populate(&manual);
        fs::remove_file(manual.join("Children.kt")).unwrap();
        fs::remove_file(manual.join("Leaf.kt")).unwrap();
        fs::write(manual.join("Concrete.java"), "package sample; public final class Concrete implements Narrow<String> { public int answer() { return 42; } public String getLabel() { return \"ok\"; } public Category getCategory() { return Category.CONTAINER; } }\n").unwrap();
        fs::write(manual.join("Choice.java"), "package sample; public enum Choice implements Narrow<String> { ONE; public int answer() { return 42; } public String getLabel() { return \"ok\"; } public Category getCategory() { return Category.CONTAINER; } }\n").unwrap();
        fs::write(manual.join("Leaf.java"), "package sample; public interface Leaf extends Contract { default int answer() { return 42; } default String getLabel() { return \"ok\"; } default CategoryView getCategory() { return Category.CONTAINER; } }\n").unwrap();
        let after = validate_jvm_sources(&config(
            &manual,
            scratch.0.join(format!("control-classes-{mode}")),
            mode,
            false,
        ))
        .expect("Java bodies compile and run against retained Kotlin");
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .trim()
                .replace('\r', ""),
            expected,
            "mode {mode}"
        );
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin plugin"]
fn translated_default_bodies_match_original_behavior_in_every_mode() {
    let scratch = Scratch::new();
    for mode in ["disable", "enable", "no-compatibility"] {
        let original = scratch.0.join(format!("original-{mode}"));
        populate_with_overloads(&original);
        let before = validate_jvm_sources(&config(
            &original,
            scratch.0.join(format!("original-classes-{mode}")),
            mode,
            false,
        ))
        .expect("original Kotlin compiles and runs");
        let expected = before
            .commands
            .last()
            .unwrap()
            .stdout
            .trim()
            .replace('\r', "");
        for lombok in [false, true] {
            let migrated = scratch.0.join(format!("translated-{mode}-{lombok}"));
            populate_with_overloads(&migrated);
            translate(&migrated, lombok);
            let after = validate_jvm_sources(&config(
                &migrated,
                scratch.0.join(format!("mixed-classes-{mode}-{lombok}")),
                mode,
                lombok,
            ))
            .expect("translated output compiles and runs");
            assert_eq!(
                after
                    .commands
                    .last()
                    .unwrap()
                    .stdout
                    .trim()
                    .replace('\r', ""),
                expected,
                "mode {mode}, Lombok {lombok}"
            );
        }
    }
}

#[test]
#[ignore = "requires JVM tools and Kotlin stdlib"]
fn materialized_defaults_preserve_interface_initialization() {
    let scratch = Scratch::new();
    for mode in ["disable", "enable", "no-compatibility"] {
        let original = scratch.0.join(format!("original-{mode}"));
        populate(&original);
        let leaf = fs::read_to_string(original.join("Leaf.kt")).unwrap().replace(
            "interface Leaf : Contract {",
            "interface Leaf : Contract {\n companion object { @JvmField val initialized: String = Effects.mark(\"leaf\") }",
        );
        fs::write(original.join("Leaf.kt"), &leaf).unwrap();
        let runner = fs::read_to_string(original.join("Runner.kt"))
            .unwrap()
            .replace(
                " val leaf = KeptLeaf()",
                " val leaf = KeptLeaf()\n println(\"before:\" + Effects.events)",
            );
        fs::write(original.join("Runner.kt"), &runner).unwrap();
        let before = validate_jvm_sources(&config(
            &original,
            scratch.0.join(format!("original-classes-{mode}")),
            mode,
            false,
        ))
        .unwrap();
        let migrated = scratch.0.join(format!("translated-{mode}"));
        populate(&migrated);
        fs::write(migrated.join("Leaf.kt"), leaf).unwrap();
        fs::write(migrated.join("Runner.kt"), runner).unwrap();
        translate_owners(&migrated, false, &["Concrete", "Choice"]);
        assert!(
            migrated.join("Leaf.kt").exists(),
            "preserve observable companion initialization"
        );
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{mode}")),
            mode,
            false,
        ))
        .unwrap();
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            before.commands.last().unwrap().stdout.trim(),
            "mode {mode}"
        );
    }
}

#[test]
#[ignore = "requires JVM tools and Kotlin stdlib"]
fn sealed_provider_preserves_a_buildable_mixed_hierarchy() {
    let scratch = Scratch::new();
    for mode in ["disable", "enable", "no-compatibility"] {
        let original = scratch.0.join(format!("original-{mode}"));
        populate(&original);
        let contract = fs::read_to_string(original.join("Contract.kt"))
            .unwrap()
            .replace("interface Contract", "sealed interface Contract");
        fs::write(original.join("Contract.kt"), &contract).unwrap();
        let before = validate_jvm_sources(&config(
            &original,
            scratch.0.join(format!("original-classes-{mode}")),
            mode,
            false,
        ))
        .unwrap();
        let migrated = scratch.0.join(format!("translated-{mode}"));
        populate(&migrated);
        fs::write(migrated.join("Contract.kt"), contract).unwrap();
        translate_owners(&migrated, false, &["Concrete", "Choice"]);
        assert!(migrated.join("Leaf.kt").exists());
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{mode}")),
            mode,
            false,
        ))
        .unwrap();
        assert_eq!(
            after.commands.last().unwrap().stdout.trim(),
            before.commands.last().unwrap().stdout.trim(),
            "mode {mode}"
        );
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin plugin"]
fn complete_property_and_method_contracts_match_kotlin() {
    property_and_method_contracts_match_kotlin(false);
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin plugin"]
fn compact_property_and_method_contracts_match_kotlin() {
    property_and_method_contracts_match_kotlin(true);
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin plugin"]
fn redundant_nullable_property_has_the_same_java_contract() {
    fn populate_nullable(root: &Path) {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("Model.kt"), "package sample\ninterface Has<T>{val item:T??}\nclass Example(override val item:String?):Has<String>\n").unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package sample
fun main() {
 val absent = Example(null)
 val present = Example("value")
 val typed: Has<String> = absent
 println("${absent.item == null}|${present.item}|${typed.item == null}")
}
"#,
        )
        .unwrap();
    }
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate_nullable(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        "enable",
        false,
    ))
    .unwrap();
    let expected = before.commands.last().unwrap().stdout.trim();
    assert_eq!(expected, "true|value|true");
    for lombok in [false, true] {
        let migrated = scratch.0.join(format!("translated-{lombok}"));
        populate_nullable(&migrated);
        let binary = std::env::var_os("NOTLIN_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
        let mut command = std::process::Command::new(binary);
        command.current_dir(&migrated).arg("--in-place");
        if lombok {
            command.arg("--lombok");
        }
        let result = command.arg("Model.kt").output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        for owner in ["Has", "Example"] {
            assert!(migrated.join(format!("{owner}.java")).exists());
        }
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok}")),
            "enable",
            lombok,
        ))
        .unwrap();
        assert_eq!(after.commands.last().unwrap().stdout.trim(), expected);
    }
}

fn property_and_method_contracts_match_kotlin(compact: bool) {
    fn populate_contracts(root: &Path, compact: bool) {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Contracts.kt"),
            r#"package sample
enum class Kind { PRIMARY }
interface PropertyRoot {
 val category: Kind
}

interface Defaulted : PropertyRoot {
 override val category: Kind get() = Kind.PRIMARY
}
interface Combined : PropertyRoot, Defaulted
interface ExactRoot {
 val label: String
}
interface MethodRoot {
 fun key(): String
}
interface ConstantRoot {
 val id: String get() = "fixed"
}
class MethodConsumer : OverrideChild {
 override fun key(): String = "override"
}
class ConstantConsumer : ConstantChild
@kotlin.jvm.JvmInline value class ValueConsumer(val raw: String) : ConstantChild
"#,
        )
        .unwrap();
        fs::write(
            root.join("Owners.kt"),
            r#"package sample
data class Exact(override val label: String) : ExactRoot
data class Diamond(val value: String) : Combined
interface OverrideChild : MethodRoot { override fun key(): String
}
interface ConstantChild : ConstantRoot
"#,
        )
        .unwrap();
        fs::write(root.join("Runner.kt"), r#"package sample
fun main() {
 val exact = Exact("payload")
 val root: ExactRoot = exact
 val diamond = Diamond("value")
 val category: PropertyRoot = diamond
 val method = MethodConsumer()
 val constant = ConstantConsumer()
 val value = ValueConsumer("raw")
 val boxed: ConstantRoot = value
 println("${exact.label}|${root.label}|${diamond.category}|${category.category}|${method.key()}|${constant.id}|${value.id}|${boxed.id}")
}
"#).unwrap();
        if compact {
            for name in ["Contracts.kt", "Owners.kt"] {
                let path = root.join(name);
                let source = fs::read_to_string(&path)
                    .unwrap()
                    .replace("{\n", "{")
                    .replace("\n}", "}");
                fs::write(path, source).unwrap();
            }
        }
        let index = notlin::workspace::SourceIndex::discover(root).unwrap();
        let names = index
            .declarations()
            .map(|declaration| declaration.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            std::collections::BTreeSet::from([
                "Kind",
                "PropertyRoot",
                "Defaulted",
                "Combined",
                "ExactRoot",
                "MethodRoot",
                "ConstantRoot",
                "MethodConsumer",
                "ConstantConsumer",
                "ValueConsumer",
                "Exact",
                "Diamond",
                "OverrideChild",
                "ConstantChild",
            ]),
            "all fourteen type owners must remain visible"
        );
    }
    let scratch = Scratch::new();
    for mode in ["disable", "enable", "no-compatibility"] {
        let original = scratch.0.join(format!("original-{mode}"));
        populate_contracts(&original, compact);
        let before = validate_jvm_sources(&config(
            &original,
            scratch.0.join(format!("original-classes-{mode}")),
            mode,
            false,
        ))
        .unwrap();
        let expected = before.commands.last().unwrap().stdout.trim();
        assert_eq!(
            expected,
            "payload|payload|PRIMARY|PRIMARY|override|fixed|fixed|fixed"
        );
        for lombok in [false, true] {
            let migrated = scratch.0.join(format!("translated-{mode}-{lombok}"));
            populate_contracts(&migrated, compact);
            let contracts_before = fs::read_to_string(migrated.join("Contracts.kt")).unwrap();
            let binary = std::env::var_os("NOTLIN_TEST_BINARY")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
            let mut command = std::process::Command::new(binary);
            command.current_dir(&migrated).arg("--in-place");
            if lombok {
                command.arg("--lombok");
            }
            let result = command.arg("Owners.kt").output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            for owner in ["Exact", "Diamond", "OverrideChild", "ConstantChild"] {
                assert!(
                    migrated.join(format!("{owner}.java")).exists(),
                    "{owner}: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            let child = fs::read_to_string(migrated.join("ConstantChild.java")).unwrap();
            assert!(
                child.contains("default java.lang.String getId()"),
                "{child}"
            );
            assert_eq!(
                fs::read_to_string(migrated.join("Contracts.kt")).unwrap(),
                contracts_before,
                "the Java implementations need no additional Kotlin bridges"
            );
            let after = validate_jvm_sources(&config(
                &migrated,
                scratch.0.join(format!("mixed-classes-{mode}-{lombok}")),
                mode,
                lombok,
            ))
            .unwrap();
            assert_eq!(
                after.commands.last().unwrap().stdout.trim(),
                expected,
                "mode {mode}, Lombok {lombok}"
            );
        }
    }
}

fn populate_method_chains(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in [
        (
            "Contracts.kt",
            r#"package sample
interface View
interface Wide { fun getSelected(): View }
class Key(private var stored: String) : View {
    var text: String
        get() { Trace.textRead(); return stored }
        set(value) { stored = value }
    fun render(): String { Trace.renderRead(); return stored }
    fun render(value: Int): String = "argument"
}
interface Base<T : View> : Platform {
    override fun key(): T
    val description: String
}
interface Left<T : View> : Base<T>
interface Right<T : View> : Base<T>
interface Provider : Wide, Left<Key>, Right<Key> {
    private fun unusedImplementationDetail(): String = "private"
    override fun getSelected(): Key = key()
    val text: String get() = key().text
    fun render(): String { return key().render() }
    fun self(): Provider = this
    fun opaqueSelf(): Any = this
    fun view(): View = key()
}
"#,
        ),
        (
            "Owners.kt",
            r#"package sample
class Subject(private val stored: Key) : Provider {
    override val description: String get() = "desc"
    override fun key(): Key { Trace.keyRead(); return stored }
}
class Explicit(private val stored: Key) : Provider {
    override val description: String get() = "desc"
    override fun key(): Key { Trace.keyRead(); return stored }
    override fun render(): String = super<Provider>.render()
}
enum class Choice(private val stored: Key) : Provider {
    ONE(Key("value"));
    override val description: String get() = "desc"
    override fun key(): Key { Trace.keyRead(); return stored }
}
interface Forwarded : Provider { fun own(): String }
interface GenericForwarded<T : View> : Base<T> { fun own(): String }
"#,
        ),
        (
            "Kept.kt",
            r#"package sample
class Kept(private val stored: Key) : Forwarded {
    override val description: String get() = "desc"
    override fun key(): Key { Trace.keyRead(); return stored }
    override fun own(): String = "own"
}
class KeptGeneric(private val stored: Key) : GenericForwarded<Key> {
    override fun key(): Key = stored
    override val description: String get() = "desc"
    override fun own(): String = "own"
}
@JvmInline value class ValueConsumer(val subject: Forwarded) {
    fun read(): String = subject.render()
}
"#,
        ),
        (
            "PlatformRoot.java",
            r#"package sample;
public interface PlatformRoot { String legacy(); View key(); }
"#,
        ),
        (
            "Platform.java",
            r#"package sample;
public interface Platform extends PlatformRoot {
    String KIND = "platform";
    default String legacy() { return "java"; }
    default String legacy(int value) { return "java" + value; }
    default View forwardedKey() { return key(); }
}
"#,
        ),
        (
            "Trace.java",
            r#"package sample;
public class Trace {
    public static int keys, texts, renders;
    public static int keyRead() { return ++keys; }
    public static int textRead() { return ++texts; }
    public static int renderRead() { return ++renders; }
    public static void reset() { keys = texts = renders = 0; }
}
"#,
        ),
        (
            "Probe.java",
            r#"package sample;
public class Probe {
    public static String read(Provider p) {
        Wide wide = p;
        return p.getText() + "/" + p.render() + "/" + p.getSelected().getText()
            + "/" + p.getDescription() + "/" + (p.self() == p) + "/" + (wide.getSelected() == p.getSelected())
            + "/" + (p.opaqueSelf() == p) + "/" + (p.view() == p.getSelected())
            + "/" + p.legacy() + "/" + p.legacy(2) + "/" + Platform.KIND + "/" + (p.forwardedKey() == p.getSelected());
    }
}
"#,
        ),
        (
            "Runner.kt",
            r#"package sample
fun main() {
    val kept = Kept(Key("value"))
    for (value in listOf(Subject(Key("value")), Explicit(Key("value")), Choice.ONE, kept)) {
        value.key().text = "changed"
        Trace.reset()
        println("${Probe.read(value)}|${Trace.keys}|${Trace.texts}|${Trace.renders}")
    }
    println(ValueConsumer(kept).read())
    val generic: Base<Key> = KeptGeneric(Key("value"))
    println("${generic.key().text}/${generic.description}")
}
"#,
        ),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
}

fn translate_method_chains(root: &Path, lombok: bool) {
    let before = ["Contracts.kt", "Kept.kt"].map(|name| (name, fs::read(root.join(name)).unwrap()));
    let binary = std::env::var_os("NOTLIN_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
    let mut command = std::process::Command::new(binary);
    command.current_dir(root).arg("--in-place");
    if lombok {
        command.arg("--lombok");
    }
    let result = command.arg("Owners.kt").output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    for name in [
        "Subject",
        "Explicit",
        "Choice",
        "Forwarded",
        "GenericForwarded",
    ] {
        assert!(
            root.join(format!("{name}.java")).exists(),
            "{name}: {}\n{}",
            String::from_utf8_lossy(&result.stderr),
            fs::read_to_string(root.join("Owners.kt")).unwrap_or_default()
        );
    }
    for (name, expected) in before {
        assert_eq!(
            fs::read(root.join(name)).unwrap(),
            expected,
            "{name}: no Kotlin bridges are needed"
        );
    }
    for name in ["Subject", "Explicit", "Choice", "Forwarded"] {
        let java = fs::read_to_string(root.join(format!("{name}.java"))).unwrap();
        assert!(java.contains("this.key().render()"), "{java}");
        assert!(java.contains("this.key().getText()"), "{java}");
    }
}

#[test]
fn source_method_chains_translate_classes_enums_and_child_interfaces() {
    let scratch = Scratch::new();
    populate_method_chains(&scratch.0);
    translate_method_chains(&scratch.0, false);
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin plugin"]
fn method_chains_preserve_generic_dispatch_receivers_and_effects() {
    let scratch = Scratch::new();
    for mode in ["disable", "enable", "no-compatibility"] {
        let original = scratch.0.join(format!("original-{mode}"));
        populate_method_chains(&original);
        let before = validate_jvm_sources(&config(
            &original,
            scratch.0.join(format!("original-classes-{mode}")),
            mode,
            false,
        ))
        .unwrap();
        let expected = before
            .commands
            .last()
            .unwrap()
            .stdout
            .trim()
            .replace('\r', "");
        assert_eq!(
            expected,
            format!(
                "{}changed\nvalue/desc",
                "changed/changed/changed/desc/true/true/true/true/java/java2/platform/true|9|2|1\n"
                    .repeat(4)
            )
        );
        for lombok in [false, true] {
            let migrated = scratch.0.join(format!("mixed-{mode}-{lombok}"));
            populate_method_chains(&migrated);
            translate_method_chains(&migrated, lombok);
            let after = validate_jvm_sources(&config(
                &migrated,
                scratch.0.join(format!("mixed-classes-{mode}-{lombok}")),
                mode,
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
                    .replace('\r', ""),
                expected,
                "{mode}, Lombok {lombok}"
            );
        }
    }
}
