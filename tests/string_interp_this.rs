use std::fs;
use std::path::Path;
use std::process::Command;

/// `$name` interpolation inside a string template must splice the implicit
/// `this` property accessor (`this.getName()`), not a bare unqualified
/// symbol — on interfaces javac rejects the bare field form.
#[test]
fn interpolated_property_reads_through_this() {
    let root = Path::new("tests/tmp_scratch_interp");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("m.kt"),
        "package p\n\ninterface INamed {\n    val name: String\n\n    val alias: String\n        get() = \"onomatic/$name\"\n}\n",
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let java = fs::read_to_string(root.join("INamed.java")).unwrap();
    assert!(
        java.contains("\"onomatic/\" + this.getName()"),
        "interpolated `val name` must read through this.getName():\n{java}"
    );
    assert!(
        !java.contains("\"onomatic/\" + name"),
        "bare interpolated property must not be emitted:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
