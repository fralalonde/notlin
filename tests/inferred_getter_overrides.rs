use notlin::property_abi::repair_virtual_file;
use notlin::workspace::SourceIndex;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ROOT_KOTLIN: &str = "package sample\ninterface Root { val label: Any; val text: String; val maybe: String?; val unit: Any; val block: Any }\n";
const ROOT_JAVA: &str = "// NOTLIN: generated from Root.kt\npackage sample; public interface Root { Object getLabel(); String getText(); String getMaybe(); Object getUnit(); Object getBlock(); }\n";
const PRODUCER: &str = "package sample; public class Producer { public int calls; private final boolean empty; public Producer(boolean empty){this.empty=empty;} public String getValue(){calls++;return empty?null:\"value\";} }\n";
const MARK: &str = "package sample; @java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD) @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME) public @interface Mark {}\n";
const KEEP: &str = r#"package sample
open class Keep(val producer: Producer): Root {
    @get:Mark final override val label get() = producer.value
    override val text get() = producer.value
    override val maybe get() = producer.value
    override val unit get() = Unit
    override val block get(): String { return producer.value }
    suspend fun retain() {}
}
"#;
const RUNNER: &str = r#"package sample
fun attempt(block:()->Any?):String=try {java.lang.String.valueOf(block())} catch(e:Throwable){e.javaClass.simpleName}
fun main() {
    for(empty in listOf(false,true)) {
        val producer=Producer(empty)
        val value=Keep(producer)
        val root:Root=value
        println(listOf(attempt {value.label}, attempt {value.label.length}, attempt {value.text}, attempt {value.maybe}, attempt {value.unit}, attempt {value.block}, attempt {root.label}, attempt {root.block}).joinToString("|"))
        println("calls="+producer.calls)
        println(value.javaClass.getMethod("getLabel").isAnnotationPresent(Mark::class.java))
    }
}
"#;

fn repair(root: &Path) -> String {
    let path = root.join("Keep.kt");
    let index = SourceIndex::discover(root).unwrap();
    let (source, count) = repair_virtual_file(&index, &path, KEEP, &HashSet::new());
    assert_eq!(count, 5, "{source}");
    assert!(
        source.contains("@Mark\nfinal override fun getLabel() = producer.value"),
        "{source}"
    );
    assert!(
        source.contains("override fun getBlock(): String { return producer.value }"),
        "{source}"
    );
    assert!(!source.contains("notlinProperty"), "{source}");
    source
}

fn write_case(root: &Path, repaired: bool) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in [
        ("Producer.java", PRODUCER),
        ("Mark.java", MARK),
        ("Keep.kt", KEEP),
        ("Runner.kt", RUNNER),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
    if repaired {
        fs::write(root.join("Root.java"), ROOT_JAVA).unwrap();
        fs::write(root.join("Keep.kt"), repair(root)).unwrap();
    } else {
        fs::write(root.join("Root.kt"), ROOT_KOTLIN).unwrap();
    }
}

#[test]
fn repairs_inferred_class_getters_without_widening_the_return_type() {
    let root = std::env::temp_dir().join(format!(
        "notlin-inferred-getter-shape-{}",
        std::process::id()
    ));
    assert!(!root.exists());
    write_case(&root, true);
    assert!(root.starts_with(std::env::temp_dir()));
    fs::remove_dir_all(root).unwrap();
}

fn run(root: &Path, label: &str, command: &mut Command) -> Output {
    let output = command.output().unwrap();
    fs::write(root.join(format!("{label}.stdout")), &output.stdout).unwrap();
    fs::write(root.join(format!("{label}.stderr")), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{label}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
#[ignore = "requires JVM tools and Kotlin stdlib in NOTLIN_* variables"]
fn real_toolchain_preserves_inferred_getter_types_nulls_effects_and_annotations() {
    let base = std::env::temp_dir().join(format!(
        "notlin-inferred-getter-native-{}",
        std::process::id()
    ));
    assert!(!base.exists());
    let stdlib = PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap());
    let mut original = None;
    for repaired in [false, true] {
        let root = base.join(if repaired { "repaired" } else { "original" });
        write_case(&root, repaired);
        let classes = root.join("classes");
        fs::create_dir_all(&classes).unwrap();
        let classpath = std::env::join_paths([&classes, &stdlib]).unwrap();
        let mut java = Command::new(std::env::var_os("NOTLIN_JAVAC").unwrap());
        java.args(["--release", "17", "-d"])
            .arg(&classes)
            .args([root.join("Producer.java"), root.join("Mark.java")]);
        if repaired {
            java.arg(root.join("Root.java"));
        }
        run(&root, "javac", &mut java);
        let mut kotlin = Command::new(std::env::var_os("NOTLIN_KOTLINC").unwrap());
        kotlin
            .args(["-jvm-target", "17", "-classpath"])
            .arg(&classpath)
            .arg("-d")
            .arg(&classes)
            .args([root.join("Keep.kt"), root.join("Runner.kt")]);
        if !repaired {
            kotlin.arg(root.join("Root.kt"));
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
        let output = String::from_utf8(output.stdout).unwrap();
        assert_eq!(output.matches("calls=7").count(), 2, "{output}");
        assert_eq!(
            output.lines().filter(|line| *line == "true").count(),
            2,
            "{output}"
        );
        assert!(
            output.contains("null|NullPointerException|null|null|kotlin.Unit|NullPointerException|null|NullPointerException"),
            "{output}"
        );
        if let Some(original) = &original {
            assert_eq!(&output, original);
        } else {
            original = Some(output);
        }
    }
    assert!(base.starts_with(std::env::temp_dir()));
    fs::remove_dir_all(base).unwrap();
}
