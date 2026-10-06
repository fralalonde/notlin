use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn enum_selects_a_concrete_property_default_beside_an_abstract_contract() {
    let root = Path::new("tests/tmp_scratch_enum_default_bridge");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("types.kt"),
        r#"package neutral.defaults
interface ConcreteCategory {
    val category: String
        get() = "concrete"
}
interface AbstractCategory {
    val category: String
}
enum class Variant : ConcreteCategory, AbstractCategory { ONE }
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root)
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");

    let java = fs::read_to_string(root.join("Variant.java")).unwrap_or_default();
    assert!(
        java.contains("public String getCategory()")
            && java.contains("return ConcreteCategory.super.getCategory();"),
        "the enum must explicitly select Kotlin's concrete default:\n{java}\n{stderr}"
    );
    fs::remove_dir_all(root).unwrap();
}
