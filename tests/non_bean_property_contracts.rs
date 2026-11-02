use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
const ROOT: &str = r#"@file:Suppress("INAPPLICABLE_JVM_NAME")
package sample
interface Root {
    @get:kotlin.jvm.JvmName("readValue")
    @set:kotlin.jvm.JvmName("writeValue")
    var value: String
    fun decorated(): String = "[" + value + "]"
}
interface ReadRoot {
    @get:kotlin.jvm.JvmName("readOnly")
    val value: String
}
"#;
const STORED: &str = r#"@file:Suppress("INAPPLICABLE_JVM_NAME")
package sample
class Stored(
    @get:kotlin.jvm.JvmName("readValue") @set:kotlin.jvm.JvmName("writeValue")
    override var value: String
): Root { suspend fun keep() {} }
class MutableStored(@get:kotlin.jvm.JvmName("readOnly") override var value: String): ReadRoot {
    suspend fun keep() {}
}
data class DataStored(
    @get:kotlin.jvm.JvmName("readValue") @set:kotlin.jvm.JvmName("writeValue")
    override var value: String
): Root { suspend fun keep() {} }
class Immediate(
    @get:kotlin.jvm.JvmName("readValue") @set:kotlin.jvm.JvmName("writeValue")
    override var value: String
): Root
"#;
const CALLER: &str = r#"package sample
class Caller {
    fun read(root: Root): String = root.value
    fun write(root: Root, value: String) { root.value = value }
    suspend fun keep() {}
}
class Use {
    fun read(root: Root): String = root.value
    fun write(root: Root, value: String) { root.value = value }
}
"#;
const RUNNER: &str = r#"package sample;
public class Runner {
 public static void main(String[] args) throws Exception {
  Root root = new Stored("old");
  Caller caller = new Caller();
  Use use = new Use();
  System.out.println(root.readValue()+":"+root.decorated()+":"+caller.read(root)+":"+use.read(root));
  caller.write(root,"one");
  use.write(root,"two");
  System.out.println(root.readValue()+":"+root.decorated());
  MutableStored mutable = new MutableStored("first");
  mutable.setValue("second");
  System.out.println(((ReadRoot)mutable).readOnly());
  System.out.println(new Immediate("now").readValue());
  System.out.println(java.lang.reflect.Modifier.isPrivate(Stored.class.getDeclaredField("value").getModifiers()));
  System.out.println(java.lang.reflect.Modifier.isPrivate(MutableStored.class.getDeclaredField("value").getModifiers()));
  DataStored data = new DataStored("data");
  data.writeValue("changed");
  System.out.println(data.readValue()+":"+data.copy("copied").readValue()+":"+java.lang.reflect.Modifier.isPrivate(DataStored.class.getDeclaredField("value").getModifiers()));
  for(com.fasterxml.jackson.databind.ObjectMapper mapper : new com.fasterxml.jackson.databind.ObjectMapper[]{
    new com.fasterxml.jackson.databind.ObjectMapper(),
    new com.fasterxml.jackson.databind.ObjectMapper().registerModule(new com.fasterxml.jackson.module.kotlin.KotlinModule.Builder().build())
  }) {
   System.out.println(mapper.writeValueAsString(root)+":"+mapper.writeValueAsString(mutable)+":"+mapper.writeValueAsString(new Immediate("json"))+":"+mapper.writeValueAsString(data));
  }
 }
}
"#;

fn fixture(label: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "non-bean-contracts-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
    assert!(!root.exists());
    fs::create_dir_all(&root).unwrap();
    for (name, text) in [
        ("Root.kt", ROOT),
        ("Stored.kt", STORED),
        ("Caller.kt", CALLER),
        ("Runner.java", RUNNER),
    ] {
        fs::write(root.join(name), text).unwrap();
    }
    root
}

fn translate(root: &Path, lombok: bool) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command.current_dir(root).args([
        "--root",
        ".",
        "--in-place",
        "--jvm-facts",
        "off",
        "--annotations",
        "none",
    ]);
    if lombok {
        command.arg("--lombok");
    }
    let output = command.arg(".").output().unwrap();
    fs::write(root.join("translation.stdout"), &output.stdout).unwrap();
    fs::write(root.join("translation.stderr"), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{root:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in ["Root", "ReadRoot", "Use", "Immediate"] {
        assert!(
            root.join(format!("{name}.java")).exists(),
            "missing {name}: {root:?}"
        );
    }
    let stored = fs::read_to_string(root.join("Stored.kt")).unwrap();
    assert!(stored.contains("override fun readValue()"), "{stored}");
    assert!(stored.contains("override fun writeValue("), "{stored}");
    assert!(
        !stored.contains("@JvmField"),
        "non-bean storage must remain private: {stored}"
    );
    let caller = fs::read_to_string(root.join("Caller.kt")).unwrap();
    assert!(caller.contains("root.readValue()"), "{caller}");
    assert!(caller.contains("root.writeValue("), "{caller}");
}

#[test]
fn source_property_identity_survives_non_bean_interface_emission() {
    for (label, lombok) in [("plain", false), ("lombok", true)] {
        translate(&fixture(label), lombok);
    }
}

#[test]
fn file_suppression_binds_by_identity_and_stays_with_retained_kotlin() {
    for (label, annotation, import) in [
        ("qualified", "kotlin.Suppress", ""),
        ("imported", "Suppress", "import kotlin.Suppress\n"),
        ("alias", "Quiet", "import kotlin.Suppress as Quiet\n"),
    ] {
        let root = fixture(label);
        for name in ["Root.kt", "Stored.kt"] {
            let source = fs::read_to_string(root.join(name))
                .unwrap()
                .replacen("@file:Suppress", &format!("@file:{annotation}"), 1)
                .replacen("package sample\n", &format!("package sample\n{import}"), 1);
            fs::write(root.join(name), source).unwrap();
        }
        translate(&root, false);
        let retained = fs::read_to_string(root.join("Stored.kt")).unwrap();
        assert!(
            retained.starts_with(&format!("@file:{annotation}(")),
            "{retained}"
        );
    }
    let root = fixture("foreign");
    fs::write(
        root.join("Root.kt"),
        ROOT.replacen("@file:Suppress", "@file:custom.Suppress", 1),
    )
    .unwrap();
    fs::write(root.join("Foreign.kt"), "package custom\n@Target(AnnotationTarget.FILE)\nannotation class Suppress(val value: String)\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root)
        .args([
            "--root",
            ".",
            "--in-place",
            "--jvm-facts",
            "off",
            "--annotations",
            "none",
            ".",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.join("Root.java").exists());
    assert!(
        fs::read_to_string(root.join("Root.kt"))
            .unwrap()
            .starts_with("@file:custom.Suppress(")
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib/reflect, Jackson and Lombok in NOTLIN_* variables"]
fn real_toolchain_preserves_non_bean_interface_storage_dispatch_and_setters() {
    use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
    for (mode, converted, lombok) in [
        ("original", false, false),
        ("plain", true, false),
        ("lombok", true, true),
    ] {
        let root = fixture(mode);
        if converted {
            translate(&root, lombok);
        }
        let mut config = JvmValidationConfig::new(
            std::env::var_os("NOTLIN_KOTLINC").unwrap(),
            std::env::var_os("NOTLIN_JAVAC").unwrap(),
            std::env::var_os("NOTLIN_JAVA").unwrap(),
            root.join("classes"),
        );
        config
            .kotlinc_args
            .extend(["-jvm-target".into(), "17".into()]);
        config.javac_args.extend([
            "--release".into(),
            "17".into(),
            if lombok { "-proc:full" } else { "-proc:none" }.into(),
        ]);
        config
            .classpath
            .push(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap().into());
        config.classpath.push(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"),
        );
        for key in [
            "NOTLIN_KOTLIN_REFLECT",
            "NOTLIN_JACKSON_CORE",
            "NOTLIN_JACKSON_ANNOTATIONS",
            "NOTLIN_JACKSON_DATABIND",
            "NOTLIN_JACKSON_KOTLIN",
        ] {
            config.classpath.push(std::env::var_os(key).unwrap().into());
        }
        if lombok {
            config
                .classpath
                .push(std::env::var_os("NOTLIN_LOMBOK").unwrap().into());
        }
        for entry in fs::read_dir(&root).unwrap() {
            let file = entry.unwrap().path();
            match file.extension().and_then(|extension| extension.to_str()) {
                Some("java") => config.java_sources.push(file),
                Some("kt") => config.kotlin_sources.push(file),
                _ => (),
            }
        }
        config.run_main_class = Some("sample.Runner".into());
        let result = validate_jvm_sources(&config)
            .unwrap_or_else(|error| panic!("{mode} {root:?}: {error:?}"));
        fs::write(root.join("native-validation.log"), format!("{result:#?}")).unwrap();
        assert_eq!(
            result.commands.last().unwrap().stdout.replace("\r\n", "\n"),
            "old:[old]:old:old\ntwo:[two]\nsecond\nnow\ntrue\ntrue\nchanged:copied:true\n{}:{}:{}:{}\n{}:{}:{}:{}\n",
            "{mode}"
        );
    }
}
