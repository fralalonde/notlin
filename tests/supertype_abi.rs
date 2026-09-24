use std::fs;
use std::path::Path;
use std::process::Command;

/// A class implementing a RETAINED Kotlin interface whose abstract property
/// `items: List<Item>` conflicts with the class's `List<ItemImpl>` — Java
/// return types must match exactly. The class must stay Kotlin (taint) rather
/// than emit a Java twin javac rejects.
#[test]
fn retained_supertype_abi_mismatch_taints() {
    let root = Path::new("tests/tmp_scratch_abi");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("iface.kt"),
        "package a\n\n@Deprecated(\"keep\")\ninterface Item { val n: String }\n\n@Deprecated(\"keep\")\ninterface Box {\n    val items: List<Item>\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("impl.kt"),
        "package a\n\nclass ItemImpl(override val n: String) : Item\n\nclass Holder(\n    override val items: List<ItemImpl>\n) : Box\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("impl.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let out_java = fs::read_to_string(root.join("Holder.java")).unwrap_or_default();
    assert!(
        out_java.is_empty(),
        "ABI-mismatched class must stay Kotlin, got:\n{out_java}"
    );
    let _ = fs::remove_dir_all(root);
}

/// Transitive mismatch: Holder implements Box, Box extends DeepBox, and the
/// RETAINED DeepBox declares `items: List<Item>` while Holder has
/// `List<ItemImpl>` — the conflict surfaces two hops away and must taint.
#[test]
fn transitive_retained_supertype_abi_mismatch_taints() {
    let root = Path::new("tests/tmp_scratch_abi2");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("iface.kt"),
        "package b\n\n@Deprecated(\"keep\")\ninterface Item { val n: String }\n\n@Deprecated(\"keep\")\ninterface DeepBox {\n    val items: List<Item>\n}\n\ninterface Box : DeepBox\n",
    )
    .unwrap();
    fs::write(
        root.join("impl.kt"),
        "package b\n\nclass ItemImpl(override val n: String) : Item\n\nclass Holder(\n    override val items: List<ItemImpl>\n) : Box\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("impl.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let out_java = fs::read_to_string(root.join("Holder.java")).unwrap_or_default();
    assert!(
        out_java.is_empty(),
        "transitive ABI mismatch must keep the class Kotlin, got:\n{out_java}"
    );
    let _ = fs::remove_dir_all(root);
}
