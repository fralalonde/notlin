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
fn interface_with_only_selected_kotlin_subtypes_translate_via_fixpoint() {
    // Superseded by the workspace-level retention fixpoint
    // (tests/fixpoint_retention.rs): a hub interface with only selected,
    // intrinsically CLEAN Kotlin subtypes translates together with them.
    // The old blanket rule (retain on any Kotlin subtype) remains in force
    // only for single-file mode without a workspace index.
    let root = Path::new("tests/tmp_scratch_subtype_scope");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    // One file, both selected: hub interface + Kotlin implementor. Translating
    // the DIRECTORY (not the single file) enables workspace fixpoint mode.
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
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let speaker_java = fs::read_to_string(root.join("Speaker.java")).unwrap_or_default();
    let dog_java = fs::read_to_string(root.join("Dog.java")).unwrap_or_default();
    assert!(
        !speaker_java.is_empty() && !dog_java.is_empty(),
        "clean hub and implementor in one selection must both translate.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("N7395"),
        "selected-subtype retention must not fire:\n{stderr}"
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
