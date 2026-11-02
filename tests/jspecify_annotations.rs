use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "notlin-jspecify-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn translate(root: &Path, source: &str, lombok: bool) -> Vec<PathBuf> {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("Model.kt"), source).unwrap();
    let mut command = Command::new(
        std::env::var_os("NOTLIN_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into()),
    );
    command.current_dir(root).arg("--in-place");
    if lombok {
        command.arg("--lombok");
    }
    let result = command.arg(".").output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !root.join("Model.kt").exists(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fs::read_dir(root)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().is_some_and(|ext| ext == "java")).then_some(path)
        })
        .collect()
}

const RETURNS: &str = "package sample\nimport org.jspecify.annotations.NonNull\ninterface Base {fun getText():String\nfun getId():java.util.UUID\nfun getValues():Array<String>}\ninterface Child:Base {override fun getText():String\noverride fun getId():java.util.UUID\noverride fun getValues():Array<String>}\n";
const COLLISION: &str = "package sample\nclass NonNull\ninterface Base {fun getToken():NonNull}\ninterface Child:Base {override fun getToken():NonNull}\n";
const LITERAL: &str = "package sample\ninterface Base {fun getText():String}\ninterface Child:Base {override fun getText():String=\"@org.jspecify.annotations.NonNull\"}\n";
const LOMBOK: &str = "package sample\ninterface Base {val text:String?}\ndata class Child(override val text:String=\"value\"):Base\n";
const NULLABLE: &str = "package sample\nclass NullableTypes(val id:java.util.UUID?, val values:Array<String>?) {fun echo(input:java.util.UUID?):java.util.UUID?=input}\n";

#[test]
fn nullable_qualified_types_and_arrays_annotate_the_outer_type() {
    let scratch = Scratch::new();
    translate(&scratch.0, NULLABLE, false);
    let java = fs::read_to_string(scratch.0.join("NullableTypes.java")).unwrap();
    assert!(!java.contains("@Nullable java.util"), "{java}");
    assert!(!java.contains("@Nullable String[]"), "{java}");
    assert!(java.contains("java.util.@Nullable UUID"), "{java}");
    assert!(java.contains("String @Nullable []"), "{java}");
}

#[test]
fn default_jspecify_annotations_use_imports_and_legal_type_use_positions() {
    let scratch = Scratch::new();
    translate(&scratch.0, RETURNS, false);
    let java = fs::read_to_string(scratch.0.join("Child.java")).unwrap();
    assert_eq!(
        java.matches("import org.jspecify.annotations.NonNull;")
            .count(),
        1,
        "{java}"
    );
    assert!(!java.contains("org.jetbrains.annotations"), "{java}");
    assert!(java.contains("@NonNull String getText()"), "{java}");
    assert!(java.contains("java.util.@NonNull UUID getId()"), "{java}");
    assert!(java.contains("String @NonNull [] getValues()"), "{java}");
}

#[test]
fn annotation_import_does_not_shadow_a_source_type_or_rewrite_a_literal() {
    let scratch = Scratch::new();
    let collision = scratch.0.join("collision");
    translate(&collision, COLLISION, false);
    let java = fs::read_to_string(collision.join("Child.java")).unwrap();
    assert!(
        !java.contains("import org.jspecify.annotations.NonNull;"),
        "{java}"
    );
    assert!(
        java.contains("@org.jspecify.annotations.NonNull NonNull"),
        "{java}"
    );
    let literal = scratch.0.join("literal");
    translate(&literal, LITERAL, false);
    let java = fs::read_to_string(literal.join("Child.java")).unwrap();
    assert!(
        java.contains("\"@org.jspecify.annotations.NonNull\""),
        "{java}"
    );
    assert_eq!(
        java.matches("import org.jspecify.annotations.NonNull;")
            .count(),
        1,
        "{java}"
    );
}

#[test]
fn lombok_null_checks_do_not_steal_the_jspecify_import() {
    let scratch = Scratch::new();
    translate(&scratch.0, LOMBOK, true);
    let java = fs::read_to_string(scratch.0.join("Child.java")).unwrap();
    assert!(java.contains("@lombok.NonNull"), "{java}");
    assert!(!java.contains("import lombok.NonNull;"), "{java}");
    assert!(java.contains("@NonNull String getText()"), "{java}");
    assert_eq!(
        java.matches("import org.jspecify.annotations.NonNull;")
            .count(),
        1,
        "{java}"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, JSpecify, and Lombok jars in NOTLIN_* variables"]
fn real_toolchain_compiles_jspecify_returns_and_lombok_without_import_conflicts() {
    let scratch = Scratch::new();
    let jar = PathBuf::from(std::env::var_os("NOTLIN_JSPECIFY").expect("NOTLIN_JSPECIFY"));
    for (label, source, lombok) in [
        ("returns", RETURNS, false),
        ("collision", COLLISION, false),
        ("literal", LITERAL, false),
        ("lombok", LOMBOK, true),
        ("nullable", NULLABLE, false),
    ] {
        let root = scratch.0.join(label);
        let java = translate(&root, source, lombok);
        let mut cp = vec![jar.clone()];
        if lombok {
            cp.push(std::env::var_os("NOTLIN_LOMBOK").unwrap().into());
        }
        let result = Command::new(std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"))
            .args([
                "--release",
                "21",
                if lombok { "-proc:full" } else { "-proc:none" },
            ])
            .arg("-classpath")
            .arg(std::env::join_paths(&cp).unwrap())
            .arg("-d")
            .arg(root.join("classes"))
            .args(java)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        if lombok {
            let caller = root.join("Caller.kt");
            fs::write(
                &caller,
                "package sample\nsuspend fun read(child:Child):Int=child.text.length\n",
            )
            .unwrap();
            cp.push(root.join("classes"));
            cp.push(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap().into());
            let result = Command::new(std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"))
                .args(["-jvm-target", "21", "-classpath"])
                .arg(std::env::join_paths(cp).unwrap())
                .arg("-d")
                .arg(root.join("kotlin-classes"))
                .arg(caller)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "retained Kotlin must see the narrowed non-null getter: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
}
