use std::fs;
use std::path::Path;
use std::process::Command;

/// Kotlin subclass delegating a constructor argument to the parent
/// (`class Sub(...) : Parent("template")`) must forward it: the Java twin
/// of the parent has no zero-arg ctor, so an explicit ctor with
/// `super("template")` is required or nothing instantiates.
#[test]
fn superclass_ctor_args_forwarded() {
    let root = Path::new("tests/tmp_scratch_super");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("m.kt"),
        "package w\n\nabstract class ParentS(val text: String)\n\nclass SubS(\n    val id: Int,\n    val flag: Boolean\n) : ParentS(\"template\")\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let java = fs::read_to_string(root.join("SubS.java"))
        .or_else(|_| fs::read_to_string(root.join("Sub.java")))
        .unwrap_or_default();
    assert!(
        java.contains("super("),
        "super(...) forwarding missing:\n{java}\nstdout:\n{stdout}"
    );
    let _ = fs::remove_dir_all(root);
}
