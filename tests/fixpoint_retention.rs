//! Workspace-level retention fixpoint: retention is order-dependent, so the
//! CLI computes a least-fixpoint of the retained set over probe passes BEFORE
//! writing anything. A hub interface retains only when one of its Kotlin
//! subtypes is ITSELF retained (intrinsically tainted — declaration
//! annotation, enum-entries ABI, KClass...); a clean hub-and-implementor
//! family translates together. The naive top-down scope-loosening
//! (translating hub+implementor whenever both are selected) over-reached:
//! target builds failed on 5499 kotlinc errors because subtypes retained by
//! unrelated rules were suddenly implementing translated-away interfaces.

use std::fs;
use std::path::Path;
use std::process::Command;

/// Hub + clean implementor in the same selection: BOTH translate. The
/// implementor is a plain class with no intrinsic blockers, so the fixpoint
/// never seeds it and the hub's subtype rule lets go.
#[test]
fn clean_hub_and_implementor_translate_together() {
    let root = Path::new("tests/tmp_scratch_fixpoint_clean");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("hub.kt"),
        "package neutral.fixpoint\n\
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
        "clean hub and implementor must both translate.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("N7395"),
        "no retention diagnostic expected:\n{stderr}"
    );
    // Both were stripped from the .kt source.
    let hub_kt = fs::read_to_string(root.join("hub.kt")).unwrap_or_default();
    assert!(
        !hub_kt.contains("class Dog"),
        "translated declarations must be stripped in-place; remaining:\n{hub_kt}"
    );
    let _ = fs::remove_dir_all(root);
}

/// Hub + a Java-representable Kotlin annotation: the annotation declaration,
/// implementor, and hub all translate in one fixpoint. A marker annotation has
/// a direct Java `@interface` representation and is not an intrinsic blocker.
#[test]
fn marker_annotation_implementor_translates_with_hub() {
    let root = Path::new("tests/tmp_scratch_fixpoint_tainted");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("mine.kt"),
        "package neutral.fixpoint2\n\
         \n\
         annotation class KotlinOnly\n",
    )
    .unwrap();
    fs::write(
        root.join("hub.kt"),
        "package neutral.fixpoint2\n\
         \n\
         interface Speaker {\n\
         \x20   fun speak(): String\n\
         }\n\
         \n\
         @KotlinOnly\n\
         class Parrot : Speaker {\n\
         \x20   override fun speak(): String = \"squawk\"\n\
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
    let parrot_java = fs::read_to_string(root.join("Parrot.java")).unwrap_or_default();
    let annotation_java = fs::read_to_string(root.join("KotlinOnly.java")).unwrap_or_default();
    assert!(
        !speaker_java.is_empty()
            && !parrot_java.is_empty()
            && annotation_java.contains("@interface KotlinOnly"),
        "Java-representable annotation family must translate.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("N7395"),
        "no retention diagnostic expected:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}
