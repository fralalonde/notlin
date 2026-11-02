use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BOX: &str = r#"package sample;
import org.jspecify.annotations.NonNull;
import org.jspecify.annotations.Nullable;
public class Box {
    public int calls;
    private final boolean empty;
    public Box(boolean empty) { this.empty=empty; }
    private java.util.UUID value() { return new java.util.UUID(0,1); }
    public java.util.@NonNull UUID getId() { calls++; return value(); }
    public java.util.@NonNull UUID id() { calls++; return value(); }
    public java.util.@Nullable UUID getMaybeId() { calls++; return empty?null:value(); }
    public java.util.@Nullable UUID maybeId() { calls++; return empty?null:value(); }
    public java.lang.@Nullable Double getNumber() { calls++; return empty?null:1.5; }
    public java.lang.@Nullable Double number() { calls++; return empty?null:1.5; }
    public sample.@NonNull Token getToken() { calls++; return new Token(); }
    public sample.@NonNull Token token() { calls++; return new Token(); }
    public sample.Types.@NonNull Nested getNested() { calls++; return new Types.Nested(); }
    public sample.Types.@NonNull Nested nested() { calls++; return new Types.Nested(); }
    public java.util.@NonNull UUID getBadId() { calls++; return null; }
    public java.util.@NonNull UUID badId() { calls++; return null; }
    public int getCount() { calls++; return 7; }
    public int count() { calls++; return 7; }
}
"#;
const TOKEN: &str = "package sample; public class Token { public @org.jspecify.annotations.NonNull String getText(){return \"token\";} }";
const TYPES: &str = "package sample; public class Types { public static class Nested { public @org.jspecify.annotations.NonNull String getText(){return \"nested\";} } }";
const READER: &str = r#"package sample
class Reader(val box:Box) {
    fun property():String=box.id.toString()
    fun method():String=box.id().toString()
    fun maybeProperty():String?=box.maybeId?.toString()
    fun maybeMethod():String?=box.maybeId()?.toString()
    fun numberProperty():String?=box.number?.toString()
    fun numberMethod():String?=box.number()?.toString()
    fun tokenProperty():String=box.token.text
    fun tokenMethod():String=box.token().text
    fun nestedProperty():String=box.nested.text
    fun nestedMethod():String=box.nested().text
    fun badProperty():String=box.badId.toString()
    fun badMethod():String=box.badId().toString()
    fun nullableProperty(box:Box?):String?=box?.id?.toString()
    fun nullableMethod(box:Box?):String?=box?.id()?.toString()
    fun nullableIntProperty(box:Box?)=box?.count
    fun nullableIntMethod(box:Box?)=box?.count()
}
"#;
const RUNNER: &str = r#"package sample
fun failure(block:()->Any?):String=try {block(); "ok"} catch(e:Throwable){e.javaClass.simpleName}
fun main() {
    for(empty in listOf(false,true)) {
        val box=Box(empty)
        val reader=Reader(box)
        println(listOf(reader.property(),reader.method(),reader.maybeProperty(),reader.maybeMethod(),reader.numberProperty(),reader.numberMethod(),reader.tokenProperty(),reader.tokenMethod(),reader.nestedProperty(),reader.nestedMethod()).joinToString("|"))
        println(failure {reader.badProperty()}+"/"+failure {reader.badMethod()}+":"+box.calls)
    }
    val box=Box(false)
    val reader=Reader(box)
    println(listOf(reader.nullableProperty(null),reader.nullableMethod(null),reader.nullableIntProperty(null),reader.nullableIntMethod(null)).joinToString("|"))
    println(listOf(reader.nullableProperty(box),reader.nullableMethod(box),reader.nullableIntProperty(box),reader.nullableIntMethod(box)).joinToString("|")+":"+box.calls)
}
"#;

fn run(case: &Path, name: &str, command: &mut Command) -> Output {
    let output = command.output().unwrap();
    fs::write(case.join(format!("{name}.stdout")), &output.stdout).unwrap();
    fs::write(case.join(format!("{name}.stderr")), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn type_use_getter_and_method_chains_translate_as_complete_owners() {
    let base =
        std::env::temp_dir().join(format!("notlin-java-type-use-plans-{}", std::process::id()));
    assert!(!base.exists());
    fs::create_dir_all(&base).unwrap();
    let mut retained = Vec::new();
    for (index, method) in READER
        .lines()
        .filter(|line| line.trim_start().starts_with("fun "))
        .enumerate()
    {
        let case = base.join(index.to_string());
        fs::create_dir_all(&case).unwrap();
        for (name, source) in [
            ("Box.java", BOX),
            ("Token.java", TOKEN),
            ("Types.java", TYPES),
        ] {
            fs::write(case.join(name), source).unwrap();
        }
        fs::write(
            case.join("Reader.kt"),
            format!("package sample\nclass Reader(val box:Box) {{\n{method}\n}}\n"),
        )
        .unwrap();
        run(
            &case,
            "translation",
            Command::new(env!("CARGO_BIN_EXE_notlin"))
                .current_dir(&case)
                .args(["--in-place", "--jvm-facts", "off", "Reader.kt"]),
        );
        if case.join("Reader.kt").exists() {
            retained.push((
                method.to_owned(),
                fs::read_to_string(case.join("Reader.kt")).unwrap(),
            ));
        }
    }
    assert!(retained.is_empty(), "{retained:#?}");
    assert!(base.starts_with(std::env::temp_dir()));
    fs::remove_dir_all(base).unwrap();
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, JSpecify and Lombok in NOTLIN_* variables"]
fn real_toolchain_preserves_type_use_returns_effects_and_null_behavior() {
    let base = std::env::temp_dir().join(format!("notlin-java-type-use-{}", std::process::id()));
    assert!(!base.exists(), "{base:?}");
    fs::create_dir_all(&base).unwrap();
    let java = std::env::var_os("NOTLIN_JAVA").unwrap();
    let javac = std::env::var_os("NOTLIN_JAVAC").unwrap();
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").unwrap();
    let jars = [
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap()),
        PathBuf::from(std::env::var_os("NOTLIN_JSPECIFY").unwrap()),
        PathBuf::from(std::env::var_os("NOTLIN_LOMBOK").unwrap()),
    ];
    let mut original = None;
    for (label, translate, lombok) in [
        ("original", false, false),
        ("plain", true, false),
        ("lombok", true, true),
    ] {
        let case = base.join(label);
        let classes = case.join("classes");
        fs::create_dir_all(&classes).unwrap();
        for (name, source) in [
            ("Box.java", BOX),
            ("Token.java", TOKEN),
            ("Types.java", TYPES),
            ("Reader.kt", READER),
            ("Runner.kt", RUNNER),
        ] {
            fs::write(case.join(name), source).unwrap();
        }
        if translate {
            let binary = std::env::var_os("NOTLIN_TEST_BINARY")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
            let mut cmd = Command::new(binary);
            cmd.current_dir(&case)
                .args(["--in-place", "--jvm-facts", "off"]);
            if lombok {
                cmd.arg("--lombok");
            }
            run(&case, "translation", cmd.arg("Reader.kt"));
            assert!(
                !case.join("Reader.kt").exists(),
                "Reader retained in {case:?}"
            );
            assert!(case.join("Reader.java").exists(), "{case:?}");
            assert_eq!(fs::read_to_string(case.join("Runner.kt")).unwrap(), RUNNER);
        }
        let classpath = std::env::join_paths(std::iter::once(&classes).chain(jars.iter())).unwrap();
        let mut sources = vec![
            case.join("Box.java"),
            case.join("Token.java"),
            case.join("Types.java"),
        ];
        if translate {
            sources.push(case.join("Reader.java"));
        }
        run(
            &case,
            "javac",
            Command::new(&javac)
                .args(["--release", "25", "-proc:full", "-cp"])
                .arg(&classpath)
                .arg("-d")
                .arg(&classes)
                .args(&sources),
        );
        let mut kotlin = Command::new(&kotlinc);
        kotlin
            .args(["-jvm-target", "17", "-classpath"])
            .arg(&classpath)
            .arg("-d")
            .arg(&classes);
        if !translate {
            kotlin.arg(case.join("Reader.kt"));
        }
        run(&case, "kotlinc", kotlin.arg(case.join("Runner.kt")));
        let output = run(
            &case,
            "runtime",
            Command::new(&java)
                .arg("-cp")
                .arg(&classpath)
                .arg("sample.RunnerKt"),
        );
        let output = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            output
                .matches("NullPointerException/NullPointerException:12")
                .count(),
            2,
            "{case:?}: {output}"
        );
        assert!(output.contains("null|null|null|null"), "{case:?}: {output}");
        assert!(output.contains("|7|7:4"), "{case:?}: {output}");
        if let Some(original) = &original {
            assert_eq!(&output, original, "{label}");
        } else {
            original = Some(output);
        }
    }
    // Keep all compiler logs if a control fails.
    assert!(base.starts_with(std::env::temp_dir()));
    fs::remove_dir_all(base).unwrap();
}
