//! Kotlin classes are final by DEFAULT; Java's default is the opposite. An
//! ordinary Kotlin class emitted as a plain Java class silently grants an
//! extensibility the source never had, so the faithful Java form is `final`.
//!
//! Opt-outs: `open`/`abstract`/`sealed` say so in the source (`open` is not a
//! Java keyword — its Java form is simply the absence of `final`), and anything
//! the workspace extends must stay open, since both javac and kotlinc reject
//! inheriting a final class.
//!
//! The "something extends it" half is only honoured for KOTLIN subtypes today:
//! `has_subtype_named` does not see Java subtypes, which is recorded below as
//! an ignored test rather than a quiet expectation.

use std::fs;
use std::path::Path;
use std::process::Command;

fn java_of(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join(format!("{name}.java"))).unwrap_or_default()
}

#[test]
fn ordinary_classes_are_final_and_open_classes_are_not() {
    let root = Path::new("tests/tmp_scratch_final_class");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("classes.kt"),
        r#"package neutral.fin

class Plain(val name: String)

open class Open(val name: String)

abstract class Base {
    abstract fun go(): String
}
"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("classes.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let plain = java_of(root, "Plain");
    assert!(
        plain.contains("final class Plain"),
        "an ordinary Kotlin class must be emitted final:\n{plain}"
    );

    let open = java_of(root, "Open");
    assert!(
        open.contains("class Open"),
        "an open class must still be emitted:\n{open}"
    );
    assert!(
        !open.contains("final class Open"),
        "an `open` class must not be final:\n{open}"
    );
    assert!(
        !open.contains("open class"),
        "`open` is not a Java keyword and must never be emitted:\n{open}"
    );

    let base = java_of(root, "Base");
    assert!(
        base.contains("abstract class Base"),
        "an abstract class stays abstract:\n{base}"
    );
    assert!(
        !base.contains("final "),
        "an abstract class cannot be final:\n{base}"
    );

    let _ = fs::remove_dir_all(root);
}

/// Kotlin's `final` keyword is authoritative and is never traded away. Valid
/// Kotlin cannot extend a `final` class at all, so an opt-out that would strip
/// the keyword is firing on evidence that cannot be legitimate — and dropping
/// it silently hands callers an extensibility the source explicitly refused.
///
/// Both opt-out triggers are present on purpose (a stray Java subtype the index
/// sees, and `@Entity`): the fixture is invalid Java, which is exactly the
/// false-positive shape the keyword has to outrank.
#[test]
fn an_explicit_final_class_keeps_its_modifier() {
    let root = Path::new("tests/tmp_scratch_final_explicit");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("types.kt"),
        r#"package neutral.finexp

final class Explicit(val name: String)

@Entity
final class Persistent(val id: String)
"#,
    )
    .unwrap();
    fs::write(
        root.join("Ghost.java"),
        "package neutral.finexp;\n\npublic class Ghost extends Explicit {\n    public Ghost() { super(\"x\"); }\n}\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("types.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    for name in ["Explicit", "Persistent"] {
        let java = java_of(root, name);
        let line = java
            .lines()
            .find(|l| l.contains("class ") && !l.trim_start().starts_with("//"))
            .unwrap_or_default()
            .trim()
            .to_string();
        assert!(
            line.contains("final class"),
            "an explicit `final` must survive every opt-out ({name}):\n{java}"
        );
    }

    let _ = fs::remove_dir_all(root);
}

/// KNOWN GAP, and the assertion is the behaviour we WANT: a hand-written Java
/// class in the workspace may extend a translated Kotlin class, and `final` on
/// that class is then a hard javac error ("cannot inherit from final class").
/// `has_subtype_named` does not see Java subtypes yet — the probe reproduces it
/// (`JavaBase` stays final beside `JSub extends JavaBase`).
///
/// Ignored rather than deleted or inverted: deleting hides the defect, and
/// inverting enshrines it.
#[test]
#[ignore = "gap: has_subtype_named does not see Java subtypes"]
fn a_class_a_java_source_extends_is_not_final() {
    let root = Path::new("tests/tmp_scratch_final_class_java_sub");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("base.kt"),
        "package neutral.finsub\n\nclass JavaBase(val name: String)\n",
    )
    .unwrap();
    fs::write(
        root.join("JSub.java"),
        "package neutral.finsub;\n\npublic class JSub extends JavaBase {\n    public JSub() { super(\"x\"); }\n}\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("base.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let base = java_of(root, "JavaBase");
    assert!(
        !base.contains("final class JavaBase"),
        "a class its workspace extends must not be final:\n{base}"
    );

    let _ = fs::remove_dir_all(root);
}
