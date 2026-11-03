use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "notlin-gradle-default-literals-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        assert!(self.0.is_absolute() && self.0.parent() == Some(std::env::temp_dir().as_path()));
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SOURCES: [(&str, &str); 4] = [
    (
        "Wide.kt",
        "package sample\ninterface Wide {fun getCode():CharSequence}\n",
    ),
    (
        "Source.kt",
        "package sample\n@Deprecated(\"retained metadata\") interface Source:Wide {val token:String;override fun getCode():String=token;fun decorate(value:String):String=\"<$value>\";fun <T> identity(value:T):T=value}\n",
    ),
    (
        "Subject.kt",
        "package sample\nenum class Subject(override val token:String):Source {ONE(\"enum\")}\n",
    ),
    (
        "Runner.kt",
        r#"package sample
class Legacy(override val token:String):Source {suspend fun retained(){}}
fun inspect(label:String,value:Source) {
    val wide:Wide=value
    println("$label:${value.token}:${value.getCode()}:${wide.getCode()}:${value.decorate(value.identity("x"))}")
    println(value.javaClass.getMethod("getCode").returnType.simpleName)
}
fun main() {inspect("enum",Subject.ONE);inspect("kotlin",Legacy("legacy"));println(Subject.values().map {it.token})}
"#,
    ),
];

fn populate(root: &Path, dollar: bool) {
    fs::create_dir_all(root).unwrap();
    for (name, text) in SOURCES {
        fs::write(root.join(name), text).unwrap();
    }
    let pattern = if dollar {
        "def pattern = $/jvmDefault = JvmDefaultMode.DISABLE; escaped closing $/$$; \\w+/$\ndef named = $/class ${typeName}/$"
    } else {
        r#"def matched = (source =~ /entry\(([\w.]+)\.class\)/)
def stripped = source.replaceAll(/(?m)\/\/.*$/, '')
def pattern = /jvmDefault = JvmDefaultMode.DISABLE/
def named = (source =~ /class\s+${typeName}\b/)"#
    };
    fs::write(
        root.join("build.gradle"),
        format!("plugins {{ id 'org.jetbrains.kotlin.jvm' version '2.3.21' }}\ndef typeName = 'Subject'\n{pattern}\n"),
    )
    .unwrap();
}

fn translate(root: &Path, lombok: bool, migrates: bool) {
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
    let output = command.args(["Wide.kt", "Subject.kt"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        root.join("Subject.java").is_file() == migrates,
        "enum migration must agree with the proven JVM-default mode: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(root.join("Subject.kt")).unwrap_or_default()
    );
    assert!(root.join("Wide.java").is_file());
    assert_eq!(root.join("Subject.kt").exists(), !migrates);
    for (name, source) in SOURCES
        .into_iter()
        .filter(|(name, _)| matches!(*name, "Source.kt" | "Runner.kt"))
    {
        assert_eq!(fs::read_to_string(root.join(name)).unwrap(), source);
    }
}

#[test]
fn enum_translation_survives_unrelated_groovy_pattern_literals() {
    let scratch = Scratch::new();
    for dollar in [false, true] {
        for lombok in [false, true] {
            let root = scratch.0.join(format!("{dollar}-{lombok}"));
            populate(&root, dollar);
            translate(&root, lombok, true);
        }
    }
}

fn run(root: &Path, output: PathBuf, mode: &str, lombok: bool) -> String {
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").unwrap(),
        std::env::var_os("NOTLIN_JAVAC").unwrap(),
        std::env::var_os("NOTLIN_JAVA").unwrap(),
        output,
    );
    config.kotlinc_args.extend([
        "-jvm-target".into(),
        "17".into(),
        format!("-jvm-default={mode}").into(),
    ]);
    config.javac_args.extend(["--release".into(), "17".into()]);
    config
        .classpath
        .push(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap().into());
    if lombok {
        let jar = std::env::var_os("NOTLIN_LOMBOK").unwrap();
        config.classpath.push(jar.clone().into());
        config.javac_args.extend([
            "-processorpath".into(),
            jar,
            "-processor".into(),
            "lombok.launch.AnnotationProcessorHider$AnnotationProcessor".into(),
        ]);
        let mut plugin = std::ffi::OsString::from("-Xplugin=");
        plugin.push(std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").unwrap());
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
    let report = validate_jvm_sources(&config)
        .unwrap_or_else(|error| panic!("{mode}, lombok={lombok}: {error:?}"));
    report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n")
}

#[test]
#[ignore = "requires the Kotlin compiler, JDK, Kotlin stdlib and Lombok in NOTLIN_* variables"]
fn enum_default_dispatch_matches_kotlin_in_modern_and_legacy_jvm_modes() {
    let scratch = Scratch::new();
    for (mode, declared) in [
        ("enable", "ENABLE"),
        ("no-compatibility", "NO_COMPATIBILITY"),
        ("disable", "DISABLE"),
    ] {
        let configure = |root: &Path, dollar| {
            populate(root, dollar);
            let path = root.join("build.gradle");
            let text = fs::read_to_string(&path).unwrap();
            fs::write(path, format!("{text}\nkotlin {{ compilerOptions {{ jvmDefault = JvmDefaultMode.{declared} }} }}\n")).unwrap();
        };
        let original = scratch.0.join(format!("original-{mode}"));
        configure(&original, false);
        let expected = run(&original, original.join("classes"), mode, false);
        assert_eq!(
            expected,
            "enum:enum:enum:enum:<x>\nString\nkotlin:legacy:legacy:legacy:<x>\nString\n[enum]"
        );
        for lombok in [false, true] {
            let root = scratch.0.join(format!("{mode}-{lombok}"));
            configure(&root, lombok);
            translate(&root, lombok, mode != "disable");
            assert_eq!(run(&root, root.join("classes"), mode, lombok), expected);
        }
    }
}

fn populate_independent_slots(root: &Path, narrow_property: bool, shape: &str, separated: bool) {
    fs::create_dir_all(root).unwrap();
    let (property_type, function_type) = if narrow_property {
        ("String", "Any")
    } else {
        ("Any", "String")
    };
    let owner_annotation = if separated {
        "@kotlin.Suppress(\"INAPPLICABLE_JVM_NAME\") "
    } else {
        ""
    };
    let getter_annotation = if separated {
        "@get:kotlin.jvm.JvmName(\"readValue\") "
    } else {
        ""
    };
    fs::write(root.join("Root.kt"), format!("package sample\n{owner_annotation}interface Root {{{getter_annotation}val value:{property_type}}}\n")).unwrap();
    fs::write(
        root.join("Functions.kt"),
        format!("package sample\ninterface Functions {{fun getValue():{function_type}}}\n"),
    )
    .unwrap();
    fs::write(root.join("Default.kt"), format!("package sample\ninterface Default:Root,Functions {{override fun getValue():{function_type}=\"method\"}}\n")).unwrap();
    let source = match shape {
        "enum" => format!(
            "{owner_annotation}enum class Choice({getter_annotation}override val value:{property_type}):Default {{ONE(\"property\")}}"
        ),
        "class" => format!(
            "{owner_annotation}class Choice({getter_annotation}override val value:{property_type}):Default"
        ),
        "inherited" => format!(
            "open class Stored(override val value:{property_type}):Root\nclass Choice:Stored(\"property\"),Default"
        ),
        _ => panic!("unknown shape"),
    };
    fs::write(
        root.join("Choice.kt"),
        format!("package sample\n{source}\n"),
    )
    .unwrap();
    fs::write(
        root.join("Plain.kt"),
        "package sample\nclass Plain(val text:String)\n",
    )
    .unwrap();
    let choice = match shape {
        "enum" => "Choice.ONE",
        "class" => "Choice(\"property\")",
        _ => "Choice()",
    };
    fs::write(root.join("Runner.kt"), format!("package sample\nfun main(){{val x={choice};println(\"${{x.value}}|${{x.getValue()}}|${{Plain(\"plain\").text}}\")}}\n")).unwrap();
    fs::write(
        root.join("build.gradle"),
        "plugins { id 'org.jetbrains.kotlin.jvm' version '2.3.21' }\n",
    )
    .unwrap();
}

fn translate_independent_slots(root: &Path, lombok: bool, migrates: bool) {
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
    let result = command.args(["Choice.kt", "Plain.kt"]).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        root.join("Plain.java").is_file(),
        "unrelated declarations must still translate"
    );
    assert_eq!(
        root.join("Choice.java").exists(),
        migrates,
        "distinct property/function descriptors require coordinated naming: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn use_distinct_function_name(root: &Path) {
    for name in ["Functions.kt", "Default.kt", "Runner.kt"] {
        let path = root.join(name);
        fs::write(
            &path,
            fs::read_to_string(&path)
                .unwrap()
                .replace("getValue", "getCode"),
        )
        .unwrap();
    }
}

#[test]
fn independent_interface_getters_keep_their_roles_on_enums_and_classes() {
    let scratch = Scratch::new();
    for shape in ["enum", "class", "inherited"] {
        for narrow_property in [false, true] {
            for lombok in [false, true] {
                let root = scratch
                    .0
                    .join(format!("{shape}-{narrow_property}-{lombok}"));
                populate_independent_slots(&root, narrow_property, shape, false);
                translate_independent_slots(&root, lombok, false);
            }
        }
    }
    for shape in ["enum", "class"] {
        for lombok in [false, true] {
            let root = scratch.0.join(format!("separated-{shape}-{lombok}"));
            populate_independent_slots(&root, false, shape, true);
            translate_independent_slots(&root, lombok, false);
            let root = scratch.0.join(format!("distinct-{shape}-{lombok}"));
            populate_independent_slots(&root, false, shape, true);
            use_distinct_function_name(&root);
            translate_independent_slots(&root, lombok, true);
        }
    }
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib and Lombok in NOTLIN_* variables"]
fn getter_slot_roles_match_original_execution_after_partial_migration() {
    let scratch = Scratch::new();
    for shape in ["enum", "class", "inherited"] {
        for narrow_property in [false, true] {
            let modes: &[&str] = if shape == "enum" {
                &["enable", "disable", "no-compatibility"]
            } else {
                &["enable"]
            };
            for mode in modes {
                let original = scratch
                    .0
                    .join(format!("original-{shape}-{narrow_property}-{mode}"));
                populate_independent_slots(&original, narrow_property, shape, false);
                let expected = run(&original, original.join("classes"), mode, false);
                assert_eq!(expected, "property|method|plain");
                for lombok in [false, true] {
                    let mixed = scratch
                        .0
                        .join(format!("mixed-{shape}-{narrow_property}-{mode}-{lombok}"));
                    populate_independent_slots(&mixed, narrow_property, shape, false);
                    translate_independent_slots(&mixed, lombok, false);
                    assert_eq!(run(&mixed, mixed.join("classes"), mode, lombok), expected);
                }
            }
        }
    }
    for shape in ["enum", "class"] {
        let original = scratch.0.join(format!("separated-original-{shape}"));
        populate_independent_slots(&original, false, shape, true);
        let expected = run(&original, original.join("classes"), "enable", false);
        assert_eq!(expected, "property|method|plain");
        for lombok in [false, true] {
            let mixed = scratch.0.join(format!("separated-mixed-{shape}-{lombok}"));
            populate_independent_slots(&mixed, false, shape, true);
            translate_independent_slots(&mixed, lombok, false);
            assert_eq!(
                run(&mixed, mixed.join("classes"), "enable", lombok),
                expected
            );
        }
    }
    for shape in ["enum", "class"] {
        let original = scratch.0.join(format!("distinct-original-{shape}"));
        populate_independent_slots(&original, false, shape, true);
        use_distinct_function_name(&original);
        let expected = run(&original, original.join("classes"), "enable", false);
        assert_eq!(expected, "property|method|plain");
        for lombok in [false, true] {
            let mixed = scratch.0.join(format!("distinct-mixed-{shape}-{lombok}"));
            populate_independent_slots(&mixed, false, shape, true);
            use_distinct_function_name(&mixed);
            translate_independent_slots(&mixed, lombok, true);
            assert_eq!(
                run(&mixed, mixed.join("classes"), "enable", lombok),
                expected
            );
        }
    }
}

fn populate_storage_alias(root: &Path, explicit: bool) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("Root.java"), "package sample; public interface Root { Object readId(); default Object getId(){return readId();} void writeId(Object value); default void setId(Object value){writeId(value);} }\n").unwrap();
    let field = if explicit { "this.id" } else { "id" };
    fs::write(root.join("Base.kt"), format!("package sample\n@kotlin.Suppress(\"INAPPLICABLE_JVM_NAME\")\nopen class Base(@get:kotlin.jvm.JvmName(\"readId\") @set:kotlin.jvm.JvmName(\"writeId\") private var id:Any):Root {{\n override fun readId():Any={field}\n override fun writeId(value:Any){{{field}=value}}\n override fun getId():Any=readId()\n suspend fun retained(){{}}\n}}\nclass Child:Base(\"stored\") {{ override fun getId():String=\"${{readId()}}/lookup\" }}\n")).unwrap();
    fs::write(
        root.join("Plain.kt"),
        "package sample\nclass Plain(val text:String)\n",
    )
    .unwrap();
    fs::write(root.join("Runner.kt"), "package sample\nfun main(){val x=Child();val root:Root=x;println(\"${x.readId()}|${root.getId()}|${Plain(\"plain\").text}\");root.setId(\"changed\");println(\"${x.readId()}|${root.getId()}\")}\n").unwrap();
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib and Lombok in NOTLIN_* variables"]
fn renamed_storage_getters_and_setters_preserve_original_dispatch() {
    let scratch = Scratch::new();
    for explicit in [false, true] {
        let original = scratch.0.join(format!("storage-original-{explicit}"));
        populate_storage_alias(&original, explicit);
        let expected = run(&original, original.join("classes"), "enable", false);
        assert_eq!(
            expected,
            "stored|stored/lookup|plain\nchanged|changed/lookup"
        );
        for lombok in [false, true] {
            let mixed = scratch.0.join(format!("storage-mixed-{explicit}-{lombok}"));
            populate_storage_alias(&mixed, explicit);
            let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
            command.current_dir(&mixed).args([
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
            assert!(command.arg("Plain.kt").output().unwrap().status.success());
            assert!(mixed.join("Plain.java").exists());
            let index = notlin::workspace::SourceIndex::discover(&mixed).unwrap();
            let path = mixed.join("Base.kt");
            let source = fs::read_to_string(&path).unwrap();
            let contract = notlin::property_callsite::PropertyAccessorContract {
                owner_type: "sample.Root".into(),
                property: "id".into(),
                getter: "getId".into(),
                setter: Some("setId".into()),
            };
            let (updated, count) =
                notlin::property_callsite::rewrite_file(&index, &path, &source, &[contract]);
            assert_eq!(
                count, 0,
                "storage bodies must not call their virtual aliases: {updated}"
            );
            fs::write(&path, updated).unwrap();
            assert_eq!(
                run(&mixed, mixed.join("classes"), "enable", lombok),
                expected
            );
        }
    }
}
