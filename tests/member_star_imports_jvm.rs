//! Compare source member-star imports with Kotlin's actual factory binding.
use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "notlin-member-stars-{}-{nonce}",
            std::process::id()
        )))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SOURCES: &[(&str, &str)] = &[
    (
        "Choice.java",
        "package lib; public enum Choice {FIRST,LAST}",
    ),
    (
        "Parent.java",
        "package lib; public class Parent {public static final String LABEL=\"unused\";}",
    ),
    (
        "Constants.java",
        "package lib; public class Constants extends Parent {}",
    ),
    (
        "Outer.java",
        "package lib; public class Outer {public static class Flags {public static final int FLAG=1;}}",
    ),
    (
        "Factory.java",
        "package lib; public class Factory {public static String mapOf(){return \"method\";}}",
    ),
    (
        "Callable.java",
        "package lib; public class Callable {public static final kotlin.jvm.functions.Function0<String> mapOf=()->\"field\";}",
    ),
    (
        "Reader.kt",
        r#"package sample
import lib.Choice.*
import lib.Constants.*
import lib.Outer.Flags.*
class Reader {
    fun empty(): Map<String,Int> = mapOf()
    fun populated(): Map<String,Int> = mapOf("a" to 1,"b" to 2)
    fun sequence(): List<Int> = listOf(1,2)
    fun distinct(): Set<Int> = setOf(1,1,2)
}
"#,
    ),
    (
        "Defaults.kt",
        r#"package sample
import lib.Choice.*
import lib.Constants.*
data class Defaults(val values: Map<String,Int> = mapOf())
"#,
    ),
    (
        "MethodShadow.kt",
        "package sample\nimport lib.Factory.*\nclass MethodShadow {fun value():String=mapOf()}\n",
    ),
    (
        "FieldShadow.kt",
        "package sample\nimport lib.Callable.*\nclass FieldShadow {fun value():String=mapOf()}\n",
    ),
    (
        "Runner.kt",
        r#"package sample
import lib.Choice.*
fun main() {
    val reader=Reader()
    println("${reader.empty().size}|${reader.populated()}|${reader.sequence()}|${reader.distinct()}|${Defaults().values.size}|${FIRST.name}")
    println("${MethodShadow().value()}|${FieldShadow().value()}")
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

fn translate(root: &Path, lombok: bool) {
    let binary = std::env::var_os("NOTLIN_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
    let mut command = Command::new(binary);
    command
        .current_dir(root)
        .args(["--root", ".", "--in-place"]);
    if lombok {
        command.arg("--lombok");
    }
    let output = command
        .args([
            "Reader.kt",
            "Defaults.kt",
            "MethodShadow.kt",
            "FieldShadow.kt",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in ["Reader", "Defaults"] {
        assert!(
            root.join(format!("{name}.java")).exists(),
            "{name} must translate: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(root.join(format!("{name}.kt"))).unwrap_or_default()
        );
    }
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
        .extend(["-jvm-target".into(), "17".into()]);
    config.javac_args.extend(["--release".into(), "17".into()]);
    config.classpath.push(
        std::env::var_os("NOTLIN_KOTLIN_STDLIB")
            .expect("NOTLIN_KOTLIN_STDLIB")
            .into(),
    );
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
    for file in fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
    {
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
fn unrelated_source_member_stars_allow_standard_collection_factories_in_both_output_modes() {
    let scratch = Scratch::new();
    for lombok in [false, true] {
        let root = scratch.0.join(format!("candidate-{lombok}"));
        populate(&root);
        translate(&root, lombok);
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin compiler plugin"]
fn real_toolchain_preserves_imported_method_and_field_bindings_and_standard_factories() {
    fn run(root: &Path, classes: PathBuf, lombok: bool) -> String {
        validate_jvm_sources(&config(root, classes, lombok))
            .expect("compile and run mixed sources")
            .commands
            .last()
            .unwrap()
            .stdout
            .trim()
            .replace("\r\n", "\n")
    }
    let scratch = Scratch::new();
    let original = scratch.0.join("original");
    populate(&original);
    let expected = run(&original, scratch.0.join("original-classes"), false);
    assert_eq!(expected, "0|{a=1, b=2}|[1, 2]|[1, 2]|0|FIRST\nmethod|field");
    for lombok in [false, true] {
        let root = scratch.0.join(format!("mixed-{lombok}"));
        populate(&root);
        translate(&root, lombok);
        assert_eq!(
            run(
                &root,
                scratch.0.join(format!("mixed-classes-{lombok}")),
                lombok
            ),
            expected
        );
    }
}

fn populate_call_families(root: &Path, mode: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("build.gradle"),
        format!(
            "plugins {{ id 'org.jetbrains.kotlin.jvm' version '2.3.21' }}\nkotlin {{ compilerOptions {{ jvmDefault = JvmDefaultMode.{} }} }}\n",
            match mode {
                "enable" => "ENABLE",
                "no-compatibility" => "NO_COMPATIBILITY",
                _ => "DISABLE",
            }
        ),
    )
    .unwrap();
    for (name, source) in [
        (
            "NarrowParent.kt",
            "package sample\nopen class NarrowParent {fun value(input:Byte):Byte=input\nfun small(input:Short):Short=input\nfun add(input:Byte):Byte=input}\n",
        ),
        (
            "NarrowChild.kt",
            "package sample\nclass NarrowChild:NarrowParent(){fun run():Byte=super.value(127)\nfun minus():Byte=super.value(-128)\nfun shortValue():Short=super.small(32767)\nfun typed(value:Byte):Byte=super.value(value)}\n",
        ),
        (
            "NarrowCaller.kt",
            "package sample\nclass NarrowCaller {fun run(parent:NarrowParent):Byte=parent.add(127)}\n",
        ),
        ("Input.java", "package sample; public interface Input {}"),
        (
            "Payload.java",
            "package sample; public final class Payload implements Input {}",
        ),
        (
            "Root.kt",
            "package sample\nopen class Root {open fun value(input:Input):String=\"root\"\nfun value():String=\"zero\"}\n",
        ),
        (
            "Middle.kt",
            "package sample\nopen class Middle:Root(){override fun value(input:Input):String=\"middle\"}\n",
        ),
        (
            "Child.kt",
            "package sample\nclass Child:Middle(){fun run(value:Input):String=super.value(value)}\n",
        ),
        (
            "Overloaded.kt",
            "package sample\nopen class Overloaded {open fun value(input:String):String=\"string:$input\"\nfun value(input:Int):String=\"int:$input\"}\n",
        ),
        (
            "OverloadChild.kt",
            "package sample\nclass OverloadChild:Overloaded(){fun run():String=super.value(\"root\")}\n",
        ),
        (
            "Parent.kt",
            "package sample\ninterface Parent {fun value(input:Input):String=\"interface\"}\n",
        ),
        (
            "Left.kt",
            "package sample\ninterface Left {fun value(input:Input):String=\"left\"}\n",
        ),
        (
            "Right.kt",
            "package sample\ninterface Right {fun value(input:Input):String=\"right\"}\n",
        ),
        (
            "InterfaceChild.kt",
            "package sample\ninterface InterfaceChild:Parent {override fun value(value:Input):String=super.value(value)}\n",
        ),
        (
            "Qualified.kt",
            "package sample\nclass Qualified:Left,Right {override fun value(value:Input):String=super<Right>.value(value)}\n",
        ),
        (
            "Factory.kt",
            "package sample\nimport lib.Choice.*\nclass Factory {companion object {fun of(input:Int):String=\"int:$input\"\nfun of(input:Long):String=\"long:$input\"\nfun of(input:String):String=\"string:$input\"}}\n",
        ),
        (
            "Choice.java",
            "package lib; public enum Choice {FIRST,LAST}",
        ),
        (
            "Numbers.kt",
            "package sample\nclass Numbers {fun intValue():String=Factory.of(95)\nfun longValue():String=Factory.of(95L)\nfun large():String=Factory.of(2147483648)\nfun hex():String=Factory.of(0x5F)\nfun binary():String=Factory.of(0b101)\nfun min():Int = -2147483648}\n",
        ),
        (
            "Runner.kt",
            "package sample\nfun main(){val input=Payload()\nprintln(Child().run(input)+\"|\"+OverloadChild().run()+\"|\"+object:InterfaceChild{}.value(input)+\"|\"+Qualified().value(input))\nval n=Numbers()\nprintln(\"${n.intValue()}|${n.longValue()}|${n.large()}|${n.hex()}|${n.binary()}|${n.min()}\")\nval narrow=NarrowChild()\nprintln(\"${narrow.run()}|${narrow.minus()}|${narrow.shortValue()}|${narrow.typed(6)}|${NarrowCaller().run(NarrowParent())}\")}\n",
        ),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin compiler plugin"]
fn real_toolchain_preserves_argument_super_dispatch_and_integer_overloads() {
    let scratch = Scratch::new();
    for mode in ["disable", "enable", "no-compatibility"] {
        let run = |root: &Path, classes: PathBuf, lombok| {
            let mut config = config(root, classes, lombok);
            config
                .kotlinc_args
                .push(format!("-jvm-default={mode}").into());
            validate_jvm_sources(&config)
                .expect("compile and execute call families")
                .commands
                .last()
                .unwrap()
                .stdout
                .trim()
                .replace("\r\n", "\n")
        };
        let original = scratch.0.join(format!("family-original-{mode}"));
        populate_call_families(&original, mode);
        let expected = run(
            &original,
            scratch.0.join(format!("family-original-classes-{mode}")),
            false,
        );
        assert_eq!(
            expected,
            "middle|string:root|interface|right\nint:95|long:95|long:2147483648|int:95|int:5|-2147483648\n127|-128|32767|6|127"
        );
        for lombok in [false, true] {
            let root = scratch.0.join(format!("family-{mode}-{lombok}"));
            populate_call_families(&root, mode);
            let binary = std::env::var_os("NOTLIN_TEST_BINARY")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
            let mut command = Command::new(binary);
            command
                .current_dir(&root)
                .args(["--root", ".", "--in-place"]);
            if lombok {
                command.arg("--lombok");
            }
            let result = command
                .args([
                    "Child.kt",
                    "OverloadChild.kt",
                    "InterfaceChild.kt",
                    "Qualified.kt",
                    "Numbers.kt",
                    "NarrowChild.kt",
                    "NarrowCaller.kt",
                ])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            for name in [
                "Child",
                "OverloadChild",
                "Numbers",
                "NarrowChild",
                "NarrowCaller",
            ] {
                assert!(
                    root.join(format!("{name}.java")).exists(),
                    "{mode}/{lombok}: {name}: {}\n{}",
                    String::from_utf8_lossy(&result.stderr),
                    fs::read_to_string(root.join(format!("{name}.kt"))).unwrap_or_default()
                );
            }
            if mode != "disable" {
                for name in ["InterfaceChild", "Qualified"] {
                    assert!(
                        root.join(format!("{name}.java")).exists(),
                        "{mode}/{lombok}: {name}: {}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                }
            }
            assert_eq!(
                run(
                    &root,
                    scratch.0.join(format!("family-classes-{mode}-{lombok}")),
                    lombok
                ),
                expected
            );
        }
    }
}
