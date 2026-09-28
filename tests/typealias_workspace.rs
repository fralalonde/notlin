use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn workspace_typealias_is_replaced_with_its_java_type() {
    let root = Path::new("tests/tmp_scratch_typealias_workspace");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("model.kt"),
        "package neutral.aliases\n\nimport java.util.UUID\n\ntypealias Identifier = UUID\n",
    )
    .unwrap();
    fs::write(
        root.join("consumer.kt"),
        "package neutral.consumer\n\nimport neutral.aliases.Identifier\n\ndata class Entry(val id: Identifier)\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");
    let java = fs::read_to_string(root.join("Entry.java")).expect("translated consumer");
    assert!(java.contains("UUID"), "alias was not lowered:\n{java}");
    assert!(
        !java.contains("Identifier"),
        "Java references Kotlin-only typealias:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
