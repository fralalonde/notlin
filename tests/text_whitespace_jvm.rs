//! Compare Kotlin whitespace, nullable queries and receiver effects on the JVM.
use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SOURCES: &[(&str, &str)] = &[
    (
        "Reader.kt",
        r#"package sample
import kotlin.text.trim
import kotlin.text.trim as hashCode
import kotlin.text.trimStart
import kotlin.text.trimStart as leftClean
import domain.*
class Reader {
 fun trim(value: String): String = value.trim()
 fun inferred(value: String) = value.trim()
 fun inferredBlank(value: String?) = value?.isBlank()
 fun start(value: String): String = value.trimStart()
 fun end(value: String): String = value.trimEnd()
 fun alias(value: String): String = value.leftClean()
 fun sequence(value: CharSequence): CharSequence = value.trim()
 fun sequenceStart(value: CharSequence): CharSequence = value.trimStart()
 fun sequenceEnd(value: CharSequence): CharSequence = value.trimEnd()
 fun blank(value: CharSequence): Boolean = value.isBlank()
 fun notBlank(value: String): Boolean = value.isNotBlank()
 fun nullBlank(value: CharSequence?): Boolean = value.isNullOrBlank()
 fun nullEmpty(value: CharSequence?): Boolean = value.isNullOrEmpty()
 fun empty(value: String?): String = value.orEmpty()
 fun safe(value: String?): String? = value?.trim()
 fun chain(value: String?): Boolean = value.orEmpty().trim().isBlank()
 fun parenthesized(value: String): String = (value).trim()
 fun memberHash(value: String): Int = value.hashCode()
 fun effect(producer: Producer): String = producer.next().trim()
 fun nullableEffect(producer: Producer): Boolean = producer.nullable().isNullOrBlank()
 fun safeEffect(producer: Producer): String? = producer.nullable()?.trim()
}
"#,
    ),
    (
        "Holder.kt",
        r#"package sample
class Holder(private val text: String?) {
 fun blank(): Boolean = text.isNullOrBlank()
 fun empty(): String = this.text.orEmpty()
}
"#,
    ),
    (
        "Producer.java",
        r#"package sample;
public final class Producer {
 private int calls;
 @org.jetbrains.annotations.NotNull public String next() { calls++; return "\u00a0ok\u2007"; }
 @org.jetbrains.annotations.Nullable public String nullable() { calls++; return null; }
 public int calls() { return calls; }
}
"#,
    ),
    (
        "Word.kt",
        "package sample\nclass Word { fun trim(): String = \"own\" }\n",
    ),
    ("Other.java", "package domain; public interface Other {}"),
    (
        "Payload.java",
        "package foreign; public enum Payload { LEFT, RIGHT }",
    ),
    ("Decoy.java", "package sample; final class Payload {}"),
    (
        "Left.java",
        r#"package foreign;
public interface Left {
 default Payload select() { Effects.calls++; return Payload.LEFT; }
}
"#,
    ),
    (
        "Right.java",
        r#"package foreign;
public interface Right {
 default Payload select() { Effects.calls++; return Payload.RIGHT; }
}
"#,
    ),
    (
        "Effects.java",
        "package foreign; public final class Effects { public static int calls; }",
    ),
    (
        "Values.java",
        r#"package foreign;
public interface Values {
 default int number() { return 7; }
 @org.jetbrains.annotations.Nullable default Integer boxed() { return null; }
 @org.jetbrains.annotations.NotNull default Integer certain() { return 9; }
}
"#,
    ),
    (
        "Choices.kt",
        r#"package sample
import foreign.Left
import foreign.Right
import foreign.Values
enum class Choice: Left, Right {
 ONE;
 override fun select() = super<foreign.Left>.select()
}
class ChoiceClass: Left, Right {
 override fun select() = super<Right>.select()
}
class Inferred {
 fun number(value: Values) = value.number()
 fun boxed(value: Values) = value.boxed()
 fun certain(value: Values) = value.certain()
 fun safeNumber(value: Values?) = value?.number()
}
"#,
    ),
    (
        "Fields.kt",
        r#"package sample
interface Identified { fun getId(): String }
data class FieldQuery(@JvmField val code: String): Identified {
 @JvmField val id: String = "stored"
 override fun getId(): String = id
 fun direct(): String = this.id
 fun text(): String = "$id/$code"
 fun shadow(id: String): String = id
}
class FieldAfter: Identified {
 override fun getId(): String = id
 fun direct(): String = this.id
 @JvmField val id: String = "after"
}
"#,
    ),
    (
        "Records.kt",
        r#"package sample
data class Label(val value: String)
interface Named { val name: Label }
data class Entry(override val name: Label): Named, Comparable<Entry> {
 fun normalized(): String = name.value.trim()
 override fun compareTo(other: Entry): Int = name.value.compareTo(other.name.value)
}
"#,
    ),
    (
        "Traced.java",
        r#"package sample;
public final class Traced implements CharSequence {
 public static String events = "";
 private final String text;
 public Traced(String text) { this.text = text; }
 public int length() { events += "L;"; return text.length(); }
 public char charAt(int index) { events += "C" + index + ";"; return text.charAt(index); }
 public CharSequence subSequence(int from, int to) { events += "S" + from + "," + to + ";"; return new Traced(text.substring(from, to)); }
 public String toString() { events += "T;"; return text; }
}
"#,
    ),
    (
        "JavaCalls.java",
        r#"package sample;
public final class JavaCalls {
 public static String trim(String value) { return value.trim(); }
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
private fun codes(value: String?): String = value?.toCharArray()?.joinToString(",") { it.code.toString(16) } ?: "NULL"
fun main() {
 suspend fun keepRunner() {}
 val reader = Reader()
 println("MEMBER|${reader.memberHash("abc")}")
 val query = FieldQuery("code")
 println("FIELDS|${query.id}|${query.getId()}|${query.direct()}|${query.text()}|${query.shadow("local")}|${FieldAfter().getId()}|${FieldAfter().direct()}")
 println("OWN|${Word().trim()}")
 println("INFERRED|${codes(reader.inferred("\u00a0ok\u2007"))}|${reader.inferredBlank(null)}|${reader.inferredBlank("\u00a0")}")
 val selected: foreign.Payload = Choice.ONE.select()
 println("SUPER|${selected.name}|${ChoiceClass().select().name}|${foreign.Effects.calls}")
 val values = object: foreign.Values {}
 val inferred = Inferred()
 println("JAVA-RETURNS|${inferred.number(values)}|${inferred.boxed(values)}|${inferred.certain(values)}|${inferred.safeNumber(null)}|${inferred.safeNumber(values)}")
 println("RECORD|${codes(Entry(Label("\u00a0ok\u2007")).normalized())}")
 for (text in listOf("", " \t\n", "\u00a0ok\u2007", "\u0000ok\u0000", "\u202f\u2003", "\u200b")) {
  println("S|${codes(reader.trim(text))}|${codes(reader.start(text))}|${codes(reader.end(text))}|${codes(reader.alias(text))}|${codes(reader.parenthesized(text))}|${reader.blank(text)}|${reader.notBlank(text)}")
 }
 for (text in listOf<String?>(null, "", "\u00a0", "x")) {
  println("N|${reader.nullBlank(text)}|${reader.nullEmpty(text)}|${codes(reader.empty(text))}|${codes(reader.safe(text))}|${reader.chain(text)}")
  println("H|${Holder(text).blank()}|${codes(Holder(text).empty())}")
 }
 val producer = Producer()
 println("P|${codes(reader.effect(producer))}|${reader.nullableEffect(producer)}|${codes(reader.safeEffect(producer))}|${producer.calls()}")
 for (mode in 0..2) {
  Traced.events = ""
  val input = Traced("\u00a0ok\u2007")
  val output = when (mode) { 0 -> reader.sequence(input); 1 -> reader.sequenceStart(input); else -> reader.sequenceEnd(input) }
  println("C|${output.javaClass.simpleName}|${codes(output.toString())}|${Traced.events}")
 }
 println("JAVA|${codes(JavaCalls.trim("\u00a0ok\u2007"))}")
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

fn config(root: &Path, output: PathBuf, lombok: bool) -> JvmValidationConfig {
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"),
        std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"),
        std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA"),
        output,
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "21".into()]);
    config.javac_args.extend(["--release".into(), "21".into()]);
    config.classpath.push(
        std::env::var_os("NOTLIN_KOTLIN_STDLIB")
            .expect("NOTLIN_KOTLIN_STDLIB")
            .into(),
    );
    config
        .classpath
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"));
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
    for entry in fs::read_dir(root).unwrap() {
        let file = entry.unwrap().path();
        match file.extension().and_then(|ext| ext.to_str()) {
            Some("kt") => config.kotlin_sources.push(file),
            Some("java") => config.java_sources.push(file),
            _ => {}
        }
    }
    config.run_main_class = Some("sample.RunnerKt".into());
    config
}

fn scratch() -> Scratch {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Scratch(std::env::temp_dir().join(format!(
        "notlin-text-whitespace-{}-{nonce}",
        std::process::id()
    )))
}

fn translate(root: &Path, lombok: bool) {
    let binary = std::env::var_os("NOTLIN_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into());
    let before = fs::read(root.join("JavaCalls.java")).unwrap();
    let mut command = Command::new(binary);
    command.current_dir(root).arg("--in-place");
    if lombok {
        command.arg("--lombok");
    }
    let output = command.arg(".").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for name in [
        "Reader",
        "Holder",
        "Word",
        "Label",
        "Named",
        "Entry",
        "Choice",
        "ChoiceClass",
        "Inferred",
        "Identified",
        "FieldQuery",
        "FieldAfter",
    ] {
        assert!(
            root.join(format!("{name}.java")).exists(),
            "{name} must translate: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(root.join(format!("{name}.kt"))).unwrap_or_default()
        );
    }
    assert!(root.join("Runner.kt").exists());
    assert_eq!(before, fs::read(root.join("JavaCalls.java")).unwrap());
}

#[test]
fn text_operations_translate_as_one_family_with_nullable_and_getter_receivers() {
    let scratch = scratch();
    populate(&scratch.0);
    translate(&scratch.0, false);
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok and its Kotlin plugin"]
fn unicode_whitespace_views_nulls_and_receiver_evaluation_match_original_kotlin() {
    let scratch = scratch();
    let original = scratch.0.join("original");
    populate(&original);
    let before = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        false,
    ))
    .expect("original Kotlin compiles and runs");
    let expected = before
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .replace("\r\n", "\n");
    assert!(expected.contains("P|6f,6b|true|NULL|3"), "{expected}");
    assert!(expected.contains("SUPER|LEFT|RIGHT|2"), "{expected}");
    assert!(
        expected.contains("FIELDS|stored|stored|stored|stored/code|local|after|after"),
        "{expected}"
    );
    assert!(
        expected.contains("JAVA-RETURNS|7|null|9|null|7"),
        "{expected}"
    );
    assert!(expected.contains("INFERRED|6f,6b|null|true"), "{expected}");
    assert!(
        expected.contains("JAVA|a0,6f,6b,2007"),
        "Java trim must retain its JDK semantics: {expected}"
    );
    assert!(
        expected.contains("C|Traced|6f,6b|"),
        "CharSequence trim must keep the view: {expected}"
    );
    for lombok in [false, true] {
        let root = scratch.0.join(format!("candidate-{lombok}"));
        populate(&root);
        translate(&root, lombok);
        let after = validate_jvm_sources(&config(
            &root,
            scratch.0.join(format!("mixed-{lombok}")),
            lombok,
        ))
        .expect("converted text family compiles and runs");
        assert_eq!(
            after
                .commands
                .last()
                .unwrap()
                .stdout
                .trim()
                .replace("\r\n", "\n"),
            expected,
            "Lombok={lombok}"
        );
    }
}
