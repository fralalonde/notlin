//! Kotlin smart casts over Java-style property getters.
//!
//! The same getter call is evaluated in both the `is` condition and the branch.
//! Java does not retain Kotlin's smart-cast narrowing, so the branch must cast
//! the property expression before accessing a subtype-only member.

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn smart_cast_property_branch_casts_to_the_checked_subtype() {
    let root = Path::new("tests/tmp_scratch_smart_cast_property");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Source.kt"),
        "package neutral.smartcast\n\ninterface Base\nclass Special(val key: String) : Base\ndata class Holder(val value: Base)\nfun read(holder: Holder): String {\n    if (holder.value is Special) {\n        return holder.value.key\n    }\n    return \"\"\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Source.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let java = fs::read_to_string(root.join("Source.java")).unwrap_or_default();

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        java.contains("((Special) holder.value()).getKey()"),
        "smart-cast branch must narrow the repeated Java getter call:\n{java}"
    );

    let _ = fs::remove_dir_all(root);
}
