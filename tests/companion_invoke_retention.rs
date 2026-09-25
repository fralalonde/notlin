use std::fs;
use std::path::Path;
use std::process::Command;

/// Kotlin's `Type(...)` syntax may resolve to a companion `operator fun
/// invoke`. Java exposes the type's constructors instead, so a translated
/// class cannot preserve that ABI for residual Kotlin callers.
#[test]
fn companion_invoke_type_stays_kotlin_for_external_kotlin_callers() {
    let root = Path::new("tests/tmp_scratch_companion_invoke");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Factory.kt"),
        "package neutral.companion\nclass Factory {\n    class Part(val value: String) {\n        constructor() : this(\"fallback\")\n    }\n    companion object {\n        operator fun invoke(value: String): Factory = Factory()\n    }\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("Use.kt"),
        "package neutral.companion\nfun make(): Factory = Factory(\"value\")\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Factory.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        root.join("Factory.kt").exists(),
        "companion-invoke class was translated despite a residual Kotlin caller:\n{stderr}"
    );
    assert!(
        stderr.contains("companion operator `invoke`"),
        "missing ABI retention diagnostic:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}
