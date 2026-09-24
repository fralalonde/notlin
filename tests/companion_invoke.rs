use std::fs;
use std::path::Path;
use std::process::Command;

/// Kotlin `sealed class V { companion object { @JvmStatic operator fun
/// invoke(v: String): Single = Single(...) } }` — a call `V("x")` is a
/// FACTORY call. Java `new V("x")` does not compile (sealed/abstract);
/// it must emit `V.invoke("x")`.
#[test]
fn companion_invoke_is_factory_call() {
    let root = Path::new("tests/tmp_scratchInvoke");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("m.kt"),
        "package q\n\nsealed class Q {\n    companion object {\n        @JvmStatic\n        operator fun invoke(v: String): QSub = QSub(v)\n    }\n}\n\ndata class QSub(val v: String) : Q()\n\nfun make(): Q = Q(\"hello\")\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let mut java = String::new();
    if let Ok(entries) = fs::read_dir(root) {
        for e in entries.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("java") {
                java.push_str(&fs::read_to_string(e.path()).unwrap_or_default());
            }
        }
    }
    assert!(
        !java.contains("new Q("),
        "sealed factory call must not emit new, got:\n{java}\nstdout:\n{stdout}"
    );
    assert!(
        java.contains("Q.invoke(\"hello\")"),
        "expected companion invoke routing, got:\n{java}\nstdout:\n{stdout}"
    );
    let _ = fs::remove_dir_all(root);
}
