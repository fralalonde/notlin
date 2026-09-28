//! Workspace-level retention fixpoint: retention is order-dependent, so the
//! CLI computes a least-fixpoint of the retained set over probe passes BEFORE
//! writing anything. A hub interface retains only when one of its Kotlin
//! subtypes is ITSELF retained (intrinsically tainted — declaration
//! annotation, enum-entries ABI, KClass...); a clean hub-and-implementor
//! family translates together. The naive top-down scope-loosening
//! (translating hub+implementor whenever both are selected) over-reached:
//! target builds failed on 5499 kotlinc errors because subtypes retained by
//! unrelated rules were suddenly implementing translated-away interfaces.

use std::collections::HashSet;
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
        !stderr.contains("remain Kotlin"),
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

#[test]
fn retained_enum_keeps_its_kotlin_supertype_in_all_roots_mode() {
    let root = Path::new("tests/tmp_scratch_fixpoint_retained_enum");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("types.kt"),
        "package neutral.fixpoint3\n\
         \n\
         interface Speaker {\n\
         \x20   fun speak(): String\n\
         }\n\
         \n\
         enum class Mood : Speaker {\n\
         \x20   CALM;\n\
         \x20   override fun speak(): String = \"calm\"\n\
         }\n",
    )
    .unwrap();
    fs::write(
        root.join("usage.kt"),
        "package neutral.fixpoint3\nfun currentMood(): Mood = Mood.CALM\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");
    let speaker_java = fs::read_to_string(root.join("Speaker.java")).unwrap_or_default();
    assert!(
        speaker_java.is_empty(),
        "interface with an intrinsically retained Kotlin subtype must stay Kotlin; got:\n{speaker_java}\n{stderr}"
    );
    let types = fs::read_to_string(root.join("types.kt")).unwrap_or_default();
    assert!(types.contains("interface Speaker") && types.contains("enum class Mood"));
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
        !stderr.contains("remain Kotlin"),
        "no retention diagnostic expected:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn retained_delta_rechecks_only_reverse_dependency_files() {
    let root = Path::new("tests/tmp_scratch_fixpoint_dependencies");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    let hub = root.join("hub.kt");
    let retained = root.join("retained.kt");
    let child = root.join("child.kt");
    let unrelated = root.join("unrelated.kt");
    fs::write(&hub, "interface Hub\n").unwrap();
    fs::write(&retained, "interface Retained : Hub\n").unwrap();
    fs::write(&child, "class Child : Retained\n").unwrap();
    fs::write(&unrelated, "class Unrelated\n").unwrap();

    let index = notlin::workspace::SourceIndex::discover(root).unwrap();
    let delta = HashSet::from(["Retained".to_string()]);
    assert!(index.retained_delta_can_affect(&hub, &delta));
    assert!(index.retained_delta_can_affect(&retained, &delta));
    assert!(index.retained_delta_can_affect(&child, &delta));
    assert!(!index.retained_delta_can_affect(&unrelated, &delta));
    let _ = fs::remove_dir_all(root);
}
