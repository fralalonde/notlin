use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn array_of_any_uses_java_object_component_type() {
    let root = Path::new("tests/tmp_scratch_array_any");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("model.kt"),
        "package fixture.arrayany\n\nclass Holder(val values: Array<Any>)\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("model.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");

    let java = fs::read_to_string(root.join("Holder.java")).unwrap();
    assert!(java.contains("Object[] values"), "wrong Java type:\n{java}");
    assert!(!java.contains("Any[]"), "Kotlin type leaked:\n{java}");
    let _ = fs::remove_dir_all(root);
}
