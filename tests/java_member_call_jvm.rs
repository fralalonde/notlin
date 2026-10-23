//! Real-toolchain differential for the declaration-wide lowering contract.
//!
//! The fixture is the validated representation oracle (`Receipt.kt`):
//!
//! * `link(values: Stream<Int>)` discards `values.sorted()`;
//! * `opaque(values: Stream<Int>): Any?` returns it through a nullable
//!   `java.lang.Object` ABI.
//!
//! Both are *invocation* observations, not type guesses: a `java.lang.reflect`
//! proxy implements `Stream`, counts `sorted` invocations and returns `null`.
//! The original Kotlin program must print `discard=1` / `opaque=2:true` — one
//! invocation each, no Kotlin null-check on the unused or nullable result, no
//! member other than `sorted` touched. The migrated Java must print exactly the
//! same bytes: same call count, same null propagation, same result
//! observability. That is what a wrong `Unit`/discard/lossy representation
//! would break.
//!
//! Only `sorted` is in scope. `Stream.close()` is *not* a case this slice
//! accepts: K2 resolves it to its declaring interface
//! `java.util.stream.BaseStream`, not to `java.util.stream.Stream`, and the
//! exact declared-classifier gate rejects that mismatch. The module pins that
//! boundary instead of widening the owner match.
//!
//! Ignored by default: needs a JDK, kotlinc, the Kotlin stdlib and both helper
//! jars built from this worktree's `jvm-facts/`.
//!
//! ```sh
//! NOTLIN_KOTLINC=… NOTLIN_JAVAC=… NOTLIN_JAVA=… NOTLIN_KOTLIN_STDLIB=… \
//! NOTLIN_JVM_FACTS_JAR=…/notlin-jvm-facts.jar \
//! NOTLIN_JVM_FACTS_FULL_JAR=…/notlin-jvm-facts-full.jar \
//! cargo test --locked --test java_member_call_jvm -- --ignored --nocapture
//! ```

use notlin::facts::runtime::FactsRuntime;
use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "notlin-java-member-jvm-{}-{id}",
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

const SOURCE: &str = r#"package probe

import java.util.stream.Stream

fun link(values: Stream<Int>) {
    values.sorted()
}

fun opaque(values: Stream<Int>): Any? = values.sorted()
"#;

/// Invocation oracle: count `sorted` calls and observe the returned value.
///
/// `facade` is the emitted class holding the two functions — `ReceiptKt` for
/// the retained Kotlin, `Receipt` for the migrated Java. Everything else is
/// identical, so the printed bytes are a direct differential.
fn oracle_main(class_name: &str, facade: &str) -> String {
    format!(
        r#"package probe;

import java.util.concurrent.atomic.AtomicInteger;
import java.util.stream.Stream;

public final class {class_name} {{
    public static void main(String[] args) throws Exception {{
        AtomicInteger calls = new AtomicInteger();
        Object stream = java.lang.reflect.Proxy.newProxyInstance(
            Stream.class.getClassLoader(), new Class<?>[] {{Stream.class}},
            (proxy, method, values) -> {{
                if (method.getName().equals("sorted")) {{
                    calls.incrementAndGet();
                    return null;
                }}
                throw new AssertionError(method.getName());
            }});
        Class<?> facade = Class.forName("{facade}");
        facade.getMethod("link", Stream.class).invoke(null, stream);
        System.out.println("discard=" + calls.get());
        Object value = facade.getMethod("opaque", Stream.class).invoke(null, stream);
        System.out.println("opaque=" + calls.get() + ":" + (value == null));
    }}
}}
"#
    )
}

/// Harness for the import-form fixtures: a `void` proved call.
const VOID_REFERENCE_MAIN: &str = r#"package probe;

import java.util.stream.Stream;

public final class ReferenceMain {
    public static void main(String[] args) {
        StreamMemberKt.sorted(Stream.of(3, 1, 2));
        System.out.println("ok");
    }
}
"#;

const VOID_MIGRATED_MAIN: &str = r#"package probe;

import java.util.stream.Stream;

public final class MigratedMain {
    public static void main(String[] args) {
        StreamMember.sorted(Stream.of(3, 1, 2));
        System.out.println("ok");
    }
}
"#;

fn java_of(name: &str) -> std::ffi::OsString {
    std::env::var_os(name).unwrap_or_else(|| panic!("{name} must be set"))
}

fn config(
    root: &Path,
    output: PathBuf,
    stdlib: &Path,
    run_main: Option<&str>,
) -> JvmValidationConfig {
    let mut config = JvmValidationConfig::new(
        java_of("NOTLIN_KOTLINC"),
        java_of("NOTLIN_JAVAC"),
        java_of("NOTLIN_JAVA"),
        output,
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    config
        .javac_args
        .extend(["--release".into(), "17".into(), "-proc:none".into()]);
    config.classpath.push(stdlib.to_path_buf());
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => config.kotlin_sources.push(path),
            Some("java") => config.java_sources.push(path),
            _ => {}
        }
    }
    config.run_main_class = run_main.map(str::to_owned);
    config
}

fn migrate(root: &Path, jar: Option<&Path>, allow_approximations: bool) -> (String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .arg("--root")
        .arg(root)
        .args(["--annotations", "none", "--in-place", "--jvm-facts"])
        .arg(if jar.is_some() { "on" } else { "off" })
        .arg(root)
        .current_dir(root)
        .env_remove("NOTLIN_CLASSPATH")
        .env_remove("NOTLIN_JVM_FACTS_KOTLIN");
    if allow_approximations {
        command.arg("--allow-approximations");
    }
    match jar {
        Some(jar) => {
            command
                .env("NOTLIN_JVM_FACTS_JAR", jar)
                .env("NOTLIN_KOTLIN_SOURCES", root);
        }
        None => {
            command.env_remove("NOTLIN_JVM_FACTS_JAR");
            command.env_remove("NOTLIN_KOTLIN_SOURCES");
        }
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "notlin failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the slim/full helper jars"]
fn full_helper_emits_the_discarded_and_nullable_object_exits() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let slim = PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_JAR").expect("slim helper jar"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();

    // 1. Original Kotlin: the invocation oracle is the runtime reference.
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("Receipt.kt"), SOURCE).unwrap();
    fs::write(
        original.join("ReferenceMain.java"),
        oracle_main("ReferenceMain", "probe.ReceiptKt"),
    )
    .unwrap();
    let reference = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        Some("probe.ReferenceMain"),
    ))
    .expect("the original Kotlin must compile and run");
    let expected = reference.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected, "discard=1\nopaque=2:true",
        "the oracle fixture itself changed"
    );

    // 2. Helper off and the slim helper retain both declarations.
    for (mode, jar) in [("off", None), ("slim", Some(slim.as_path()))] {
        let tree = scratch.0.join(format!("migrated-{mode}"));
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("Receipt.kt"), SOURCE).unwrap();
        fs::write(
            tree.join("ReferenceMain.java"),
            oracle_main("ReferenceMain", "probe.ReceiptKt"),
        )
        .unwrap();
        let _ = migrate(&tree, jar, false);
        assert!(
            !tree.join("Receipt.java").exists(),
            "{mode}: no Track B capability must keep the declaration in Kotlin"
        );
        let report = validate_jvm_sources(&config(
            &tree,
            scratch.0.join(format!("migrated-{mode}-classes")),
            &stdlib,
            Some("probe.ReferenceMain"),
        ))
        .unwrap_or_else(|error| panic!("{mode}: retained Kotlin must compile and run: {error}"));
        assert_eq!(
            report.commands.last().unwrap().stdout.trim(),
            expected,
            "{mode}"
        );
    }

    // 3. The full helper signs both call sites and emits them verbatim.
    let tree = scratch.0.join("migrated-full");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("Receipt.kt"), SOURCE).unwrap();
    let (stdout, stderr) = migrate(&tree, Some(full.as_path()), false);
    let java = fs::read_to_string(tree.join("Receipt.java")).unwrap_or_else(|error| {
        panic!("the proved member calls must translate: {error}\n{stdout}\n{stderr}")
    });
    assert!(
        java.contains("values.sorted();"),
        "expected the direct sorted() call:\n{java}"
    );
    assert!(
        !java.contains(".stream().sorted()") && !java.contains("Collectors.toList"),
        "the Kotlin-collection rewrite must not run:\n{java}"
    );
    assert!(
        !java.contains("requireNonNull") && !java.contains("checkNotNull"),
        "neither admitted exit observes the value, so no null check may appear:\n{java}"
    );
    fs::write(
        tree.join("MigratedMain.java"),
        oracle_main("MigratedMain", "probe.Receipt"),
    )
    .unwrap();
    let report = validate_jvm_sources(&config(
        &tree,
        scratch.0.join("migrated-full-classes"),
        &stdlib,
        Some("probe.MigratedMain"),
    ))
    .unwrap_or_else(|error| panic!("full: migrated Java must compile and run: {error}"));
    assert_eq!(
        report.commands.last().unwrap().stdout.trim(),
        expected,
        "the migrated program must print the same invocation/null/result observation"
    );
}

/// Import-form differential: the emitted Java type is the *source* spelling, so
/// only forms whose spelling is the Java simple name can be accepted.
///
/// Unaliased headers (space-separated, tab-separated and carrying a block
/// comment) are proven and translated, and the generated Java must compile and
/// print the original value. The aliased spelling is legal Kotlin too, but the
/// emitter writes `S<Integer>` and drops the aliased import, so it must be
/// retained rather than translated.
#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_proves_unaliased_import_spellings_and_retains_aliases() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();

    for (label, header) in [
        ("plain", "import java.util.stream.Stream"),
        ("tab", "import\tjava.util.stream.Stream"),
        ("comment", "import java.util.stream.Stream /* keep */"),
    ] {
        let source = format!(
            "package probe\n\n{header}\n\nfun sorted(values: Stream<Int>) {{\n    values.sorted()\n}}\n"
        );
        // The fixture must be legal Kotlin on its own; the facade harness is
        // the runtime reference for the retained and migrated trees alike.
        let original = scratch.0.join(format!("original-{label}"));
        fs::create_dir_all(&original).unwrap();
        fs::write(original.join("StreamMember.kt"), &source).unwrap();
        fs::write(original.join("ReferenceMain.java"), VOID_REFERENCE_MAIN).unwrap();
        let reference = validate_jvm_sources(&config(
            &original,
            scratch.0.join(format!("original-{label}-classes")),
            &stdlib,
            Some("probe.ReferenceMain"),
        ))
        .unwrap_or_else(|error| panic!("{label}: the original Kotlin must compile: {error}"));
        let expected = reference.commands.last().unwrap().stdout.trim().to_owned();
        assert_eq!(expected, "ok", "{label}");

        let tree = scratch.0.join(format!("migrated-{label}"));
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("StreamMember.kt"), &source).unwrap();
        let _ = migrate(&tree, Some(full.as_path()), false);
        let java = fs::read_to_string(tree.join("StreamMember.java")).unwrap_or_else(|error| {
            panic!("{label}: the proved member call must translate: {error}")
        });
        assert!(
            java.contains("values.sorted();"),
            "{label}: expected the direct member call:\n{java}"
        );
        fs::write(tree.join("MigratedMain.java"), VOID_MIGRATED_MAIN).unwrap();
        let report = validate_jvm_sources(&config(
            &tree,
            scratch.0.join(format!("migrated-{label}-classes")),
            &stdlib,
            Some("probe.MigratedMain"),
        ))
        .unwrap_or_else(|error| panic!("{label}: migrated Java must compile and run: {error}"));
        assert_eq!(
            report.commands.last().unwrap().stdout.trim(),
            expected,
            "{label}: the migrated program must print the original value"
        );
    }

    // Aliased spelling: legal Kotlin, but no provable Java form.
    let source = "package probe\n\nimport java.util.stream.Stream as S\n\nfun sorted(values: S<Int>) {\n    values.sorted()\n}\n";
    let original = scratch.0.join("original-alias");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("StreamMember.kt"), source).unwrap();
    fs::write(original.join("ReferenceMain.java"), VOID_REFERENCE_MAIN).unwrap();
    let reference = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-alias-classes"),
        &stdlib,
        Some("probe.ReferenceMain"),
    ))
    .expect("the aliased original Kotlin must compile");
    let expected = reference.commands.last().unwrap().stdout.trim().to_owned();

    let tree = scratch.0.join("migrated-alias");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("StreamMember.kt"), source).unwrap();
    fs::write(tree.join("ReferenceMain.java"), VOID_REFERENCE_MAIN).unwrap();
    let _ = migrate(&tree, Some(full.as_path()), false);
    assert!(
        !tree.join("StreamMember.java").exists(),
        "an aliased receiver spelling has no proved Java form, so it must stay Kotlin"
    );
    let report = validate_jvm_sources(&config(
        &tree,
        scratch.0.join("migrated-alias-classes"),
        &stdlib,
        Some("probe.ReferenceMain"),
    ))
    .expect("the retained alias fixture must still compile and run");
    assert_eq!(report.commands.last().unwrap().stdout.trim(), expected);
}

/// The physical evidence the new proof consumes: checked throws are extracted
/// for methods and constructors, and unchecked-only declarations stay `[]`
/// (never a fabricated checked name, never `null` for a loaded owner).
#[test]
#[ignore = "requires NOTLIN_JVM_FACTS_JAR (slim helper built from this worktree)"]
fn helper_extracts_checked_throws_and_filters_unchecked_ones() {
    let jar = PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_JAR").expect("slim helper jar"));
    let runtime = FactsRuntime::start(&jar).expect("helper must start");

    let stream = runtime
        .member_facts("java.util.stream.Stream")
        .expect("Stream is loadable");
    let sorted = stream
        .iter()
        .find(|member| member.name == "sorted" && member.parameter_types.is_empty())
        .expect("zero-parameter Stream.sorted");
    assert_eq!(sorted.checked_exceptions, Some(vec![]));

    // A genuine checked throws clause.
    let input = runtime
        .member_facts("java.io.InputStream")
        .expect("InputStream is loadable");
    let read = input
        .iter()
        .find(|member| member.name == "read" && member.parameter_types.is_empty())
        .expect("zero-parameter InputStream.read");
    assert_eq!(
        read.checked_exceptions,
        Some(vec!["java.io.IOException".to_string()])
    );

    // `Optional.get` declares NoSuchElementException, which is unchecked: it
    // must not appear in the checked list.
    let optional = runtime
        .member_facts("java.util.Optional")
        .expect("Optional is loadable");
    let get = optional
        .iter()
        .find(|member| member.name == "get" && member.parameter_types.is_empty())
        .expect("zero-parameter Optional.get");
    assert_eq!(get.checked_exceptions, Some(vec![]));

    // Constructors carry the same evidence.
    let stream_input = runtime
        .member_facts("java.io.FileInputStream")
        .expect("FileInputStream is loadable");
    let constructor = stream_input
        .iter()
        .find(|member| {
            member.name == "<init>" && member.parameter_types == ["java.lang.String".to_string()]
        })
        .expect("FileInputStream(String)");
    assert_eq!(
        constructor.checked_exceptions,
        Some(vec!["java.io.FileNotFoundException".to_string()])
    );
}

/// Whole-declaration checked exceptions, measured with real tools.
///
/// The original Kotlin is valid (Kotlin has no checked exceptions) and runs.
/// The declaration also contains a call the 4.4 member proof establishes
/// (`Stream.sorted()`), so the declaration becomes eligible — but
/// `InputStream.read()` physically declares `java.io.IOException`, and the
/// emitted Java method has no `throws`/`catch` model. `javac` rejects the
/// earlier candidate output for exactly that reason. The only sound answer is
/// to keep the whole declaration in Kotlin, with or without
/// `--allow-approximations`.
const MIXED_SOURCE: &str = r#"package probe

import java.io.InputStream
import java.util.stream.Stream

fun mixed(input: InputStream, values: Stream<Int>) {
    input.read()
    values.sorted()
}
"#;

const MIXED_REFERENCE_MAIN: &str = r#"package probe;

import java.io.ByteArrayInputStream;
import java.io.InputStream;
import java.util.stream.Stream;

public final class ReferenceMain {
    public static void main(String[] args) {
        InputStream input = new ByteArrayInputStream(new byte[] {'x'});
        MixedKt.mixed(input, Stream.of(3, 1, 2));
        System.out.println("ok");
    }
}
"#;

#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_retains_a_mixed_declaration_with_a_checked_throwing_call() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();

    // 1. The original Kotlin is the runtime reference.
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("Mixed.kt"), MIXED_SOURCE).unwrap();
    fs::write(original.join("ReferenceMain.java"), MIXED_REFERENCE_MAIN).unwrap();
    let reference = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        Some("probe.ReferenceMain"),
    ))
    .expect("the original Kotlin must compile and run");
    let expected = reference.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(expected, "ok");

    // 2. Full helper, with and without `--allow-approximations`: the
    //    declaration stays Kotlin and says why.
    for (label, allow) in [("strict", false), ("approximations", true)] {
        let tree = scratch.0.join(format!("migrated-{label}"));
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("Mixed.kt"), MIXED_SOURCE).unwrap();
        fs::write(tree.join("ReferenceMain.java"), MIXED_REFERENCE_MAIN).unwrap();
        migrate(&tree, Some(full.as_path()), allow);
        assert!(
            !tree.join("Mixed.java").exists(),
            "{label}: a checked-throwing call must not be emitted as Java"
        );
        let retained = fs::read_to_string(tree.join("Mixed.kt")).expect("retained Kotlin");
        assert!(
            retained.contains("java.io.IOException"),
            "{label}: the retention reason must name the checked exception:\n{retained}"
        );
        let report = validate_jvm_sources(&config(
            &tree,
            scratch.0.join(format!("migrated-{label}-classes")),
            &stdlib,
            Some("probe.ReferenceMain"),
        ))
        .unwrap_or_else(|error| panic!("{label}: retained Kotlin must compile and run: {error}"));
        assert_eq!(
            report.commands.last().unwrap().stdout.trim(),
            expected,
            "{label}: the retained program must print the original value"
        );
    }
}

/// One file, two declarations: the sibling whose calls are proved translates,
/// the checked-throwing sibling stays Kotlin, and the mixed tree still compiles
/// and runs. This is the per-declaration (not per-file) retention contract with
/// the real helper, which re-analyses the trimmed revision between passes.
const SIBLINGS_SOURCE: &str = r#"package probe

import java.io.InputStream
import java.util.stream.Stream

fun sortedOnly(values: Stream<Int>) {
    values.sorted()
}

fun readIt(input: InputStream): Int = input.read()
"#;

const SIBLINGS_REFERENCE_MAIN: &str = r#"package probe;

import java.io.ByteArrayInputStream;
import java.util.stream.Stream;

public final class ReferenceMain {
    public static void main(String[] args) {
        SiblingsKt.sortedOnly(Stream.of(3, 1, 2));
        int read = SiblingsKt.readIt(new ByteArrayInputStream(new byte[] {'x'}));
        System.out.println("ok:" + read);
    }
}
"#;

const SIBLINGS_MIGRATED_MAIN: &str = r#"package probe;

import java.io.ByteArrayInputStream;
import java.util.stream.Stream;

public final class MigratedMain {
    public static void main(String[] args) {
        Siblings.sortedOnly(Stream.of(3, 1, 2));
        int read = SiblingsKt.readIt(new ByteArrayInputStream(new byte[] {'x'}));
        System.out.println("ok:" + read);
    }
}
"#;

#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_retains_only_the_checked_throwing_declaration_of_a_file() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();

    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("Siblings.kt"), SIBLINGS_SOURCE).unwrap();
    fs::write(original.join("ReferenceMain.java"), SIBLINGS_REFERENCE_MAIN).unwrap();
    let reference = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        Some("probe.ReferenceMain"),
    ))
    .expect("the original Kotlin must compile and run");
    let expected = reference.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(expected, "ok:120");

    let tree = scratch.0.join("migrated");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("Siblings.kt"), SIBLINGS_SOURCE).unwrap();
    fs::write(tree.join("MigratedMain.java"), SIBLINGS_MIGRATED_MAIN).unwrap();
    let _ = migrate(&tree, Some(full.as_path()), false);

    let java =
        fs::read_to_string(tree.join("Siblings.java")).expect("the proved sibling must translate");
    assert!(java.contains("sortedOnly("), "{java}");
    assert!(
        !java.contains("readIt("),
        "the checked-throwing sibling must not be emitted:\n{java}"
    );
    let kotlin = fs::read_to_string(tree.join("Siblings.kt"))
        .expect("the checked-throwing sibling must stay in Kotlin");
    assert!(kotlin.contains("fun readIt("), "{kotlin}");
    assert!(!kotlin.contains("fun sortedOnly("), "{kotlin}");

    let report = validate_jvm_sources(&config(
        &tree,
        scratch.0.join("migrated-classes"),
        &stdlib,
        Some("probe.MigratedMain"),
    ))
    .expect("the mixed Java + retained Kotlin tree must compile and run");
    assert_eq!(
        report.commands.last().unwrap().stdout.trim(),
        expected,
        "the mixed tree must print the original value"
    );
}

/// Acceptance regression for the closed architecture blocker.
///
/// A declaration no checked-exception rule can protect: `String.trim()` lowers
/// to Java `String.trim()`, and Kotlin's no-argument `trim()` strips Unicode
/// whitespace (U+00A0 included) while Java's strips only `<= ' '`. The member
/// proof *does* sign `Stream.sorted()` in the same declaration, so before the
/// declaration-wide contract the migration emitted Java that compiled and
/// printed a different value.
///
/// The contract now refuses the declaration: `println` and `trim` are
/// operations with no lowering contract, so the unit is retained before any
/// write. This test asserts the *sound* outcome with real tools — the original
/// Kotlin value, no emitted Java, and the retained Kotlin recompiling to the
/// same bytes — instead of the defective emission. A `trim` name blacklist
/// would not satisfy it: the refusal names the missing operation generically.
#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn new_gain_through_trim_is_retained_and_keeps_runtime_behavior() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();

    let source = "package probe\n\nimport java.util.stream.Stream\n\nfun mixed(values: Stream<Int>, text: String) {\n    println(text.trim())\n    values.sorted()\n}\n";
    let main = "package probe;\n\nimport java.util.stream.Stream;\n\npublic final class Main {\n    public static void main(String[] args) {\n        TrimKt.mixed(Stream.of(3, 1), \"\u{00a0}a\u{00a0}\");\n    }\n}\n";

    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("Trim.kt"), source).unwrap();
    fs::write(original.join("Main.java"), main).unwrap();
    let reference = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        Some("probe.Main"),
    ))
    .expect("the original Kotlin must compile and run");
    let kotlin_output = reference.commands.last().unwrap().stdout.clone();
    assert_eq!(
        kotlin_output.trim(),
        "a",
        "Kotlin trim() strips U+00A0; this is the value Java trim() would change"
    );

    let tree = scratch.0.join("migrated");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("Trim.kt"), source).unwrap();
    fs::write(tree.join("Main.java"), main).unwrap();
    let (stdout, stderr) = migrate(&tree, Some(full.as_path()), false);
    assert!(
        !tree.join("Trim.java").exists(),
        "a declaration whose other operations have no lowering contract must not be emitted\n{stdout}\n{stderr}"
    );
    let retained = fs::read_to_string(tree.join("Trim.kt")).expect("retained Kotlin");
    assert!(
        retained.contains("text.trim()") && retained.contains("values.sorted()"),
        "both operations must survive in the retained source:\n{retained}"
    );
    assert!(
        retained.contains("no lowering contract"),
        "the refusal must name the missing operation, not a `trim` blacklist:\n{retained}"
    );

    let report = validate_jvm_sources(&config(
        &tree,
        scratch.0.join("migrated-classes"),
        &stdlib,
        Some("probe.Main"),
    ))
    .expect("the retained Kotlin must still compile and run");
    assert_eq!(
        report.commands.last().unwrap().stdout,
        kotlin_output,
        "the retained program must keep the original Kotlin behavior"
    );
}

/// The physical signature of one compiled class, as `javap -s` prints it.
///
/// A descriptor is the ABI claim under test: two Java sources can look
/// plausible and still declare different methods, which is exactly what a
/// spelling-derived type produces.
fn javap_signature(classes: &Path, class_name: &str) -> String {
    let javac = PathBuf::from(java_of("NOTLIN_JAVAC"));
    let javap = javac.with_file_name("javap");
    let output = Command::new(javap)
        .args(["-p", "-s", "-classpath"])
        .arg(classes)
        .arg(class_name)
        .output()
        .unwrap_or_else(|error| panic!("javap must run for {class_name}: {error}"));
    assert!(
        output.status.success(),
        "javap failed for {class_name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Parent-reviewed fixture (`parent-review/noncall-probe-source/Boundary.kt`):
/// a proved `Stream.sorted()` in the same block body as `left + right`.
///
/// Kotlin's `+` on `BigDecimal` is `BigDecimal.plus`, which the emitter had no
/// contract for; the frozen candidate wrote `int sum = left.plus(right);` and
/// javac rejected it. The declaration must be retained before any write, with
/// and without `--allow-approximations`, and the retained Kotlin must keep
/// running as it did.
#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_retains_a_class_whose_sibling_operator_is_unproved() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();

    let source = "package probe\n\nimport java.math.BigDecimal\nimport java.util.stream.Stream\n\nclass Boundary {\n    fun touch(values: Stream<Int>, left: BigDecimal, right: BigDecimal) {\n        values.sorted()\n        val sum = left + right\n    }\n}\n";
    let main = "package probe;\n\nimport java.math.BigDecimal;\nimport java.util.stream.Stream;\n\npublic final class Main {\n    public static void main(String[] args) {\n        new Boundary().touch(Stream.of(3, 1, 2), new BigDecimal(\"1.5\"), new BigDecimal(\"2.5\"));\n        System.out.println(\"ok\");\n    }\n}\n";

    // 1. The original Kotlin is the runtime reference.
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("Boundary.kt"), source).unwrap();
    fs::write(original.join("Main.java"), main).unwrap();
    let reference = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        Some("probe.Main"),
    ))
    .expect("the original Kotlin must compile and run");
    let expected = reference.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(expected, "ok");

    // 2. Neither strict nor approximating mode may emit the class.
    for (label, allow) in [("strict", false), ("approximations", true)] {
        let tree = scratch.0.join(format!("migrated-{label}"));
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("Boundary.kt"), source).unwrap();
        fs::write(tree.join("Main.java"), main).unwrap();
        migrate(&tree, Some(full.as_path()), allow);
        assert!(
            !tree.join("Boundary.java").exists(),
            "{label}: `left + right` has no lowering contract, so the class must stay Kotlin"
        );
        let retained = fs::read_to_string(tree.join("Boundary.kt")).expect("retained Kotlin");
        assert!(
            retained.contains("values.sorted()") && retained.contains("left + right"),
            "{label}: both operations must survive:\n{retained}"
        );
        assert!(
            retained.contains("no lowering contract"),
            "{label}: the refusal must name the missing operation contract:\n{retained}"
        );
        let report = validate_jvm_sources(&config(
            &tree,
            scratch.0.join(format!("migrated-{label}-classes")),
            &stdlib,
            Some("probe.Main"),
        ))
        .unwrap_or_else(|error| panic!("{label}: retained Kotlin must compile and run: {error}"));
        assert_eq!(
            report.commands.last().unwrap().stdout.trim(),
            expected,
            "{label}"
        );
    }
}

/// Parent-reviewed fixture (`parent-review/alias-probe-source/Alias.kt`):
/// `typealias Any = Stream<Int>` and an `Any?` exit.
///
/// The Kotlin descriptor is `(Ljava/util/stream/Stream;)Ljava/util/stream/Stream;`.
/// Reading `Any?` off its spelling emitted `Object opaque(Stream<Integer>)`,
/// descriptor `…Ljava/lang/Object;` — javac accepts it and it is a different
/// method. The name is bound to a typealias, so the exit is not the builtin and
/// the declaration must be retained; retention keeps the descriptor.
#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_retains_an_exit_whose_builtin_spelling_is_aliased() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();

    let source = "package probe2\n\nimport java.util.stream.Stream\n\ntypealias Any = Stream<Int>\n\nfun opaque(values: Stream<Int>): Any? = values.sorted()\n";
    let expected_descriptor = "(Ljava/util/stream/Stream;)Ljava/util/stream/Stream;";

    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("Alias.kt"), source).unwrap();
    validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        None,
    ))
    .expect("the aliased original Kotlin must compile");
    let reference = javap_signature(&scratch.0.join("original-classes"), "probe2.AliasKt");
    assert!(
        reference.contains(expected_descriptor),
        "the Kotlin ABI under test changed:\n{reference}"
    );

    for (label, allow) in [("strict", false), ("approximations", true)] {
        let tree = scratch.0.join(format!("migrated-{label}"));
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("Alias.kt"), source).unwrap();
        migrate(&tree, Some(full.as_path()), allow);
        assert!(
            !tree.join("Alias.java").exists(),
            "{label}: an aliased `Any?` is not java.lang.Object; emitting it changes the descriptor"
        );
        let retained = fs::read_to_string(tree.join("Alias.kt")).expect("retained Kotlin");
        assert!(
            retained.contains("typealias Any"),
            "{label}: the alias must survive:\n{retained}"
        );
        validate_jvm_sources(&config(
            &tree,
            scratch.0.join(format!("migrated-{label}-classes")),
            &stdlib,
            None,
        ))
        .unwrap_or_else(|error| panic!("{label}: retained Kotlin must compile: {error}"));
        let retained_descriptor = javap_signature(
            &scratch.0.join(format!("migrated-{label}-classes")),
            "probe2.AliasKt",
        );
        assert!(
            retained_descriptor.contains(expected_descriptor),
            "{label}: retention must preserve the physical ABI:\n{retained_descriptor}"
        );
    }
}

/// Parent-reviewed fixture
/// (`parent-review/parameter-alias-probe-source/ParameterAlias.kt`):
/// `typealias Int = String` and an unused formal `unused: Int`.
///
/// Kotlin's descriptor is `(Ljava/util/stream/Stream;Ljava/lang/String;)V`.
/// The candidate emitted `void link(Stream<Integer> values, int unused)` —
/// `(…;I)V` — because it spelled the parameter type instead of binding it. An
/// unused formal is still part of the emitted ABI.
#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_retains_a_formal_parameter_spelled_as_an_aliased_builtin() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();

    let source = "package probe3\n\nimport java.util.stream.Stream\n\ntypealias Int = String\n\nfun link(values: Stream<Int>, unused: Int) {\n    values.sorted()\n}\n";
    let expected_descriptor = "(Ljava/util/stream/Stream;Ljava/lang/String;)V";

    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("ParameterAlias.kt"), source).unwrap();
    validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        None,
    ))
    .expect("the aliased original Kotlin must compile");
    let reference = javap_signature(
        &scratch.0.join("original-classes"),
        "probe3.ParameterAliasKt",
    );
    assert!(
        reference.contains(expected_descriptor),
        "the Kotlin ABI under test changed:\n{reference}"
    );

    for (label, allow) in [("strict", false), ("approximations", true)] {
        let tree = scratch.0.join(format!("migrated-{label}"));
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("ParameterAlias.kt"), source).unwrap();
        migrate(&tree, Some(full.as_path()), allow);
        assert!(
            !tree.join("ParameterAlias.java").exists(),
            "{label}: an aliased formal-parameter spelling changes the JVM descriptor"
        );
        let retained = fs::read_to_string(tree.join("ParameterAlias.kt")).expect("retained Kotlin");
        assert!(
            retained.contains("unused: Int"),
            "{label}: the parameter must survive:\n{retained}"
        );
        validate_jvm_sources(&config(
            &tree,
            scratch.0.join(format!("migrated-{label}-classes")),
            &stdlib,
            None,
        ))
        .unwrap_or_else(|error| panic!("{label}: retained Kotlin must compile: {error}"));
        let retained_descriptor = javap_signature(
            &scratch.0.join(format!("migrated-{label}-classes")),
            "probe3.ParameterAliasKt",
        );
        assert!(
            retained_descriptor.contains(expected_descriptor),
            "{label}: retention must preserve the physical ABI:\n{retained_descriptor}"
        );
    }
}

// ---------------------------------------------------------------------------
// Declaration-shape and non-statement acceptance regressions.
//
// Every fixture here is legal Kotlin (kotlinc proves it), contains a signed
// `Stream.sorted()` call so the declaration is in Required mode, and must be
// *retained* — never crash the process, never write Java javac rejects.
// ---------------------------------------------------------------------------

fn write_sources(root: &Path, files: &[(&str, &str)]) {
    fs::create_dir_all(root).unwrap();
    for (name, source) in files {
        fs::write(root.join(name), source).unwrap();
    }
}

fn compile_kotlin(root: &Path, classes: &Path, stdlib: &Path) -> Result<(), String> {
    let mut config = config(root, classes.to_path_buf(), stdlib, None);
    config.java_sources.clear();
    validate_jvm_sources(&config)
        .map(|_| ())
        .map_err(|error| format!("{error:?}"))
}

/// Compile a migrated tree whose retained Kotlin depends on Java the same run
/// emitted: javac first, then kotlinc with those classes on its classpath.
fn compile_migrated_tree(root: &Path, classes: &Path, stdlib: &Path) -> Result<(), String> {
    fs::create_dir_all(classes).unwrap();
    let mut java_sources = Vec::new();
    let mut kotlin_sources = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("java") => java_sources.push(path),
            Some("kt") => kotlin_sources.push(path),
            _ => {}
        }
    }
    java_sources.sort();
    kotlin_sources.sort();
    if !java_sources.is_empty() {
        let output = Command::new(java_of("NOTLIN_JAVAC"))
            .args(["--release", "17", "-proc:none", "-cp"])
            .arg(stdlib)
            .arg("-d")
            .arg(classes)
            .args(&java_sources)
            .output()
            .map_err(|error| format!("javac did not run: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "javac failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    if !kotlin_sources.is_empty() {
        let classpath = format!("{}:{}", classes.display(), stdlib.display());
        let output = Command::new(java_of("NOTLIN_KOTLINC"))
            .args(["-jvm-target", "17", "-cp"])
            .arg(&classpath)
            .arg("-d")
            .arg(classes)
            .args(&kotlin_sources)
            .output()
            .map_err(|error| format!("kotlinc did not run: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "kotlinc failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_retains_star_and_function_type_formals_without_aborting() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();
    let source = "package probe\n\nimport java.util.stream.Stream\n\nfun star(values: Stream<*>) {\n    values.sorted()\n}\n\nfun fnty(values: Stream<() -> Unit>) {\n    values.sorted()\n}\n";

    let original = scratch.0.join("original");
    write_sources(&original, &[("Shapes.kt", source)]);
    compile_kotlin(&original, &scratch.0.join("original-classes"), &stdlib)
        .expect("the original Kotlin must compile");

    for (label, allow) in [("strict", false), ("approximations", true)] {
        let tree = scratch.0.join(format!("migrated-{label}"));
        write_sources(&tree, &[("Shapes.kt", source)]);
        // `migrate` asserts the process exits successfully: a stack overflow
        // (exit 134/-6) fails here instead of passing silently.
        let (stdout, stderr) = migrate(&tree, Some(full.as_path()), allow);
        assert!(
            !tree.join("Shapes.java").exists(),
            "{label}: a star/function-type formal has no proved Java ABI\n{stdout}\n{stderr}"
        );
        let retained = fs::read_to_string(tree.join("Shapes.kt")).unwrap();
        assert!(
            retained.contains("star projection"),
            "{label}: the star projection must be named:\n{retained}"
        );
        assert!(
            retained.contains("function type"),
            "{label}: the function type must be named:\n{retained}"
        );
        compile_kotlin(
            &tree,
            &scratch.0.join(format!("migrated-{label}-classes")),
            &stdlib,
        )
        .unwrap_or_else(|error| panic!("{label}: retained Kotlin must compile: {error}"));
    }
}

/// An explicit import whose simple name is a Kotlin builtin spelling, and a
/// fully qualified source typealias path. Both are legal Kotlin whose JVM
/// descriptors the emitted Java would silently change; retention keeps them.
#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_retains_an_imported_builtin_spelling_and_a_qualified_alias() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();
    let caller = "package probe\n\nimport java.util.stream.Stream\nimport foreign.Int\n\nfun link(values: Stream<String>, unused: Int) {\n    values.sorted()\n}\n";
    let owner = "package foreign\n\nclass Int\n";
    let aliased = "package probe2\n\nimport java.util.stream.Stream\n\nfun link(values: Stream<String>, unused: aliases.Int) {\n    values.sorted()\n}\n";
    let alias = "package aliases\n\ntypealias Int = String\n";
    let files: &[(&str, &str)] = &[
        ("ImportCaller.kt", caller),
        ("Int.kt", owner),
        ("QualifiedAlias.kt", aliased),
        ("Aliases.kt", alias),
    ];

    let original = scratch.0.join("original");
    write_sources(&original, files);
    compile_kotlin(&original, &scratch.0.join("original-classes"), &stdlib)
        .expect("the original Kotlin must compile");
    let reference = javap_signature(&scratch.0.join("original-classes"), "probe.ImportCallerKt");
    assert!(
        reference.contains("(Ljava/util/stream/Stream;Lforeign/Int;)V"),
        "the Kotlin ABI under test changed:\n{reference}"
    );

    for (label, allow) in [("strict", false), ("approximations", true)] {
        let tree = scratch.0.join(format!("migrated-{label}"));
        write_sources(&tree, files);
        let (stdout, stderr) = migrate(&tree, Some(full.as_path()), allow);
        assert!(
            !tree.join("ImportCaller.java").exists(),
            "{label}: an imported `Int` is not the Kotlin builtin\n{stdout}\n{stderr}"
        );
        assert!(
            !tree.join("QualifiedAlias.java").exists(),
            "{label}: `aliases.Int` is not a Java class\n{stdout}\n{stderr}"
        );
        let retained = fs::read_to_string(tree.join("ImportCaller.kt")).unwrap();
        assert!(
            retained.contains("not a proved Java type"),
            "{label}: the refusal must name the unproved ABI:\n{retained}"
        );
        let classes = scratch.0.join(format!("migrated-{label}-classes"));
        compile_migrated_tree(&tree, &classes, &stdlib)
            .unwrap_or_else(|error| panic!("{label}: the migrated tree must compile: {error}"));
        let retained_signature = javap_signature(&classes, "probe.ImportCallerKt");
        assert!(
            retained_signature.contains("(Ljava/util/stream/Stream;Lforeign/Int;)V"),
            "{label}: retention must preserve the physical ABI:\n{retained_signature}"
        );
    }
}

/// A bare literal, a bare identifier and a parenthesized call are not Java
/// statement expressions. Kotlin accepts all three as block statements; writing
/// them out produced `1;` / `values;` / `(values.sorted());` and deleted the
/// Kotlin source.
#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_retains_non_statement_expression_bodies() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();
    let source = "package probe\n\nimport java.util.stream.Stream\n\nfun literal(values: Stream<String>) {\n    values.sorted()\n    1\n}\n\nfun identifier(values: Stream<String>) {\n    values.sorted()\n    values\n}\n\nfun parenthesized(values: Stream<String>) {\n    (values.sorted())\n}\n";

    let original = scratch.0.join("original");
    write_sources(&original, &[("Literal.kt", source)]);
    compile_kotlin(&original, &scratch.0.join("original-classes"), &stdlib)
        .expect("the original Kotlin must compile");

    for (label, allow) in [("strict", false), ("approximations", true)] {
        let tree = scratch.0.join(format!("migrated-{label}"));
        write_sources(&tree, &[("Literal.kt", source)]);
        let (stdout, stderr) = migrate(&tree, Some(full.as_path()), allow);
        assert!(
            !tree.join("Literal.java").exists(),
            "{label}: a non-statement expression has no Java statement form\n{stdout}\n{stderr}"
        );
        let retained = fs::read_to_string(tree.join("Literal.kt")).unwrap();
        assert!(
            retained.contains("no lowering contract"),
            "{label}: the refusal must name the operation:\n{retained}"
        );
        compile_kotlin(
            &tree,
            &scratch.0.join(format!("migrated-{label}-classes")),
            &stdlib,
        )
        .unwrap_or_else(|error| panic!("{label}: retained Kotlin must compile: {error}"));
    }
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_preserves_retained_interface_clients_in_every_default_mode() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full = PathBuf::from(java_of("NOTLIN_JVM_FACTS_FULL_JAR"));
    let scratch = Scratch::new();
    let source = "package probe\n\nimport java.util.stream.Stream\n\ninterface Endpoint {\n    fun touch(values: Stream<String>) {\n        values.sorted()\n    }\n}\n";
    let client = "package probe\n\nimport java.util.stream.Stream\n\nclass Client : Endpoint\n\nfun main() {\n    Client().touch(Stream.of(\"b\", \"a\"))\n    println(\"ok\")\n}\n";
    for mode in ["disable", "enable", "no-compatibility"] {
        let original = scratch.0.join(format!("original-{mode}"));
        write_sources(&original, &[("Endpoint.kt", source), ("Client.kt", client)]);
        let original_classes = scratch.0.join(format!("original-{mode}-classes"));
        let mut reference = config(
            &original,
            original_classes.clone(),
            &stdlib,
            Some("probe.ClientKt"),
        );
        reference
            .kotlinc_args
            .push(format!("-jvm-default={mode}").into());
        let result = validate_jvm_sources(&reference)
            .unwrap_or_else(|error| panic!("{mode}: original interface/client must run: {error}"));
        assert_eq!(result.commands.last().unwrap().stdout.trim(), "ok");
        let consumers = scratch.0.join(format!("consumers-{mode}"));
        fs::create_dir_all(consumers.join("probe")).unwrap();
        for name in ["Client.class", "ClientKt.class"] {
            fs::copy(
                original_classes.join("probe").join(name),
                consumers.join("probe").join(name),
            )
            .unwrap();
        }
        for (label, allow) in [("strict", false), ("approximations", true)] {
            let tree = scratch.0.join(format!("migrated-{mode}-{label}"));
            write_sources(&tree, &[("Endpoint.kt", source)]);
            let _ = migrate(&tree, Some(&full), allow);
            assert!(
                !tree.join("Endpoint.java").exists(),
                "{mode}/{label}: interface ABI must not be replaced by a Java default method"
            );
            let retained =
                fs::read_to_string(tree.join("Endpoint.kt")).expect("retained interface");
            assert!(
                retained.contains("interface") && retained.contains("not proved"),
                "{retained}"
            );
            let classes = scratch.0.join(format!("migrated-{mode}-{label}-classes"));
            let mut migrated = config(&tree, classes.clone(), &stdlib, Some("probe.ClientKt"));
            migrated
                .kotlinc_args
                .push(format!("-jvm-default={mode}").into());
            migrated.classpath.push(consumers.clone());
            let result = validate_jvm_sources(&migrated).unwrap_or_else(|error| {
                panic!("{mode}/{label}: unchanged compiled client must link: {error}")
            });
            assert_eq!(result.commands.last().unwrap().stdout.trim(), "ok");
            assert_eq!(
                javap_signature(&classes, "probe.Endpoint"),
                javap_signature(&original_classes, "probe.Endpoint")
            );
            if mode != "no-compatibility" {
                assert_eq!(
                    javap_signature(&classes, "probe.Endpoint$DefaultImpls"),
                    javap_signature(&original_classes, "probe.Endpoint$DefaultImpls")
                );
            }
        }
    }
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_keeps_comment_trivia_out_of_the_invocation_contract() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full = PathBuf::from(java_of("NOTLIN_JVM_FACTS_FULL_JAR"));
    let scratch = Scratch::new();
    let source = SOURCE
        .replace(
            "    values.sorted()",
            "    // before\n    /* block */\n    values.sorted()\n    // after",
        )
        .replace(
            "= values.sorted()",
            "{\n    return /* result */ (/* outer */ (\n        // inner\n        values.sorted()\n    ))\n}",
        );
    let original = scratch.0.join("original");
    write_sources(&original, &[("Receipt.kt", &source)]);
    fs::write(
        original.join("ReferenceMain.java"),
        oracle_main("ReferenceMain", "probe.ReceiptKt"),
    )
    .unwrap();
    let reference = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        Some("probe.ReferenceMain"),
    ))
    .expect("original commented Kotlin must compile and run");
    let expected = reference.commands.last().unwrap().stdout.trim();
    assert_eq!(expected, "discard=1\nopaque=2:true");
    for (label, allow) in [("strict", false), ("approximations", true)] {
        let tree = scratch.0.join(label);
        write_sources(&tree, &[("Receipt.kt", &source)]);
        let _ = migrate(&tree, Some(&full), allow);
        assert!(
            tree.join("Receipt.java").exists(),
            "{label}: comments must not cause retention"
        );
        assert!(
            !tree.join("Receipt.kt").exists(),
            "{label}: both declarations must translate"
        );
        fs::write(
            tree.join("MigratedMain.java"),
            oracle_main("MigratedMain", "probe.Receipt"),
        )
        .unwrap();
        let result = validate_jvm_sources(&config(
            &tree,
            scratch.0.join(format!("{label}-classes")),
            &stdlib,
            Some("probe.MigratedMain"),
        ))
        .unwrap_or_else(|error| {
            panic!("{label}: commented call/return must preserve behavior: {error}")
        });
        assert_eq!(result.commands.last().unwrap().stdout.trim(), expected);
    }
}

/// `"$name"` parses as `string_content` siblings and `"${expr}"` as
/// `interpolation`; neither has a signed template lowering.
#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the full helper jar"]
fn full_helper_retains_string_templates_in_a_required_declaration() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();
    let source = "package probe\n\nimport java.util.stream.Stream\n\nfun name(values: Stream<Int>): Any? {\n    values.sorted()\n    return \"probe: $values\"\n}\n\nfun expression(values: Stream<Int>): Any? {\n    values.sorted()\n    return \"probe: ${values.hashCode()}\"\n}\n";

    let original = scratch.0.join("original");
    write_sources(&original, &[("Template.kt", source)]);
    compile_kotlin(&original, &scratch.0.join("original-classes"), &stdlib)
        .expect("the original Kotlin must compile");

    for (label, allow) in [("strict", false), ("approximations", true)] {
        let tree = scratch.0.join(format!("migrated-{label}"));
        write_sources(&tree, &[("Template.kt", source)]);
        let (stdout, stderr) = migrate(&tree, Some(full.as_path()), allow);
        assert!(
            !tree.join("Template.java").exists(),
            "{label}: a string template has no signed lowering\n{stdout}\n{stderr}"
        );
        let retained = fs::read_to_string(tree.join("Template.kt")).unwrap();
        assert!(
            retained.contains("string_literal"),
            "{label}: the template operation must be named:\n{retained}"
        );
        compile_kotlin(
            &tree,
            &scratch.0.join(format!("migrated-{label}-classes")),
            &stdlib,
        )
        .unwrap_or_else(|error| panic!("{label}: retained Kotlin must compile: {error}"));
    }
}

// ---------------------------------------------------------------------------
// Bound nullable `java.lang.String` exit: real null / evaluation / ABI proof
// ---------------------------------------------------------------------------
//
// `f.getName()` on a `java.io.File` subclass that returns `null` is the oracle
// that separates the two exits:
//
// * `String?` — Kotlin emits no null check, so `null` propagates and the
//   override runs exactly once;
// * `String`  — Kotlin emits `Intrinsics.checkNotNullExpressionValue`, so the
//   same null throws `NullPointerException` after exactly one invocation. That
//   declaration must stay Kotlin: this slice has no intrinsic-check lowering.
//
// The migrated Java must reproduce the nullable exit: same single invocation,
// same `null`, same `java.lang.String` descriptor, and no
// `requireNonNull`/`checkNotNull`/`toString` coercion.
//
// The two declarations are migrated from *separate* roots on purpose. A file
// that mixes a translated declaration with a retained one is re-planned
// without receipts in a later pass, and in that pass the older platform
// member path emits `f.getName()` for the retained declaration — a
// pre-existing declaration-coverage gap that predates this slice (the accepted
// comparator binary reproduces it). Pinning it here as a mixed fixture would
// test that gap instead of this contract, so each fixture is one declaration.

const NULLABLE_NAME_SOURCE: &str = r#"package probe

import java.io.File

fun nullableName(f: File): String? = f.getName()
"#;

const STRICT_NAME_SOURCE: &str = r#"package probe

import java.io.File

fun strictName(f: File): String = f.getName()
"#;

const NEVER_NAME: &str = r#"package probe;

public final class NeverName extends java.io.File {
    private int calls;

    public NeverName() {
        super("ordinal");
    }

    @Override
    public String getName() {
        calls++;
        return null;
    }

    public int calls() {
        return calls;
    }
}
"#;

/// `facade` names the same function in the retained Kotlin
/// (`probe.NullableNameKt`) and in the migrated Java (`probe.NullableName`), so
/// the printed bytes are a direct differential.
fn nullable_name_main(class_name: &str, facade: &str) -> String {
    format!(
        r#"package probe;

public final class {class_name} {{
    public static void main(String[] args) {{
        NeverName nullReturning = new NeverName();
        Object value = {facade}.nullableName(nullReturning);
        System.out.println("nullable=" + value + ":calls=" + nullReturning.calls());
        System.out.println(
            "control=" + {facade}.nullableName(new java.io.File("keep.txt")));
    }}
}}
"#
    )
}

fn strict_name_main(class_name: &str, facade: &str) -> String {
    format!(
        r#"package probe;

public final class {class_name} {{
    public static void main(String[] args) {{
        NeverName nullReturning = new NeverName();
        try {{
            System.out.println(
                "strict=" + {facade}.strictName(nullReturning)
                    + ":calls=" + nullReturning.calls());
        }} catch (NullPointerException error) {{
            System.out.println("strict=NPE:calls=" + nullReturning.calls());
        }}
    }}
}}
"#
    )
}

fn java_tool(name: &str) -> std::ffi::OsString {
    PathBuf::from(java_of("NOTLIN_JAVA"))
        .with_file_name(name)
        .into_os_string()
}

fn javap(classes: &Path, class_name: &str) -> String {
    let output = Command::new(java_tool("javap"))
        .arg("-cp")
        .arg(classes)
        .arg(class_name)
        .output()
        .expect("javap must run");
    assert!(
        output.status.success(),
        "javap {class_name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the slim/full helper jars"]
fn full_helper_emits_the_bound_nullable_string_exit_with_real_null_semantics() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let slim = PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_JAR").expect("slim helper jar"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();

    // 1. Original Kotlin: the runtime reference for null propagation, single
    //    evaluation and the intrinsic null check of the non-null exit.
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("NullableName.kt"), NULLABLE_NAME_SOURCE).unwrap();
    fs::write(original.join("StrictName.kt"), STRICT_NAME_SOURCE).unwrap();
    fs::write(original.join("NeverName.java"), NEVER_NAME).unwrap();
    fs::write(
        original.join("NullableMain.java"),
        nullable_name_main("NullableMain", "probe.NullableNameKt"),
    )
    .unwrap();
    fs::write(
        original.join("StrictMain.java"),
        strict_name_main("StrictMain", "probe.StrictNameKt"),
    )
    .unwrap();
    // The nullable oracle and the strict oracle are separate processes so each
    // printed stream is directly comparable with its migrated counterpart.
    let nullable_reference = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-nullable-classes"),
        &stdlib,
        Some("probe.NullableMain"),
    ))
    .expect("the original nullable Kotlin must compile and run");
    let nullable_expected = nullable_reference
        .commands
        .last()
        .unwrap()
        .stdout
        .trim()
        .to_owned();
    assert_eq!(
        nullable_expected, "nullable=null:calls=1\ncontrol=keep.txt",
        "the nullable oracle fixture itself changed"
    );
    let reference_abi = javap(
        &scratch.0.join("original-nullable-classes"),
        "probe.NullableNameKt",
    );
    assert!(
        reference_abi.contains("java.lang.String nullableName(java.io.File)"),
        "the original nullable exit must be a java.lang.String descriptor:\n{reference_abi}"
    );

    // 2. The non-null destination: real Kotlin throws on the same null. This is
    //    the recorded original behaviour, not an invented NPE.
    let strict_reference = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-strict-classes"),
        &stdlib,
        Some("probe.StrictMain"),
    ))
    .expect("the original strict Kotlin must compile and run");
    assert_eq!(
        strict_reference.commands.last().unwrap().stdout.trim(),
        "strict=NPE:calls=1",
        "the non-null exit's original intrinsic check changed"
    );

    // 3. Helper off and the slim helper keep the nullable declaration Kotlin.
    for (mode, jar) in [("off", None), ("slim", Some(slim.as_path()))] {
        let tree = scratch.0.join(format!("retained-{mode}"));
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("NullableName.kt"), NULLABLE_NAME_SOURCE).unwrap();
        fs::write(tree.join("NeverName.java"), NEVER_NAME).unwrap();
        fs::write(
            tree.join("NullableMain.java"),
            nullable_name_main("NullableMain", "probe.NullableNameKt"),
        )
        .unwrap();
        let _ = migrate(&tree, jar, false);
        assert!(
            !tree.join("NullableName.java").exists(),
            "{mode}: no nullable String exit without a physical receipt"
        );
        let report = validate_jvm_sources(&config(
            &tree,
            scratch.0.join(format!("retained-{mode}-classes")),
            &stdlib,
            Some("probe.NullableMain"),
        ))
        .unwrap_or_else(|error| panic!("{mode}: retained Kotlin must compile and run: {error}"));
        assert_eq!(
            report.commands.last().unwrap().stdout.trim(),
            nullable_expected,
            "{mode}"
        );
    }

    // 4. The full helper signs the nullable exit and emits the same virtual
    //    call.
    let tree = scratch.0.join("migrated-nullable");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("NullableName.kt"), NULLABLE_NAME_SOURCE).unwrap();
    let (stdout, stderr) = migrate(&tree, Some(full.as_path()), false);
    let java = fs::read_to_string(tree.join("NullableName.java")).unwrap_or_else(|error| {
        panic!("the proved nullable String exit must translate: {error}\n{stdout}\n{stderr}")
    });
    assert!(
        java.contains("return f.getName();"),
        "the exit must be the same virtual call, not a coercion:\n{java}"
    );
    assert!(
        !java.contains("requireNonNull")
            && !java.contains("checkNotNull")
            && !java.contains("toString()"),
        "the nullable exit must not observe null or the payload:\n{java}"
    );
    assert!(
        !tree.join("NullableName.kt").exists(),
        "the fully translated unit must leave the Kotlin file"
    );
    fs::write(tree.join("NeverName.java"), NEVER_NAME).unwrap();
    fs::write(
        tree.join("MigratedMain.java"),
        nullable_name_main("MigratedMain", "probe.NullableName"),
    )
    .unwrap();
    let report = validate_jvm_sources(&config(
        &tree,
        scratch.0.join("migrated-nullable-classes"),
        &stdlib,
        Some("probe.MigratedMain"),
    ))
    .unwrap_or_else(|error| panic!("migrated Java must compile and run: {error}"));
    assert_eq!(
        report.commands.last().unwrap().stdout.trim(),
        nullable_expected,
        "the migrated nullable exit must propagate null after a single invocation"
    );
    let migrated_abi = javap(
        &scratch.0.join("migrated-nullable-classes"),
        "probe.NullableName",
    );
    assert!(
        migrated_abi.contains("java.lang.String nullableName(java.io.File)"),
        "the migrated exit must keep the source ABI descriptor:\n{migrated_abi}"
    );

    // 5. The non-null sibling stays Kotlin, so its intrinsic check is still
    //    the original one.
    let strict = scratch.0.join("retained-strict");
    fs::create_dir_all(&strict).unwrap();
    fs::write(strict.join("StrictName.kt"), STRICT_NAME_SOURCE).unwrap();
    fs::write(strict.join("NeverName.java"), NEVER_NAME).unwrap();
    fs::write(
        strict.join("StrictMain.java"),
        strict_name_main("StrictMain", "probe.StrictNameKt"),
    )
    .unwrap();
    let _ = migrate(&strict, Some(full.as_path()), false);
    assert!(
        !strict.join("StrictName.java").exists(),
        "a non-null `String` destination has no proved intrinsic check"
    );
    let report = validate_jvm_sources(&config(
        &strict,
        scratch.0.join("retained-strict-classes"),
        &stdlib,
        Some("probe.StrictMain"),
    ))
    .unwrap_or_else(|error| panic!("retained strict Kotlin must compile and run: {error}"));
    assert_eq!(
        report.commands.last().unwrap().stdout.trim(),
        "strict=NPE:calls=1",
        "retaining the non-null declaration keeps its original behaviour"
    );
}

/// One physical file with a provable exit and a sibling that has no proved Java
/// form. Unlike the one-declaration-per-root fixtures above, this is the shape a
/// real workspace has, and it is the shape that exposed the re-plan bug: pass 1
/// translates `nullableName`, and pass 2 re-plans the rewritten file, whose
/// bytes the helper never answered for.
///
/// The migrated tree therefore owns *two* facades — the generated
/// `probe.Mixed.nullableName` and the retained `probe.MixedKt.strictName` — and
/// both must behave exactly like the all-Kotlin original:
///
/// ```text
/// nullable=null:calls=1     # no intrinsic check on the nullable exit
/// strict=NPE:calls=1        # the retained declaration keeps its null check
/// ```
///
/// The old bug printed `strict=null:calls=1`: the strict declaration was emitted
/// by the platform member path after its receipts disappeared, so Kotlin's
/// intrinsic null check was silently dropped. This test is the end-to-end
/// counterexample, against the real JDK and the real helper.
const MIXED_NAME_SOURCE: &str = r#"package probe

import java.io.File

fun nullableName(f: File): String? = f.getName()

fun strictName(f: File): String = f.getName()
"#;

/// `nullableFacade`/`strictFacade` are the classes that actually own each
/// declaration: identical in the original (`probe.MixedKt`), split after
/// migration.
fn mixed_name_main(class_name: &str, nullable_facade: &str, strict_facade: &str) -> String {
    format!(
        r#"package probe;

public final class {class_name} {{
    public static void main(String[] args) {{
        NeverName nullable = new NeverName();
        System.out.println(
            "nullable=" + {nullable_facade}.nullableName(nullable)
                + ":calls=" + nullable.calls());
        NeverName strict = new NeverName();
        try {{
            System.out.println(
                "strict=" + {strict_facade}.strictName(strict)
                    + ":calls=" + strict.calls());
        }} catch (NullPointerException error) {{
            System.out.println("strict=NPE:calls=" + strict.calls());
        }}
    }}
}}
"#
    )
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB and the slim/full helper jars"]
fn a_mixed_top_level_file_keeps_its_strict_sibling_kotlin() {
    let stdlib = PathBuf::from(java_of("NOTLIN_KOTLIN_STDLIB"));
    let full =
        PathBuf::from(std::env::var_os("NOTLIN_JVM_FACTS_FULL_JAR").expect("full helper jar"));
    let scratch = Scratch::new();

    // 1. The original program: one file, both declarations, recorded behaviour.
    let original = scratch.0.join("original");
    fs::create_dir_all(&original).unwrap();
    fs::write(original.join("Mixed.kt"), MIXED_NAME_SOURCE).unwrap();
    fs::write(original.join("NeverName.java"), NEVER_NAME).unwrap();
    fs::write(
        original.join("OriginalMain.java"),
        mixed_name_main("OriginalMain", "probe.MixedKt", "probe.MixedKt"),
    )
    .unwrap();
    let reference = validate_jvm_sources(&config(
        &original,
        scratch.0.join("original-classes"),
        &stdlib,
        Some("probe.OriginalMain"),
    ))
    .expect("the original mixed Kotlin must compile and run");
    let expected = reference.commands.last().unwrap().stdout.trim().to_owned();
    assert_eq!(
        expected, "nullable=null:calls=1\nstrict=NPE:calls=1",
        "the original mixed fixture itself changed"
    );

    // 2. The full helper: only the proved declaration moves to Java, and the
    //    retained sibling keeps its Kotlin facade and its null check.
    let tree = scratch.0.join("migrated");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("Mixed.kt"), MIXED_NAME_SOURCE).unwrap();
    fs::write(tree.join("NeverName.java"), NEVER_NAME).unwrap();
    let (stdout, stderr) = migrate(&tree, Some(full.as_path()), false);
    let java = fs::read_to_string(tree.join("Mixed.java")).unwrap_or_else(|error| {
        panic!("the proved nullable exit must still translate: {error}\n{stdout}\n{stderr}")
    });
    assert!(
        java.contains("return f.getName();") && !java.contains("strictName"),
        "the sibling must not ride the translation:\n{java}"
    );
    let kotlin =
        fs::read_to_string(tree.join("Mixed.kt")).expect("the strict sibling must stay Kotlin");
    assert!(
        kotlin.contains("fun strictName(f: File): String = f.getName()")
            && !kotlin.contains("fun nullableName"),
        "the retained file must hold exactly the strict declaration:\n{kotlin}"
    );
    assert!(
        kotlin.contains("earlier snapshot"),
        "the retention must name the carried obligation:\n{kotlin}"
    );
    fs::write(
        tree.join("MixedMain.java"),
        mixed_name_main("MixedMain", "probe.Mixed", "probe.MixedKt"),
    )
    .unwrap();
    let report = validate_jvm_sources(&config(
        &tree,
        scratch.0.join("migrated-classes"),
        &stdlib,
        Some("probe.MixedMain"),
    ))
    .unwrap_or_else(|error| panic!("the migrated mixed tree must compile and run: {error}"));
    assert_eq!(
        report.commands.last().unwrap().stdout.trim(),
        expected,
        "the migrated tree must reproduce the original bytes: same null propagation, same \
         single evaluation, and the retained declaration's intrinsic null check"
    );

    // 3. Physical ABI: the generated class holds the nullable exit and nothing
    //    else; the retained Kotlin keeps the strict one.
    let migrated_abi = javap(&scratch.0.join("migrated-classes"), "probe.Mixed");
    assert!(
        migrated_abi.contains("java.lang.String nullableName(java.io.File)")
            && !migrated_abi.contains("strictName"),
        "the generated class must expose only the proved declaration:\n{migrated_abi}"
    );
    let retained_abi = javap(&scratch.0.join("migrated-classes"), "probe.MixedKt");
    assert!(
        retained_abi.contains("java.lang.String strictName(java.io.File)"),
        "the retained declaration must keep its Kotlin facade:\n{retained_abi}"
    );

    // 4. Helper off: no facts, no obligation, the whole file stays Kotlin and
    //    behaves like the original.
    let off = scratch.0.join("retained-off");
    fs::create_dir_all(&off).unwrap();
    fs::write(off.join("Mixed.kt"), MIXED_NAME_SOURCE).unwrap();
    fs::write(off.join("NeverName.java"), NEVER_NAME).unwrap();
    let _ = migrate(&off, None, false);
    assert!(
        !off.join("Mixed.java").exists(),
        "helper off must not invent a translation"
    );
    fs::write(
        off.join("OffMain.java"),
        mixed_name_main("OffMain", "probe.MixedKt", "probe.MixedKt"),
    )
    .unwrap();
    let off_report = validate_jvm_sources(&config(
        &off,
        scratch.0.join("retained-off-classes"),
        &stdlib,
        Some("probe.OffMain"),
    ))
    .expect("the helper-off tree must compile and run");
    assert_eq!(
        off_report.commands.last().unwrap().stdout.trim(),
        expected,
        "without a helper the file is untouched Kotlin"
    );
}
