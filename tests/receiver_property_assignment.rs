//! Assignment to a property OWNED BY THE RECEIVER's class (`other.parent = v`
//! inside a method of some other class) must lower to the receiver's setter
//! ABI. The same-class registry (`class_props`) only knows the enclosing
//! class's own properties; a receiver-owned property used to fall through to
//! the read path and emit the illegal `other.getParent() = v`. Resolution is
//! allowed only through proven receiver facts (parameters and locals with an
//! established type, resolved in the declaring file's import context).

use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::fs;
use std::path::{Path, PathBuf};

fn workspace(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("notlin-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn transpile_in_workspace(root: &Path, name: &str, source: &str) -> (Vec<(String, String)>, usize) {
    let cli = Cli::parse_from(vec![
        "notlin",
        "--root",
        root.to_string_lossy().as_ref(),
        name,
    ]);
    let index = SourceIndex::discover(root).expect("index");
    let owned = root.to_path_buf();
    let (files, errors, _warnings, _coverage) = transpiler::transpile_with_workspace(
        source,
        &PathBuf::from(name),
        &cli,
        Some(&index),
        std::slice::from_ref(&owned),
    );
    (files, errors)
}

#[test]
fn receiver_var_property_write_lowers_to_the_setter() {
    let root = workspace("receiver-write-var");
    let source = "package neutral.receiverwrite\n\
class Hub {\n    val count: Int = 0\n    fun link(other: Thing, h: Hub) {\n        other.parent = h\n    }\n\
}\n\nclass Thing {\n    var parent: Hub? = null\n}\n";
    fs::write(root.join("RW.kt"), source).unwrap();
    let (files, errors) = transpile_in_workspace(&root, "RW.kt", source);
    assert_eq!(errors, 0, "receiver-owned property write must translate");
    let hub = files
        .iter()
        .find(|(name, _)| name == "Hub.java")
        .map(|(_, text)| text.clone())
        .expect("Hub.java");
    assert!(
        hub.contains("other.setParent(h);"),
        "expected the receiver's setter ABI, got:\n{hub}"
    );
    assert!(
        !hub.contains("getParent() ="),
        "the illegal getter-assignment shape must not be emitted:\n{hub}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn receiver_lateinit_property_write_lowers_to_the_setter() {
    let root = workspace("receiver-write-lateinit");
    let source = "package neutral.receiverwritelateinit\n\
class Hub2 {\n    fun link(other: Thing2, h: Hub2) {\n        other.parent = h\n    }\n\
}\n\nclass Thing2 {\n    lateinit var parent: Hub2\n}\n";
    fs::write(root.join("RWL.kt"), source).unwrap();
    let (files, errors) = transpile_in_workspace(&root, "RWL.kt", source);
    assert_eq!(errors, 0, "lateinit receiver property write must translate");
    let hub = files
        .iter()
        .find(|(name, _)| name == "Hub2.java")
        .map(|(_, text)| text.clone())
        .expect("Hub2.java");
    assert!(
        hub.contains("other.setParent(h);"),
        "expected the receiver's setter ABI, got:\n{hub}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn receiver_property_local_variable_type_is_proven_too() {
    let root = workspace("receiver-write-local");
    let source = "package neutral.receiverwritelocal\n\
class Hub3 {\n    fun link(h: Hub3, other: Thing3) {\n        other.parent = h\n    }\n\
}\n\nclass Thing3 {\n    var parent: Hub3? = null\n}\n";
    fs::write(root.join("RVL.kt"), source).unwrap();
    let (files, errors) = transpile_in_workspace(&root, "RVL.kt", source);
    assert_eq!(errors, 0);
    let hub = files
        .iter()
        .find(|(name, _)| name == "Hub3.java")
        .map(|(_, text)| text.clone())
        .expect("Hub3.java");
    assert!(hub.contains("other.setParent(h);"), "got:\n{hub}");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unresolvable_receiver_keeps_the_existing_conservative_behaviour() {
    // A receiver whose type is not proven (here: a chained member of unknown
    // type) must NOT gain a guessed setter rewrite; today that call site is
    // retained/tainted rather than emitting `x.getParent() = v`.
    let root = workspace("receiver-write-unknown");
    let source = "package neutral.receiverwriteunknown\n\
class Hub4 {\n    var other: Any? = null\n    fun link(v: Hub4) {\n        getHolder().target = v\n    }\n\
    fun getHolder(): Hub4 = this\n}\n";
    fs::write(root.join("RWU.kt"), source).unwrap();
    let (files, _errors) = transpile_in_workspace(&root, "RWU.kt", source);
    let hub = files
        .iter()
        .find(|(name, _)| name == "Hub4.java")
        .map(|(_, text)| text.clone())
        .unwrap_or_default();
    assert!(
        !hub.contains("setTarget("),
        "no setter may be invented for an unproven receiver property:\n{hub}"
    );
    fs::remove_dir_all(root).unwrap();
}
