//! A Kotlin interface whose every subtype is INSIDE the selected translation
//! roots must not be retained merely for having Kotlin subtypes: those
//! subtypes translate in the same run, so the interface's Java ABI will have
//! Java clients. Retention stays correct when a Kotlin subtype lives OUTSIDE
//! the selection (its residual Kotlin `implements` would target a Java
//! interface and fail) — that case is covered by has_unselected_kotlin_subtype
//! and the retained-property fake-override rules.

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn interface_with_only_selected_kotlin_subtypes_still_retained() {
    // The selection-scope experiment (translating hub+implementor together)
    // over-reached: a Kotlin subtype retained by an unrelated rule (enum
    // entries ABI, declaration annotation, KClass) cannot implement a
    // translated-away interface — target builds failed on exactly that
    // shape. Until retention is decided bottom-up (subtypes first), an
    // interface retains whenever ANY Kotlin subtype exists.
    let root = Path::new("tests/tmp_scratch_subtype_scope");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    // One file, both selected: hub interface + Kotlin implementor.
    fs::write(
        root.join("hub.kt"),
        "package neutral.subtype\n\
         \n\
         interface Speaker {\n\
         \x20   fun speak(): String\n\
         }\n\
         \n\
         class Dog : Speaker {\n\
         \x20   override fun speak(): String = \"woof\"\n\
         }\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("hub.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let speaker_java = fs::read_to_string(root.join("Speaker.java")).unwrap_or_default();
    assert!(
        speaker_java.is_empty(),
        "hub with any Kotlin subtype must stay retained for now, got:\n{speaker_java}"
    );
    assert!(
        stderr.contains("N7395"),
        "expected the retention diagnostic:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn interface_with_unselected_kotlin_subtype_still_retained() {
    let root = Path::new("tests/tmp_scratch_subtype_scope2");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("selected")).unwrap();
    fs::create_dir_all(root.join("elsewhere")).unwrap();
    fs::write(
        root.join("selected").join("hub.kt"),
        "package neutral.subtype2\n\
         \n\
         interface Speaker {\n\
         \x20   fun speak(): String\n\
         }\n",
    )
    .unwrap();
    fs::write(
        root.join("elsewhere").join("impl.kt"),
        "package neutral.subtype2\n\
         \n\
         class Cat : Speaker {\n\
         \x20   override fun speak(): String = \"meow\"\n\
         }\n",
    )
    .unwrap();
    // Translate ONLY the selected subtree; the Cat file stays Kotlin outside it.
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("selected").join("hub.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let speaker_java =
        fs::read_to_string(root.join("selected").join("Speaker.java")).unwrap_or_default();
    assert!(
        speaker_java.is_empty(),
        "hub with an UNSELECTED Kotlin subtype must stay Kotlin, got:\n{speaker_java}"
    );
    assert!(
        stderr.contains("N7395"),
        "expected the retention diagnostic, stderr:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}
