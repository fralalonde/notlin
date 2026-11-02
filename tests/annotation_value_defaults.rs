use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
const CASE: &str = r#"package sample
import com.fasterxml.jackson.annotation.JsonSubTypes
import sample.Tags as Alias
@sample.Tags interface Bare
@sample.Tags() interface Empty
@sample.Tags(/* no values */) interface Comments
@Alias(marker = 11) interface Named
@sample.Tags(value = ["first", "second"], marker = 12) interface Explicit
@Defaults() interface Defaulted
@Single() interface Scalar
@Numbers() interface Primitive
@Types() interface Classes
@RequiredExtra(count = 3) interface Extra
@JsonSubTypes() interface JacksonEmpty
@com.fasterxml.jackson.annotation.JsonSubTypes interface JacksonBare
"#;
const RUNNER: &str = r#"package sample
import com.fasterxml.jackson.annotation.JsonSubTypes
fun main() {
    for(type in listOf(Bare::class.java, Empty::class.java, Comments::class.java, Named::class.java, Explicit::class.java)) {
        val tag=type.getAnnotation(Tags::class.java)
        println(tag.value.joinToString("/")+":"+tag.marker)
    }
    println(Defaulted::class.java.getAnnotation(Defaults::class.java).value.joinToString("/"))
    println(Scalar::class.java.getAnnotation(Single::class.java).value)
    println(Primitive::class.java.getAnnotation(Numbers::class.java).value.size)
    println(Classes::class.java.getAnnotation(Types::class.java).value.size)
    val extra=Extra::class.java.getAnnotation(RequiredExtra::class.java)
    println("${extra.count}:${extra.value.size}")
    println(JacksonEmpty::class.java.getAnnotation(JsonSubTypes::class.java).value.size)
    println(JacksonBare::class.java.getAnnotation(JsonSubTypes::class.java).value.size)
}
"#;

fn write_case(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, elements) in [
        ("Tags", "String[] value(); int marker() default 7;"),
        ("Defaults", "String[] value() default {\"fallback\"};"),
        ("Single", "String value() default \"single\";"),
        ("Numbers", "int[] value();"),
        ("Types", "Class<?>[] value();"),
        ("RequiredExtra", "String[] value(); int count();"),
    ] {
        fs::write(root.join(format!("{name}.java")), format!("package sample; @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME) public @interface {name} {{ {elements} }}\n")).unwrap();
    }
    fs::write(root.join("Case.kt"), CASE).unwrap();
    fs::write(root.join("Runner.kt"), RUNNER).unwrap();
}

fn new_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "notlin-annotation-values-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    assert!(!root.exists());
    root
}

fn run(root: &Path, name: &str, command: &mut Command) -> Output {
    let output = command.output().unwrap();
    fs::write(root.join(format!("{name}.stdout")), &output.stdout).unwrap();
    fs::write(root.join(format!("{name}.stderr")), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{root:?} {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn translate(root: &Path, lombok: bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_notlin"));
    cmd.current_dir(root)
        .args(["--in-place", "--jvm-facts", "off", "--annotations", "none"]);
    if lombok {
        cmd.arg("--lombok");
    }
    run(root, "translation", cmd.arg("Case.kt"));
    assert!(!root.join("Case.kt").exists(), "{root:?}");
    for (name, annotation) in [
        ("Bare", "@sample.Tags({})"),
        ("Empty", "@sample.Tags({})"),
        ("Comments", "@sample.Tags({})"),
        ("Named", "@sample.Tags(value = {}, marker = 11)"),
        (
            "Explicit",
            "@sample.Tags(value = {\"first\", \"second\"}, marker = 12)",
        ),
        ("Defaulted", "@Defaults()"),
        ("Scalar", "@Single()"),
        ("Primitive", "@Numbers({})"),
        ("Classes", "@Types({})"),
        ("Extra", "@RequiredExtra(value = {}, count = 3)"),
        ("JacksonEmpty", "@JsonSubTypes({})"),
        (
            "JacksonBare",
            "@com.fasterxml.jackson.annotation.JsonSubTypes({})",
        ),
    ] {
        let java = fs::read_to_string(root.join(format!("{name}.java"))).unwrap();
        assert!(java.contains(annotation), "{name}: {java}");
    }
    assert_eq!(fs::read_to_string(root.join("Runner.kt")).unwrap(), RUNNER);
}

#[test]
fn omitted_required_annotation_arrays_are_explicit_and_defaults_stay_omitted() {
    let root = new_root("plans");
    write_case(&root);
    translate(&root, false);
    assert!(root.starts_with(std::env::temp_dir()));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn annotation_array_contracts_follow_imports_and_reject_name_guesses() {
    for (imports, annotation, peer, expected) in [
        (
            "",
            "@foreign.JsonSubTypes()",
            "JsonSubTypes",
            "@foreign.JsonSubTypes()",
        ),
        (
            "import foreign.JsonSubTypes as Choices\n",
            "@Choices()",
            "JsonSubTypes",
            "@foreign.JsonSubTypes()",
        ),
        (
            "import foreign.*\n",
            "@JsonSubTypes()",
            "JsonSubTypes",
            "@JsonSubTypes()",
        ),
        ("", "@Tags()", "Tags", "@Tags()"),
        (
            "import com.fasterxml.jackson.annotation.JsonSubTypes as Choices\n",
            "@Choices()",
            "Witness",
            "@com.fasterxml.jackson.annotation.JsonSubTypes({})",
        ),
        (
            "import com.fasterxml.jackson.annotation.*\nimport foreign.*\n",
            "@JsonSubTypes()",
            "Witness",
            "@JsonSubTypes({})",
        ),
    ] {
        let root = new_root("binding");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(format!("{peer}.java")),
            format!(
                "package foreign; public @interface {peer} {{ String value() default \"kept\"; }}"
            ),
        )
        .unwrap();
        fs::write(
            root.join("Case.kt"),
            format!("package sample\n{imports}{annotation}\ninterface Bound\n"),
        )
        .unwrap();
        run(
            &root,
            "translation",
            Command::new(env!("CARGO_BIN_EXE_notlin"))
                .current_dir(&root)
                .args([
                    "--in-place",
                    "--jvm-facts",
                    "off",
                    "--annotations",
                    "none",
                    "Case.kt",
                ]),
        );
        let java = fs::read_to_string(root.join("Bound.java")).unwrap();
        assert!(java.contains(expected), "{imports}{annotation}: {java}");
        if !expected.contains("{}") {
            assert!(!java.contains("{}"), "{java}");
        }
        assert!(root.starts_with(std::env::temp_dir()));
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Jackson annotations and Lombok in NOTLIN_* variables"]
fn real_toolchain_preserves_empty_annotation_arrays_and_declared_defaults() {
    let base = new_root("native");
    let jars = [
        "NOTLIN_KOTLIN_STDLIB",
        "NOTLIN_JACKSON_ANNOTATIONS",
        "NOTLIN_LOMBOK",
    ]
    .map(|key| PathBuf::from(std::env::var_os(key).unwrap()));
    let mut original = None;
    for (label, translated, lombok) in [
        ("original", false, false),
        ("plain", true, false),
        ("lombok", true, true),
    ] {
        let root = base.join(label);
        write_case(&root);
        if translated {
            translate(&root, lombok);
        }
        let classes = root.join("classes");
        fs::create_dir_all(&classes).unwrap();
        let classpath = std::env::join_paths(std::iter::once(&classes).chain(jars.iter())).unwrap();
        let sources = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "java"))
            .collect::<Vec<_>>();
        run(
            &root,
            "javac",
            Command::new(std::env::var_os("NOTLIN_JAVAC").unwrap())
                .args(["--release", "17", "-proc:full", "-cp"])
                .arg(&classpath)
                .arg("-d")
                .arg(&classes)
                .args(&sources),
        );
        let mut kotlin = Command::new(std::env::var_os("NOTLIN_KOTLINC").unwrap());
        kotlin
            .args(["-jvm-target", "17", "-classpath"])
            .arg(&classpath)
            .arg("-d")
            .arg(&classes)
            .arg(root.join("Runner.kt"));
        if !translated {
            kotlin.arg(root.join("Case.kt"));
        }
        run(&root, "kotlinc", &mut kotlin);
        let output = run(
            &root,
            "runtime",
            Command::new(std::env::var_os("NOTLIN_JAVA").unwrap())
                .arg("-cp")
                .arg(&classpath)
                .arg("sample.RunnerKt"),
        );
        let output = String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n");
        assert!(
            output.contains("fallback\nsingle\n0\n0\n3:0\n0\n0\n"),
            "{output}"
        );
        if let Some(original) = &original {
            assert_eq!(&output, original, "{label}");
        } else {
            original = Some(output);
        }
    }
    assert!(base.starts_with(std::env::temp_dir()));
    fs::remove_dir_all(base).unwrap();
}
