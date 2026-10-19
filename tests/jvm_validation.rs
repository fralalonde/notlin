use notlin::jvm_validation::{
    JvmValidationConfig, JvmValidationError, JvmValidationStage, validate_jvm_sources,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_explicit_super_method_dispatch() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("explicit super method dispatch");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    let sources = [
        (
            "Parent.java",
            "package boundary; public interface Parent { default String label() { return \"parent\"; } }\n",
        ),
        (
            "Child.kt",
            "package boundary\ninterface Child : Parent {\n    override fun label(): String = super.label() + \"/child\"\n}\n",
        ),
        (
            "Runner.kt",
            "package boundary\nclass KeptChild : Child {\n    suspend fun retainedHook() {}\n}\nfun main() {\n    suspend fun retainedHarness() {}\n    val child: Parent = KeptChild()\n    println(child.label() + \"|\" + Derived().label() + \"|\" + Constants.DEFAULT + \"|\" + StaticReader.read())\n}\n",
        ),
        (
            "Constants.kt",
            "package boundary\ninterface Constants {\n    companion object {\n        @JvmStatic\n        val DEFAULT: String = \"constant\"\n    }\n}\n",
        ),
        (
            "StaticReader.java",
            "package boundary; public class StaticReader { public static String read() { return Constants.getDEFAULT(); } }\n",
        ),
        (
            "Base.java",
            "package boundary; public class Base { public String label() { return \"base\"; } }\n",
        ),
        (
            "Derived.kt",
            "package boundary\nclass Derived : Base() {\n    override fun label(): String = super.label() + \"/derived\"\n}\n",
        ),
    ];
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        for (name, source) in sources {
            fs::write(root.join(name), source).unwrap();
        }
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.java_sources.push(original.join("Parent.java"));
    baseline.java_sources.push(original.join("Base.java"));
    baseline
        .java_sources
        .push(original.join("StaticReader.java"));
    baseline.kotlin_sources.extend([
        original.join("Child.kt"),
        original.join("Derived.kt"),
        original.join("Constants.kt"),
        original.join("Runner.kt"),
    ]);
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original super call compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        "parent/child|base/derived|constant|constant"
    );
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(&migrated)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("Child.java").is_file(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let child = fs::read_to_string(migrated.join("Child.java")).unwrap();
    assert!(child.contains("Parent.super.label()"), "{child}");
    let constants = fs::read_to_string(migrated.join("Constants.java")).unwrap();
    assert!(!constants.contains("@JvmStatic"), "{constants}");
    assert!(
        constants.contains("static String getDEFAULT()"),
        "{constants}"
    );
    let derived = fs::read_to_string(migrated.join("Derived.java")).unwrap();
    assert!(
        derived.contains("super.label()") && !derived.contains("Base.super"),
        "{derived}"
    );
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    for entry in fs::read_dir(&migrated).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(path),
            Some("java") => mixed.java_sources.push(path),
            _ => {}
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("migrated super call compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_member_call_before_unrelated_top_level_function() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("member before unrelated global");
    let root = scratch.0.join("sources");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Global.kt"),
        "package unrelated\nfun getData(): String = \"global\"\n",
    )
    .unwrap();
    fs::write(
        root.join("Box.kt"),
        r#"package boundary
import unrelated.getData
class Box(private val value: String) {
    fun getData(): String = value
    val label: String get() = getData()
    fun output(): String = label
}
fun globalLabel(): String = getData()
"#,
    )
    .unwrap();
    fs::write(
        root.join("Runner.kt"),
        r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    println(Box("member").output() + "|" + globalLabel())
}
"#,
    )
    .unwrap();
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources.extend([
        root.join("Global.kt"),
        root.join("Box.kt"),
        root.join("Runner.kt"),
    ]);
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original member dispatch compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        "member|global"
    );
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        root.join("Box.java").is_file(),
        "member caller must translate: {}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    for entry in fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(path),
            Some("java") => mixed.java_sources.push(path),
            _ => {}
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("mixed member dispatch compiles");
    assert_eq!(
        after.commands.last().unwrap().stdout.trim(),
        "member|global"
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_bounded_generic_property_bridges() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("bounded generic property bridges");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Identifier.kt"),
            "package boundary\ninterface Identifier\nclass ConcreteId : Identifier\n",
        )
        .unwrap();
        fs::write(
            root.join("RootApi.kt"),
            "package boundary\ninterface RootApi {\n    val identifier: Identifier\n}\n",
        )
        .unwrap();
        fs::write(root.join("GenericEvent.kt"), "package boundary\ninterface GenericEvent<S : Identifier> : RootApi {\n    override val identifier: S\n    suspend fun pending(): String\n}\n").unwrap();
        fs::write(root.join("KeptEvent.kt"), "package boundary\nclass KeptEvent(override val identifier: ConcreteId) : GenericEvent<ConcreteId> {\n    override suspend fun pending(): String = \"kept\"\n}\n").unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val expected = ConcreteId()
    val event: GenericEvent<ConcreteId> = KeptEvent(expected)
    val root: RootApi = event
    println("${event.identifier === expected}|${root.identifier === expected}")
}
"#,
        )
        .unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = fs::read_dir(&original)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before =
        validate_jvm_sources(&baseline).expect("original bounded generic property compiles");
    assert_eq!(before.commands.last().unwrap().stdout.trim(), "true|true");
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(&migrated)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("RootApi.java").is_file(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let retained = fs::read_to_string(migrated.join("GenericEvent.kt")).unwrap();
    assert!(retained.contains("fun getIdentifier(): S"), "{retained}");
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    for entry in fs::read_dir(&migrated).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(path),
            Some("java") => mixed.java_sources.push(path),
            _ => {}
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after =
        validate_jvm_sources(&mixed).expect("mixed bounded generic property bridge compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_nested_generic_constructor_defaults() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("nested generic constructor defaults");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    let sources = [
        (
            "Settings.kt",
            r#"package boundary
data class Settings<T> @JvmOverloads constructor(
    val value: T? = null
)
"#,
        ),
        (
            "Lines.kt",
            r#"package boundary
data class Lines<T> @JvmOverloads constructor(
    val settings: Settings<T>? = Settings(),
    val label: String? = null
)
"#,
        ),
        (
            "Bundle.kt",
            r#"package boundary
data class Bundle<T> @JvmOverloads constructor(
    val settings: Settings<T>? = Settings(),
    val label: String? = null,
    val entries: Map<String, T>? = null,
    val lines: Lines<T>? = Lines()
)
"#,
        ),
        (
            "Runner.kt",
            r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val bundle = Bundle<String>()
    println("${bundle.settings != null}|${bundle.lines != null}|${bundle.lines?.settings != null}|${bundle.lines?.label == null}")
}
"#,
        ),
    ];
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        for (name, source) in sources {
            fs::write(root.join(name), source).unwrap();
        }
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = ["Settings.kt", "Lines.kt", "Bundle.kt", "Runner.kt"]
        .into_iter()
        .map(|name| original.join(name))
        .collect();
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original nested generic defaults compile");
    let expected = "true|true|true|true";
    assert_eq!(before.commands.last().unwrap().stdout.trim(), expected);

    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(&migrated)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    for name in ["Settings", "Lines", "Bundle"] {
        assert!(
            migrated.join(format!("{name}.java")).is_file(),
            "{name} should translate: {}",
            String::from_utf8_lossy(&translation.stderr)
        );
    }

    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    for entry in fs::read_dir(&migrated).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(path),
            Some("java") => mixed.java_sources.push(path),
            _ => {}
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("mixed nested generic defaults compile");
    assert_eq!(after.commands.last().unwrap().stdout.trim(), expected);
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_instant_factory_and_inferred_local() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("instant factory");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Marker.kt"),
            "package helpers\ninterface Marker\n",
        )
        .unwrap();
        fs::write(
            root.join("ClockSource.kt"),
            r#"package boundary
import helpers.*
import java.time.Instant
class ClockSource {
    fun now(): Instant = Instant.now()
    fun local(): Instant {
        val instant = Instant.now()
        return instant
    }
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package boundary
import java.time.Instant
fun main() {
    val before = Instant.now()
    val source = ClockSource()
    val values = listOf(source.now(), source.local())
    val after = Instant.now()
    println(values.all { !it.isBefore(before) && !it.isAfter(after) })
}
"#,
        )
        .unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = ["Marker.kt", "ClockSource.kt", "Runner.kt"]
        .iter()
        .map(|name| original.join(name))
        .collect();
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original clock source compiles");
    assert_eq!(before.commands.last().unwrap().stdout.trim(), "true");
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(migrated.join("ClockSource.kt"))
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("ClockSource.java").is_file(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = vec![migrated.join("Marker.kt"), migrated.join("Runner.kt")];
    mixed.java_sources.push(migrated.join("ClockSource.java"));
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("translated clock source compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_locale_sensitive_string_case_conversion() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("locale sensitive string conversion");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Casing.kt"),
            r#"package boundary
import java.util.*
class Casing {
    fun lowerInvariant(value: String): String = value.lowercase()
    fun upperInvariant(value: String): String = value.uppercase()
    fun lowerDefault(value: String): String = value.lowercase(Locale.getDefault())
    fun upperDefault(value: String): String = value.uppercase(Locale.getDefault())
    fun lowerLocal(value: String): String {
        val locale = Locale.getDefault()
        return value.lowercase(locale)
    }
    fun lowerExplicit(value: String, locale: Locale): String = value.lowercase(locale)
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package boundary
import java.util.Locale
fun main() {
    Locale.setDefault(Locale.forLanguageTag("tr-TR"))
    val casing = Casing()
    val values = listOf(casing.lowerInvariant("TITLE"), casing.upperInvariant("title"),
        casing.lowerDefault("TITLE"), casing.upperDefault("title"),
        casing.lowerLocal("TITLE"), casing.lowerExplicit("TITLE", Locale.getDefault()))
    println(values.map { text -> text.map { it.code }.joinToString(",") }.joinToString("|"))
}
"#,
        )
        .unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = vec![original.join("Casing.kt"), original.join("Runner.kt")];
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original locale conversion compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        "116,105,116,108,101|84,73,84,76,69|116,305,116,108,101|84,304,84,76,69|116,305,116,108,101|116,305,116,108,101"
    );
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(migrated.join("Casing.kt"))
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let emitted = fs::read_to_string(migrated.join("Casing.java"))
        .expect("locale conversion class translates");
    assert!(
        emitted.contains("toLowerCase(java.util.Locale.ROOT)"),
        "{emitted}"
    );
    assert!(
        emitted.contains("toUpperCase(java.util.Locale.ROOT)"),
        "{emitted}"
    );
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources.push(migrated.join("Runner.kt"));
    mixed.java_sources.push(migrated.join("Casing.java"));
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("translated locale conversion compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_covariant_collection_getter_methods() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("covariant collection getter methods");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("BaseEntry.java"),
            "package boundary; public interface BaseEntry { String label(); }\n",
        )
        .unwrap();
        fs::write(root.join("ChildEntry.java"), "package boundary; public class ChildEntry implements BaseEntry { public String label() { return \"entry\"; } }\n").unwrap();
        fs::write(
            root.join("Api.kt"),
            "package boundary\ninterface Api {\n fun getItems(): List<BaseEntry>\n}\n",
        )
        .unwrap();
        fs::write(root.join("Batch.kt"), "package boundary\nclass Batch(private val values: List<ChildEntry>) : Api { override fun getItems(): List<ChildEntry> = values }\n").unwrap();
        fs::write(root.join("Runner.kt"), "package boundary\nfun main() {\n val entries = listOf(ChildEntry())\n val batch = Batch(entries)\n val api: Api = batch\n println(\"${batch.getItems().single().label()}|${api.getItems().single().label()}|${batch.getItems() === entries}|${api.getItems() === entries}\")\n}\n").unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = ["Api.kt", "Batch.kt", "Runner.kt"]
        .iter()
        .map(|name| original.join(name))
        .collect();
    baseline.java_sources = ["BaseEntry.java", "ChildEntry.java"]
        .iter()
        .map(|name| original.join(name))
        .collect();
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before =
        validate_jvm_sources(&baseline).expect("original covariant getter methods compile");
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(migrated.join("Api.kt"))
        .arg(migrated.join("Batch.kt"))
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let api = fs::read_to_string(migrated.join("Api.java")).expect("getter interface translates");
    let batch = fs::read_to_string(migrated.join("Batch.java"))
        .expect("narrow getter implementation translates");
    assert!(
        api.contains("List<? extends BaseEntry> getItems()"),
        "{api}"
    );
    assert!(batch.contains("List<ChildEntry> getItems()"), "{batch}");
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources.push(migrated.join("Runner.kt"));
    mixed.java_sources = [
        "Api.java",
        "Batch.java",
        "BaseEntry.java",
        "ChildEntry.java",
    ]
    .iter()
    .map(|name| migrated.join(name))
    .collect();
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed)
        .expect("translated generic getters compile with Kotlin caller");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        "entry|entry|true|true"
    );
    assert_eq!(
        before.commands.last().unwrap().stdout,
        after.commands.last().unwrap().stdout
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_parallel_readonly_collection_contracts() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("parallel readonly collection contracts");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("BaseEntry.java"),
            "package boundary; public interface BaseEntry { String label(); }\n",
        )
        .unwrap();
        fs::write(root.join("ChildEntry.java"), "package boundary; public class ChildEntry implements BaseEntry { public String label() { return \"entry\"; } }\n").unwrap();
        fs::write(
            root.join("NarrowApi.kt"),
            "package boundary\ninterface NarrowApi { val entries: List<ChildEntry> }\n",
        )
        .unwrap();
        fs::write(
            root.join("WideApi.kt"),
            "package boundary\ninterface WideApi { val entries: List<BaseEntry> }\n",
        )
        .unwrap();
        fs::write(root.join("Batch.kt"), "package boundary\nclass Batch(override val entries: List<ChildEntry>) : NarrowApi, WideApi { suspend fun kotlinOnly() {} }\n").unwrap();
        fs::write(root.join("Runner.kt"), "package boundary\nfun main() {\n val entries = listOf(ChildEntry())\n val batch = Batch(entries)\n val narrow: NarrowApi = batch\n val wide: WideApi = batch\n println(\"${narrow.entries.single().label()}|${wide.entries.single().label()}|${narrow.entries === entries}|${wide.entries === entries}\")\n}\n").unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = ["NarrowApi.kt", "WideApi.kt", "Batch.kt", "Runner.kt"]
        .iter()
        .map(|name| original.join(name))
        .collect();
    baseline.java_sources = ["BaseEntry.java", "ChildEntry.java"]
        .iter()
        .map(|name| original.join(name))
        .collect();
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before =
        validate_jvm_sources(&baseline).expect("original parallel collection contracts compile");
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(migrated.join("NarrowApi.kt"))
        .arg(migrated.join("WideApi.kt"))
        .arg(migrated.join("Batch.kt"))
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("NarrowApi.java").is_file(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("WideApi.java").is_file(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(migrated.join("Batch.kt").is_file());
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = ["Batch.kt", "Runner.kt"]
        .iter()
        .map(|name| migrated.join(name))
        .collect();
    mixed.java_sources = [
        "NarrowApi.java",
        "WideApi.java",
        "BaseEntry.java",
        "ChildEntry.java",
    ]
    .iter()
    .map(|name| migrated.join(name))
    .collect();
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed)
        .expect("parallel Java contracts and retained Kotlin implementation compile");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        "entry|entry|true|true"
    );
    assert_eq!(
        before.commands.last().unwrap().stdout,
        after.commands.last().unwrap().stdout
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_uuid_factory_defaults_and_inferred_receivers() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("UUID factory defaults and inferred receivers");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("Identifier.kt"), "package boundary\nimport java.util.*\nclass Identifier(val id: UUID = UUID.randomUUID()) {\n constructor(marker: String) : this(UUID.randomUUID())\n fun freshText(): String = UUID.randomUUID().toString()\n fun inferredText(): String { val value = UUID.randomUUID(); return value.toString() }\n}\n").unwrap();
        fs::write(root.join("InstanceFactory.kt"), "package boundary\nimport java.util.UUID\nfun UUID.randomUUID(): UUID = UUID(0, 0)\nclass KeptInstance(val id: UUID) {\n constructor(source: UUID, marker: Int) : this(source.randomUUID())\n}\n").unwrap();
        fs::write(root.join("Runner.kt"), "package boundary\nfun main() {\n suspend fun retainedHarness() {}\n val value = Identifier()\n val delegated = Identifier(\"fresh\")\n val fromExtension = KeptInstance(value.id, 1)\n println(\"${value.id.version()}|${value.freshText().length}|${value.inferredText().length}|${delegated.id.version()}|${delegated.id.variant()}|${fromExtension.id.version()}\")\n}\n").unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = ["Identifier.kt", "InstanceFactory.kt", "Runner.kt"]
        .iter()
        .map(|name| original.join(name))
        .collect();
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original UUID factory usage compiles");
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(&migrated)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("Identifier.java").is_file(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(!migrated.join("KeptInstance.java").exists());
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources.push(migrated.join("Runner.kt"));
    mixed
        .kotlin_sources
        .push(migrated.join("InstanceFactory.kt"));
    mixed.java_sources.push(migrated.join("Identifier.java"));
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed)
        .expect("translated UUID factories compile with Kotlin callers");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        "4|36|36|4|2|0"
    );
    assert_eq!(
        before.commands.last().unwrap().stdout,
        after.commands.last().unwrap().stdout
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_descendant_getter_dispatch_across_a_diamond() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("descendant getter diamond dispatch");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("ReferenceApi.java"),
            "package boundary; public interface ReferenceApi { String getReferenceNo(); }\n",
        )
        .unwrap();
        fs::write(root.join("Models.kt"), "package boundary\ninterface RootContext : ReferenceApi {}\ninterface NarrowContext : RootContext {\n override fun getReferenceNo(): String = \"specific\"\n}\ninterface SiblingContext : RootContext, ReferenceApi {}\ninterface NarrowBranch : NarrowContext {}\nclass Entry : NarrowBranch, SiblingContext {}\nclass Reader(val entry: Entry) {\n fun readReferenceNo(): String = entry.getReferenceNo()\n}\n").unwrap();
        fs::write(root.join("Runner.kt"), "package boundary\nfun main() {\n val entry = Entry()\n val api: ReferenceApi = entry\n println(\"${Reader(entry).readReferenceNo()}|${api.getReferenceNo()}\")\n}\n").unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = ["Models.kt", "Runner.kt"]
        .iter()
        .map(|name| original.join(name))
        .collect();
    baseline
        .java_sources
        .push(original.join("ReferenceApi.java"));
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original mixed getter diamond compiles");
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(migrated.join("Models.kt"))
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("Reader.java").is_file(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    for file in fs::read_dir(&migrated)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
    {
        match file.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(file),
            Some("java") => mixed.java_sources.push(file),
            _ => {}
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("translated getter diamond compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        "specific|specific"
    );
    assert_eq!(
        before.commands.last().unwrap().stdout,
        after.commands.last().unwrap().stdout
    );
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        Self::new_under(&std::env::temp_dir(), label)
    }

    fn new_under(base: &Path, label: &str) -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!(
            "notlin jvm validation {label} {} {id}",
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

fn fake_tool(dir: &Path, name: &str, stdout: &str, stderr: &str, exit_code: i32) -> PathBuf {
    #[cfg(windows)]
    let path = dir.join(format!("{name}.cmd"));
    #[cfg(not(windows))]
    let path = dir.join(name);

    #[cfg(windows)]
    let contents = format!(
        "@echo off\r\n@echo {stdout}\r\n@echo args=%*\r
         @echo {stderr} 1>&2\r\n@exit /b {exit_code}\r\n"
    );
    #[cfg(not(windows))]
    let contents = format!(
        "#!/bin/sh\nprintf '%s\\n' '{}'\nprintf 'args:%s\\n' \"$*\"\nprintf '%s\\n' '{}' >&2\nexit {}\n",
        stdout.replace('\'', "'\\''"),
        stderr.replace('\'', "'\\''"),
        exit_code
    );
    fs::write(&path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }
    path
}

fn fixture_sources(dir: &Path) -> (PathBuf, PathBuf) {
    let kt = dir.join("Kotlin source.kt");
    let java = dir.join("Java source.java");
    fs::write(&kt, "class KotlinSource").unwrap();
    fs::write(&java, "class JavaSource {}").unwrap();
    (kt, java)
}

fn target_java_17(config: &mut JvmValidationConfig) {
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config.javac_args.extend(["--release".into(), "17".into()]);
}

fn enable_lombok_processing(config: &mut JvmValidationConfig) {
    let jar = std::env::var_os("NOTLIN_LOMBOK")
        .expect("set NOTLIN_LOMBOK to the Lombok jar for generated Lombok classes");
    config.classpath.push(PathBuf::from(jar));
    config.javac_args.push("-proc:full".into());
    let plugin = std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN")
        .expect("set NOTLIN_KOTLIN_LOMBOK_PLUGIN to the matching Kotlin Lombok compiler plugin");
    let mut argument = std::ffi::OsString::from("-Xplugin=");
    argument.push(plugin);
    config.kotlinc_args.push(argument);
}

#[test]
fn compiles_kotlin_then_java_and_preserves_paths_with_spaces() {
    let scratch = Scratch::new("ordered compile");
    let (kt, java) = fixture_sources(&scratch.0);
    let tools_dir = scratch.0.join("fake tools");
    fs::create_dir_all(&tools_dir).unwrap();
    let kotlinc = fake_tool(&tools_dir, "kotlinc", "kotlinc ran", "", 0);
    let javac = fake_tool(&tools_dir, "javac", "javac ran", "", 0);
    let runtime = fake_tool(&tools_dir, "java", "runtime ran", "", 0);
    let dependency = scratch.0.join("dependency classes");
    let mut config =
        JvmValidationConfig::new(kotlinc, javac, runtime, scratch.0.join("compiled classes"));
    config.kotlin_sources.push(kt);
    config.java_sources.push(java);
    config.classpath.push(dependency);
    config.run_main_class = Some("sample.Main".into());

    let report = validate_jvm_sources(&config).unwrap();
    assert_eq!(
        report.commands.iter().map(|c| c.stage).collect::<Vec<_>>(),
        vec![
            JvmValidationStage::KotlinCompile,
            JvmValidationStage::JavaCompile,
            JvmValidationStage::Runtime,
        ]
    );
    assert!(report.commands[0].stdout.contains("kotlinc ran"));
    assert!(report.commands[1].stdout.contains("javac ran"));
    assert!(report.commands[2].stdout.contains("runtime ran"));
    let cp = std::env::join_paths([config.output_dir.as_path(), config.classpath[0].as_path()])
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let kotlin_args = &report.commands[0].stdout;
    assert!(kotlin_args.contains("Kotlin source.kt"));
    assert!(kotlin_args.contains("Java source.java"));
    assert!(kotlin_args.contains(&cp));
    let java_args = &report.commands[1].stdout;
    assert!(java_args.contains("Java source.java"));
    assert!(java_args.contains(&cp));
    assert!(report.commands[2].stdout.contains("sample.Main"));
}

#[test]
fn returns_compiler_diagnostics_and_stops_at_first_failed_stage() {
    let scratch = Scratch::new("compiler failure");
    let (kt, java) = fixture_sources(&scratch.0);
    let tools_dir = scratch.0.join("fake tools");
    fs::create_dir_all(&tools_dir).unwrap();
    let kotlinc = fake_tool(
        &tools_dir,
        "kotlinc",
        "",
        "unresolved reference: Missing",
        3,
    );
    let javac = tools_dir.join(if cfg!(windows) {
        "missing-javac.cmd"
    } else {
        "missing-javac"
    });
    let runtime = tools_dir.join(if cfg!(windows) {
        "missing-java.cmd"
    } else {
        "missing-java"
    });
    let mut config = JvmValidationConfig::new(kotlinc, javac, runtime, scratch.0.join("out"));
    config.kotlin_sources.push(kt);
    config.java_sources.push(java);

    let error = validate_jvm_sources(&config).unwrap_err();
    match error {
        JvmValidationError::Failed(result) => {
            assert_eq!(result.stage, JvmValidationStage::KotlinCompile);
            assert_eq!(result.status_code, Some(3));
            assert!(
                result
                    .diagnostics()
                    .contains("unresolved reference: Missing")
            );
        }
        other => panic!("expected compiler failure, got {other}"),
    }
}

#[test]
fn rejects_empty_source_sets() {
    let scratch = Scratch::new("empty");
    let config = JvmValidationConfig::new("kotlinc", "javac", "java", scratch.0.join("out"));
    assert!(matches!(
        validate_jvm_sources(&config),
        Err(JvmValidationError::InvalidConfig(_))
    ));
}

#[test]
fn rejects_stale_classes_before_launching_a_compiler() {
    let scratch = Scratch::new("stale output");
    let output = scratch.0.join("classes");
    fs::create_dir_all(&output).unwrap();
    fs::write(output.join("Old.class"), b"stale bytecode").unwrap();
    let mut config =
        JvmValidationConfig::new("missing-kotlinc", "missing-javac", "missing-java", output);
    config.java_sources.push(scratch.0.join("New.java"));
    let error = validate_jvm_sources(&config).unwrap_err();
    assert!(
        matches!(error, JvmValidationError::InvalidConfig(ref message) if message.contains("stale classes"))
    );
}

#[test]
fn rejects_invalid_runtime_request_before_creating_output() {
    let scratch = Scratch::new("invalid runtime");
    let output = scratch.0.join("classes");
    let mut config =
        JvmValidationConfig::new("missing-kotlinc", "missing-javac", "missing-java", &output);
    config.java_sources.push(scratch.0.join("New.java"));
    config.run_main_class = Some(" ".into());
    assert!(matches!(
        validate_jvm_sources(&config),
        Err(JvmValidationError::InvalidConfig(_))
    ));
    assert!(!output.exists());
}

/// Set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB to
/// exercise the retained Kotlin plus transpiler-generated Java boundary.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_compiles_transpiled_java_with_retained_kotlin() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    // The transpiler runs under the repository sandbox, so keep this isolated
    // integration fixture inside the workspace rather than the system temp dir.
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "real toolchain",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let kt = root.join("Greeter.kt");
    let retained = root.join("Retained.kt");
    fs::write(
        &kt,
        "package boundary\nclass Greeter {\n    fun fromRetained(): String = Retained().label()\n}\n",
    )
    .unwrap();
    fs::write(
        &retained,
        "package boundary\nclass Retained {\n    fun label(): String = \"from-kotlin\"\n    suspend fun suspendedLabel(): String = label()\n    fun generatedReference(): String = Greeter().fromRetained()\n}\nfun main() {\n    println(Greeter().fromRetained())\n}\n",
    )
    .unwrap();

    // Record the original Kotlin behavior before migration. This compile uses
    // both source files directly, so the same retained class and entry point
    // become the reference behavior for the mixed Kotlin/Java run below.
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline
        .kotlin_sources
        .extend([kt.clone(), retained.clone()]);
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RetainedKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    assert_eq!(baseline_report.commands.len(), 2);
    let baseline_stdout = baseline_report.commands[1].stdout.trim().to_owned();

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none"])
        .arg("--in-place")
        .arg(&kt)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let generated_java = root.join("Greeter.java");
    assert!(
        generated_java.is_file(),
        "transpiler did not emit Greeter.java; outputs: {:?}; stderr: {}",
        fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>(),
        String::from_utf8_lossy(&transpile.stderr)
    );
    let mut config =
        JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut config);
    config.kotlin_sources.push(retained);
    config.java_sources.push(generated_java);
    config.classpath.push(PathBuf::from(stdlib));
    config.run_main_class = Some("boundary.RetainedKt".into());
    let report = validate_jvm_sources(&config).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "transpiled Java plus retained Kotlin must match the original Kotlin runtime output"
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_ordered_nullable_map_transformations() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "ordered nullable map transformations",
    );
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    let source = r#"package mapcase
data class Entry(val key: String?, val value: String?)
fun remap(source: Map<String?, String?>): Map<String?, String?> =
    source.mapKeys { entry -> if (entry.key == "A") "a" else entry.key }
fun keep(source: Map<String?, String?>): Map<String?, String?> =
    source.filterValues { value -> value == null || value != "drop" }
fun index(values: List<Entry>): Map<String?, String?> =
    values.associateBy({ entry -> entry.key }, { entry -> entry.value })
fun main() {
    val source: MutableMap<String?, String?> = mutableMapOf()
    source["A"] = null
    source["a"] = "later"
    source[null] = "null-key"
    source["B"] = "drop"
    source["C"] = "stay"
    val filtered = keep(remap(source)) as MutableMap<String?, String?>
    filtered["z"] = null
    val values: MutableList<Entry> = mutableListOf()
    values.add(Entry(null, null))
    values.add(Entry("same", "first"))
    values.add(Entry("other", "middle"))
    values.add(Entry("same", "last"))
    println(filtered)
    println(index(values))
}
"#;
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("Main.kt"), source).unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources.push(original.join("Main.kt"));
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("mapcase.MainKt".into());
    let before = validate_jvm_sources(&baseline).expect("original collection fixture compiles");

    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(migrated.join("Main.kt"))
        .output()
        .expect("run notlin transpiler");
    assert!(
        translation.status.success(),
        "default-mode translation failed: {}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let generated = migrated.join("Main.java");
    assert!(
        generated.is_file(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.java_sources.push(generated);
    let entry = migrated.join("Entry.java");
    assert!(
        entry.is_file(),
        "collection fixture's entry class must translate"
    );
    mixed.java_sources.push(entry);
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("mapcase.Main".into());
    let after = validate_jvm_sources(&mixed).expect("transpiled collection fixture compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim(),
        "Java collection lowering must preserve duplicate-key overwrite, null keys/values, order, and mutability"
    );
}

/// This guards generic type binding across Java intermediate interfaces and
/// the bridge method javac emits for a concrete Kotlin override.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_generic_getter_bridge_through_java_intermediates() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "generic getter bridge",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let source_interface = root.join("PayloadSource.kt");
    let payload = root.join("Payload.kt");
    let runner = root.join("Runner.kt");
    let concrete = root.join("Concrete.kt");
    let intermediate = root.join("Intermediate.java");
    let leaf = root.join("Leaf.java");

    fs::write(
        &source_interface,
        "package boundary\ninterface PayloadSource<U> {\n    fun readPayload(): U\n}\n",
    )
    .unwrap();
    fs::write(&payload, "package boundary\nclass Payload\n").unwrap();
    fs::write(
        &runner,
        "package boundary\nfun main() {\n    val payload = Payload()\n    val concrete = Concrete(payload)\n    val source: PayloadSource<Payload> = concrete\n    println(\"${concrete.readPayload() === payload}|${source.readPayload() === payload}\")\n}\n",
    )
    .unwrap();
    fs::write(
        &concrete,
        "package boundary\nclass Concrete(private val payload: Payload) : Leaf<Payload> {\n    override fun readPayload(): Payload = payload\n}\n",
    )
    .unwrap();
    fs::write(
        &intermediate,
        "package boundary;\npublic interface Intermediate<U> extends PayloadSource<U> {}\n",
    )
    .unwrap();
    fs::write(
        &leaf,
        "package boundary;\npublic interface Leaf<V> extends Intermediate<V> {}\n",
    )
    .unwrap();

    let stdlib_path = PathBuf::from(stdlib.clone());
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    baseline.kotlin_sources.extend([
        source_interface.clone(),
        payload.clone(),
        runner.clone(),
        concrete.clone(),
    ]);
    baseline
        .java_sources
        .extend([intermediate.clone(), leaf.clone()]);
    baseline.classpath.push(stdlib_path.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report.commands[2].stdout.trim().to_owned();
    assert_eq!(baseline_stdout, "true|true");

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none"])
        .arg("--in-place")
        .arg(&concrete)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let generated_concrete = root.join("Concrete.java");
    assert!(
        generated_concrete.is_file(),
        "transpiler did not emit Concrete.java"
    );

    let mut migrated =
        JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    migrated
        .kotlin_sources
        .extend([source_interface, payload, runner]);
    migrated
        .java_sources
        .extend([intermediate, leaf, generated_concrete]);
    migrated.classpath.push(stdlib_path);
    migrated.run_main_class = Some("boundary.RunnerKt".into());
    let migrated_report = validate_jvm_sources(&migrated).unwrap();
    let migrated_stdout = migrated_report.commands[2].stdout.trim();
    assert_eq!(
        migrated_stdout, baseline_stdout,
        "generic payload dispatch and identity must survive the Java bridge"
    );
}

/// Exercise Java/Kotlin ownership seams together: synthesized properties,
/// default constructors, companion dispatch, data-class equality, JVM argument
/// evaluation order, and a translated subclass of a retained Kotlin base.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_mixed_runtime_boundaries() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "mixed runtime boundaries",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let features = root.join("Features.kt");
    let runner = root.join("Runner.kt");
    fs::write(
        &features,
        "package boundary\n\
         data class Measurement(val amount: Int, val label: String = \"unit\")\n\
         class Device(val id: Int) {\n\
             companion object {\n\
                 fun create(id: Int): Device = Device(id)\n\
             }\n\
         }\n\
         class MutableCounter(var count: Int) {\n\
             fun increment() { count += 1 }\n\
         }\n\
         fun combine(first: Int, second: Int): Int = first * 10 + second\n\
         class GeneratedChild(seed: Int) : RetainedBase(seed) {\n\
             fun childValue(): Int = seed\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &runner,
        "package boundary\n\
         open class RetainedBase(val seed: Int) {\n\
             suspend fun retainedBoundary() {}\n\
         }\n\
         private var evaluationOrder = \"\"\n\
         private fun first(): Int { evaluationOrder += \"1\"; return 3 }\n\
         private fun second(): Int { evaluationOrder += \"2\"; return 4 }\n\
         fun main() {\n\
         \x20\x20\x20\x20val defaulted = Measurement(7)\n\
         \x20\x20\x20\x20val equal = Measurement(7, \"unit\") == defaulted\n\
         \x20\x20\x20\x20val device = Device.Companion.create(9)\n\
         \x20\x20\x20\x20val counter = MutableCounter(2); counter.increment()\n\
         \x20\x20\x20\x20val combined = combine(first(), second())\n\
         \x20\x20\x20\x20val child = GeneratedChild(5)\n\
         \x20\x20\x20\x20println(\"${defaulted.amount}:${defaulted.label}:$equal:${device.id}:${counter.count}:$evaluationOrder:$combined:${child.childValue()}:${child.seed}\")\n\
         }\n",
    ).unwrap();

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline
        .kotlin_sources
        .extend([features.clone(), runner.clone()]);
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    assert_eq!(baseline_stdout, "7:unit:true:9:3:12:34:5:5");

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "{}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let kotlin_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "kt"))
        .collect::<Vec<_>>();
    let java_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "java"))
        .collect::<Vec<_>>();
    assert!(
        !java_sources.is_empty(),
        "translation must emit Java sources"
    );
    for expected in [
        "Measurement.java",
        "Device.java",
        "MutableCounter.java",
        "GeneratedChild.java",
        "Features.java",
    ] {
        assert!(
            java_sources
                .iter()
                .any(|path| path.file_name().is_some_and(|name| name == expected)),
            "missing generated runtime boundary {expected}; outputs: {java_sources:?}; Kotlin: {:?}; stderr: {}",
            kotlin_sources
                .iter()
                .map(|path| (path.clone(), fs::read_to_string(path).unwrap_or_default()))
                .collect::<Vec<_>>(),
            String::from_utf8_lossy(&transpile.stderr)
        );
    }
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = kotlin_sources;
    mixed.java_sources = java_sources;
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some(if runner.exists() {
        "boundary.RunnerKt".into()
    } else {
        "boundary.Runner".into()
    });
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "mixed translated Java and retained Kotlin must match the original Kotlin execution"
    );
}

/// Lombok mode must preserve Kotlin initialization semantics when a data
/// class has initialized body properties or init blocks. These cases use the
/// explicit Java fallback and therefore need no Lombok annotation processor.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_initialized_data_class_behavior_in_lombok_mode() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "lombok initialized data class",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let model = root.join("Initialized.kt");
    let runner = root.join("Runner.kt");
    fs::write(
        &model,
        "package boundary\n\
         data class Initialized(val base: Int, var mutable: Int) {\n\
             val first = base + 1\n\
             init { mutable += first }\n\
             var second = mutable + first\n\
             init { second += mutable }\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &runner,
        "package boundary\n\
         fun main() {\n\
             val value = Initialized(4, 3)\n\
             println(\"${value.base}:${value.mutable}:${value.first}:${value.second}\")\n\
         }\n",
    )
    .unwrap();

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline
        .kotlin_sources
        .extend([model.clone(), runner.clone()]);
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    assert_eq!(baseline_stdout, "4:8:5:21");

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "{}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let kotlin_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect::<Vec<_>>();
    let java_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect::<Vec<_>>();
    assert!(
        java_sources.iter().any(|path| path
            .file_name()
            .is_some_and(|name| name == "Initialized.java")),
        "missing generated Initialized.java; outputs: {java_sources:?}; stderr: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = kotlin_sources;
    mixed.java_sources = java_sources;
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some(if runner.exists() {
        "boundary.RunnerKt".into()
    } else {
        "boundary.Runner".into()
    });
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(
        report.commands.len(),
        2 + usize::from(!mixed.kotlin_sources.is_empty())
    );
    assert_eq!(
        report.commands.last().unwrap().stdout.trim(),
        baseline_stdout,
        "Lombok-mode Java must preserve initialized data class field values and init ordering"
    );
}

/// Keep a retained interface's body-backed property getter callable after a
/// parallel generated interface's same-named property is migrated to Java.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_parallel_interface_property_accessors() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "parallel interface properties",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let generated_api = root.join("GeneratedApi.kt");
    let retained_api = root.join("RetainedApi.kt");
    let implementation = root.join("Implementation.kt");
    let callers = root.join("Callers.kt");
    let runner = root.join("Runner.kt");
    fs::write(
        &generated_api,
        "package boundary\ninterface GeneratedApi { val category: String }\n",
    )
    .unwrap();
    fs::write(
        &retained_api,
        "package boundary\ninterface Api { val category: String; val alias: Int get() = category.length; suspend fun retainedMarker(): Unit }\n",
    )
    .unwrap();
    fs::write(
        &implementation,
        "package boundary\ninterface ApiChild : Api, GeneratedApi {}\nclass Impl(override val category: String) : ApiChild { override suspend fun retainedMarker(): Unit {} }\n",
    )
    .unwrap();
    fs::write(
        &callers,
        "package boundary\nfun readCategory(value: Api): String = value.category\nfun readAlias(value: Api): Int = value.alias\n",
    )
    .unwrap();
    fs::write(
        &runner,
        "package boundary\nfun main() { val value = Impl(\"alpha\"); println(\"${readCategory(value)}:${readAlias(value)}\") }\n",
    )
    .unwrap();

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources.extend([
        generated_api.clone(),
        retained_api.clone(),
        implementation.clone(),
        callers.clone(),
        runner.clone(),
    ]);
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    assert_eq!(baseline_stdout, "alpha:5");

    // Model the generated interface ABI directly, then exercise the public
    // property repair pipeline on the retained Kotlin sources.
    let generated_java = root.join("GeneratedApi.java");
    let java_ir = notlin::java_ir::parse_java(
        "package boundary; public interface GeneratedApi { String getCategory(); }",
    )
    .unwrap();
    fs::write(&generated_java, notlin::java_ir::render(&java_ir)).unwrap();
    fs::remove_file(&generated_api).unwrap();

    let preindex = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let generated_java_paths = [fs::canonicalize(&generated_java).unwrap()]
        .into_iter()
        .collect();
    let contracts =
        notlin::property_abi::repaired_callsite_contracts(&preindex, &generated_java_paths);
    let mut repaired_sources = preindex
        .kotlin_files()
        .map(|source_file| {
            (
                source_file.path.clone(),
                source_file.source_text().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    notlin::property_abi::repair_virtual_sources(
        &preindex,
        &mut repaired_sources,
        &generated_java_paths,
    );
    for (path, source) in &repaired_sources {
        fs::write(path, source).unwrap();
    }
    let postindex = notlin::workspace::SourceIndex::discover(&root).unwrap();
    for (path, source) in &repaired_sources {
        let (rewritten, _) =
            notlin::property_callsite::rewrite_file(&postindex, path, source, &contracts);
        fs::write(path, rewritten).unwrap();
    }

    let kotlin_sources = repaired_sources
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    let java_sources = vec![generated_java];
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = kotlin_sources;
    mixed.java_sources = java_sources;
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some(if runner.exists() {
        "boundary.RunnerKt".into()
    } else {
        "boundary.Runner".into()
    });
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "mixed Kotlin and Java must preserve both same-named property accessors"
    );
}

/// Compare Kotlin event dispatch with the mixed runtime after an event
/// interface crosses the covariant-property ABI repair boundary.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_covariant_event_property_dispatch() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "covariant event properties",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let root_event = root.join("RootEvent.kt");
    let identifiers = root.join("Identifiers.kt");
    let broad_event = root.join("BroadEvent.kt");
    let implementation = root.join("Implementation.kt");
    let callers = root.join("Callers.kt");
    let runner = root.join("Runner.kt");
    fs::write(
        &root_event,
        "package boundary\n\
         interface ScalarRoot {\n\
             val label: String\n\
         }\n\
         interface ScalarLeft : ScalarRoot {}\n\
         interface ScalarRight : ScalarRoot {}\n\
         interface RootEvent : ScalarLeft, ScalarRight {\n\
             val id: ChildId\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &identifiers,
        "package boundary\ninterface Id {\n    fun text(): String\n}\n\
         class ChildId : Id {\n    override fun text(): String = \"item-7\"\n}\n",
    )
    .unwrap();
    fs::write(
        &broad_event,
        "package boundary\n\
         interface BroadEvent<T : Id> {\n\
             val id: T\n\
         val display: String get() = \"event:\" + id.text()\n\
             suspend fun retainedMarker(): Unit\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &implementation,
        "package boundary\ninterface EventChild : RootEvent, BroadEvent<Id> {}\n\
         class EventImpl(override val id: ChildId, override val label: String) : EventChild {\n\
             override suspend fun retainedMarker(): Unit {}\n\
         }\n",
    )
    .unwrap();
    fs::write(
        &callers,
        "package boundary\nfun readRootId(value: RootEvent): String = value.id.text()\nfun readBroadId(value: BroadEvent<Id>): String = value.id.text()\nfun readDisplay(value: BroadEvent<Id>): String = value.display\n",
    )
    .unwrap();
    fs::write(
        &runner,
        "package boundary\nfun main() { val value = EventImpl(ChildId(), \"record\"); println(\"${readRootId(value)}:${readBroadId(value)}:${readDisplay(value)}:${value.label}\") }\n",
    )
    .unwrap();

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources.extend([
        root_event.clone(),
        identifiers.clone(),
        broad_event.clone(),
        implementation.clone(),
        callers.clone(),
        runner.clone(),
    ]);
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    assert_eq!(baseline_stdout, "item-7:item-7:event:item-7:record");

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        root.join("RootEvent.java").is_file(),
        "RootEvent should migrate to Java; stderr: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        broad_event.is_file() && !root.join("BroadEvent.java").exists(),
        "the body-backed broad interface should remain Kotlin"
    );

    let kotlin_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect::<Vec<_>>();
    let java_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect::<Vec<_>>();
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = kotlin_sources;
    mixed.java_sources = java_sources;
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some(if runner.exists() {
        "boundary.RunnerKt".into()
    } else {
        "boundary.Runner".into()
    });
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "Lombok-mode mixed Kotlin and Java must preserve covariant event property dispatch"
    );
}

/// Compare Kotlin's literal `split(...).last()` behavior with the generated
/// Java default methods across a Java-defined lookup contract.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_uuid_identifier_and_literal_split_behavior() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "UUID identifier split behavior",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("LookupKey.java"),
            "package boundary; public interface LookupKey { String getLookupKey(); }\n",
        )
        .unwrap();
        fs::write(
            root.join("Identifier.kt"),
            "package boundary\n\
             import java.util.*\n\
             private const val KEY_DELIMITER: String = \"/\"\n\
             interface Identifier : LookupKey {\n\
                 val objectId: UUID\n\
                 fun text(): String { return objectId.toString() }\n\
                 fun separatorText(): String { return \"before:$KEY_DELIMITER:${KEY_DELIMITER}\" }\n\
                 val externalKey: String\n\
                     get() { return getLookupKey().split(KEY_DELIMITER).last() }\n\
                 fun lastPiece(value: String, delimiter: String): String {\n\
                     return value.split(delimiter).last()\n\
                 }\n\
                 fun nullableLast(values: List<String?>): String? { return values.last() }\n\
             }\n",
        )
        .unwrap();
        fs::write(
            root.join("Implementation.kt"),
            "package boundary\n\
             import java.util.UUID\n\
             class Implementation(override val objectId: UUID, private val key: String) : Identifier {\n\
                 override fun getLookupKey(): String { return key }\n\
                 suspend fun retainedBoundary(): Unit {}\n\
             }\n",
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            "package boundary\n\
             import java.util.UUID\n\
             fun encoded(value: String): String = \"${value.length}:$value\"\n\
             fun main() {\n\
                 val value = Implementation(UUID.fromString(\"123e4567-e89b-12d3-a456-426614174000\"), \"scheme/object/\")\n\
                 val nullable = value.nullableLast(listOf(\"present\", null)) ?: \"<null>\"\n\
                 val emptyFailure = try {\n\
                     value.nullableLast(emptyList())\n\
                     \"no-exception\"\n\
                 } catch (failure: NoSuchElementException) { failure.javaClass.name }\n\
                 println(listOf(value.text(), encoded(value.externalKey), encoded(value.separatorText()), encoded(value.lastPiece(\"a.b.\", \".\")), encoded(value.lastPiece(\"a|b||\", \"|\")), encoded(value.lastPiece(\"abc\", \"\")), nullable, emptyFailure).joinToString(\";\"))\n\
             }\n",
        )
        .unwrap();
    }

    let baseline_kotlin = [
        baseline_root.join("Identifier.kt"),
        baseline_root.join("Implementation.kt"),
        baseline_root.join("Runner.kt"),
    ];
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = baseline_kotlin.to_vec();
    baseline
        .java_sources
        .push(baseline_root.join("LookupKey.java"));
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&mixed_root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        mixed_root.join("Identifier.java").is_file(),
        "Identifier should migrate; stderr: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );

    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "mixed Kotlin and Java must preserve UUID lookup and literal split/last behavior"
    );
}

/// A UUID interface can cross to Java while a retained Kotlin superinterface
/// still owns a generic `id` property; repairing the UUID accessor must leave
/// the unrelated Kotlin property override intact.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_parallel_generic_id_during_uuid_bridge() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "parallel generic id during UUID bridge",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Identity.kt"),
            "package boundary\n\
             import java.util.UUID\n\
             interface BaseId {\n    fun text(): String\n}\n\
             data class ExampleId(private val value: String) : BaseId {\n\
                 override fun text(): String = value\n\
             }\n\
             interface UuidIdentity {\n    val objectId: UUID\n}\n\
             interface IdView<T : BaseId> {\n\
                 val id: T\n\
                 suspend fun retainedMarker(): Unit\n\
             }\n\
             interface Identifier<T : BaseId> : UuidIdentity, IdView<T>\n\
             class ExampleInfo(override val objectId: UUID, override val id: ExampleId) : Identifier<ExampleId> {\n\
                 override suspend fun retainedMarker(): Unit {}\n\
             }\n\
             fun main() {\n\
                 val value = ExampleInfo(UUID.fromString(\"123e4567-e89b-12d3-a456-426614174000\"), ExampleId(\"value-7\"))\n\
                 println(\"${value.objectId}:${value.id.text()}\")\n\
             }\n",
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline
        .kotlin_sources
        .push(baseline_root.join("Identity.kt"));
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.IdentityKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&mixed_root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        mixed_root.join("UuidIdentity.java").is_file(),
        "UUID interface should migrate; stderr: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        mixed_root.join("Identity.kt").is_file(),
        "generic id branch and implementation should stay Kotlin"
    );
    let mixed_source = fs::read_to_string(mixed_root.join("Identity.kt")).unwrap();
    assert!(
        mixed_source.contains("override val id: ExampleId"),
        "the unrelated generic property override should retain Kotlin property syntax:\n{mixed_source}"
    );
    assert!(
        !mixed_source.contains("@JvmField val id"),
        "the UUID bridge must not replace the parallel id property with a field:\n{mixed_source}"
    );

    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.IdentityKt".into());
    enable_lombok_processing(&mut mixed);
    let report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(report.commands.len(), 3);
    assert_eq!(
        report.commands[2].stdout.trim(),
        baseline_stdout,
        "mixed Kotlin and Java must preserve both UUID and generic id behavior"
    );
}

/// A Kotlin child can extend a retained abstract JPA superclass from Java only
/// when its positional super call exactly forwards a matching constructor
/// parameter and the superclass emits the omitted-default overload.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_constructor_call_to_retained_mapped_superclass() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "retained mapped superclass constructor",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root.join("jakarta/persistence")).unwrap();
        fs::write(
            root.join("jakarta/persistence/MappedSuperclass.java"),
            "package jakarta.persistence;\npublic @interface MappedSuperclass {}\n",
        )
        .unwrap();
        fs::write(
            root.join("Parent.kt"),
            r#"package boundary
import jakarta.persistence.MappedSuperclass

@MappedSuperclass
abstract class Parent @JvmOverloads constructor(
    open val id: String,
    open val version: Int = 7
) {
    suspend fun retainedBoundary(): Unit {}
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("Child.kt"),
            r#"package boundary
class Child(id: String, val token: String) : Parent(id)

class AmountUpdate(val amount: String, val unit: String)

class Entry(var amount: String, var unit: String) {
    fun setAmount(value: AmountUpdate) {
        this.amount = value.amount
        this.unit = value.unit
    }
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package boundary
fun main() {
    val child = Child("id-9", "token-4")
    val entry = Entry("amount-before", "unit-before")
    entry.setAmount(AmountUpdate("amount-updated", "unit-updated"))
    entry.amount = "direct-updated"
    println("${child.id}:${child.version}:${child.token}:${entry.amount}:${entry.unit}")
}
"#,
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [
        baseline_root.join("Parent.kt"),
        baseline_root.join("Child.kt"),
        baseline_root.join("Runner.kt"),
    ]
    .into();
    baseline
        .java_sources
        .push(baseline_root.join("jakarta/persistence/MappedSuperclass.java"));
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline).unwrap();
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&mixed_root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let child_java = mixed_root.join("Child.java");
    assert!(
        child_java.is_file(),
        "the exact constructor proof should migrate Child; stderr: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        fs::read_to_string(mixed_root.join("Parent.kt"))
            .unwrap()
            .contains("abstract class Parent"),
        "the suspend boundary should keep Parent in Kotlin"
    );
    assert!(
        fs::read_to_string(child_java)
            .unwrap()
            .contains("extends Parent")
    );
    let entry_java = fs::read_to_string(mixed_root.join("Entry.java"))
        .expect("the mutable constructor-property fixture should migrate");
    assert!(
        entry_java.contains("this.amount = value.getAmount();"),
        "{entry_java}"
    );
    assert!(
        entry_java.contains("this.setUnit(value.getUnit());"),
        "{entry_java}"
    );

    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed
        .java_sources
        .push(mixed_root.join("jakarta/persistence/MappedSuperclass.java"));
    enable_lombok_processing(&mut mixed);
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some(if mixed_root.join("Runner.kt").exists() {
        "boundary.RunnerKt".into()
    } else {
        "boundary.Runner".into()
    });
    let mixed_report = validate_jvm_sources(&mixed).unwrap();
    assert_eq!(mixed_report.commands.len(), 3);
    assert_eq!(
        mixed_report.commands[2].stdout.trim(),
        baseline_stdout,
        "the Kotlin-to-Java constructor boundary must preserve inherited field values"
    );
}

/// A Kotlin override may narrow a nullable inherited getter return. The
/// generated Java declaration must carry matching nullability metadata so a
/// retained Kotlin caller sees the narrowed type after migration.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_narrowed_nullable_getter_contract() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "narrowed nullable getter contract",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root.join("org/jetbrains/annotations")).unwrap();
        fs::write(
            root.join("NullableBase.kt"),
            r#"package boundary
import java.math.BigDecimal

interface NullableBase {
    val measure: BigDecimal?
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("NarrowItem.kt"),
            r#"package boundary
import java.math.BigDecimal

interface NarrowItem : NullableBase {
    override val measure: BigDecimal
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("Caller.kt"),
            r#"package boundary

suspend fun itemScale(item: NarrowItem): Int = item.measure.scale()
"#,
        )
        .unwrap();
        fs::write(
            root.join("org/jetbrains/annotations/NotNull.java"),
            "package org.jetbrains.annotations;\n\
             @java.lang.annotation.Target({java.lang.annotation.ElementType.METHOD, java.lang.annotation.ElementType.PARAMETER, java.lang.annotation.ElementType.FIELD, java.lang.annotation.ElementType.TYPE_USE})\n\
             @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)\n\
             public @interface NotNull {}\n",
        )
        .unwrap();
        fs::write(
            root.join("org/jetbrains/annotations/Nullable.java"),
            "package org.jetbrains.annotations;\n\
             @java.lang.annotation.Target({java.lang.annotation.ElementType.METHOD, java.lang.annotation.ElementType.PARAMETER, java.lang.annotation.ElementType.FIELD, java.lang.annotation.ElementType.TYPE_USE})\n\
             @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.CLASS)\n\
             public @interface Nullable {}\n",
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [
        baseline_root.join("NullableBase.kt"),
        baseline_root.join("NarrowItem.kt"),
        baseline_root.join("Caller.kt"),
    ]
    .into();
    baseline.java_sources = [
        baseline_root.join("org/jetbrains/annotations/NotNull.java"),
        baseline_root.join("org/jetbrains/annotations/Nullable.java"),
    ]
    .into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    validate_jvm_sources(&baseline).expect("original Kotlin hierarchy and caller compile");

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "jetbrains", "--in-place", "--lombok"])
        .arg(&mixed_root)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let narrow_java = mixed_root.join("NarrowItem.java");
    assert!(narrow_java.is_file(), "narrow interface should migrate");
    let generated = fs::read_to_string(&narrow_java).unwrap();
    assert!(
        generated.contains("@org.jetbrains.annotations.NotNull"),
        "the overriding Java getter needs non-null metadata for Kotlin callers:\n{generated}"
    );
    assert!(mixed_root.join("NullableBase.java").is_file());
    assert!(mixed_root.join("Caller.kt").is_file());

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = [mixed_root.join("Caller.kt")].into();
    mixed.java_sources = [
        narrow_java,
        mixed_root.join("NullableBase.java"),
        mixed_root.join("org/jetbrains/annotations/NotNull.java"),
        mixed_root.join("org/jetbrains/annotations/Nullable.java"),
    ]
    .into();
    mixed.classpath.push(PathBuf::from(stdlib));
    validate_jvm_sources(&mixed)
        .expect("retained Kotlin caller compiles against migrated narrowed Java getter");
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_keeps_narrow_child_property_when_parent_is_intrinsically_retained() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "retained narrow property parent",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let variants = root.join("Types.kt");
    fs::write(
        &variants,
        "package boundary\ninterface BaseVariant {\n    fun baseLabel(): String\n}\ninterface NarrowVariant : BaseVariant {\n    fun narrowLabel(): String\n}\ninterface HolderContract {\n    val variant: BaseVariant\n    suspend fun checkpoint()\n}\n",
    )
    .unwrap();
    let detail_holder = root.join("DetailHolder.kt");
    fs::write(
        &detail_holder,
        "package boundary\ninterface DetailHolder : HolderContract {\n    override val variant: NarrowVariant\n}\n",
    )
    .unwrap();
    let caller = root.join("Caller.kt");
    fs::write(
        &caller,
        "package boundary\nfun narrowLabel(info: DetailHolder): String = info.variant.narrowLabel()\n",
    )
    .unwrap();

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [variants.clone(), detail_holder.clone(), caller.clone()].into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    validate_jvm_sources(&baseline)
        .expect("original Kotlin override keeps the narrow property type visible");

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .expect("run notlin on all selected sources");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(detail_holder.is_file());
    assert!(
        !root.join("DetailHolder.java").exists(),
        "the child must remain Kotlin while its intrinsically retained parent stays Kotlin"
    );
    let migrated = fs::read_to_string(&detail_holder).unwrap();
    assert!(
        migrated.contains("override val variant: NarrowVariant"),
        "the retained child must preserve its narrowed property declaration:\n{migrated}"
    );

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    validate_jvm_sources(&mixed)
        .expect("retained Kotlin child and caller preserve the narrow property type");
}

/// A memberless interface over a retained Kotlin property diamond must stay
/// Kotlin: inserting a Java interface there can leave Kotlin's inherited
/// getter fakeoverride without a real implementation during IR lowering.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_generic_default_property_through_java_bridge() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "generic default property through Java bridge",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("ContextParent.kt"),
            r#"package boundary

interface ContextRoot<T> {
    val contextKind: T
}

interface AbstractContextPath<T> : ContextRoot<T>

interface DefaultContextPath<T> : ContextRoot<T> {
    override val contextKind: T
        get() = defaultContextValue()

    fun defaultContextValue(): T
}

interface RetainedContext<T> : AbstractContextPath<T>, DefaultContextPath<T>
"#,
        )
        .unwrap();
        fs::write(
            root.join("Bridge.kt"),
            r#"package boundary

interface Bridge<T> : RetainedContext<T> {
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("ContextImpl.kt"),
            r#"package boundary

class ContextImpl : Bridge<String> {
    override fun defaultContextValue(): String = "context"
}

fun main() {
    val value = ContextImpl()
    println(value.contextKind)
}
"#,
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [
        baseline_root.join("ContextParent.kt"),
        baseline_root.join("Bridge.kt"),
        baseline_root.join("ContextImpl.kt"),
    ]
    .into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.ContextImplKt".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("the original all-Kotlin generic diamond compiles");
    let baseline_stdout = baseline_report
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();

    let bridge_source = mixed_root.join("Bridge.kt");
    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&bridge_source)
        .output()
        .expect("run notlin transpiler");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let bridge_java = mixed_root.join("Bridge.java");
    assert!(
        !bridge_java.exists(),
        "a memberless bridge over a retained Kotlin property diamond must stay Kotlin"
    );
    assert!(
        bridge_source.is_file(),
        "retained bridge source should remain"
    );
    assert!(mixed_root.join("ContextParent.kt").is_file());
    assert!(mixed_root.join("ContextImpl.kt").is_file());

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.ContextImplKt".into());
    let mixed_report = validate_jvm_sources(&mixed)
        .expect("the retained Kotlin bridge preserves its inherited property provider");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_stdout,
        "the mixed hierarchy must preserve the inherited property value"
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_super_property_on_recovered_generic_interface() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "recovered generic property interface",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let types = root.join("Types.kt");
    fs::write(
        &types,
        "package boundary\nimport kotlin.reflect.KClass\n\
         @Target(AnnotationTarget.CLASS)\n@Retention(AnnotationRetention.RUNTIME)\n\
         annotation class Read(val using: KClass<*>)\n\
         @Target(AnnotationTarget.CLASS)\n@Retention(AnnotationRetention.RUNTIME)\n\
         annotation class Write(val using: KClass<*>)\n\
         class Reader\nclass Writer\n",
    )
    .unwrap();
    let getter_tag = root.join("GetterTag.java");
    fs::write(&getter_tag, "package boundary;\n@java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD)\npublic @interface GetterTag {}\n").unwrap();
    let parent = root.join("Parent.kt");
    fs::write(
        &parent,
        "package boundary\n\
         @Read(using = Reader::class)\n\
         @Write(using = Writer::class)\n\
         interface Parent<T> {\n\
             @get:GetterTag\n\
             val contextKind: T\n\
             suspend fun parentCheckpoint()\n\
         }\n",
    )
    .unwrap();
    let java_bridge = root.join("JavaBridge.java");
    fs::write(
        &java_bridge,
        "package boundary;\n\
         public interface JavaBridge<T> extends Parent<T> {\n\
             @Override default T getContextKind() { return defaultKind(); }\n\
             T defaultKind();\n\
         }\n",
    )
    .unwrap();
    let child = root.join("Child.kt");
    fs::write(
        &child,
        "package boundary\n\
         interface Child<T> : JavaBridge<T> {\n\
             override val contextKind: T\n\
                 get() = super<JavaBridge<T>>.contextKind\n\
             suspend fun checkpoint() {}\n\
         }\n",
    )
    .unwrap();
    let implementation = root.join("Implementation.kt");
    fs::write(
        &implementation,
        "package boundary\nclass Implementation : Child<String> {\n\
             override fun defaultKind(): String = \"root\"\n\
             override suspend fun parentCheckpoint() {}\n\
         }\n",
    )
    .unwrap();
    let runner = root.join("Runner.kt");
    fs::write(
        &runner,
        "package boundary\nobject Runner {\n\
             @JvmStatic fun main(args: Array<String>) { println(Implementation().contextKind) }\n\
         }\n",
    )
    .unwrap();

    let source_files = [
        types.clone(),
        parent.clone(),
        child.clone(),
        implementation.clone(),
        runner.clone(),
    ];
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = source_files.clone().into();
    baseline.java_sources = vec![java_bridge.clone(), getter_tag];
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.Runner".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("baseline Kotlin hierarchy compiles");
    let baseline_stdout = baseline_report.commands.last().unwrap().stdout.trim();

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .expect("run notlin on all selected generic hierarchy sources");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(java_bridge.is_file(), "the Java bridge remains available");
    let java_bridge_source = fs::read_to_string(&java_bridge).unwrap();
    assert!(
        java_bridge_source.contains("getContextKind()") && java_bridge_source.contains("default"),
        "the Java bridge must retain its default getter:\n{java_bridge_source}"
    );
    assert!(parent.is_file(), "the annotated generic root stays Kotlin");
    let child_source = fs::read_to_string(&child).unwrap();
    assert!(
        !root.join("Child.java").exists(),
        "the suspend descendant must remain a Kotlin property override"
    );
    assert!(
        child_source.contains("override val contextKind: T")
            && child_source.contains("super<JavaBridge<T>>.contextKind")
            && !child_source.contains("fun getContextKind"),
        "the retained override must preserve its Kotlin super-property expression:\n{child_source}"
    );

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.Runner".into());
    let mixed_report = validate_jvm_sources(&mixed)
        .expect("retained Kotlin descendant compiles against generated Java default getter");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_stdout,
        "mixed runtime must preserve the super-property value"
    );
}

/// Keep three Java/Kotlin boundaries in one executable fixture: a retained
/// imported object referenced from a private serialization hook, an object
/// implementing a repaired property getter, and an overload whose nullable
/// primary-constructor default must coexist with a same-arity secondary.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_object_property_and_nullable_constructor_boundaries() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "object property and nullable constructor boundaries",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root.join("provider")).unwrap();
        fs::write(
            root.join("KindApi.kt"),
            "package boundary\ninterface KindApi<T> { val kind: T }\n",
        )
        .unwrap();
        fs::write(
            root.join("LocalMarker.kt"),
            "package boundary\nimport boundary.provider.ExternalSingleton\n\
             @java.lang.Deprecated\nobject LocalMarker : KindApi<String> {\n\
                 private fun readResolve(): Any = ExternalSingleton\n\
                 override val kind: String\n                     get() = \"marker\"\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("provider/ExternalSingleton.kt"),
            "package boundary.provider\nobject ExternalSingleton\n",
        )
        .unwrap();
        fs::write(
            root.join("Choice.kt"),
            "package boundary\nclass Context(val reason: String)\n\
             class Choice(val id: String, val context: Context?) {\n\
                 constructor(id: String) : this(id, null)\n\
                 constructor(id: String, reason: String) : this(id, Context(reason))\n\
             }\n",
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            "package boundary\nfun main() {\n\
             val marker = Class.forName(\"boundary.LocalMarker\").getField(\"INSTANCE\").get(null) as KindApi<String>\n\
             println(marker.kind)\n\
             val absent = Choice(\"item\")\n\
             val present = Choice(\"item\", \"reason\")\n\
             println(\"${absent.id}:${absent.context == null}:${present.context?.reason}\")\n}\n",
        )
        .unwrap();
    }

    let baseline = {
        let mut config =
            JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
        target_java_17(&mut config);
        config.kotlin_sources = fs::read_dir(&baseline_root)
            .unwrap()
            .flatten()
            .chain(
                fs::read_dir(baseline_root.join("provider"))
                    .unwrap()
                    .flatten(),
            )
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
            .collect();
        config.classpath.push(PathBuf::from(&stdlib));
        config.run_main_class = Some("boundary.RunnerKt".into());
        validate_jvm_sources(&config).expect("the original Kotlin fixture compiles")
    };
    let baseline_stdout = baseline.commands.last().unwrap().stdout.trim().to_owned();

    let selections = [
        mixed_root.join("KindApi.kt"),
        mixed_root.join("LocalMarker.kt"),
        mixed_root.join("Choice.kt"),
    ];
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"]);
    command.args(&selections);
    let transpile = command
        .output()
        .expect("run notlin on selected declarations");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(mixed_root.join("KindApi.java").is_file());
    assert!(mixed_root.join("LocalMarker.java").is_file());
    assert!(mixed_root.join("Choice.java").is_file());
    assert!(mixed_root.join("Context.java").is_file());
    assert!(mixed_root.join("provider/ExternalSingleton.kt").is_file());
    let marker_java = fs::read_to_string(mixed_root.join("LocalMarker.java")).unwrap();
    assert!(
        marker_java.contains("String getKind()")
            && !marker_java.contains("static String getKind()"),
        "repaired object property getter must be an instance override:\n{marker_java}"
    );
    let reader_java = fs::read_to_string(mixed_root.join("LocalMarker.java")).unwrap();
    assert!(
        reader_java.contains("ExternalSingleton.INSTANCE"),
        "retained imported Kotlin object must be read through its singleton field:\n{reader_java}"
    );

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .chain(fs::read_dir(mixed_root.join("provider")).unwrap().flatten())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .chain(fs::read_dir(mixed_root.join("provider")).unwrap().flatten())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report = validate_jvm_sources(&mixed)
        .expect("the mixed object/getter/constructor boundaries compile");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_stdout,
        "the mixed runtime must preserve object-property and constructor behavior"
    );
}

/// A selected interface property must remain in Kotlin when retained class
/// descendants override that property. Converting only the interface would
/// turn the Kotlin property contract into a Java getter the Kotlin overrides
/// cannot satisfy.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_retains_selected_interface_above_retained_class_property_overrides() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "retained interface above class property overrides",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Identified.kt"),
            r#"package boundary

interface Identified {
    var id: String
}


"#,
        )
        .unwrap();
        fs::write(
            root.join("JavaRoot.kt"),
            r#"package boundary

interface JavaRoot : Identified
"#,
        )
        .unwrap();
        fs::write(
            root.join("ParentRecord.kt"),
            r#"package boundary

abstract class ParentRecord(override open var id: String) : JavaRoot {
    suspend fun checkpoint() {}
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("ChildRecord.kt"),
            r#"package boundary

class ChildRecord(override var id: String) : ParentRecord(id), Identified {
    suspend fun childCheckpoint() {}
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package boundary

fun main() {
    val record: Identified = ChildRecord("value-7")
    println(record.id)
}
"#,
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("original Kotlin property hierarchy compiles");

    let selected = [
        mixed_root.join("Identified.kt"),
        mixed_root.join("JavaRoot.kt"),
        mixed_root.join("ParentRecord.kt"),
        mixed_root.join("ChildRecord.kt"),
    ];
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .args(selected);
    let transpile = command
        .output()
        .expect("run notlin on the selected hierarchy");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        mixed_root.join("JavaRoot.kt").is_file() && !mixed_root.join("JavaRoot.java").exists(),
        "the selected JavaRoot interface must stay Kotlin above retained class property overrides"
    );
    assert!(mixed_root.join("ParentRecord.kt").is_file());
    assert!(mixed_root.join("ChildRecord.kt").is_file());

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report =
        validate_jvm_sources(&mixed).expect("retained Kotlin property hierarchy compiles");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "the mixed hierarchy must preserve the inherited property value"
    );
}

/// Kotlin's read-only List parameter is covariant even at a Java constructor
/// boundary, so emitted constructor properties need a wildcard signature.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_readonly_list_covariance_in_constructor_properties() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "readonly list covariance in constructor properties",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("BaseRecord.kt"),
            "package boundary\ninterface BaseRecord { fun label(): String }\n",
        )
        .unwrap();
        fs::write(
            root.join("SpecificRecord.kt"),
            "package boundary\nclass SpecificRecord : BaseRecord { override fun label(): String = \"specific\" }\n",
        )
        .unwrap();
        fs::write(
            root.join("RecordBatch.kt"),
            "package boundary\nclass RecordBatch(val records: List<BaseRecord>)\n",
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            "package boundary\nfun main() { println(RecordBatch(listOf(SpecificRecord())).records.single().label()) }\n",
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [
        baseline_root.join("BaseRecord.kt"),
        baseline_root.join("SpecificRecord.kt"),
        baseline_root.join("RecordBatch.kt"),
        baseline_root.join("Runner.kt"),
    ]
    .into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline)
        .expect("original covariant Kotlin constructor call compiles");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .arg(mixed_root.join("RecordBatch.kt"));
    let transpile = command.output().expect("run notlin on RecordBatch");
    assert!(
        transpile.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let generated = fs::read_to_string(mixed_root.join("RecordBatch.java")).unwrap();
    assert!(
        generated.contains("List<? extends BaseRecord> records"),
        "the Java constructor/property ABI must preserve Kotlin List covariance:\n{generated}"
    );

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = [
        mixed_root.join("BaseRecord.kt"),
        mixed_root.join("SpecificRecord.kt"),
        mixed_root.join("Runner.kt"),
    ]
    .into();
    mixed.java_sources = [mixed_root.join("RecordBatch.java")].into();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report =
        validate_jvm_sources(&mixed).expect("Java constructor accepts a covariant Kotlin List");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "the mixed runtime must preserve the list contents"
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_nonnull_collection_bridge_to_nullable_contract() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "nonnull collection nullable contract",
    );
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("BaseEntry.java"),
            "package boundary; public interface BaseEntry { String label(); }\n",
        )
        .unwrap();
        fs::write(root.join("ChildEntry.java"), "package boundary; public class ChildEntry implements BaseEntry { public String label() { return \"entry\"; } }\n").unwrap();
        fs::write(
            root.join("Api.kt"),
            "package boundary\ninterface Api {\n val entries: List<BaseEntry>?\n}\n",
        )
        .unwrap();
        fs::write(root.join("Concrete.kt"), "package boundary\nclass Concrete(override val entries: List<ChildEntry>) : Api {\n suspend fun nativeOnly() {}\n}\n").unwrap();
        fs::write(root.join("Runner.kt"), "package boundary\nfun main() {\n val entries = listOf(ChildEntry())\n val concrete = Concrete(entries)\n val api: Api = concrete\n println(\"${api.entries!!.single().label()}|${concrete.entries === entries}|${api.entries === entries}\")\n}\n").unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = ["Api.kt", "Concrete.kt", "Runner.kt"]
        .iter()
        .map(|name| original.join(name))
        .collect();
    baseline.java_sources = ["BaseEntry.java", "ChildEntry.java"]
        .iter()
        .map(|name| original.join(name))
        .collect();
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let original_result =
        validate_jvm_sources(&baseline).expect("original nullable collection contract compiles");
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(migrated.join("Api.kt"))
        .arg(migrated.join("Concrete.kt"))
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("Api.java").is_file(),
        "nullable interface must translate: {}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("Concrete.kt").is_file(),
        "suspend member keeps concrete class Kotlin"
    );
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = ["Concrete.kt", "Runner.kt"]
        .iter()
        .map(|name| migrated.join(name))
        .collect();
    mixed.java_sources = ["Api.java", "BaseEntry.java", "ChildEntry.java"]
        .iter()
        .map(|name| migrated.join(name))
        .collect();
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let migrated_result = validate_jvm_sources(&mixed)
        .expect("Java interface and retained Kotlin getter bridge compile");
    assert_eq!(
        original_result.commands.last().unwrap().stdout.trim(),
        "entry|true|true"
    );
    assert_eq!(
        original_result.commands.last().unwrap().stdout,
        migrated_result.commands.last().unwrap().stdout
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_default_collection_factory_with_platform_wildcard() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "collection factory platform wildcard",
    );
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Entry.java"),
            "package boundary; public final class Entry {}\n",
        )
        .unwrap();
        fs::write(root.join("Variant.kt"), "package boundary\nimport java.util.*\ninterface Variant {\n val entries: List<Entry>\n  get() = arrayListOf()\n val names: List<String>\n  get() = emptyList()\n val tags: Set<String>\n  get() = emptySet()\n val values: Map<String, String>\n  get() = emptyMap()\n}\n").unwrap();
        fs::write(
            root.join("Provider.kt"),
            "package boundary\nclass Provider : Variant\n",
        )
        .unwrap();
        fs::write(root.join("Message.kt"), "package boundary\nimport java.util.*\nimport java.io.Serializable\ndata class Message @JvmOverloads constructor(val values: Map<String, String> = hashMapOf()) : Serializable\n").unwrap();
        fs::write(root.join("Runner.kt"), "package boundary\nfun main() {\n val provider: Variant = Provider()\n val first = provider.entries as MutableList<Entry>\n first.add(Entry())\n val second = provider.entries\n val message = Message()\n val other = Message()\n (message.values as MutableMap<String, String>)[\"key\"] = \"value\"\n println(\"${first.size}|${second.size}|${first === second}|${message.values.size}|${other.values.size}|${message.values === other.values}\")\n}\n").unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlinc_args.push("-Xjvm-default=all".into());
    baseline.kotlin_sources = ["Variant.kt", "Provider.kt", "Message.kt", "Runner.kt"]
        .iter()
        .map(|name| original.join(name))
        .collect();
    baseline.java_sources = vec![original.join("Entry.java")];
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let original_result =
        validate_jvm_sources(&baseline).expect("original default factory compiles");
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(migrated.join("Variant.kt"))
        .arg(migrated.join("Provider.kt"))
        .arg(migrated.join("Message.kt"))
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("Variant.java").is_file(),
        "default factory interface must translate: {}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("Message.java").is_file(),
        "constructor factory default must translate: {}",
        String::from_utf8_lossy(&translation.stderr)
    );
    fs::write(migrated.join("PolicyRunner.java"), r#"package boundary; public class PolicyRunner { public static void main(String[] args) { Provider provider = new Provider(); java.util.List<?> names = provider.getNames(); names.add(null); java.util.Set<?> tags = provider.getTags(); tags.add(null); java.util.Map<?, ?> values = provider.getValues(); values.put(null, null); System.out.println(names.size() + "|" + tags.size() + "|" + values.size()); } }"#).unwrap();
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlinc_args.push("-Xjvm-default=all".into());
    for entry in fs::read_dir(&migrated).unwrap().flatten() {
        let path = entry.path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(path),
            Some("java") => mixed.java_sources.push(path),
            _ => {}
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let migrated_result = validate_jvm_sources(&mixed).expect("migrated default factory compiles");
    assert_eq!(
        original_result.commands.last().unwrap().stdout.trim(),
        "1|0|false|1|0|false"
    );
    assert_eq!(
        original_result.commands.last().unwrap().stdout,
        migrated_result.commands.last().unwrap().stdout
    );
    let mut policy = mixed.clone();
    policy.output_dir = scratch.0.join("policy classes");
    policy.run_main_class = Some("boundary.PolicyRunner".into());
    let policy_result =
        validate_jvm_sources(&policy).expect("standard Java collections permit mutation");
    assert_eq!(
        policy_result.commands.last().unwrap().stdout.trim(),
        "1|1|1"
    );
}

#[test]
#[ignore = "requires JVM tool paths, Kotlin stdlib, Lombok, and Kotlin Lombok plugin"]
fn real_toolchain_preserves_plain_class_identity_with_lombok_accessors() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "plain class identity lombok",
    );
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("Holder.kt"), "package boundary\nimport java.util.*\nclass Holder(val key: String, var count: Int) {\n val entries: MutableList<String> = mutableListOf()\n}\n").unwrap();
        fs::write(root.join("Runner.kt"), "package boundary\nfun main() {\n val first = Holder(\"key\", 1)\n val second = Holder(\"key\", 1)\n val seen = hashSetOf(first)\n first.entries.add(\"entry\")\n first.count = 2\n println(\"${first === second}|${first == second}|${seen.contains(first)}|${first.count}|${first.entries.size}|${first.toString().startsWith(\"boundary.Holder@\")}\")\n}\n").unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = ["Holder.kt", "Runner.kt"]
        .iter()
        .map(|name| original.join(name))
        .collect();
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let original_result = validate_jvm_sources(&baseline).expect("original plain class compiles");
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--lombok", "--in-place"])
        .arg(migrated.join("Holder.kt"))
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let generated = fs::read_to_string(migrated.join("Holder.java")).unwrap();
    assert!(generated.contains("@Getter"), "{generated}");
    assert!(
        !generated.contains("@Data"),
        "plain classes must retain identity equality: {generated}"
    );
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    enable_lombok_processing(&mut mixed);
    mixed.kotlin_sources = vec![migrated.join("Runner.kt")];
    mixed.java_sources = vec![migrated.join("Holder.java")];
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let migrated_result = validate_jvm_sources(&mixed)
        .expect("Lombok accessors compile without adding value equality");
    assert_eq!(
        original_result.commands.last().unwrap().stdout.trim(),
        "false|false|true|2|1|true"
    );
    assert_eq!(
        original_result.commands.last().unwrap().stdout,
        migrated_result.commands.last().unwrap().stdout
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_enum_with_retained_generic_collection_contract() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "enum covariant constructor input",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("Identifiable.kt"),
            "package boundary\ninterface Identifiable {\n    fun label(): String\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("SpecificEntry.kt"),
            "package boundary\nclass SpecificEntry : Identifiable {\n    override fun label(): String = \"specific-entry\"\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("BatchContract.kt"),
            "package boundary\ninterface BatchContract<T> {\n    val entries: List<T>\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("EntryBatch.kt"),
            "package boundary\nenum class EntryBatch(\n    override val entries: List<Identifiable>\n) : BatchContract<Identifiable> {\n    SINGLE(listOf(SpecificEntry()))\n}\n",
        )
        .unwrap();
        fs::write(root.join("Runner.kt"), "package boundary\nfun main() { val batch: BatchContract<Identifiable> = EntryBatch.SINGLE; println(batch.entries.single().label()) }\n").unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("baseline generic enum property compiles");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .arg(mixed_root.join("EntryBatch.kt"));
    let output = command.output().expect("run notlin on the enum");
    assert!(
        output.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        mixed_root.join("EntryBatch.kt").is_file(),
        "the enum remains Kotlin when its inherited generic property contract is retained"
    );
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report =
        validate_jvm_sources(&mixed).expect("migrated enum compiles over retained Kotlin contract");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_invariant_getters_with_covariant_constructor_inputs() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "invariant getters with covariant constructor inputs",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("BaseRecord.kt"),
            "package boundary\ninterface BaseRecord { fun label(): String }\n",
        )
        .unwrap();
        fs::write(root.join("SpecificRecord.kt"), "package boundary\nclass SpecificRecord : BaseRecord { override fun label(): String = \"specific\" }\n").unwrap();
        fs::write(
            root.join("BaseMetadata.kt"),
            "package boundary\ninterface BaseMetadata { fun value(): String }\n",
        )
        .unwrap();
        fs::write(root.join("SpecificMetadata.kt"), "package boundary\nclass SpecificMetadata : BaseMetadata { override fun value(): String = \"meta\" }\n").unwrap();
        fs::write(root.join("RecordHolder.kt"), "package boundary\nclass RecordHolder(val entries: List<BaseRecord>, val metadata: Map<String, BaseMetadata>)\n").unwrap();
        fs::write(root.join("RecordBatch.kt"), "package boundary\ndata class RecordBatch(val entries: List<BaseRecord>, val metadata: Map<String, BaseMetadata>)\n").unwrap();
        fs::write(root.join("Runner.kt"), r#"package boundary
fun main() {
    val entries: List<SpecificRecord> = listOf(SpecificRecord())
    val metadata: Map<String, SpecificMetadata> = mapOf("origin" to SpecificMetadata())
    val holder = RecordHolder(entries, metadata)
    println("${holder.entries.single().label()}:${holder.metadata.getValue("origin").value()}:${JavaConsumer.verify(holder, entries, metadata)}")
    val batch = RecordBatch(entries, metadata)
    println("${batch.entries.single().label()}:${batch.metadata.getValue("origin").value()}:${JavaConsumer.verify(batch, entries, metadata)}")
}
"#).unwrap();
        fs::write(root.join("JavaConsumer.java"), r#"package boundary;
import java.util.List;
import java.util.Map;
public final class JavaConsumer {
    public static boolean verify(RecordHolder batch, List<SpecificRecord> originalEntries, Map<String, SpecificMetadata> originalMetadata) {
        List<BaseRecord> entries = batch.getEntries();
        Map<String, BaseMetadata> metadata = batch.getMetadata();
        return ((Object) entries == originalEntries) && ((Object) metadata == originalMetadata)
                && entries.get(0) == originalEntries.get(0) && metadata.get("origin") == originalMetadata.get("origin");
    }
    public static boolean verify(RecordBatch batch, List<SpecificRecord> originalEntries, Map<String, SpecificMetadata> originalMetadata) {
        List<BaseRecord> entries = batch.getEntries();
        Map<String, BaseMetadata> metadata = batch.getMetadata();
        return ((Object) entries == originalEntries) && ((Object) metadata == originalMetadata)
                && entries.get(0) == originalEntries.get(0) && metadata.get("origin") == originalMetadata.get("origin");
    }
}
"#).unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    baseline.java_sources = [baseline_root.join("JavaConsumer.java")].into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline)
        .expect("original Kotlin constructor/getter ABI compiles for Java consumer");

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(&mixed_root);
    let output = command.output().expect("run notlin on RecordBatch");
    assert!(
        output.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let generated = fs::read_to_string(mixed_root.join("RecordHolder.java")).unwrap();
    assert!(
        generated.contains("private final List<BaseRecord> entries;")
            && generated.contains("private final Map<String,BaseMetadata> metadata;")
            && generated.contains("@Getter")
            && !generated.contains("@Data"),
        "getters must remain invariant:\n{generated}"
    );
    assert!(
        generated.contains("List<? extends BaseRecord> entries")
            && generated.contains("Map<String, ? extends BaseMetadata> metadata"),
        "constructor inputs must preserve Kotlin covariance:\n{generated}"
    );
    let generated_data = fs::read_to_string(mixed_root.join("RecordBatch.java")).unwrap();
    assert!(
        generated_data.contains("List<BaseRecord> getEntries()")
            && generated_data.contains("Map<String,BaseMetadata> getMetadata()"),
        "data class getters must remain invariant:\n{generated_data}"
    );
    assert!(
        generated_data.contains("List<? extends BaseRecord> entries")
            && generated_data.contains("Map<String, ? extends BaseMetadata> metadata"),
        "data class constructor inputs must preserve Kotlin covariance:\n{generated_data}"
    );
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| {
                extension == "kt"
                    && path
                        .file_name()
                        .is_some_and(|name| name != "RecordHolder.kt" && name != "RecordBatch.kt")
            })
        })
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    enable_lombok_processing(&mut mixed);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report = validate_jvm_sources(&mixed)
        .expect("migrated class preserves getter typing and constructor variance");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "constructor bridge must preserve collection identity"
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_getter_routing_annotation_on_generic_command_interface() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "getter routing annotation on command interface",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::create_dir_all(root.join("routing")).unwrap();
        fs::write(
            root.join("routing/RoutingKey.java"),
            "package routing;\nimport java.lang.annotation.ElementType;\nimport java.lang.annotation.Retention;\nimport java.lang.annotation.RetentionPolicy;\nimport java.lang.annotation.Target;\n@Retention(RetentionPolicy.RUNTIME)\n@Target(ElementType.METHOD)\npublic @interface RoutingKey {}\n",
        )
        .unwrap();
        fs::write(
            root.join("CommandKey.kt"),
            "package boundary\ninterface CommandKey {\n    fun value(): String\n}\nclass SpecificKey(private val text: String) : CommandKey {\n    override fun value(): String = text\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("BaseCommand.kt"),
            "package boundary\nimport routing.RoutingKey\ninterface BaseCommand<T : CommandKey> {\n    val key: T\n    @get:RoutingKey val aggregateIdentifier: CommandKey\n        get() = key\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("GeneratedCommand.kt"),
            "package boundary\ndata class GeneratedCommand(\n    override val key: SpecificKey\n) : BaseCommand<SpecificKey>\n",
        )
        .unwrap();
        fs::write(
            root.join("Runner.kt"),
            r#"package boundary
import routing.RoutingKey
fun main() {
    val getter = BaseCommand::class.java.getMethod("getAggregateIdentifier")
    val command = GeneratedCommand(SpecificKey("command-key"))
    println("${getter.isAnnotationPresent(RoutingKey::class.java)}:${command.aggregateIdentifier.value()}")
}
"#,
        )
        .unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    baseline.java_sources = [baseline_root.join("routing/RoutingKey.java")].into();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("baseline annotated Kotlin interface compiles");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .arg(mixed_root.join("BaseCommand.kt"))
        .arg(mixed_root.join("GeneratedCommand.kt"))
        .output()
        .expect("run notlin on the generic command interface");
    assert!(
        output.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        mixed_root.join("BaseCommand.java").is_file(),
        "the annotated generic command interface should translate"
    );
    assert!(
        mixed_root.join("GeneratedCommand.java").is_file(),
        "the concrete data command should translate"
    );
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "kt")
                && !path.with_extension("java").is_file()
        })
        .collect();
    mixed.java_sources = vec![mixed_root.join("routing/RoutingKey.java")];
    mixed.java_sources.extend(
        fs::read_dir(&mixed_root)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "java")
            })
            .collect::<Vec<_>>(),
    );
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report = validate_jvm_sources(&mixed)
        .expect("migrated generic command interface and data class compile");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "the runtime routing annotation and command value must be preserved"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, and MapStruct jars in NOTLIN_* environment variables"]
fn real_toolchain_preserves_most_specific_inherited_reference_getter() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "most specific inherited reference getter",
    );
    let mapstruct = std::env::var_os("NOTLIN_MAPSTRUCT").map(PathBuf::from);
    let mapstruct_processor = std::env::var_os("NOTLIN_MAPSTRUCT_PROCESSOR").map(PathBuf::from);
    let (Some(mapstruct), Some(mapstruct_processor)) = (mapstruct, mapstruct_processor) else {
        panic!(
            "set NOTLIN_MAPSTRUCT and NOTLIN_MAPSTRUCT_PROCESSOR to the MapStruct API and processor jars"
        );
    };
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("ReferenceId.kt"),
            "package boundary\ninterface Identifier {\n    fun value(): String\n}\ninterface ObjectId : Identifier\ndata class ReferenceId(private val raw: String) : ObjectId {\n    override fun value(): String = raw\n}\nobject ReferenceIds {\n    val managed = ReferenceId(\"managed\")\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("ReferenceRoot.kt"),
            "package boundary\ninterface ReferenceRoot {\n    val reference: Identifier\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("NarrowReferenceContract.kt"),
            "package boundary\ninterface NarrowReferenceContract : ReferenceRoot {\n    override val reference: ReferenceId\n        get() = ReferenceIds.managed\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("NarrowReferenceProvider.kt"),
            "package boundary\ninterface NarrowReferenceProvider : NarrowReferenceContract\n",
        )
        .unwrap();
        fs::write(
            root.join("BroadReferenceContract.kt"),
            "package boundary\ninterface BroadReferenceContract : ReferenceRoot\n",
        )
        .unwrap();
        fs::write(
            root.join("RetainedReferenceContract.kt"),
            "package boundary\ninterface RetainedReferenceContract : NarrowReferenceProvider, BroadReferenceContract {\n    suspend fun pending(): String\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("ReferenceOutput.kt"),
            "package boundary\ndata class ReferenceOutput(val marker: String) : RetainedReferenceContract {\n    val memoizedMarker by lazy { marker.lowercase() }\n    override suspend fun pending(): String = marker\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("ParentEntity.java"),
            "package boundary;\npublic class ParentEntity {}\n",
        )
        .unwrap();
        fs::write(
            root.join("ChildEntity.java"),
            "package boundary;\npublic final class ChildEntity {\n    private ParentEntity parent;\n    public ParentEntity getParent() { return parent; }\n    public void setParent(ParentEntity parent) { this.parent = parent; }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("ParentResolver.java"),
            "package boundary;\nimport org.mapstruct.TargetType;\npublic interface ParentResolver {\n    ParentEntity MANAGED = new ParentEntity();\n    default <T extends ParentEntity> T resolve(ObjectId source, @TargetType Class<T> type) { return type.cast(MANAGED); }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("ReferenceMapper.java"),
            "package boundary;\nimport org.mapstruct.Mapper;\nimport org.mapstruct.Mapping;\n@Mapper\npublic interface ReferenceMapper extends ParentResolver {\n    @Mapping(target = \"parent\", source = \"reference\")\n    ChildEntity map(ReferenceOutput source);\n}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("generated-java")).unwrap();
        let runner = r#"package boundary
import java.beans.Introspector
fun main() {
    val descriptor = Introspector.getBeanInfo(ReferenceOutput::class.java).propertyDescriptors
        .first { it.name == "reference" }
    val source = ReferenceOutput("kept")
    val mapper = Class.forName("boundary.ReferenceMapperImpl").getConstructor().newInstance()
    val mapped = mapper.javaClass.getMethod("map", ReferenceOutput::class.java).invoke(mapper, source)
    val parent = mapped.javaClass.getMethod("getParent").invoke(mapped)
    println("${descriptor.readMethod.returnType.name}:${parent === ParentResolver.MANAGED}")
}

"#;
        fs::write(root.join("Runner.kt"), runner).unwrap();
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.java_sources = fs::read_dir(&baseline_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    baseline.classpath.push(mapstruct.clone());
    let processor_path = std::env::join_paths([&mapstruct_processor, &mapstruct]).unwrap();
    baseline.javac_args.extend([
        "-processorpath".into(),
        processor_path,
        "-processor".into(),
        "org.mapstruct.ap.MappingProcessor".into(),
        "-Amapstruct.suppressGeneratorTimestamp=true".into(),
        "-s".into(),
        baseline_root.join("generated-java").into_os_string(),
    ]);
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("baseline intersecting Kotlin interfaces compile");
    let baseline_mapper =
        fs::read_to_string(baseline_root.join("generated-java/boundary/ReferenceMapperImpl.java"))
            .expect("baseline MapStruct implementation is generated");
    assert!(
        baseline_mapper.contains("resolve("),
        "baseline mapping must use the generic resolver:\n{baseline_mapper}"
    );
    assert!(
        !baseline_mapper.contains("ToParentEntity("),
        "baseline must not forge an identifier-to-parent mapping:\n{baseline_mapper}"
    );
    assert_eq!(
        baseline_report.commands.last().unwrap().stdout.trim(),
        "boundary.ReferenceId:true",
        "the baseline must expose and resolve the narrow default getter"
    );

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .arg(mixed_root.join("ReferenceRoot.kt"))
        .arg(mixed_root.join("NarrowReferenceContract.kt"))
        .arg(mixed_root.join("NarrowReferenceProvider.kt"))
        .arg(mixed_root.join("BroadReferenceContract.kt"))
        .arg(mixed_root.join("RetainedReferenceContract.kt"))
        .arg(mixed_root.join("ReferenceOutput.kt"))
        .output()
        .expect("run notlin on the broad and narrow reference interfaces");
    assert!(
        output.status.success(),
        "transpiler failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(mixed_root.join("ReferenceRoot.java").is_file());
    assert!(
        !mixed_root.join("NarrowReferenceContract.java").exists(),
        "the selected covariant default provider should stay Kotlin:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!mixed_root.join("RetainedReferenceContract.java").exists());
    assert!(!mixed_root.join("ReferenceOutput.java").exists());

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "kt")
                && !path.with_extension("java").is_file()
        })
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    mixed.classpath.push(mapstruct.clone());
    let processor_path = std::env::join_paths([&mapstruct_processor, &mapstruct]).unwrap();
    mixed.javac_args.extend([
        "-processorpath".into(),
        processor_path,
        "-processor".into(),
        "org.mapstruct.ap.MappingProcessor".into(),
        "-Amapstruct.suppressGeneratorTimestamp=true".into(),
        "-s".into(),
        mixed_root.join("generated-java").into_os_string(),
    ]);
    let mixed_report =
        validate_jvm_sources(&mixed).expect("migrated broad/narrow reference bridge compiles");
    let mixed_mapper =
        fs::read_to_string(mixed_root.join("generated-java/boundary/ReferenceMapperImpl.java"))
            .expect("migrated MapStruct implementation is generated");
    assert!(
        mixed_mapper.contains("resolve("),
        "migrated mapping must use the generic resolver:\n{mixed_mapper}"
    );
    assert!(
        !mixed_mapper.contains("ToParentEntity("),
        "migrated mapping must not forge an identifier-to-parent mapping:\n{mixed_mapper}"
    );
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "reflection and any available MapStruct processor must select the same most-specific getter"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_multiple_runtime_class_annotations_on_lombok_class() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "multiple runtime class annotations",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::create_dir_all(root.join("jakarta/persistence")).unwrap();
        fs::create_dir_all(root.join("org/hibernate/annotations")).unwrap();
        fs::write(
            root.join("RecordBase.kt"),
            "package boundary\nopen class RecordBase(val id: String = \"\")\ninterface RecordRepository<T>\n",
        )
        .unwrap();
        fs::write(
            root.join("jakarta/persistence/Entity.java"),
            "package jakarta.persistence;\n@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)\n@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)\npublic @interface Entity {}\n",
        )
        .unwrap();
        fs::write(
            root.join("jakarta/persistence/Table.java"),
            "package jakarta.persistence;\n@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)\n@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)\npublic @interface Table { String name(); }\n",
        )
        .unwrap();
        fs::write(
            root.join("org/hibernate/annotations/Immutable.java"),
            "package org.hibernate.annotations;\n@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)\n@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)\npublic @interface Immutable {}\n",
        )
        .unwrap();
        fs::write(
            root.join("org/hibernate/annotations/SQLRestriction.java"),
            "package org.hibernate.annotations;\n@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)\n@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE)\npublic @interface SQLRestriction { String value(); }\n",
        )
        .unwrap();
        fs::write(
            root.join("ResourceRecord.kt"),
            r#"package boundary
import jakarta.persistence.*
import org.hibernate.annotations.Immutable
import org.hibernate.annotations.SQLRestriction

@Entity
@Table(name="RESOURCE")
@Immutable
@SQLRestriction("CLASS='RESOURCE_SET'")
class ResourceRecord(
    id: String = "",
    val label: String = "record",
    val enabled: Boolean = true
) : RecordBase(id)

interface ResourceRecordRepository : RecordRepository<ResourceRecord>
"#,
        )
        .unwrap();
        fs::write(
            root.join("AnnotationProbe.java"),
            r#"package boundary;
import java.util.Arrays;
import java.util.Set;
import java.util.stream.Collectors;
public final class AnnotationProbe {
    public static void main(String[] args) {
        Class<?> type = ResourceRecord.class;
        Set<Class<?>> checked = Set.of(jakarta.persistence.Entity.class, jakarta.persistence.Table.class,
                org.hibernate.annotations.Immutable.class, org.hibernate.annotations.SQLRestriction.class);
        String values = Arrays.stream(type.getAnnotations())
                .filter(annotation -> checked.contains(annotation.annotationType()))
                .map(annotation -> annotation.annotationType().getName() + "=" + annotation.toString())
                .sorted().collect(Collectors.joining("|"));
        long count = Arrays.stream(type.getAnnotations())
                .filter(annotation -> checked.contains(annotation.annotationType())).count();
        System.out.println(count + "|" + values);
    }
}
"#,
        )
        .unwrap();
    }

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let annotated_source = fs::read_to_string(mixed_root.join("ResourceRecord.kt")).unwrap();
    let annotated_tree = parser.parse(&annotated_source, None).unwrap();
    fn annotation_chain_depth(node: tree_sitter::Node<'_>) -> usize {
        let mut depth = 0;
        if node.kind() == "annotated_expression" {
            depth = 1;
            for index in 0..node.named_child_count() {
                let child = node.named_child(index).unwrap();
                if child.kind() == "annotated_expression" {
                    depth = depth.max(1 + annotation_chain_depth(child));
                }
            }
        } else {
            for index in 0..node.named_child_count() {
                depth = depth.max(annotation_chain_depth(node.named_child(index).unwrap()));
            }
        }
        depth
    }
    assert!(
        annotation_chain_depth(annotated_tree.root_node()) >= 4,
        "fixture must reproduce four nested top-level annotated_expression wrappers:\n{}",
        annotated_tree.root_node().to_sexp()
    );

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [
        baseline_root.join("RecordBase.kt"),
        baseline_root.join("ResourceRecord.kt"),
    ]
    .into();
    baseline.java_sources = vec![
        baseline_root.join("jakarta/persistence/Entity.java"),
        baseline_root.join("jakarta/persistence/Table.java"),
        baseline_root.join("org/hibernate/annotations/Immutable.java"),
        baseline_root.join("org/hibernate/annotations/SQLRestriction.java"),
        baseline_root.join("AnnotationProbe.java"),
    ];
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.AnnotationProbe".into());
    let baseline_report =
        validate_jvm_sources(&baseline).expect("baseline annotated Kotlin class compiles");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place", "--lombok"])
        .arg(mixed_root.join("ResourceRecord.kt"))
        .output()
        .expect("run notlin on the annotated ordinary class");
    assert!(
        output.status.success(),
        "transpiler failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        mixed_root.join("ResourceRecord.java").is_file(),
        "the annotated ordinary class should be converted"
    );
    let generated = fs::read_to_string(mixed_root.join("ResourceRecord.java")).unwrap();
    assert!(
        generated.contains("@Getter") && !generated.contains("@Data"),
        "the ordinary class should exercise Lombok class emission:\n{generated}"
    );
    for expected in [
        "Entity",
        "Table",
        "RESOURCE",
        "SQLRestriction",
        "CLASS='RESOURCE_SET'",
        "Immutable",
    ] {
        assert!(
            generated.contains(expected),
            "class annotation {expected} must be emitted with its value:\n{generated}"
        );
    }

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = vec![
        mixed_root.join("jakarta/persistence/Entity.java"),
        mixed_root.join("jakarta/persistence/Table.java"),
        mixed_root.join("org/hibernate/annotations/Immutable.java"),
        mixed_root.join("org/hibernate/annotations/SQLRestriction.java"),
        mixed_root.join("AnnotationProbe.java"),
    ];
    for file_name in ["ResourceRecord.java", "ResourceRecordRepository.java"] {
        let path = mixed_root.join(file_name);
        if path.is_file() {
            mixed.java_sources.push(path);
        }
    }
    mixed.classpath.push(PathBuf::from(stdlib));
    enable_lombok_processing(&mut mixed);
    mixed.run_main_class = Some("boundary.AnnotationProbe".into());
    let mixed_report =
        validate_jvm_sources(&mixed).expect("migrated annotated Lombok class compiles and runs");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_report.commands.last().unwrap().stdout.trim(),
        "all runtime class annotations and annotation values must match"
    );
    assert!(
        mixed_report
            .commands
            .last()
            .unwrap()
            .stdout
            .starts_with("4|"),
        "all four mapped class annotations must be visible at runtime: {}",
        mixed_report.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_recovers_stacked_java_annotations_on_default_interface_getters() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "stacked annotations on interface getters",
    );
    let baseline_root = scratch.0.join("baseline source");
    let mixed_root = scratch.0.join("mixed source");
    let source = r#"package boundary

interface Before {
    fun before(): String
}

@Marker(label = "contract", one = TypeA::class, many = [TypeB::class, TypeC::class])
@Qualifier(TypeD::class)
interface Annotated {
    @get:Marker(label = "getter", one = TypeE::class, many = [TypeF::class, TypeG::class])
    val value: Any

    val fallback: String
        get() = "default-value"
}

interface After {
    fun after(): String
}
"#;
    let runner_source = r#"package boundary

class Provider(private val payload: Any) : Annotated {
    override val value: Any
        get() = payload
}

fun main() {
    val payload = Any()
    val instance = Provider(payload)
    val type = Annotated::class.java
    val marker = type.getAnnotation(Marker::class.java)
    val qualifier = type.getAnnotation(Qualifier::class.java)
    val getterMarker = type.getMethod("getValue").getAnnotation(Marker::class.java)
    val defaultGetter = type.getMethod("getFallback")
    val types = marker.many.joinToString(",") { it.java.name }
    val getterTypes = getterMarker.many.joinToString(",") { it.java.name }
    println(listOf(
        marker.label, marker.one.java.name, types,
        qualifier.value.java.name,
        getterMarker.label, getterMarker.one.java.name, getterTypes,
        defaultGetter.isDefault.toString(), instance.fallback,
        (instance.value === payload).toString()
    ).joinToString("|"))
}
"#;

    let marker_classes = [
        "TypeA", "TypeB", "TypeC", "TypeD", "TypeE", "TypeF", "TypeG",
    ];
    for root in [&baseline_root, &mixed_root] {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("Declarations.kt"), source).unwrap();
        fs::write(root.join("Runner.kt"), runner_source).unwrap();
        fs::write(
            root.join("Marker.java"),
            "package boundary;\nimport java.lang.annotation.ElementType;\nimport java.lang.annotation.Retention;\nimport java.lang.annotation.RetentionPolicy;\nimport java.lang.annotation.Target;\n@Retention(RetentionPolicy.RUNTIME)\n@Target({ElementType.TYPE, ElementType.METHOD})\npublic @interface Marker { String label(); Class<?> one(); Class<?>[] many(); }\n",
        )
        .unwrap();
        fs::write(
            root.join("Qualifier.java"),
            "package boundary;\nimport java.lang.annotation.ElementType;\nimport java.lang.annotation.Retention;\nimport java.lang.annotation.RetentionPolicy;\nimport java.lang.annotation.Target;\n@Retention(RetentionPolicy.RUNTIME)\n@Target(ElementType.TYPE)\npublic @interface Qualifier { Class<?> value(); }\n",
        )
        .unwrap();
        for name in marker_classes {
            fs::write(
                root.join(format!("{name}.java")),
                format!("package boundary;\npublic final class {name} {{}}\n"),
            )
            .unwrap();
        }
    }

    let ast = notlin::transpiler::dump_ast(source);
    assert!(
        ast.contains("annotated_expression"),
        "fixture must include the parser's annotated_expression recovery shape:\n{ast}"
    );
    assert!(
        ast.contains("infix_expression"),
        "fixture must reproduce the class-literal annotation's infix_expression ambiguity:\n{ast}"
    );

    let stdlib = PathBuf::from(stdlib);
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline.kotlinc_args.push("-Xjvm-default=all".into());
    baseline.kotlin_sources = [
        baseline_root.join("Declarations.kt"),
        baseline_root.join("Runner.kt"),
    ]
    .into();
    baseline.java_sources = vec![
        baseline_root.join("Marker.java"),
        baseline_root.join("Qualifier.java"),
    ];
    baseline.java_sources.extend(
        marker_classes
            .iter()
            .map(|name| baseline_root.join(format!("{name}.java"))),
    );
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let baseline_report = validate_jvm_sources(&baseline)
        .expect("baseline Kotlin interface and runtime annotations compile");
    let baseline_output = baseline_report.commands.last().unwrap().stdout.trim();

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&mixed_root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&mixed_root)
        .output()
        .expect("run notlin on the annotated interface declarations");
    assert!(
        transpile.status.success(),
        "transpiler failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&transpile.stdout),
        String::from_utf8_lossy(&transpile.stderr)
    );
    let generated = mixed_root.join("Annotated.java");
    assert!(
        generated.is_file(),
        "annotated interface between declarations should be recovered and translated:\n{}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let generated_source = fs::read_to_string(&generated).unwrap();
    assert!(
        generated_source.contains(
            "@Marker(label = \"contract\", one = TypeA.class, many = {TypeB.class, TypeC.class})"
        ),
        "stacked annotation Class and Class[] values must be emitted:\n{generated_source}"
    );
    assert!(
        generated_source.contains("@Qualifier(TypeD.class)"),
        "second stacked annotation must be emitted:\n{generated_source}"
    );
    assert!(
        generated_source.contains(
            "@Marker(label = \"getter\", one = TypeE.class, many = {TypeF.class, TypeG.class})"
        ),
        "@get annotation with Class and Class[] values must decorate the Java getter:\n{generated_source}"
    );

    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlinc_args.push("-Xjvm-default=all".into());
    mixed.kotlin_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .collect();
    mixed.java_sources = fs::read_dir(&mixed_root)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .collect();
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let mixed_report = validate_jvm_sources(&mixed)
        .expect("migrated annotated interface and retained Kotlin runner compile");
    assert_eq!(
        mixed_report.commands.last().unwrap().stdout.trim(),
        baseline_output,
        "reflection values, abstract payload getter identity, and default getter behavior must match"
    );
}

/// Compare Kotlin data-class copy calls with the translated Java lowering,
/// including source-order and single-evaluation guarantees for qualified
/// receivers and named arguments.
#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_data_class_copy_behavior() {
    let (Some(kotlinc), Some(javac), Some(java), Some(stdlib)) = (
        std::env::var_os("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA"),
        std::env::var_os("NOTLIN_KOTLIN_STDLIB"),
    ) else {
        panic!("set NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB");
    };
    let scratch = Scratch::new_under(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/jvm-validation"),
        "data class copy behavior",
    );
    let root = scratch.0.join("source root");
    fs::create_dir_all(&root).unwrap();
    let model = root.join("Record.kt");
    let runner = root.join("Runner.kt");
    fs::write(
        &model,
        r#"package boundary
data class Record<T>(val count: Int, val item: T?, val label: String, val enabled: Boolean = false) {
    var receiverCalls: Int = 0
    var trace: String = ""

    private fun markInt(tag: String, value: Int): Int {
        trace += tag
        return value
    }

    private fun markString(tag: String, value: String): String {
        trace += tag
        return value
    }

    private fun combine(left: String, right: String): String = left + right
    private fun add(left: Int, right: Int): Int = left + right

    fun selfCopy(nextCount: Int, nextLabel: String): Record<T> =
        copy(
            label = combine(markString("self-left", nextLabel), markString("self-right", "!")),
            count = nextCount
        )

    private fun nextRecord(): Record<T> {
        receiverCalls += 1
        return Record(10, item, "source", true)
    }

    fun qualifiedCopy(): Record<T> {
        var mutableCount = 4
        var nullableItem: T? = null
        return nextRecord().copy(
            label = combine(markString("qual-left", "new"), markString("qual-right", "!")),
            count = add(markInt("count-left", mutableCount), markInt("count-right", 2)),
            item = nullableItem
        )
    }
}

data class GuardedCopy(@JvmField val item: String) {
    fun getItem(): String = "wrong"
    fun item(): String = "wrong"
    fun duplicate(): GuardedCopy = copy()
}
class SuppliedCopy {
    fun replaced(original: GuardedCopy, next: String): GuardedCopy = original.copy(item = next)
}

"#,
    )
    .unwrap();
    fs::write(
        &runner,
        r#"package boundary
fun main() {
    val original = Record<String>(1, null, "base", true)
    suspend fun retainedCopyHarness() {}
    val self = original.selfCopy(5, "self")
    val qualified = original.qualifiedCopy()
    val guarded = GuardedCopy("right").duplicate()
    val supplied = SuppliedCopy().replaced(guarded, "fresh")
    println("${self.count}|${self.item}|${self.label}|${self.enabled}|${qualified.count}|${qualified.item}|${qualified.label}|${qualified.enabled}|${original.trace}|${original.receiverCalls}|${guarded.item}|${supplied.item}")
}
suspend fun retainedBoundary() {}
"#,
    )
    .unwrap();

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("baseline classes"));
    target_java_17(&mut baseline);
    baseline
        .kotlin_sources
        .extend([model.clone(), runner.clone()]);
    baseline.classpath.push(PathBuf::from(&stdlib));
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original Kotlin copy behavior compiles");
    let before_stdout = before.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        before_stdout,
        "5|null|self!|true|6|null|new!|true|self-leftself-rightqual-leftqual-rightcount-leftcount-right|1|right|fresh"
    );

    let transpile = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root"])
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .expect("run notlin data-class copy migration");
    assert!(
        transpile.status.success(),
        "data-class copy migration failed: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    let generated = root.join("Record.java");
    assert!(
        generated.is_file(),
        "data-class copy owner was retained instead of translated; stderr: {}",
        String::from_utf8_lossy(&transpile.stderr)
    );
    assert!(
        runner.is_file(),
        "suspend boundary should retain the Kotlin runner"
    );
    assert!(
        model.is_file() && !root.join("GuardedCopy.java").exists(),
        "a copy with conflicting custom component accessors must retain its owner"
    );

    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.kotlin_sources.push(runner);
    mixed.kotlin_sources.push(model);
    mixed.java_sources.push(generated);
    let supplied_java = root.join("SuppliedCopy.java");
    assert!(
        supplied_java.is_file(),
        "an explicitly supplied copy should still translate across the retained owner boundary"
    );
    mixed.java_sources.push(supplied_java);
    mixed.classpath.push(PathBuf::from(stdlib));
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("translated copy behavior compiles");
    assert_eq!(
        after.commands.last().unwrap().stdout.trim(),
        before_stdout,
        "translated data-class copies must preserve values, evaluation order, and receiver count"
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_readonly_list_contract_identity_and_getter_annotations() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = PathBuf::from(std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"));
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("readonly list contract identity");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("GetterTag.java"), "package boundary; @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME) @java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD) public @interface GetterTag {}\n").unwrap();
        fs::write(
            root.join("Definitions.kt"),
            "package boundary\ninterface Base {}\nclass Derived : Base {}\n",
        )
        .unwrap();
        fs::write(root.join("Parent.kt"), "package boundary\ninterface Parent {\n @get:GetterTag\n val items: List<Base>\n companion object {\n  suspend fun remainKotlin() {}\n }\n}\n").unwrap();
        fs::write(root.join("Child.kt"), "package boundary\nclass Child(@get:GetterTag override val items: List<Derived>) : Parent {}\n").unwrap();
        fs::write(root.join("Sibling.kt"), "package boundary\nclass Sibling(override val items: List<Derived>) : Parent {\n suspend fun remainKotlin() {}\n}\n").unwrap();
        fs::write(root.join("Runner.kt"), r#"package boundary
fun main() {
 suspend fun retainedHarness() {}
 val items = listOf(Derived())
 val child = Child(items)
 val parent: Parent = child
 val sibling: Parent = Sibling(items)
 val getter = Parent::class.java.getDeclaredMethod("getItems")
 val childGetter = Child::class.java.getDeclaredMethod("getItems")
 val physical = Parent::class.java.declaredMethods.count { it.name == "getItems" }
 println("${child.items === items}|${parent.items === items}|${sibling.items === items}|${getter.isAnnotationPresent(GetterTag::class.java)}|${childGetter.isAnnotationPresent(GetterTag::class.java)}|$physical")
}


"#).unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    for entry in fs::read_dir(&original).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => baseline.kotlin_sources.push(path),
            Some("java") => baseline.java_sources.push(path),
            _ => {}
        }
    }
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original list contracts compile");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        "true|true|true|true|true|1"
    );
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(&migrated)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("Child.java").is_file(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let parent = fs::read_to_string(migrated.join("Parent.kt")).unwrap();
    assert!(
        parent.contains("List<@kotlin.jvm.JvmWildcard Base>"),
        "{parent}"
    );
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    for entry in fs::read_dir(&migrated).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(path),
            Some("java") => mixed.java_sources.push(path),
            _ => {}
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("mixed list contracts compile");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim()
    );
    let javap = javac
        .parent()
        .unwrap()
        .join(if cfg!(windows) { "javap.exe" } else { "javap" });
    let inspection = std::process::Command::new(javap)
        .args(["-p", "-classpath"])
        .arg(&mixed.output_dir)
        .arg("boundary.Parent")
        .output()
        .unwrap();
    assert!(inspection.status.success());
    let signature = String::from_utf8_lossy(&inspection.stdout);
    assert!(
        signature.contains("java.util.List<? extends boundary.Base> getItems()"),
        "{signature}"
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_generic_default_getters_over_retained_descendants() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("generic default getter paths");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("GetterTag.java"), "package boundary; @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME) @java.lang.annotation.Target(java.lang.annotation.ElementType.METHOD) public @interface GetterTag {}\n").unwrap();
        fs::write(
            root.join("RootAware.java"),
            "package boundary; public interface RootAware extends java.io.Serializable { BaseKey getKey(); }\n",
        )
        .unwrap();
        fs::write(root.join("GenericBase.java"), "package boundary; public interface GenericBase<E extends Entity<K>, K extends BaseKey> extends RootAware { K getKey(); }\n").unwrap();
        fs::write(
            root.join("Model.kt"),
            r#"package boundary
interface BaseKey {}
class KeyBox<V>(val value: V) : BaseKey {}
interface Entity<K : BaseKey> : RootAware {
    override fun getKey(): K
    suspend fun keepEntity()
}
class Payload<K : BaseKey>(private val provided: K) : Entity<K> {
    override fun getKey(): K = provided
    override suspend fun keepEntity() {}
}
"#,
        )
        .unwrap();
        fs::write(
            root.join("DefaultEvent.kt"),
            r#"package boundary
interface DefaultEvent<E : Entity<K>, K : BaseKey> : GenericBase<E, K> {
    fun getPayload(): E
    @GetterTag
    override fun getKey(): K = getPayload().key
    @get:GetterTag
    val envelope: Any get() = getKey()
}
interface ForwardEvent<X> : DefaultEvent<Payload<KeyBox<List<X>>>, KeyBox<List<X>>> {}
"#,
        )
        .unwrap();
        fs::write(
            root.join("KeptEvent.kt"),
            r#"package boundary
class KeptEvent<X>(private val provided: Payload<KeyBox<List<X>>>) : ForwardEvent<X> {
    override fun getPayload(): Payload<KeyBox<List<X>>> = provided
    suspend fun remainKotlin() {}
}
class ConcreteEvent(private val provided: Payload<KeyBox<List<String>>>) : ForwardEvent<String>, Comparable<ConcreteEvent> {
    override fun getPayload(): Payload<KeyBox<List<String>>> = provided
    override fun compareTo(other: ConcreteEvent): Int = 0
    suspend fun remainKotlin() {}
}
"#,
        )
        .unwrap();
        fs::write(root.join("Runner.kt"), r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val items = listOf("value")
    val key = KeyBox(items)
    val payload = Payload(key)
    val event: ForwardEvent<String> = KeptEvent(payload)
    val concrete = ConcreteEvent(payload)
    val generic: GenericBase<Payload<KeyBox<List<String>>>, KeyBox<List<String>>> = event
    val broad: RootAware = event
    val keyGetter = DefaultEvent::class.java.getDeclaredMethod("getKey")
    val envelopeGetter = DefaultEvent::class.java.getDeclaredMethod("getEnvelope")
    val physical = DefaultEvent::class.java.declaredMethods.count { it.name == "getKey" && !it.isBridge }
    println("${event.getKey() === key}|${generic.getKey() === key}|${broad.getKey() === key}|${concrete.getKey() === key}|${event.envelope === key}|${key.value === items}|${keyGetter.isAnnotationPresent(GetterTag::class.java)}|${envelopeGetter.isAnnotationPresent(GetterTag::class.java)}|$physical")
}
"#).unwrap();
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    for entry in fs::read_dir(&original).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => baseline.kotlin_sources.push(path),
            Some("java") => baseline.java_sources.push(path),
            _ => {}
        }
    }
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original generic default getters compile");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        "true|true|true|true|true|true|true|true|1"
    );
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(&migrated)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("DefaultEvent.java").is_file(),
        "default getter provider must translate: {}\n{}",
        String::from_utf8_lossy(&translation.stderr),
        fs::read_to_string(migrated.join("DefaultEvent.kt")).unwrap_or_default()
    );
    assert!(
        migrated.join("KeptEvent.kt").is_file(),
        "suspend descendants must remain Kotlin"
    );
    let mut mixed =
        JvmValidationConfig::new(&kotlinc, &javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    for entry in fs::read_dir(&migrated).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(path),
            Some("java") => mixed.java_sources.push(path),
            _ => {}
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("mixed generic default getters compile");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_chained_calls_with_producer_scoped_return_types() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("producer scoped chained returns");
    let root = scratch.0.join("sources");
    fs::create_dir_all(&root).unwrap();
    let mut original_sources = Vec::new();
    for (name, source) in [
        (
            "model/Payload.kt",
            "package model\nclass Payload(private val value: String) {\n val item: Item get() = Item(value)\n fun getId(): String = value\n}\n",
        ),
        (
            "model/Item.kt",
            "package model\nclass Item(private val value: String) {\n fun getId(): String = value\n}\n",
        ),
        (
            "other/Payload.kt",
            "package other\nclass Payload {\n fun getId(index: Int): Int = index\n}\n",
        ),
        (
            "api/Provider.kt",
            "package api\nimport model.Payload\nclass Provider(private val payload: Payload) {\n fun getPayload(): Payload = payload\n}\n",
        ),
        (
            "consumer/Reader.kt",
            "package consumer\nimport api.Provider\nimport other.Payload\nclass Reader {\n fun read(provider: Provider): String = provider.getPayload().item.getId()\n}\n",
        ),
        (
            "consumer/Runner.kt",
            "package consumer\nimport api.Provider\nimport model.Payload\nfun main() {\n suspend fun retainedHarness() {}\n println(Reader().read(Provider(Payload(\"value\"))))\n}\n",
        ),
    ] {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();
        original_sources.push(path);
    }
    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = original_sources;
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("consumer.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original producer-scoped chain compiles");
    assert_eq!(before.commands.last().unwrap().stdout.trim(), "value");

    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        root.join("consumer/Reader.java").is_file(),
        "producer-scoped caller should translate: {}\n{}",
        String::from_utf8_lossy(&translation.stderr),
        fs::read_to_string(root.join("consumer/Reader.kt")).unwrap_or_default()
    );
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    let mut directories = vec![root];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                directories.push(path);
            } else {
                match path.extension().and_then(|extension| extension.to_str()) {
                    Some("kt") => mixed.kotlin_sources.push(path),
                    Some("java") => mixed.java_sources.push(path),
                    _ => {}
                }
            }
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("consumer.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("mixed producer-scoped chain compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_comparison_through_declared_interface_properties() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("declared property comparison");
    let root = scratch.0.join("sources");
    fs::create_dir_all(&root).unwrap();
    for (name, source) in [
        (
            "Keyed.kt",
            "package sample\ninterface Keyed : Comparable<Keyed> {\n val key: String\n override fun compareTo(other: Keyed): Int = this.key.compareTo(other.key)\n}\n",
        ),
        (
            "Entry.kt",
            "package sample\ndata class Entry(override val key: String) : Keyed\n",
        ),
        (
            "Runner.kt",
            "package sample\nfun main() {\n suspend fun retainedHarness() {}\n val a: Keyed = Entry(\"a\")\n val b: Keyed = Entry(\"b\")\n println(a.compareTo(b).toString() + \"|\" + b.compareTo(a) + \"|\" + a.compareTo(a))\n}\n",
        ),
    ] {
        fs::write(root.join(name), source).unwrap();
    }
    let mut original =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut original);
    original.kotlin_sources = ["Keyed.kt", "Entry.kt", "Runner.kt"]
        .into_iter()
        .map(|name| root.join(name))
        .collect();
    original.classpath.push(stdlib.clone());
    original.run_main_class = Some("sample.RunnerKt".into());
    let before = validate_jvm_sources(&original).expect("original property comparison compiles");
    assert_eq!(before.commands.last().unwrap().stdout.trim(), "-1|1|0");
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        root.join("Keyed.java").is_file(),
        "comparison interface should translate"
    );
    assert!(
        root.join("Entry.java").is_file(),
        "implementation should translate"
    );
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    for entry in fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(path),
            Some("java") => mixed.java_sources.push(path),
            _ => {}
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("sample.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("mixed property comparison compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_proven_decimal_arithmetic_chains() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("proven decimal chains");
    let root = scratch.0.join("sources");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("DecimalAmount.kt"), "package sample\nimport java.math.BigDecimal\nclass DecimalAmount(private val value: BigDecimal) {\n fun compute(other: BigDecimal): String = value.add(other).divide(BigDecimal.TEN).negate().stripTrailingZeros().toPlainString()\n fun compare(other: BigDecimal): Int = this.value.compareTo(other)\n}\n").unwrap();
    fs::write(root.join("Runner.kt"), "package sample\nimport java.math.BigDecimal\nfun main() {\n suspend fun retainedHarness() {}\n val amount = DecimalAmount(BigDecimal(\"5.0\"))\n println(amount.compute(BigDecimal(\"7.5\")) + \"|\" + amount.compare(BigDecimal(\"20\")))\n}\n").unwrap();
    let mut original =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut original);
    original.kotlin_sources = vec![root.join("DecimalAmount.kt"), root.join("Runner.kt")];
    original.classpath.push(stdlib.clone());
    original.run_main_class = Some("sample.RunnerKt".into());
    let before = validate_jvm_sources(&original).expect("original decimal chain compiles");
    assert_eq!(before.commands.last().unwrap().stdout.trim(), "-1.25|-1");
    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        root.join("DecimalAmount.java").is_file(),
        "decimal arithmetic class should translate: {}",
        fs::read_to_string(root.join("DecimalAmount.kt")).unwrap_or_default()
    );
    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    mixed.java_sources.push(root.join("DecimalAmount.java"));
    mixed.kotlin_sources.push(root.join("Runner.kt"));
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("sample.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("mixed decimal chain compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_bounded_generic_getter_chains() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("bounded generic getter chain");
    let root = scratch.0.join("sources");
    fs::create_dir_all(&root).unwrap();
    for (name, source) in [
        (
            "ObjectId.kt",
            r#"package sample
interface ObjectId {
    fun getObjectId(): String
}
"#,
        ),
        (
            "HasId.kt",
            r#"package sample
interface HasId<T : ObjectId> {
    fun getId(): T
    fun describeId(): String = getId().getObjectId()
}
"#,
        ),
        (
            "SpecificId.kt",
            r#"package sample
data class SpecificId(private val value: String) : ObjectId {
    override fun getObjectId(): String = value
}
"#,
        ),
        (
            "RetainedHolder.kt",
            r#"package sample
class RetainedHolder(private val id: SpecificId) : HasId<SpecificId> {
    override fun getId(): SpecificId = id
    suspend fun retainedHook() {}
}
"#,
        ),
        (
            "Runner.kt",
            r#"package sample
fun main() {
    suspend fun retainedHarness() {}
    val value: HasId<SpecificId> = RetainedHolder(SpecificId("generic-id"))
    val genericReturn = HasId::class.java.getDeclaredMethod("getId").genericReturnType.typeName
    println(value.describeId() + "|" + genericReturn)
}
"#,
        ),
    ] {
        fs::write(root.join(name), source).unwrap();
    }

    let mut original =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut original);
    original.kotlin_sources = [
        "ObjectId.kt",
        "HasId.kt",
        "SpecificId.kt",
        "RetainedHolder.kt",
        "Runner.kt",
    ]
    .into_iter()
    .map(|name| root.join(name))
    .collect();
    original.classpath.push(stdlib.clone());
    original.run_main_class = Some("sample.RunnerKt".into());
    let before = validate_jvm_sources(&original).expect("original generic chain compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        "generic-id|T"
    );

    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&root)
        .args(["--annotations", "none", "--in-place"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    let has_id_java = root.join("HasId.java");
    assert!(
        has_id_java.is_file(),
        "generic interface should translate; retained source: {}",
        fs::read_to_string(root.join("HasId.kt")).unwrap_or_default()
    );
    let java_contract = fs::read_to_string(has_id_java).unwrap();
    assert!(
        java_contract.contains("interface HasId<T extends ObjectId>"),
        "{java_contract}"
    );
    assert!(
        java_contract.contains("T getId();"),
        "generic return type was widened:\n{java_contract}"
    );

    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    for entry in fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(path),
            Some("java") => mixed.java_sources.push(path),
            _ => {}
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("sample.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("mixed bounded generic chain compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim()
    );
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_retains_child_over_defaultimpls_dependent_kotlin_parent() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("forwarded child over retained generic method parent");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    let sources = [
        (
            "Contract.kt",
            r#"package boundary
interface Contract<T> {
    fun payload(): T
    fun key(): String
    fun defaultLabel(): String = key() + ":" + payload().toString()

    companion object {
        suspend fun retainedHelper(): String = "kept"
    }
}
"#,
        ),
        (
            "Forwarded.kt",
            r#"package boundary
interface Forwarded<T> : Contract<T> {
    fun label(): String = defaultLabel() + ":child"
}
"#,
        ),
        (
            "Kept.kt",
            r#"package boundary
class Kept<T>(private val content: T, private val keyText: String) : Forwarded<T> {
    override fun payload(): T = content
    override fun key(): String = keyText
    suspend fun retainedHook() {}
}
"#,
        ),
        (
            "Runner.kt",
            r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val child: Forwarded<String> = Kept("payload", "key")
    val parent: Contract<String> = child
    val genericParent = Forwarded::class.java.genericInterfaces.single().typeName
    println(child.label() + "|" + parent.defaultLabel() + "|" + child.payload() + "|" + genericParent)
}
"#,
        ),
    ];
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        for (name, source) in sources {
            fs::write(root.join(name), source).unwrap();
        }
    }

    let modes = ["disable", "all"];
    let mut before_outputs = Vec::new();
    for mode in modes {
        let mut baseline = JvmValidationConfig::new(
            &kotlinc,
            &javac,
            &java,
            scratch.0.join(format!("original classes {mode}")),
        );
        target_java_17(&mut baseline);
        baseline
            .kotlinc_args
            .push(format!("-Xjvm-default={mode}").into());
        baseline.kotlin_sources = ["Contract.kt", "Forwarded.kt", "Kept.kt", "Runner.kt"]
            .into_iter()
            .map(|name| original.join(name))
            .collect();
        baseline.classpath.push(stdlib.clone());
        baseline.run_main_class = Some("boundary.RunnerKt".into());
        let before = validate_jvm_sources(&baseline)
            .unwrap_or_else(|error| panic!("original {mode} hierarchy failed: {error}"));
        let output = before.commands.last().unwrap().stdout.trim().to_owned();
        assert_eq!(
            output, "key:payload:child|key:payload|payload|boundary.Contract<T>",
            "original output in -Xjvm-default={mode}"
        );
        before_outputs.push(output);
    }

    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(&migrated)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("Contract.kt").is_file() && !migrated.join("Contract.java").exists(),
        "the unsupported companion must keep the generic contract in Kotlin"
    );
    assert!(
        migrated.join("Forwarded.kt").is_file() && !migrated.join("Forwarded.java").exists(),
        "a child default depending on a retained Kotlin parent must stay Kotlin"
    );
    assert!(
        migrated.join("Kept.kt").is_file() && !migrated.join("Kept.java").exists(),
        "the suspend implementation must remain Kotlin"
    );
    for (index, mode) in modes.into_iter().enumerate() {
        let mut mixed = JvmValidationConfig::new(
            &kotlinc,
            &javac,
            &java,
            scratch.0.join(format!("mixed classes {mode}")),
        );
        target_java_17(&mut mixed);
        mixed
            .kotlinc_args
            .push(format!("-Xjvm-default={mode}").into());
        for entry in fs::read_dir(&migrated).unwrap() {
            let path = entry.unwrap().path();
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("kt") => mixed.kotlin_sources.push(path),
                Some("java") => mixed.java_sources.push(path),
                _ => {}
            }
        }
        mixed.classpath.push(stdlib.clone());
        mixed.run_main_class = Some("boundary.RunnerKt".into());
        let after = validate_jvm_sources(&mixed)
            .unwrap_or_else(|error| panic!("mixed {mode} hierarchy failed: {error}"));
        assert_eq!(
            before_outputs[index],
            after.commands.last().unwrap().stdout.trim(),
            "mixed output in -Xjvm-default={mode}"
        );
    }
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_translates_child_default_over_abstract_retained_generic_parent() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("child default over abstract generic parent");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    let sources = [
        (
            "PlatformContract.java",
            r#"package boundary;
public interface PlatformContract<T> {
    T payload();
    default String platformLabel() { return "platform"; }
    default boolean matches(T value) { return java.util.Objects.equals(payload(), value); }
    default java.util.List<T> payloads() { return java.util.Collections.singletonList(payload()); }
}
"#,
        ),
        (
            "Contract.kt",
            r#"package boundary
interface Contract<T> : PlatformContract<T> {
    override fun payload(): T
    fun key(): String
    fun defaultLabel(): String

    companion object {
        suspend fun retainedHelper(): String = "kept"
    }
}
"#,
        ),
        (
            "Forwarded.kt",
            r#"package boundary
interface Forwarded<T> : Contract<T> {
    fun label(): String = defaultLabel() + ":child:" + platformLabel()
}
"#,
        ),
        (
            "Kept.kt",
            r#"package boundary
class Kept<T>(private val content: T, private val keyText: String) : Forwarded<T> {
    override fun payload(): T = content
    override fun key(): String = keyText
    override fun defaultLabel(): String = key() + ":" + payload().toString()
    suspend fun retainedHook() {}
}
"#,
        ),
        (
            "Runner.kt",
            r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val child: Forwarded<String> = Kept("payload", "key")
    val parent: Contract<String> = child
    val genericParent = Forwarded::class.java.genericInterfaces.single().typeName
    val contractParent = Contract::class.java.genericInterfaces.single().typeName
    val platformReturn = PlatformContract::class.java.getDeclaredMethod("payload").genericReturnType.typeName
    println(child.label() + "|" + parent.defaultLabel() + "|" + child.payload() + "|" + genericParent + "|" + contractParent + "|" + platformReturn + "|" + parent.matches("payload") + "|" + parent.payloads().single())
}
"#,
        ),
    ];
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        for (name, source) in sources {
            fs::write(root.join(name), source).unwrap();
        }
    }

    let modes = ["disable", "all"];
    let mut before_outputs = Vec::new();
    for mode in modes {
        let mut baseline = JvmValidationConfig::new(
            &kotlinc,
            &javac,
            &java,
            scratch.0.join(format!("abstract original classes {mode}")),
        );
        target_java_17(&mut baseline);
        baseline
            .kotlinc_args
            .push(format!("-Xjvm-default={mode}").into());
        baseline.kotlin_sources = ["Contract.kt", "Forwarded.kt", "Kept.kt", "Runner.kt"]
            .into_iter()
            .map(|name| original.join(name))
            .collect();
        baseline
            .java_sources
            .push(original.join("PlatformContract.java"));
        baseline.classpath.push(stdlib.clone());
        baseline.run_main_class = Some("boundary.RunnerKt".into());
        let before = validate_jvm_sources(&baseline)
            .unwrap_or_else(|error| panic!("original abstract-parent {mode} failed: {error}"));
        let output = before.commands.last().unwrap().stdout.trim().to_owned();
        assert_eq!(
            output,
            "key:payload:child:platform|key:payload|payload|boundary.Contract<T>|boundary.PlatformContract<T>|T|true|payload",
            "original output in -Xjvm-default={mode}"
        );
        before_outputs.push(output);
    }

    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(&migrated)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(
        migrated.join("Contract.kt").is_file() && !migrated.join("Contract.java").exists(),
        "the unsupported companion must retain the abstract generic parent"
    );
    assert!(
        migrated.join("Forwarded.java").is_file(),
        "the self-contained child default should translate: {}",
        String::from_utf8_lossy(&translation.stderr)
    );
    assert!(migrated.join("Kept.kt").is_file());
    let child_contract = fs::read_to_string(migrated.join("Forwarded.java")).unwrap();
    let normalized = child_contract
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        normalized.contains("interface Forwarded<T> extends Contract<T>"),
        "generic parent signature was erased:\n{child_contract}"
    );
    assert!(
        child_contract.contains("default String label()")
            && child_contract.contains("defaultLabel()")
            && child_contract.contains("platformLabel()"),
        "child default should invoke the abstract parent method:\n{child_contract}"
    );

    for (index, mode) in modes.into_iter().enumerate() {
        let mut mixed = JvmValidationConfig::new(
            &kotlinc,
            &javac,
            &java,
            scratch.0.join(format!("abstract mixed classes {mode}")),
        );
        target_java_17(&mut mixed);
        mixed
            .kotlinc_args
            .push(format!("-Xjvm-default={mode}").into());
        for entry in fs::read_dir(&migrated).unwrap() {
            let path = entry.unwrap().path();
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("kt") => mixed.kotlin_sources.push(path),
                Some("java") => mixed.java_sources.push(path),
                _ => {}
            }
        }
        mixed.classpath.push(stdlib.clone());
        mixed.run_main_class = Some("boundary.RunnerKt".into());
        let after = validate_jvm_sources(&mixed)
            .unwrap_or_else(|error| panic!("mixed abstract-parent {mode} failed: {error}"));
        assert_eq!(
            before_outputs[index],
            after.commands.last().unwrap().stdout.trim(),
            "mixed output in -Xjvm-default={mode}"
        );
    }
}

#[test]
#[ignore = "requires JVM tool paths and Kotlin stdlib in NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB"]
fn real_toolchain_preserves_named_generic_bound_member_chain() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let javac = std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC");
    let java = std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA");
    let stdlib =
        PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").expect("NOTLIN_KOTLIN_STDLIB"));
    let scratch = Scratch::new("named generic bound member chain");
    let original = scratch.0.join("original");
    let migrated = scratch.0.join("migrated");
    let sources = [
        (
            "Identifier.kt",
            r#"package boundary
interface Identifier {
    fun label(): String
}

"#,
        ),
        (
            "Entity.kt",
            r#"package boundary
interface Entity<I : Identifier> {
    fun getId(): I
    fun describe(): String = getId().label()
}
"#,
        ),
        (
            "Holder.kt",
            r#"package boundary
interface Holder<I : Identifier, T : Entity<I>> {
    fun getValue(): T
    fun summary(): String = getValue().describe()
}
"#,
        ),
        (
            "Concrete.kt",
            r#"package boundary
class SpecificIdentifier(private val text: String) : Identifier {
    override fun label(): String = text
}
class SpecificEntity<I : Identifier>(private val id: I) : Entity<I> {
    override fun getId(): I = id
}
"#,
        ),
        (
            "KeptHolder.kt",
            r#"package boundary
class KeptHolder<I : Identifier, T : Entity<I>>(private val value: T) : Holder<I, T> {
    override fun getValue(): T = value
    suspend fun retainedHook() {}
}
"#,
        ),
        (
            "Runner.kt",
            r#"package boundary
fun main() {
    suspend fun retainedHarness() {}
    val identifier = SpecificIdentifier("specific-id")
    val entity = SpecificEntity(identifier)
    val holder: Holder<SpecificIdentifier, SpecificEntity<SpecificIdentifier>> = KeptHolder(entity)
    val value = holder.getValue()
    val holderReturn = Holder::class.java.getDeclaredMethod("getValue").genericReturnType.typeName
    val entityReturn = Entity::class.java.getDeclaredMethod("getId").genericReturnType.typeName
    println(holder.summary() + "|" + value.describe() + "|" + holderReturn + "|" + entityReturn)
}
"#,
        ),
    ];
    for root in [&original, &migrated] {
        fs::create_dir_all(root).unwrap();
        for (name, source) in sources {
            fs::write(root.join(name), source).unwrap();
        }
    }

    let mut baseline =
        JvmValidationConfig::new(&kotlinc, &javac, &java, scratch.0.join("original classes"));
    target_java_17(&mut baseline);
    baseline.kotlin_sources = [
        "Identifier.kt",
        "Entity.kt",
        "Holder.kt",
        "Concrete.kt",
        "KeptHolder.kt",
        "Runner.kt",
    ]
    .into_iter()
    .map(|name| original.join(name))
    .collect();
    baseline.classpath.push(stdlib.clone());
    baseline.run_main_class = Some("boundary.RunnerKt".into());
    let before = validate_jvm_sources(&baseline).expect("original named generic bound compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        "specific-id|specific-id|T|I"
    );

    let translation = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&migrated)
        .args(["--annotations", "none", "--in-place"])
        .arg(&migrated)
        .output()
        .unwrap();
    assert!(
        translation.status.success(),
        "{}",
        String::from_utf8_lossy(&translation.stderr)
    );
    for name in ["Entity", "Holder", "SpecificIdentifier", "SpecificEntity"] {
        assert!(
            migrated.join(format!("{name}.java")).is_file(),
            "{name} should translate; stderr:\n{}",
            String::from_utf8_lossy(&translation.stderr)
        );
    }
    assert!(
        migrated.join("KeptHolder.kt").is_file() && !migrated.join("KeptHolder.java").exists(),
        "the suspend concrete implementation should remain Kotlin"
    );
    let holder_java = fs::read_to_string(migrated.join("Holder.java")).unwrap();
    assert!(
        holder_java.contains("T extends Entity<I>") && holder_java.contains("T getValue();"),
        "the declared generic bound/return must remain structured:\n{holder_java}"
    );
    assert!(
        holder_java.contains("getValue().describe()"),
        "the emitted default method must use the bounded member chain:\n{holder_java}"
    );

    let mut mixed = JvmValidationConfig::new(kotlinc, javac, java, scratch.0.join("mixed classes"));
    target_java_17(&mut mixed);
    for entry in fs::read_dir(&migrated).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => mixed.kotlin_sources.push(path),
            Some("java") => mixed.java_sources.push(path),
            _ => {}
        }
    }
    mixed.classpath.push(stdlib);
    mixed.run_main_class = Some("boundary.RunnerKt".into());
    let after = validate_jvm_sources(&mixed).expect("mixed named generic bound compiles");
    assert_eq!(
        before.commands.last().unwrap().stdout.trim(),
        after.commands.last().unwrap().stdout.trim()
    );
}
