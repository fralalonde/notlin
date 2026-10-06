use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn array_to_list_uses_only_the_jdk() {
    let root = Path::new("tests/tmp_scratch_commons_lang");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(
        root.join("pkg").join("m.kt"),
        "package p\n\nfun valuesToList(values: Array<String>): List<String> = values.toList()\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("pkg").join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success());
    let java = fs::read_to_string(root.join("pkg").join("M.java")).unwrap();
    assert!(
        java.contains("java.util.Arrays.asList(values)"),
        "array.toList() should use Arrays.asList without an external dependency:\n{java}"
    );
    assert!(!java.contains("org.apache.commons"), "{java}");
    let _ = fs::remove_dir_all(root);
}
