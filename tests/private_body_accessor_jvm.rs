use notlin::java_ir::{JavaElement, JavaSyntaxNode, parse_java};
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
            "notlin-private-body-accessor-{}-{id}",
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

const SOURCES: [(&str, &str); 9] = [
    (
        "Contract.kt",
        r#"package sample
interface Contract {
    fun getKey(): String
}
"#,
    ),
    (
        "ContractMut.kt",
        r#"package sample
interface ContractMut {
    fun getKey(): String
    fun setKey(value: String)
}
"#,
    ),
    (
        "Marker.kt",
        r#"package sample
@Target(AnnotationTarget.FUNCTION)
@Retention(AnnotationRetention.RUNTIME)
annotation class Marker(val value: String)
"#,
    ),
    (
        "FunctionFirst.kt",
        r#"package sample
open class FunctionFirst : Contract {
    @Marker("function-first")
    override fun getKey(): String = key
    private val key: String = "function-first"
}
"#,
    ),
    (
        "PropertyFirst.kt",
        r#"package sample
open class PropertyFirst : Contract {
    private val key: String = "property-first"
    @Marker("property-first")
    override fun getKey(): String = key
}
"#,
    ),
    (
        "MutableFunctionFirst.kt",
        r#"package sample
open class MutableFunctionFirst : ContractMut {
    @Marker("mutable-function-first")
    override fun getKey(): String = key
    override fun setKey(value: String) { key = value }
    fun readRaw(): String = key
    private var key: String = "mutable-function-first"
}
"#,
    ),
    (
        "MutablePropertyFirst.kt",
        r#"package sample
open class MutablePropertyFirst : ContractMut {
    private var key: String = "mutable-property-first"
    @Marker("mutable-property-first")
    override fun getKey(): String = key
    override fun setKey(value: String) { key = value }
}
"#,
    ),
    (
        "JavaMutableChild.java",
        r#"package sample;
public final class JavaMutableChild extends MutableFunctionFirst {
    private String value = "java-child";
    @Override public String getKey() { return value; }
    @Override public void setKey(String value) { this.value = value; }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
private fun marker(value: Any): String =
    value.javaClass.getMethod("getKey").getAnnotation(Marker::class.java)?.value ?: "missing"

fun main() {
    val functionFirst: Contract = FunctionFirst()
    val propertyFirst: Contract = PropertyFirst()
    println("function-first=${functionFirst.getKey()}")
    println("property-first=${propertyFirst.getKey()}")

    val mutableFunctionFirst: ContractMut = MutableFunctionFirst()
    println("mutable-function-before=${mutableFunctionFirst.getKey()}")
    mutableFunctionFirst.setKey("function-changed")
    println("mutable-function-after=${mutableFunctionFirst.getKey()}")

    val mutablePropertyFirst: ContractMut = MutablePropertyFirst()
    println("mutable-property-before=${mutablePropertyFirst.getKey()}")
    mutablePropertyFirst.setKey("property-changed")
    println("mutable-property-after=${mutablePropertyFirst.getKey()}")

    val javaChildImpl = JavaMutableChild()
    val javaChild: ContractMut = javaChildImpl
    println("java-child-before=${javaChild.getKey()}")
    println("java-child-raw-before=${javaChildImpl.readRaw()}")
    javaChild.setKey("java-changed")
    println("java-child-after=${javaChild.getKey()}")
    println("java-child-raw-after=${javaChildImpl.readRaw()}")

    println("annotations=${marker(FunctionFirst())},${marker(PropertyFirst())},${marker(MutableFunctionFirst())},${marker(MutablePropertyFirst())}")
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

fn migration_sources(root: &Path) -> Vec<PathBuf> {
    let mut sources = fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "kt")
                && path
                    .file_name()
                    .is_some_and(|name| name != "Runner.kt" && name != "Marker.kt")
        })
        .collect::<Vec<_>>();
    sources.sort();
    sources
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
    command
        .args(migration_sources(root))
        .output()
        .expect("run notlin")
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

fn child_node(element: &JavaElement) -> Option<&JavaSyntaxNode> {
    match element {
        JavaElement::Declaration(node)
        | JavaElement::Type(node)
        | JavaElement::Expression(node)
        | JavaElement::Statement(node)
        | JavaElement::Annotation(node)
        | JavaElement::Import(node)
        | JavaElement::Other(node) => Some(node),
        JavaElement::Token(_) => None,
    }
}

fn visit_nodes<'a>(node: &'a JavaSyntaxNode, visit: &mut impl FnMut(&'a JavaSyntaxNode)) {
    visit(node);
    for element in &node.children {
        if let Some(child) = child_node(element) {
            visit_nodes(child, visit);
        }
    }
}

fn contains_token(node: &JavaSyntaxNode, token: &str) -> bool {
    node.children.iter().any(|element| match element {
        JavaElement::Token(value) => value.text == token,
        _ => child_node(element).is_some_and(|child| contains_token(child, token)),
    })
}

fn first_token(node: &JavaSyntaxNode) -> Option<String> {
    for element in &node.children {
        match element {
            JavaElement::Token(value) => return Some(value.text.clone()),
            _ => {
                if let Some(value) = child_node(element).and_then(first_token) {
                    return Some(value);
                }
            }
        }
    }
    None
}

fn named_child<'a>(node: &'a JavaSyntaxNode, field_name: &str) -> Option<&'a JavaSyntaxNode> {
    node.children.iter().find_map(|element| {
        child_node(element).filter(|child| child.field_name.as_deref() == Some(field_name))
    })
}

fn modifiers_contain(node: &JavaSyntaxNode, modifier: &str) -> bool {
    node.children.iter().any(|element| {
        child_node(element)
            .is_some_and(|child| child.kind == "modifiers" && contains_token(child, modifier))
    })
}

fn method_name(node: &JavaSyntaxNode) -> Option<String> {
    named_child(node, "name").and_then(first_token)
}

fn named_variable_in(node: &JavaSyntaxNode, name: &str) -> bool {
    let mut found = false;
    visit_nodes(node, &mut |candidate| {
        if candidate.kind == "variable_declarator"
            && named_child(candidate, "name")
                .and_then(first_token)
                .as_deref()
                == Some(name)
        {
            found = true;
        }
    });
    found
}

fn assert_emitted_class(root: &Path, class: &str, mutable: bool) {
    let source = fs::read_to_string(root.join(format!("{class}.java")))
        .unwrap_or_else(|_| panic!("{class}.java should be emitted"));
    let unit = parse_java(&source).expect("emitted Java parses");
    let mut methods = Vec::new();
    let mut key_fields = Vec::new();
    visit_nodes(&unit.root, &mut |node| {
        if node.kind == "method_declaration"
            && let Some(name) = method_name(node)
            && (name == "getKey" || (mutable && name == "setKey"))
        {
            methods.push((name, node));
        }
        if node.kind == "field_declaration" && named_variable_in(node, "key") {
            key_fields.push(node);
        }
    });

    assert_eq!(
        methods.iter().filter(|(name, _)| name == "getKey").count(),
        1,
        "{class} must declare exactly one getter"
    );
    assert_eq!(
        methods.iter().filter(|(name, _)| name == "setKey").count(),
        if mutable { 1 } else { 0 },
        "{class} setter declaration count"
    );
    for (name, method) in methods {
        assert!(
            modifiers_contain(method, "public"),
            "{class}.{name} must be public:\n{source}"
        );
        if name == "getKey" {
            assert!(
                modifiers_contain(method, "Marker"),
                "{class}.{name} must retain the source method annotation:\n{source}"
            );
        }
    }

    assert_eq!(key_fields.len(), 1, "{class} must have one backing field");
    assert!(
        modifiers_contain(key_fields[0], "private"),
        "{class}.key backing field must remain private:\n{source}"
    );
}

fn assert_translation(root: &Path, lombok_enabled: bool) {
    let translated = translate(root, lombok_enabled);
    assert!(
        translated.status.success(),
        "Lombok mode {lombok_enabled}: {}",
        String::from_utf8_lossy(&translated.stderr)
    );
    for source in ["Contract", "ContractMut"] {
        assert!(root.join(format!("{source}.java")).is_file());
        assert!(!root.join(format!("{source}.kt")).exists());
    }
    for source in ["Marker", "Runner"] {
        assert!(root.join(format!("{source}.kt")).is_file());
        assert!(!root.join(format!("{source}.java")).exists());
    }
    for (class, mutable) in [
        ("FunctionFirst", false),
        ("PropertyFirst", false),
        ("MutableFunctionFirst", true),
        ("MutablePropertyFirst", true),
    ] {
        assert!(root.join(format!("{class}.java")).is_file());
        assert!(!root.join(format!("{class}.kt")).exists());
        assert_emitted_class(root, class, mutable);
    }
}

#[test]
fn explicit_getters_keep_public_visibility_and_private_backing_fields() {
    let scratch = Scratch::new();
    let root = scratch.0.join("cli");
    populate(&root);
    assert_translation(&root, false);
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_private_body_accessor_contracts() {
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
    .expect("original Kotlin implementation and Java subclass compile and run");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert_eq!(
        expected,
        "function-first=function-first\nproperty-first=property-first\n\
mutable-function-before=mutable-function-first\nmutable-function-after=function-changed\n\
mutable-property-before=mutable-property-first\nmutable-property-after=property-changed\n\
java-child-before=java-child\njava-child-raw-before=mutable-function-first\n\
java-child-after=java-changed\njava-child-raw-after=mutable-function-first\n\
annotations=function-first,property-first,mutable-function-first,mutable-property-first"
    );

    for lombok_enabled in [false, true] {
        let migrated = scratch.0.join(format!("migrated-{lombok_enabled}"));
        populate(&migrated);
        assert_translation(&migrated, lombok_enabled);
        let after = validate_jvm_sources(&jvm_config(
            &migrated,
            scratch.0.join(format!("mixed-classes-{lombok_enabled}")),
            &stdlib,
            lombok_enabled,
        ))
        .expect(
            "migrated contracts and classes work with retained Kotlin driver and Java subclass",
        );
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
