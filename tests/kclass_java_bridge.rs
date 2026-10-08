use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn kclass_java_bridge_erases_when_parameter_becomes_class() {
    let root = Path::new("tests/tmp_scratch_kclass");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(
        root.join("pkg").join("m.kt"),
        "package p\n\nimport kotlin.reflect.KClass\n\nfun <T: Any> runtimeClass(type: KClass<T>): Class<T> = type.java\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args([
            "--root",
            root.to_str().unwrap(),
            "--in-place",
            "--allow-approximations",
        ])
        .arg(root.join("pkg").join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success());
    let java = fs::read_to_string(root.join("pkg").join("M.java")).unwrap();
    assert!(
        java.contains("Class<T> type"),
        "KClass parameter should lower to Class:\n{java}"
    );
    assert!(
        java.contains("return type;"),
        "KClass `.java` bridge should erase:\n{java}"
    );
    assert!(
        !java.contains("getClass()"),
        "must not call Class.getClass():\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn class_literal_uses_explicit_import_when_a_local_type_has_the_same_name() {
    let root = Path::new("tests/tmp_scratch_class_literal_import");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(
        root.join("pkg").join("m.kt"),
        "package p\n\nimport javax.measure.quantity.Volume\n\nclass Holder {\n    fun quantityType(): Class<*> = Volume::class.java\n}\n\nclass Volume\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("pkg").join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success());
    let java = fs::read_to_string(root.join("pkg").join("Holder.java")).unwrap();
    assert!(
        java.contains("return javax.measure.quantity.Volume.class;"),
        "an explicit Kotlin import must keep owning the class literal despite the local Java type name:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
