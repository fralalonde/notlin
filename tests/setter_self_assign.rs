use std::fs;
use std::path::Path;
use std::process::Command;

/// A Kotlin setter-like function that assigns its own class property
/// (`fun setFoo(v: Other) { this.foo = V(v) }`) must emit the FIELD write
/// `this.foo = ...`, NOT `this.setFoo(...)` — the latter is infinite
/// setter recursion and changes the effective setter parameter type.
#[test]
fn own_setter_body_keeps_field_write() {
    let root = Path::new("tests/tmp_scratch_setter");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("m.kt"),
        "package s\n\nsealed class V {\n    companion object {\n        @JvmStatic\n        operator fun invoke(v: String): VSub = VSub(v)\n    }\n}\n\ndata class VSub(val s: String) : V()\n\nclass Crit {\n    var foo: V? = null\n\n    fun setFoo(f: Other) {\n        this.foo = V(f.val_)\n    }\n}\n\ndata class Other(val val_: String)\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let java = fs::read_to_string(root.join("Crit.java")).unwrap_or_default();
    assert!(
        !java.contains("this.setFoo(") && !java.contains("this.foo()."),
        "own setter must write the field, got:\n{java}\nstdout:\n{stdout}"
    );
    let _ = fs::remove_dir_all(root);
}
