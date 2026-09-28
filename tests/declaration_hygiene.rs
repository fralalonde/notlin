//! Synthetic cover for two hygiene defects that produced hard javac failures
//! in a migrated workspace:
//!
//! 1. A comment inside a class/object/enum body was dispatched as an
//!    unsupported *member*, so the whole declaration was marked untranslatable
//!    (`class member not supported: line_comment`) and stayed Kotlin.
//! 2. A `.kt` file that only partly translates kept its imports in the
//!    generated `.java`. An import naming a Kotlin-only symbol (a dependency's
//!    top-level function, e.g. `xyz.jacksonObjectMapper`) then failed the whole
//!    compilation unit with "cannot find symbol: class …" even though the
//!    generated code never used it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture_root(case: &str) -> PathBuf {
    std::env::temp_dir().join(format!("notlin-hygiene-{case}-{}", std::process::id()))
}

fn run_in_place(root: &Path, file: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join(file))
        .output()
        .expect("run notlin")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "notlin failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn comments_in_class_body_do_not_taint_the_declaration() {
    let root = fixture_root("comments");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Widget.kt"),
        "package neutral.hygiene.comments\n\
         class Widget {\n\
         \x20   val name: String = \"w\"\n\
         \n\
         \x20   // line comment inside the body\n\
         \x20   fun label(): String = this.name\n\
         \n\
         \x20   /* block comment inside the body */\n\
         \x20   fun tag(): String = this.name + \"-tag\"\n\
         }\n",
    )
    .unwrap();

    let output = run_in_place(&root, "Widget.kt");
    assert_success(&output);

    let text = combined(&output);
    assert!(
        !text.contains("member not supported"),
        "a comment was dispatched as an unsupported member:\n{text}"
    );
    let java = fs::read_to_string(root.join("Widget.java"))
        .unwrap_or_else(|e| panic!("Widget.java not written ({e}); diagnostics:\n{text}"));
    assert!(java.contains("class Widget"), "unexpected output:\n{java}");
    assert!(
        java.contains("label(") && java.contains("tag("),
        "both methods must survive a commented body:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn kotlin_only_import_is_not_carried_into_generated_java() {
    let root = fixture_root("imports");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    // `com.vendor.kotlinhelper.makeThing` has no Java counterpart — the shape
    // of a Kotlin-only top-level function from a dependency. Nothing in the
    // generated Java mentions it, so it must not be emitted.
    fs::write(
        root.join("Plain.kt"),
        "package neutral.hygiene.imports\n\
         import com.vendor.kotlinhelper.makeThing\n\
         import java.util.ArrayList\n\
         class Plain {\n\
         \x20   val items: ArrayList<String> = ArrayList()\n\
         }\n",
    )
    .unwrap();

    let output = run_in_place(&root, "Plain.kt");
    assert_success(&output);

    let text = combined(&output);
    let java = fs::read_to_string(root.join("Plain.java"))
        .unwrap_or_else(|e| panic!("Plain.java not written ({e}); diagnostics:\n{text}"));
    assert!(
        !java.contains("makeThing"),
        "unresolvable Kotlin-only import leaked into generated Java:\n{java}"
    );
    assert!(
        java.contains("import java.util.*;"),
        "stdlib collections import must stay:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
