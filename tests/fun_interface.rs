use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn fun_interface_becomes_java_sam_interface() {
    let root = Path::new("tests/tmp_scratch_fun_interface");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    let input = root.join("mapper.kt");
    fs::write(
        &input,
        "package neutral.sam\n\
         \n\
         fun interface Mapper<T, R> {\n\
         \x20   fun apply(value: T): R\n\
         }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(&input)
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let java = fs::read_to_string(root.join("Mapper.java")).unwrap_or_default();
    assert!(
        java.contains("@FunctionalInterface"),
        "missing SAM marker:\n{java}"
    );
    assert!(
        java.contains("public interface Mapper<T, R>"),
        "missing generic Java interface:\n{java}"
    );
    assert!(
        java.contains("R apply(T value);"),
        "missing SAM method:\n{java}"
    );
    assert!(
        !stderr.contains("class modifier not supported: fun"),
        "{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}
