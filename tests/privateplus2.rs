use std::fs;
use std::path::Path;
use std::process::Command;

/// `private val contexts: Map<A,B>` in a primary constructor, used with Kotlin
/// Map algebra inside the class (`this.contexts + other.contexts`). The
/// emitted code must NOT pass through as `this.getContexts().plus(...)` —
/// a collection receiver's `.plus` has no Java member form, so the
/// declaration must taint (stay Kotlin) instead.
#[test]
fn private_ctor_val_plus_taints2() {
    let root = Path::new("tests/tmp_scratch_privateplus");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("p")).unwrap();
    fs::write(
        root.join("p").join("model.kt"),
        "package p\n\
data class Thing(val q: Int)

class Hold private constructor(\n\
    private val contexts: Map<Int, Thing>
    var seen: Int = 0\n\
) {\n\
    fun get(k: Int): Thing? = contexts[k] as Thing?\n\
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
