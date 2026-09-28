use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn kotlin_object_implements_translated_interface() {
    let root = Path::new("tests/tmp_scratch_object_interface");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("model.kt"),
        "package fixture.objectinterface\n\ninterface Contract\n\nobject Singleton : Contract\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("model.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");

    let java = fs::read_to_string(root.join("Singleton.java")).unwrap();
    assert!(
        java.contains("final class Singleton implements Contract"),
        "Kotlin object must implement an interface, not extend it:\n{java}"
    );
    assert!(!java.contains("extends Contract"), "invalid Java:\n{java}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn object_interface_property_is_an_instance_getter() {
    let root = Path::new("tests/tmp_scratch_object_interface_property");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("model.kt"),
        "package fixture.objectinterface\n\ninterface Contract { val type: String }\n\nobject Singleton : Contract {\n    override val type: String\n        get() = \"singleton\"\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("model.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");

    let java = fs::read_to_string(root.join("Singleton.java")).unwrap();
    assert!(java.contains("public String getType()"), "{java}");
    assert!(!java.contains("static String getType()"), "{java}");
    let _ = fs::remove_dir_all(root);
}
