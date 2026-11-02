use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "notlin-reflection-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const MODEL: &str = "package sample\ninterface Contract {val key:String}\ndata class Model(override val key:String,val isReady:Boolean,val enabled:Boolean):Contract\n";
const READER: &str = r#"package sample
import kotlin.reflect.KProperty1
import kotlin.reflect.full.memberProperties
object Reader {
    @JvmStatic fun same(a:Any,b:Any):Boolean {
        if(a::class!=b::class) return false
        return a::class.memberProperties.all {
            @Suppress("UNCHECKED_CAST")
            val property=it as KProperty1<Any,*>
            property.get(a)==property.get(b)
        }
    }
    @JvmStatic fun read(a:Any,name:String):Any? {
        @Suppress("UNCHECKED_CAST")
        val property=a::class.memberProperties.single {it.name==name} as KProperty1<Any,*>
        return property.get(a)
    }
    suspend fun retain() {}
}
class RetainedBean(val value:String) {
    var calls:Int=0
    val observed:String get() {calls++;return value}
    private val secret:String="private"
    suspend fun retain() {}
}
"#;

fn migrate(root: &Path, lombok: bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_notlin"));
    cmd.current_dir(root)
        .args(["--in-place", "--jvm-facts", "off"]);
    if lombok {
        cmd.arg("--lombok");
    }
    let result = cmd.arg(".").output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn fixture(root: &Path, reader: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("Model.kt"), MODEL).unwrap();
    fs::write(root.join("Reader.kt"), reader).unwrap();
}

#[test]
fn retained_property_readers_receive_only_used_java_getter_repairs() {
    let scratch = Scratch::new();
    for (label, reader) in [
        ("explicit", READER.to_string()),
        (
            "alias",
            READER
                .replace(
                    "import kotlin.reflect.KProperty1",
                    "import kotlin.reflect.KProperty1 as Property",
                )
                .replace("as KProperty1<", "as Property<"),
        ),
        (
            "qualified",
            READER
                .replace("import kotlin.reflect.KProperty1\n", "")
                .replace("as KProperty1<", "as kotlin.reflect.KProperty1<"),
        ),
    ] {
        let root = scratch.0.join(label);
        fixture(&root, &reader);
        migrate(&root, true);
        assert!(root.join("Model.java").is_file());
        let changed = fs::read_to_string(root.join("Reader.kt")).unwrap();
        assert_eq!(
            changed
                .matches("fun <T, R> notlinReadKotlinProperty")
                .count(),
            2,
            "{label}: {changed}"
        );
        assert!(!changed.contains("property.get(a)"), "{changed}");
        assert!(
            changed.contains("property.getter.call(receiver)"),
            "{changed}"
        );
        migrate(&root, true);
        assert_eq!(
            fs::read_to_string(root.join("Reader.kt")).unwrap(),
            changed,
            "idempotence: {label}"
        );
    }
    let root = scratch.0.join("unused");
    fixture(
        &root,
        &READER
            .replace("property.get(a)==property.get(b)", "a==b")
            .replace("return property.get(a)", "return a"),
    );
    migrate(&root, true);
    assert!(
        !fs::read_to_string(root.join("Reader.kt"))
            .unwrap()
            .contains("notlinReadKotlinProperty")
    );
    let root = scratch.0.join("foreign");
    fixture(
        &root,
        "package sample\nclass KProperty1 {fun get(a:Any)=a}\nobject Reader { fun read(property:KProperty1,a:Any):Any {return property.get(a)}\nsuspend fun retain() {} }\n",
    );
    migrate(&root, true);
    assert!(
        !fs::read_to_string(root.join("Reader.kt"))
            .unwrap()
            .contains("notlinReadKotlinProperty")
    );
    let root = scratch.0.join("no-java-models");
    fixture(&root, READER);
    fs::write(
        root.join("Model.kt"),
        MODEL.replace(":Contract\n", ":Contract {suspend fun retain() {}}\n"),
    )
    .unwrap();
    migrate(&root, true);
    assert!(!root.join("Model.java").exists());
    assert!(
        !fs::read_to_string(root.join("Reader.kt"))
            .unwrap()
            .contains("notlinReadKotlinProperty")
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin reflect/stdlib, JSpecify and Lombok jars"]
fn real_toolchain_preserves_comparisons_getters_effects_and_private_access() {
    use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
    let scratch = Scratch::new();
    let mut expected = None;
    for (label, translated, lombok) in [
        ("original", false, false),
        ("plain", true, false),
        ("lombok", true, true),
    ] {
        let root = scratch.0.join(label);
        fixture(&root, READER);
        fs::write(
            root.join("Foreign.java"),
            "package sample; public class Foreign {private final String secret=\"hidden\";}",
        )
        .unwrap();
        fs::write(
            root.join("Runner.java"),
            r#"package sample;
public class Runner {
 public static void main(String[] args) {
  Model model=new Model("key",true,false);
  System.out.println(Reader.same(model,new Model("key",true,false)));
  System.out.println(Reader.same(model,new Model("changed",true,false)));
  System.out.println(Reader.same(model,new Model("key",false,false)));
  System.out.println(Reader.same(model,new Model("key",true,true)));
  RetainedBean bean=new RetainedBean("observed");
  System.out.println(Reader.read(bean,"observed"));
  System.out.println(bean.getCalls());
  for(Object value:new Object[]{bean,new Foreign()}) {
   try {Reader.read(value,"secret");System.out.println("unexpected access");}
   catch(Exception error) {System.out.println(error.getClass().getName());}
  }
 }
}
"#,
        )
        .unwrap();
        if translated {
            migrate(&root, lombok);
        }
        let mut cfg = JvmValidationConfig::new(
            std::env::var_os("NOTLIN_KOTLINC").unwrap(),
            std::env::var_os("NOTLIN_JAVAC").unwrap(),
            std::env::var_os("NOTLIN_JAVA").unwrap(),
            root.join("classes"),
        );
        cfg.kotlinc_args.extend([
            "-jvm-target".into(),
            "17".into(),
            "-jvm-default=no-compatibility".into(),
        ]);
        cfg.javac_args.extend([
            "--release".into(),
            "17".into(),
            if lombok { "-proc:full" } else { "-proc:none" }.into(),
        ]);
        cfg.classpath.extend([
            PathBuf::from(std::env::var_os("NOTLIN_JSPECIFY").unwrap()),
            PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap()),
            PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_REFLECT").unwrap()),
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"),
        ]);
        if lombok {
            cfg.classpath
                .push(std::env::var_os("NOTLIN_LOMBOK").unwrap().into());
        }
        for entry in fs::read_dir(&root).unwrap() {
            let file = entry.unwrap().path();
            match file.extension().and_then(|s| s.to_str()) {
                Some("kt") => cfg.kotlin_sources.push(file),
                Some("java") => cfg.java_sources.push(file),
                _ => {}
            }
        }
        cfg.run_main_class = Some("sample.Runner".into());
        let result =
            validate_jvm_sources(&cfg).unwrap_or_else(|error| panic!("{label}: {error:?}"));
        let output = result.commands.last().unwrap().stdout.replace("\r\n", "\n");
        assert_eq!(
            output,
            "true\nfalse\nfalse\nfalse\nobserved\n1\nkotlin.reflect.full.IllegalCallableAccessException\nkotlin.reflect.full.IllegalCallableAccessException\n",
            "{label}"
        );
        if let Some(expected) = &expected {
            assert_eq!(&output, expected, "{label}");
        } else {
            expected = Some(output);
        }
    }
}
