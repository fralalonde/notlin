use std::fs;
use std::path::Path;
use std::process::Command;

/// `x.name` where x is a bare property of enum type must emit `name()`
/// even when OTHER files declare members with the same name and
/// different (non-enum) types — cross-file same-name shadowing must not
/// silence the enum access. The resolver must prefer the candidate
/// whose declared type is an indexed enum instead of taking the first
/// declaration scan hit.
#[test]
fn bare_enum_property_survives_cross_file_shadowing() {
    let root = Path::new("tests/tmp_scratch_shadow");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("pkg")).unwrap();

    // Poisoning neighbor: member named `type` with a non-enum class type,
    // declared EARLIER in a different file of the same workspace.
    fs::write(
        root.join("pkg").join("other.kt"),
        "package p\n\nclass ReferenceDto\n\nclass Neighbor {\n    val type: ReferenceDto = ReferenceDto()\n}\n",
    )
    .unwrap();
    // Emitting file: `type` is an enum-typed member; body reads
    // `type.name`.
    fs::write(
        root.join("pkg").join("m.kt"),
        "package p\n\nenum class Tag { RED, BLUE }\n\nclass Holder(val type: Tag) {\n    override fun toString(): String {\n        return type.name\n    }\n}\n",
    )
    .unwrap();

    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("pkg").join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let java = fs::read_to_string(root.join("pkg").join("Holder.java")).unwrap();
    assert!(
        java.contains(".name()"),
        "enum-typed bare property `.name` must read name():\n{java}"
    );
    assert!(
        !java.contains(".getName()"),
        "cross-file shadowed property must not lower to getName():\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
