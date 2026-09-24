use std::fs;
use std::path::Path;
use std::process::Command;

/// `x.name` where x's type resolves to an indexed ENUM declaration must
/// emit the JDK accessor `name()`, not `getName()` — the enum's public
/// accessor is `name()`.
#[test]
fn enum_typed_receiver_names_the_enum() {
    let root = Path::new("tests/tmp_scratch_enumname");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("m.kt"),
        "package e\n\nenum class Mode { A, B }\n\nclass Holder {\n    val tag: Mode = Mode.A\n\n    fun label(): String {\n        return tag.name\n    }\n}\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let java = fs::read_to_string(root.join("Holder.java")).unwrap_or_default();
    assert!(
        java.contains(".name()"),
        "enum `.name` must read via name():\n{java}\nstdout:\n{stdout}"
    );
    assert!(
        !java.contains("getName()"),
        "enum getter must not be used:\n{java}\nstdout:\n{stdout}"
    );
    let _ = fs::remove_dir_all(root);
}
