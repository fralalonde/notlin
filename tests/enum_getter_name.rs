use std::fs;
use std::path::Path;
use std::process::Command;

/// `x.name` where x's declared type is an indexed ENUM stays the JDK
/// accessor `name()` even when the receiver is a getter (`getType()`)
/// or a primary-ctor property of the emitted class.
#[test]
fn ctor_typed_enum_properties_use_name() {
    let root = Path::new("tests/tmp_scratch_enumgetter");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("m.kt"),
        "package e\n\nenum class Tag { RED, BLUE }\n\nclass Holder(val type: Tag) {\n    override fun toString(): String {\n        return type.name\n    }\n}\n",
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let java = fs::read_to_string(root.join("Holder.java")).unwrap();
    assert!(
        java.contains(".name()"),
        "enum property `.name` must read name():\n{java}"
    );
    assert!(
        !java.contains("getName()"),
        "enum getter must not be used:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
