use std::fs;
use std::path::Path;
use std::process::Command;

/// Kotlin stdlib `TODO("msg")` is `Nothing`-typed (throws
/// NotImplementedError). The Java emission must be a THROW statement —
/// `throw new RuntimeException("msg")` — never `new TODO(...)`,
/// which references a class that does not exist.
#[test]
fn todo_call_throws_runtime_exception_in_getter_body() {
    let root = Path::new("tests/tmp_scratch_todo");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("m.kt"),
        "package neutral.todo\n\ninterface Kind { val key: String }\n\nenum class Flag : Kind {\n    A;\n\n    override val key: String\n        get() = TODO(\"Not yet implemented\")\n}\n",
    )
    .unwrap();
    let _out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let java = fs::read_to_string(root.join("Flag.java")).unwrap_or_default();
    assert!(
        !java.contains("new TODO("),
        "Kotlin TODO() must not emit a constructor call to a nonexistent class:\n{java}"
    );
    if !java.is_empty() {
        assert!(
            java.contains("throw new RuntimeException(\"Not yet implemented\")"),
            "TODO() must lower to a RuntimeException throw:\n{java}"
        );
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn todo_expression_body_is_emitted_as_a_bare_throw() {
    let root = Path::new("tests/tmp_scratch_todo_expression");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("m.kt"),
        "package neutral.todo.expression\n\ninterface Kind { fun key(): String = TODO() }\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");
    let java = fs::read_to_string(root.join("Kind.java")).unwrap();
    assert!(
        java.contains("throw new RuntimeException();"),
        "TODO() expression body must emit a bare throw:\n{java}"
    );
    assert!(!java.contains("return throw"), "invalid Java:\n{java}");
    let _ = fs::remove_dir_all(root);
}
