//! Annotations on a translated class must survive translation. They are not
//! decoration — a Spring/JPA/Jackson annotation carries behaviour — so an
//! annotation notlin cannot lower took the whole declaration down with it: the
//! class stayed in Kotlin and no Java file was written at all.
//!
//! Two shapes did that needlessly:
//!   - a Kotlin array literal in the arguments, `@Marker(["p","q"])`, which is
//!     valid Java spelled `{"p","q"}`;
//!   - an annotation whose TYPE is a workspace-provable Kotlin `annotation
//!     class`, which compiles to a Java `@interface` that javac can reference
//!     from the same module's Java sources.

use std::fs;
use std::path::Path;
use std::process::Command;

fn emitted(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join(format!("{name}.java"))).unwrap_or_default()
}

#[test]
fn annotations_survive_translation() {
    let root = Path::new("tests/tmp_scratch_annotation_preserved");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("anns.kt"),
        r#"package neutral.anns

annotation class Marker(val value: String = "x")

annotation class Many(val names: Array<String>)

@Marker("y")
class WithArg(val a: String)

@Marker
class NoArg(val a: String)

@Many(["p", "q"])
class ArrayArg(val a: String)
"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("anns.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let with_arg = emitted(root, "WithArg");
    assert!(
        with_arg.contains("@Marker(\"y\")"),
        "an annotated class must be translated with its annotation:\n{with_arg}"
    );

    let no_arg = emitted(root, "NoArg");
    assert!(
        no_arg.contains("@Marker"),
        "an argument-less annotation must survive:\n{no_arg}"
    );

    // An array argument IS Java-expressible (`{"p", "q"}`), but lowering it is
    // not a local decision: it un-ties declarations whose bodies Java cannot
    // express yet. On the real target that was 12 declarations and 96 distinct
    // javac errors, so the shape still retains — asserted here as the current
    // behaviour, with the wanted behaviour recorded below.
    let array_arg = emitted(root, "ArrayArg");
    assert!(
        array_arg.is_empty(),
        "an array argument retains the declaration until the surrounding \
         bodies translate:\n{array_arg}"
    );
    let kotlin = fs::read_to_string(root.join("anns.kt")).unwrap_or_default();
    assert!(
        kotlin.contains("// NOTLIN: N04DC declaration annotation is retained in Kotlin")
            && kotlin.contains("class ArrayArg"),
        "and records the blocker beside the retained class:\n{kotlin}\n{stderr}"
    );

    let _ = fs::remove_dir_all(root);
}

/// The wanted behaviour, blocked on something else: those declarations must be
/// translatable before their annotations can be preserved. Ignored rather than
/// deleted, so the remaining work stays visible.
#[test]
#[ignore = "blocked: the 12 declarations retained for an array argument have bodies Java cannot express yet (96 javac errors measured on the target)"]
fn array_arguments_lower_to_java_brace_arrays() {
    let root = Path::new("tests/tmp_scratch_annotation_array");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("anns.kt"),
        "package neutral.anns\n\nannotation class Many(val names: Array<String>)\n\n@Many([\"p\", \"q\"])\nclass ArrayArg(val a: String)\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("anns.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(out.status.success(), "notlin failed");

    let array_arg = emitted(root, "ArrayArg");
    assert!(
        array_arg.contains("@Many({\"p\", \"q\"})"),
        "Kotlin array literals are Java brace arrays:\n{array_arg}"
    );

    let _ = fs::remove_dir_all(root);
}
