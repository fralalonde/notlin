//! Resolved external static fields must retain their JVM owner in Java.
use std::{fs, process::Command};

#[test]
fn aliased_static_field_receivers_emit_the_canonical_owner() {
    let root = std::env::temp_dir().join(format!(
        "notlin-static-field-emission-{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Exercise.kt"), "package neutral.fields\nimport java.math.BigDecimal as Decimal\nfun magnitude(): String = Decimal.ZERO.abs().toPlainString()\n").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root)
        .args([
            "--jvm-facts=off",
            "--annotations",
            "none",
            "-o",
            "out",
            "Exercise.kt",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let java = fs::read_to_string(root.join("out/Exercise.java")).unwrap();
    fs::remove_dir_all(root).unwrap();
    assert!(
        java.contains("java.math.BigDecimal.ZERO.abs().toPlainString()"),
        "canonical field owner missing:\n{java}"
    );
}
