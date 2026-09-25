//! The `Name::class` literal in an annotation argument lowers to `Name.class`
//! when `Name` resolves to a Java-visible declaration in the workspace index
//! (pre-existing Java or an already-translated Kotlin type); an unknown or
//! Kotlin-only name keeps the conservative taint.

use notlin::workspace::{DeclarationKind, SourceIndex};
use std::fs;
use std::path::Path;
use std::process::Command;

fn run(root: &Path, input: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(input.to_str().unwrap())
        .output()
        .expect("run notlin");
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn java_class_literal_in_annotation_lowers_to_class() {
    let root = Path::new("tests/tmp_scratch_klass_java");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("JavaPeer.java"),
        "package p;\npublic class JavaPeer {}\n",
    )
    .unwrap();
    fs::write(
        root.join("s.kt"),
        "package p\n\n@X(value = JavaPeer::class)\ninterface Good\n",
    )
    .unwrap();
    // The index must see the pre-existing java declaration.
    let index = SourceIndex::discover(root).unwrap();
    assert!(
        index
            .declarations()
            .any(|d| d.name == "JavaPeer" && d.kind == DeclarationKind::Class),
        "index must expose pre-existing java declarations"
    );
    let stderr = run(root, &root.join("s.kt"));
    let java = fs::read_to_string(root.join("Good.java")).unwrap_or_default();
    assert!(
        !java.is_empty(),
        "java-visible ::class must not taint, got:\n{stderr}"
    );
    assert!(
        java.contains("@X(value = JavaPeer.class)"),
        "class literal must lower to .class, got:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn unknown_class_literal_in_annotation_taints() {
    let root = Path::new("tests/tmp_scratch_kotlin_unknown");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("s.kt"),
        "package p\n\n@X(value = UnknownThing::class)\ninterface Bad\n",
    )
    .unwrap();
    let stderr = run(root, &root.join("s.kt"));
    let java = fs::read_to_string(root.join("Bad.java")).unwrap_or_default();
    assert!(
        java.is_empty(),
        "unknown ::class must taint the declaration, got:\n{java}"
    );
    assert!(
        stderr.contains("N04DC"),
        "expected the annotation retention diagnostic:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

/// Unnamed top-level nested annotation invocations (`@JsonSubTypes(T(...))`)
/// are Kotlin's auto-wrapped array form; Java needs the brace array literal
/// (`@JsonSubTypes({@JsonSubTypes.T(...)})`).
#[test]
fn unnamed_nested_annotation_calls_brace_wrap() {
    let root = Path::new("tests/tmp_scratch_nested_brace");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("JavaPeer.java"),
        "package p;\npublic class JavaPeer {}\npublic class OtherPeer {}\n",
    )
    .unwrap();
    fs::write(
        root.join("s.kt"),
        "package p\n\n@X(Y.Type(value = JavaPeer.class), Y.Type(value = OtherPeer.class))\ninterface Good\n",
    )
    .unwrap();
    let stderr = run(root, &root.join("s.kt"));
    let java = fs::read_to_string(root.join("Good.java")).unwrap_or_default();
    assert!(
        !java.is_empty(),
        "unnamed nested annotation calls must not taint, got:\n{stderr}"
    );
    assert!(
        java.contains("@X({@Y.Type(value = JavaPeer.class), @Y.Type(value = OtherPeer.class)})"),
        "unnamed nested annotation calls must brace-wrap, got:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

/// Named arguments holding nested annotations (`@X(subs = Y.Type(...))`) are
/// already valid Java — no brace wrapping.
#[test]
fn named_nested_annotation_argument_not_wrapped() {
    let root = Path::new("tests/tmp_scratch_nested_named");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("JavaPeer.java"),
        "package p;\npublic class JavaPeer {}\n",
    )
    .unwrap();
    fs::write(
        root.join("s.kt"),
        "package p\n\n@X(subs = Y.Type(value = JavaPeer.class))\ninterface Good\n",
    )
    .unwrap();
    let stderr = run(root, &root.join("s.kt"));
    let java = fs::read_to_string(root.join("Good.java")).unwrap_or_default();
    assert!(
        !java.is_empty(),
        "named nested annotation argument must not taint, got:\n{stderr}"
    );
    assert!(
        java.contains("@X(subs = @Y.Type(value = JavaPeer.class))"),
        "named nested annotation argument must stay unwrapped, got:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
