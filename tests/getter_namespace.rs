use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
static NEXT: AtomicUsize = AtomicUsize::new(0);

const ROOT: &str = "package sample\ninterface Root { var id:Any }\n";
const BASE: &str = "package sample\nimport jakarta.persistence.MappedSuperclass\n@MappedSuperclass abstract class Base(override var id:Any):Root\n";
const CHILD: &str = "package sample\nclass Before\nopen class Child(id:Any):Base(id) {var calls:Int=0\nfun getId():String {calls++;return id.toString()+\"/narrow\"}\nsuspend fun keep() {} }\nclass After\n";
const LEAF: &str =
    "package sample\nclass Leaf(id:Any):Child(id)\nclass Unrelated(val label:String)\n";
const CALLER: &str = "package sample\nclass Caller { fun read(child:Child,base:Base):String=base.id.toString()+\"/\"+child.getId()\nsuspend fun keep() {} }\n";
const JAVA_CALLER: &str =
    "package sample\nclass JavaCaller {fun read(child:Child):String=child.getId()}\n";
const PROBE: &str = "package sample; public class Probe {public static String read(Child child,Base base){return base.getId()+\"/\"+child.getId();}}\n";
const RUNNER: &str = "package sample\nfun main(){val child:Child=Leaf(\"wide\");val base:Base=child;println(Caller().read(child,base));println(JavaCaller().read(child));println(Probe.read(child,base));base.id=\"set\";println(Caller().read(child,base));println(Probe.read(child,base));println(child.calls)}\n";
const ANNOTATION: &str = "package jakarta.persistence; @java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE) public @interface MappedSuperclass {}\n";

fn root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "notlin-getter-namespace-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    assert!(!root.exists());
    root
}
fn write_case(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in [
        ("Root.kt", ROOT),
        ("Base.kt", BASE),
        ("Child.kt", CHILD),
        ("Leaf.kt", LEAF),
        ("Caller.kt", CALLER),
        ("JavaCaller.kt", JAVA_CALLER),
        ("Probe.java", PROBE),
        ("Runner.kt", RUNNER),
        ("MappedSuperclass.java", ANNOTATION),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
}
fn run(root: &Path, name: &str, cmd: &mut Command) -> Output {
    let output = cmd.output().unwrap();
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
    cmd.current_dir(root).args([
        "--root",
        ".",
        "--in-place",
        "--jvm-facts",
        "off",
        "--annotations",
        "none",
    ]);
    if lombok {
        cmd.arg("--lombok");
    }
    run(
        root,
        "translation",
        cmd.args([
            "Root.kt",
            "Base.kt",
            "Child.kt",
            "Leaf.kt",
            "Caller.kt",
            "JavaCaller.kt",
        ]),
    );
    assert!(root.join("Root.java").is_file(), "{root:?}");
    assert!(root.join("JavaCaller.java").is_file(), "{root:?}");
    assert!(root.join("Before.java").is_file());
    assert!(root.join("After.java").is_file());
    assert!(root.join("Leaf.kt").is_file());
    assert!(!root.join("Leaf.java").exists());
    assert!(root.join("Unrelated.java").is_file());
    let child = fs::read_to_string(root.join("Child.kt")).unwrap();
    let caller = fs::read_to_string(root.join("Caller.kt")).unwrap();
    assert!(child.contains("@kotlin.jvm.JvmName(\"getId\")"), "{child}");
    assert!(child.contains("fun notlinGetId()"), "{child}");
    assert!(caller.contains("child.notlinGetId()"), "{caller}");
    assert_eq!(fs::read_to_string(root.join("Runner.kt")).unwrap(), RUNNER);
}

#[test]
fn fresh_conversion_splits_getter_names_before_property_calls_are_rewritten() {
    let root = root("plans");
    write_case(&root);
    translate(&root, false);
    assert!(root.starts_with(std::env::temp_dir()));
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, all-open plugin and Lombok in NOTLIN_* variables"]
fn real_toolchain_preserves_wide_and_narrow_getters_for_kotlin_and_java_callers() {
    let base = root("native");
    let jars = ["NOTLIN_KOTLIN_STDLIB", "NOTLIN_LOMBOK"]
        .map(|key| PathBuf::from(std::env::var_os(key).unwrap()));
    let mut original = None;
    for (label, translated, lombok, all_open) in [
        ("original", false, false, false),
        ("plain", true, false, false),
        ("lombok", true, true, false),
        ("original-open", false, false, true),
        ("plain-open", true, false, true),
        ("lombok-open", true, true, true),
    ] {
        let root = base.join(label);
        write_case(&root);
        if all_open {
            fs::write(
                root.join("Child.kt"),
                CHILD.replace(
                    "fun getId()",
                    "@kotlin.Suppress(names = [\"unused\"])\nfun getId()",
                ),
            )
            .unwrap();
        }
        if translated {
            translate(&root, lombok);
        }
        let classes = root.join("classes");
        fs::create_dir_all(&classes).unwrap();
        let cp = std::env::join_paths(std::iter::once(&classes).chain(jars.iter())).unwrap();
        let files = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|file| {
                file.extension()
                    .is_some_and(|ext| ext == "java" || ext == "kt")
            })
            .collect::<Vec<_>>();
        let mut kotlin = Command::new(std::env::var_os("NOTLIN_KOTLINC").unwrap());
        kotlin
            .args(["-jvm-target", "17", "-classpath"])
            .arg(&cp)
            .arg("-d")
            .arg(&classes);
        if all_open {
            kotlin.arg(format!("-Xplugin={}", PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_ALLOPEN_PLUGIN").unwrap()).display())).args(["-P", "plugin:org.jetbrains.kotlin.allopen:annotation=jakarta.persistence.MappedSuperclass"]);
        }
        run(&root, "kotlinc", kotlin.args(&files));
        run(
            &root,
            "javac",
            Command::new(std::env::var_os("NOTLIN_JAVAC").unwrap())
                .args(["--release", "17", "-proc:full", "-cp"])
                .arg(&cp)
                .arg("-d")
                .arg(&classes)
                .args(
                    files
                        .iter()
                        .filter(|file| file.extension().is_some_and(|ext| ext == "java")),
                ),
        );
        let output = run(
            &root,
            "runtime",
            Command::new(std::env::var_os("NOTLIN_JAVA").unwrap())
                .arg("-cp")
                .arg(&cp)
                .arg("sample.RunnerKt"),
        );
        let output = String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n");
        assert_eq!(
            output,
            "wide/wide/narrow\nwide/narrow\nwide/wide/narrow\nset/set/narrow\nset/set/narrow\n5\n",
            "{label}"
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
