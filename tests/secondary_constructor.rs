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

#[test]
fn bodyless_secondary_constructor_delegates_to_superclass() {
    let root = Path::new("tests/tmp_scratch_secondary_ctor_super");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Child.kt"),
        "package neutral.secondary\n\
         open class Base(val id: String)\n\
         class Child : Base {\n\
         \x20   constructor() : super(\"fallback\")\n\
         }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Child.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let java = fs::read_to_string(root.join("Child.java")).unwrap_or_default();

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        !root.join("Child.kt").exists(),
        "supported super-delegating constructor retained:\n{stderr}"
    );
    assert!(
        java.contains("public Child()") && java.contains("super(\"fallback\");"),
        "secondary constructor must emit a Java super-delegating overload:\n{java}"
    );
    assert!(
        !stderr.contains("bodyless direct `this(...)` delegation"),
        "supported constructor emitted a stale diagnostic:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn secondary_constructor_with_simple_body_translates_after_delegation() {
    let root = Path::new("tests/tmp_scratch_secondary_ctor_body");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Token.kt"),
        "package neutral.secondary\nclass Token(val id: String) {\n    var label: String = \"\"\n    constructor(incoming: String) : this(\"fallback\") {\n        this.label = incoming\n    }\n}\n",
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
        "simple constructor body retained:\n{stderr}"
    );
    assert!(
        java.contains("public Token(String incoming)")
            && java.contains("this(\"fallback\");")
            && java.contains("this.setLabel(incoming);"),
        "secondary constructor body must follow its Java delegation:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

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
        stderr.contains("direct `this(...)` or `super(...)` delegation with an empty or simple assignment-only body"),
        "missing conservative secondary-constructor diagnostic:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}
