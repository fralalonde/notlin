use std::fs;
use std::path::Path;
use std::process::Command;

/// `private val contexts: Map<A,B>` in a primary constructor, used with Kotlin
/// Map algebra inside the class (`this.contexts + other.contexts`). The
/// emitted code must NOT pass through as `this.getContexts().plus(...)` —
/// a collection receiver's `.plus` has no Java member form, so the
/// declaration must taint (stay Kotlin) instead.
#[test]
fn private_ctor_val_plus_taints() {
    let root = Path::new("tests/tmp_scratch_privateplus");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("p")).unwrap();
    fs::write(
        root.join("p").join("model.kt"),
        "package p\n\
class Hold private constructor(\n\
    private val contexts: Map<Int, String>\n\
) {\n\
    fun get(k: Int): String? = contexts[k]\n\
\n\
    operator fun plus(other: Hold): Hold =\n\
        Hold(this.contexts + other.contexts)\n\
}\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .args([root.join("p").join("model.kt").to_str().unwrap()])
        .output()
        .expect("run notlin");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let out_java = fs::read_to_string(root.join("p").join("Hold.java")).unwrap_or_default();
    assert!(
        !out_java.contains(".plus("),
        "Map algebra must taint the holder, got:\n{out_java}\nstdout:\n{stdout}"
    );
    let _ = fs::remove_dir_all(root);
}

/// Cross-file same-name property collision: another file's
/// `val contexts: List<X>` must not make `this.contexts + other.contexts`
/// inside a class with `private val contexts: Map<A,B>` emit `.plus(..)`.
/// Both Map and List collection operands must taint the caller.
#[test]
fn list_collision_plus_taints() {
    let root = Path::new("tests/tmp_scratch_listplus");
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root.join("p")).unwrap();
    std::fs::write(
        root.join("p").join("defs.kt"),
        "package p\ninterface WC\n\nclass Defs {\n    val contexts: List<WC> = emptyList()\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("p").join("model.kt"),
        "package p\n\nclass Hold private constructor(\n    private val contexts: Map<Int, WC>\n) {\n    fun get(k: Int): WC? = contexts[k]\n\n    operator fun plus(other: Hold): Hold =\n        Hold(this.contexts + other.contexts)\n}\n",
    )
    .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("p").join("model.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let out_java = std::fs::read_to_string(root.join("p").join("Hold.java")).unwrap_or_default();
    assert!(
        !out_java.contains(".plus("),
        "collection algebra must taint even on cross-file List collision, got:\n{out_java}\nstdout:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(root);
}
