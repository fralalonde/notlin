use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn commons_lang_array_to_list_requires_both_opt_ins() {
    let root = Path::new("tests/tmp_scratch_commons_lang");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(
        root.join("pkg").join("m.kt"),
        "package p\n\nfun valuesToList(values: Array<String>): List<String> = values.toList()\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args([
            "--root",
            root.to_str().unwrap(),
            "--in-place",
            "--lombok",
            "--commons-lang",
        ])
        .arg(root.join("pkg").join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success());
    let java = fs::read_to_string(root.join("pkg").join("M.java")).unwrap();
    assert!(
        java.contains("org.apache.commons.lang3.ArrayUtils.toList(values)"),
        "commons-lang opt-in should lower array.toList():\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
