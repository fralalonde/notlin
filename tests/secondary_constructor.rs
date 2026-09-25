use std::fs;
use std::path::Path;
use std::process::Command;

/// A bodyless Kotlin secondary constructor that delegates to the primary
/// constructor has a direct Java representation: an overload using `this`.
#[test]
fn bodyless_secondary_constructor_delegates_to_primary() {
    let root = Path::new("tests/tmp_scratch_secondary_ctor");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Token.kt"),
        "package neutral.secondary\nclass Token(val id: String) {\n    constructor() : this(\"fallback\")\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Token.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let java = fs::read_to_string(root.join("Token.java")).unwrap_or_default();

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        !root.join("Token.kt").exists(),
        "supported secondary constructor retained:\n{stderr}"
    );
    assert!(
        java.contains("public Token()") && java.contains("this(\"fallback\");"),
        "secondary constructor must emit a Java this-delegating overload:\n{java}"
    );
    assert!(
        !stderr.contains("secondary constructors not yet supported"),
        "supported constructor emitted a stale diagnostic:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

/// A direct `this(...)` delegation is not enough by itself: complex argument
/// expressions need their own validated Java lowering. Keep this class Kotlin
/// rather than generating a plausible-but-invalid Java constructor.
#[test]
fn secondary_constructor_with_complex_delegation_argument_stays_kotlin() {
    let root = Path::new("tests/tmp_scratch_secondary_ctor_complex");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Token.kt"),
        "package neutral.secondary\nclass Token(val id: String) {\n    constructor(parts: Array<String>) : this(parts.joinToString())\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Token.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        root.join("Token.kt").exists(),
        "complex delegated argument was emitted as Java:\n{stderr}"
    );
    assert!(
        stderr.contains("bodyless direct `this(...)` delegation"),
        "missing conservative secondary-constructor diagnostic:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}
