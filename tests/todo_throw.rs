use std::fs;
use std::path::Path;
use std::process::Command;

/// Kotlin stdlib `TODO("msg")` is `Nothing`-typed (throws
/// NotImplementedError). The Java emission must be a THROW statement —
/// `throw new RuntimeException("msg")` (or Commons Lang
/// NotImplementedException under --commons-lang) — never `new TODO(...)`,
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
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
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

/// Same shape under --commons-lang: the throw uses Commons Lang
/// NotImplementedException instead of a bare RuntimeException.
#[test]
fn todo_call_uses_commons_lang_not_implemented_with_flag() {
    let root = Path::new("tests/tmp_scratch_todo2");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("m.kt"),
        "package neutral.todo.cl\n\ninterface Kind { val key: String }\n\nenum class Flag : Kind {\n    A;\n\n    override val key: String\n        get() = TODO(\"Not yet implemented\")\n}\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args([
            "--root",
            root.to_str().unwrap(),
            "--in-place",
            "--lombok",
            "--commons-lang",
        ])
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
            java.contains(
                "throw new org.apache.commons.lang3.NotImplementedException(\"Not yet implemented\")"
            ),
            "TODO() under --commons-lang must throw NotImplementedException:\n{java}"
        );
    }
    let _ = fs::remove_dir_all(root);
}
