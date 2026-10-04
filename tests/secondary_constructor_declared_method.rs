use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn secondary_constructor_delegation_accepts_declared_zero_arg_instance_method() {
    let root = Path::new("tests/tmp_scratch_secondary_ctor_declared_method");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Token.kt"),
        "package neutral.secondary\ninterface Source {\n    fun getInfo(): String\n}\nclass Token(val id: String) {\n    constructor(source: Source) : this(source.getInfo())\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");
    let java = fs::read_to_string(root.join("Token.java")).unwrap_or_default();
    assert!(
        java.contains("public Token(Source source)") && java.contains("this(source.getInfo());"),
        "a no-argument method declared by the receiver type is a Java-safe delegation argument:\n{java}\n{stderr}"
    );
    assert!(
        !root.join("Token.kt").exists(),
        "supported declared method call retained:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}
