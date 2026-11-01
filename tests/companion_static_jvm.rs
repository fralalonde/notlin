use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCES: &[(&str, &str)] = &[
    (
        "Projection.kt",
        "package sample\nclass ProjectionResource(val label: String)\ninterface ProjectionApi {\n    val resource: ProjectionResource\n}\nclass ProjectionContext(val value: ProjectionResource) {\n    val resource: ProjectionResource get() = Effects.project(value)\n    suspend fun retainedMarker() {}\n}\nclass ProjectionOwner(val context: ProjectionContext) : ProjectionApi {\n    override val resource: ProjectionResource get() = context.resource\n    suspend fun retainedMarker() {}\n}\nclass ProjectionCalls {\n    fun read(value: ProjectionOwner): String = value.resource.label\n}\n",
    ),
    (
        "GetterMeta.java",
        "package sample;\n@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)\n@java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD)\npublic @interface GetterMeta { String value(); }\n",
    ),
    (
        "EntityApi.kt",
        "package sample\ninterface EntityApi {\n    val label: String\n}\n",
    ),
    (
        "ComputedEntity.kt",
        "package sample\nimport jakarta.persistence.*\nimport sample.GetterMeta as Meta\n@Entity\nclass ComputedEntity : EntityApi {\n    suspend fun retainedMarker() {}\n    @get:Transient\n    @get:Meta(\"body\")\n    override val label: String get() = \"entity\"\n}\n",
    ),
    (
        "EntityChild.java",
        "package sample;\npublic class EntityChild extends ComputedEntity { @Override public String getLabel() { return \"child\"; } }\n",
    ),
    (
        "BridgeRoot.java",
        "// NOTLIN: generated from BridgeRoot.kt\npackage sample;\npublic interface BridgeRoot { String getName(); }\n",
    ),
    (
        "BridgeConnected.java",
        "// NOTLIN: generated from BridgeConnected.kt\npackage sample;\npublic final class BridgeConnected implements BridgeRoot, Parallel { public String getName() { return \"connected\"; } }\n",
    ),
    (
        "Parallel.kt",
        "package sample\ninterface Parallel {\n    val name: String\n    suspend fun retainedMarker() {}\n}\n",
    ),
    (
        "Factory.kt",
        r#"package sample
import kotlin.Enum
import kotlin.Number
import kotlin.Comparable
class Factory private constructor(val value: String) {
    companion object {
        suspend fun retainedMarker() {}
        @JvmStatic fun empty(): Factory {
            Effects.mark("empty")
            return Factory("empty")
        }
        @kotlin.jvm.JvmStatic fun of(value: String): Factory {
            Effects.mark("of($value)")
            return Factory(value)
        }
        @JvmStatic fun enumValue(value: Enum<*>): Enum<*> {
            Effects.mark("enum")
            return value
        }
        @JvmStatic fun number(value: Number): Number {
            Effects.mark("number")
            return value
        }
        @JvmStatic fun comparable(value: Comparable<*>): Comparable<*> {
            Effects.mark("comparable")
            return value
        }
        @JvmStatic fun javaClass(value: Class<*>): Class<*> {
            Effects.mark("class")
            return value
        }
        fun qualified(value: kotlin.Enum<*>): kotlin.Enum<*> = value
        fun qualifiedInt(value: kotlin.Int): kotlin.Int = value
    }
}
"#,
    ),
    (
        "Api.kt",
        r#"package sample
import kotlin.jvm.JvmStatic as Static
interface Api {
    companion object {
        suspend fun retainedMarker() {}
        @Static fun label(value: String): String {
            Effects.mark("label($value)")
            return "api:$value"
        }
    }
}
"#,
    ),
    (
        "Decoy.kt",
        "package other\nclass Decoy {\n companion object {\n  @JvmStatic fun empty(): kotlin.String = \"decoy\"\n }\n}\n",
    ),
    (
        "EnumShadow.kt",
        "package other\nclass Enum\nclass Number\nclass Comparable\nclass Class\nclass String\n",
    ),
    (
        "Effects.java",
        r#"package sample;
public class Effects {
    public static String events = "";
    public static int projections;
    public static int enumReads;
    public static Enum<?> nextEnum() { enumReads++; mark("readEnum"); return FactoryToken.FIRST; }
    public static ProjectionResource project(ProjectionResource value) { projections++; return value; }
    private static int sequence;
    public static void mark(String event) { events += event + "|"; }
    public static String next() {
        sequence++;
        mark("next(" + sequence + ")");
        return "v" + sequence;
    }
    public static boolean hasSetter(Class<?> owner, String name) {
        for (java.lang.reflect.Method method : owner.getMethods()) {
            if (method.getName().equals(name)) return true;
        }
        return false;
    }
    public static String metadata(Class<?> owner) throws Exception {
        var method = owner.getDeclaredMethod("getLabel");
        return method.isAnnotationPresent(jakarta.persistence.Transient.class) + "|" + method.getAnnotation(GetterMeta.class).value();
    }
}
"#,
    ),
    (
        "Callers.kt",
        r#"package sample
class DefaultRequest(val factory: Factory = Factory.empty())
class MutableRequest(var label: String, var count: Int, val token: String)
class ComputedRequest(var label: String) {
    val length: Int get() = label.length
}
class HiddenRequest(var label: String) {
    private var count: Int = 0
    fun bump(): Int { count++; return count }
}
class FactoryCalls {
    fun create(value: String): Factory = Factory.of(value)
    fun effect(): Factory = Factory.of(Effects.next())
    fun api(): String = Api.label(Effects.next())
}
enum class FactoryChoice(val label: String) {
    A(Api.label("a")), B(Api.label("b"))
}
enum class FactoryToken { FIRST, SECOND }
enum class EnumRequest(val value: Enum<*>) {
    A(Factory.enumValue(FactoryToken.FIRST))
}
class BuiltinCalls {
    fun enumeration(value: Enum<*>): Enum<*> = Factory.enumValue(value)
    fun number(value: Number): Number = Factory.number(value)
    fun comparable(value: Comparable<*>): Comparable<*> = Factory.comparable(value)
    fun javaClass(value: Class<*>): Class<*> = Factory.javaClass(value)
    fun qualified(value: kotlin.Enum<*>): kotlin.Enum<*> = Factory.qualified(value)
    fun qualifiedString(value: kotlin.String): kotlin.String = value
    fun qualifiedInt(value: kotlin.Int): kotlin.Int = Factory.qualifiedInt(value)
    fun qualifiedAny(value: kotlin.Any): kotlin.Any = value
    fun qualifiedArray(value: kotlin.Array<kotlin.String>): kotlin.Array<kotlin.String> = value
    fun qualifiedIntArray(value: kotlin.IntArray): kotlin.IntArray = value
    fun effect(): Enum<*> = Factory.enumValue(Effects.nextEnum())
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    suspend fun retainedHarness() {}
    val first = DefaultRequest().factory.value
    val calls = FactoryCalls()
    val second = calls.effect().value
    val third = calls.create("x").value
    val fourth = calls.api()
    println("$first|$second|$third|$fourth|${FactoryChoice.A.label}|${FactoryChoice.B.label}")
    println(Effects.events)
    val mutable = MutableRequest("before", 1, "token")
    mutable.label = "after"
    mutable.count = 2
    val hidden = HiddenRequest("before")
    hidden.label = "changed"
    val computed = ComputedRequest("before")
    computed.label = "after2"
    println("SETTERS|${mutable.label}|${mutable.count}|${mutable.token}|${mutable == MutableRequest("after", 2, "token")}|${Effects.hasSetter(MutableRequest::class.java, "setToken")}|${hidden.label}|${hidden.bump()}|${hidden.bump()}|${Effects.hasSetter(HiddenRequest::class.java, "setCount")}|${computed.label}|${computed.length}")
    val parallel: Parallel = BridgeConnected()
    println("BRIDGE|${parallel.name}")
    fun readEntity(value: ComputedEntity): String = value.label
    println("ANNOTATED|${readEntity(ComputedEntity())}|${readEntity(EntityChild())}|${Effects.metadata(ComputedEntity::class.java)}")
    val projection = ProjectionOwner(ProjectionContext(ProjectionResource("value")))
    println("PROJECTION|${ProjectionCalls().read(projection)}|${Effects.projections}")
    val enumeration = FactoryToken.FIRST
    val number: Number = 1000
    val comparable: Comparable<*> = "comparable"
    val type = FactoryToken::class.java
    val builtins = BuiltinCalls()
    val text = "identity"
    val array = arrayOf("value")
    val ints = intArrayOf(7)
    println("JVMREF|${builtins.enumeration(enumeration) === enumeration}|${builtins.number(number) === number}|${builtins.comparable(comparable) === comparable}|${builtins.javaClass(type) === type}|${builtins.qualified(enumeration) === enumeration}|${EnumRequest.A.value === enumeration}|${builtins.effect() === enumeration}|${Effects.enumReads}|${builtins.qualifiedString(text) === text}|${builtins.qualifiedInt(7) == 7}|${builtins.qualifiedAny(enumeration) === enumeration}|${builtins.qualifiedArray(array) === array}|${builtins.qualifiedIntArray(ints) === ints}")
    println(Effects.events)
}
"#,
    ),
];

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "notlin-companion-static-{}-{nonce}",
            std::process::id()
        )))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn populate(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in SOURCES {
        if name.ends_with(".kt") {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
                .unwrap();
            assert!(
                !parser.parse(source, None).unwrap().root_node().has_error(),
                "invalid Kotlin fixture {name}"
            );
        }
        fs::write(root.join(name), source).unwrap();
    }
}

fn translate(root: &Path, lombok: bool) {
    let binary = std::env::var_os("NOTLIN_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
    let original_java = fs::read(root.join("Effects.java")).unwrap();
    let mut command = Command::new(binary);
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
    for name in [
        "DefaultRequest",
        "FactoryCalls",
        "FactoryChoice",
        "MutableRequest",
        "ComputedRequest",
        "HiddenRequest",
        "EntityApi",
        "ProjectionResource",
        "ProjectionApi",
        "ProjectionCalls",
        "BuiltinCalls",
        "FactoryToken",
        "EnumRequest",
    ] {
        assert!(
            root.join(format!("{name}.java")).exists(),
            "{name} must translate in Lombok mode {lombok}: {}\n{}\n{}",
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(root.join("Callers.kt")).unwrap_or_default(),
            fs::read_to_string(root.join(format!("{name}.kt"))).unwrap_or_default()
        );
    }
    for name in ["Factory", "Api", "Runner", "Parallel", "ComputedEntity"] {
        assert!(root.join(format!("{name}.kt")).exists());
        assert!(!root.join(format!("{name}.java")).exists());
    }
    assert_eq!(original_java, fs::read(root.join("Effects.java")).unwrap());
    let parallel = fs::read_to_string(root.join("Parallel.kt")).unwrap();
    assert!(parallel.contains("fun getName(): String"), "{parallel}");
    let entity = fs::read_to_string(root.join("ComputedEntity.kt")).unwrap();
    assert!(
        entity.contains("override fun getLabel(): String"),
        "{entity}"
    );
    assert!(
        !entity.contains("JvmName") && !entity.contains("val label"),
        "{entity}"
    );
    if lombok {
        for name in ["MutableRequest", "ComputedRequest"] {
            let java = fs::read_to_string(root.join(format!("{name}.java"))).unwrap();
            assert_eq!(java.matches("@Setter").count(), 1, "{java}");
            assert!(
                java.find("@Setter").unwrap() < java.find(&format!("class {name}")).unwrap(),
                "{java}"
            );
        }
    }
    let projection = fs::read_to_string(root.join("Projection.kt")).unwrap();
    assert!(!projection.contains(".let"), "{projection}");
    let caller = fs::read_to_string(root.join("FactoryCalls.java")).unwrap();
    assert!(caller.contains("Factory.Companion.of("), "{caller}");
    assert!(caller.contains("Api.Companion.label("), "{caller}");
}

fn config(root: &Path, output: PathBuf, lombok: bool) -> JvmValidationConfig {
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output,
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "21".into()]);
    config.javac_args.extend(["--release".into(), "21".into()]);
    config
        .classpath
        .push(std::env::var_os("NOTLIN_JPA").expect("NOTLIN_JPA").into());
    let mut all_open = std::ffi::OsString::from("-Xplugin=");
    all_open.push(
        std::env::var_os("NOTLIN_KOTLIN_ALLOPEN_PLUGIN").expect("NOTLIN_KOTLIN_ALLOPEN_PLUGIN"),
    );
    config.kotlinc_args.extend([
        all_open,
        "-P".into(),
        "plugin:org.jetbrains.kotlin.allopen:annotation=jakarta.persistence.Entity".into(),
    ]);
    config.classpath.push(
        std::env::var_os("NOTLIN_KOTLIN_STDLIB")
            .expect("NOTLIN_KOTLIN_STDLIB")
            .into(),
    );
    config
        .classpath
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"));
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
        let file = entry.unwrap().path();
        match file.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => config.kotlin_sources.push(file),
            Some("java") => config.java_sources.push(file),
            _ => {}
        }
    }
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

#[test]
fn static_bridges_preserve_companion_instance_calls_but_not_object_instance_calls() {
    let cases = [
        (
            "class Factory { companion object { @JvmStatic fun read(): String = \"ok\" } }",
            true,
            true,
        ),
        (
            "interface Factory { companion object { @kotlin.jvm.JvmStatic fun read(): String = \"ok\" } }",
            true,
            true,
        ),
        (
            "import kotlin.jvm.JvmStatic as Static\nclass Factory { companion object { @Static fun read(): String = \"ok\" } }",
            true,
            true,
        ),
        (
            "class Factory { companion object Named { @JvmStatic fun read(): String = \"ok\" } }",
            true,
            false,
        ),
        (
            "object Factory { @JvmStatic fun read(): String = \"ok\" }",
            false,
            false,
        ),
        (
            "class Factory { companion object { object Nested { @JvmStatic fun read(): String = \"ok\" } } }",
            false,
            false,
        ),
        (
            "class Factory { companion object { @JvmStatic @JvmName(\"renamed\") fun read(): String = \"ok\" } }",
            false,
            false,
        ),
        (
            "import kotlin.jvm.JvmName as Name\nclass Factory { companion object { @JvmStatic @Name(\"renamed\") fun read(): String = \"ok\" } }",
            false,
            false,
        ),
        (
            "class Factory { companion object { @JvmStatic @JvmSynthetic fun read(): String = \"ok\" } }",
            false,
            false,
        ),
        (
            "class Factory { companion object { @JvmStatic private fun read(): String = \"ok\" } }",
            false,
            false,
        ),
        (
            "class Factory { companion object { @JvmStatic internal fun read(): String = \"ok\" } }",
            false,
            false,
        ),
        (
            "@JvmInline value class Token(val raw: String)\nclass Factory { companion object { @JvmStatic fun read(): Token = Token(\"ok\") } }",
            false,
            false,
        ),
    ];
    for (source, instance, default_companion) in cases {
        let source = format!(
            "package sample\n{}\n",
            source.replace('{', "{\n").replace('}', "\n}")
        );
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        assert!(
            !parser.parse(&source, None).unwrap().root_node().has_error(),
            "{source}"
        );
        let provider = SyntaxSemanticProvider::new([(PathBuf::from("Factory.kt"), source.clone())]);
        let members = provider.symbols_named("read");
        let [member] = members.as_slice() else {
            panic!("expected one method: {source}");
        };
        assert_eq!(
            provider.is_public_instance_callable(&member.id),
            instance,
            "{source}\n{member:?}"
        );
        assert_eq!(
            provider.has_default_companion_jvm_abi(&member.id),
            default_companion,
            "{source}"
        );
    }
}

#[test]
fn static_companion_factories_translate_defaults_and_enum_initializers() {
    let scratch = Scratch::new();
    populate(&scratch.0);
    translate(&scratch.0, false);
}

#[test]
fn builtin_reference_signatures_have_scoped_plain_jvm_names() {
    let cases = [
        ("Enum<*>", ""),
        ("kotlin.Enum<*>", ""),
        ("java.lang.Enum<*>", ""),
        ("Enum<*>", "import kotlin.Enum\n"),
        ("Number", ""),
        ("kotlin.Number", ""),
        ("java.lang.Number", ""),
        ("Number", "import kotlin.Number\n"),
        ("Comparable<*>", ""),
        ("kotlin.Comparable<*>", ""),
        ("java.lang.Comparable<*>", ""),
        ("Comparable<*>", "import kotlin.Comparable\n"),
        ("Class<*>", ""),
        ("java.lang.Class<*>", ""),
        ("kotlin.Int", ""),
        ("kotlin.Boolean", ""),
    ];
    for (ty, imports) in cases {
        let source = format!(
            "package sample\n{imports}class Factory {{\n companion object {{\n  @JvmStatic fun read(value: {ty}): {ty} = value\n }}\n}}\n"
        );
        let provider = SyntaxSemanticProvider::new([
            (PathBuf::from("Factory.kt"), source.clone()),
            (
                PathBuf::from("Other.kt"),
                "package other\nclass Enum\nclass Number\nclass Comparable\nclass Class\n"
                    .to_owned(),
            ),
        ]);
        let members = provider.symbols_named("read");
        let [member] = members.as_slice() else {
            panic!("expected one callable: {source}");
        };
        assert!(
            provider.has_default_companion_jvm_abi(&member.id),
            "{source}"
        );
    }

    for (declarations, ty) in [
        ("@JvmInline value class Number(val raw: String)\n", "Number"),
        (
            "@JvmInline value class Wrapped(val raw: String)\ntypealias Number = Wrapped\n",
            "Number",
        ),
        ("import dependency.Number\n", "Number"),
        ("import dependency.*\n", "Enum<*>"),
    ] {
        let source = format!(
            "package sample\n{declarations}class Factory {{\n companion object {{\n  @JvmStatic fun read(value: {ty}): Factory = Factory()\n }}\n}}\n"
        );
        let provider = SyntaxSemanticProvider::new([(PathBuf::from("Factory.kt"), source.clone())]);
        let members = provider.symbols_named("read");
        let [member] = members.as_slice() else {
            panic!("expected one callable: {source}");
        };
        assert!(
            !provider.has_default_companion_jvm_abi(&member.id),
            "{source}"
        );
    }

    let source = "package sample\nclass Factory {\n companion object {\n  @JvmStatic fun read(value: kotlin.Enum<*>): kotlin.Enum<*> = value\n }\n}\n";
    let provider = SyntaxSemanticProvider::new([
        (PathBuf::from("Factory.kt"), source.to_owned()),
        (
            PathBuf::from("Other.kt"),
            "package other\nclass Enum\n".to_owned(),
        ),
    ]);
    let members = provider.symbols_named("read");
    let [member] = members.as_slice() else {
        panic!("expected one callable");
    };
    assert!(provider.has_default_companion_jvm_abi(&member.id));
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin plugin"]
fn retained_static_companion_factories_match_original_behavior_and_effects() {
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original classes"),
        false,
    ))
    .expect("original Kotlin compiles and runs");
    let expected = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected.replace('\r', ""),
        "empty|v1|x|api:v2|api:a|api:b\nempty|next(1)|of(v1)|of(x)|next(2)|label(v2)|label(a)|label(b)|\nSETTERS|after|2|token|false|false|changed|1|2|false|after2|6\nBRIDGE|connected\nANNOTATED|entity|child|true|body\nPROJECTION|value|1\nJVMREF|true|true|true|true|true|true|true|1|true|true|true|true|true\nempty|next(1)|of(v1)|of(x)|next(2)|label(v2)|label(a)|label(b)|enum|number|comparable|class|enum|readEnum|enum|"
    );
    for lombok in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok}"));
        populate(&migrated);
        translate(&migrated, lombok);
        let after = validate_jvm_sources(&config(
            &migrated,
            scratch.0.join(format!("mixed classes-{lombok}")),
            lombok,
        ))
        .expect("translated callers and retained companion compile and run");
        assert_eq!(after.commands.last().unwrap().stdout.trim(), expected);
    }
}
