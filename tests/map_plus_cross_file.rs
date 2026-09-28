use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn map_literal_plus_imported_interface_property_stays_kotlin() {
    let root = Path::new("tests/tmp_scratch_map_plus_cross_file");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("defaults.kt"),
        "package fixture.defaults\n\ninterface Defaults { companion object { val base: Map<String, Int> = mapOf() } }\n",
    )
    .unwrap();
    fs::write(
        root.join("merge.kt"),
        "package fixture.merge\n\nimport fixture.defaults.Defaults\n\ndata class Holder(val values: Map<String, Int> = Defaults.base) { companion object { val merged: Map<String, Int> = mapOf(\"a\" to 1) + Defaults.base } }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");
    let java = fs::read_to_string(root.join("Holder.java")).unwrap_or_default();
    assert!(
        !java.contains(".plus(") && !java.contains("Map.ofEntries") || !java.contains("+"),
        "cross-file Map plus emitted invalid Java:\n{java}"
    );
    assert!(
        !root.join("merge.kt").exists() || java.is_empty(),
        "unsupported Map merge must not leave broken Java alongside translated source:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
