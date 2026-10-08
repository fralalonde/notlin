//! An expression-bodied function with no declared return type must infer the
//! type from its body. Defaulting to `void` emitted `return new Ticket(x);`
//! inside a void method — javac rejects that ("unexpected return value"), and
//! every call site of the function then fails too ("'void' type not allowed
//! here"), which is how a single missing inference took down a whole file of a
//! migrated workspace.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture_root(case: &str) -> PathBuf {
    std::env::temp_dir().join(format!("notlin-rettys-{case}-{}", std::process::id()))
}

fn run_in_place(root: &Path, file: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args([
            "--allow-approximations",
            "--root",
            root.to_str().unwrap(),
            "--in-place",
        ])
        .arg(root.join(file))
        .output()
        .expect("run notlin")
}

#[test]
fn constructor_expression_body_infers_its_return_type() {
    let root = fixture_root("ctor");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Ticket.kt"),
        "package neutral.rettys\nclass Ticket(val code: String)\n",
    )
    .unwrap();
    fs::write(
        root.join("Factory.kt"),
        "package neutral.rettys\n\
         object Factory {\n\
         \x20   fun ticket(value: String = \"t\") = Ticket(value)\n\
         \x20   fun unwrapped() = ticket()\n\
         }\n",
    )
    .unwrap();

    let output = run_in_place(&root, "Factory.kt");
    assert!(
        output.status.success(),
        "notlin failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let java = fs::read_to_string(root.join("Factory.java")).unwrap_or_else(|e| {
        panic!(
            "Factory.java not written ({e}); stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(
        java.contains("Ticket ticket("),
        "expression body `= Ticket(value)` must infer `Ticket`, not `void`:\n{java}"
    );
    assert!(
        !java.contains("void ticket("),
        "inferred return type was lost:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
