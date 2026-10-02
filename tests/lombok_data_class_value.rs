//! A Kotlin `data class` is an immutable value type. Under `--lombok` its
//! faithful Java form is Lombok's `@Value` — private final fields, getters,
//! equals/hashCode/toString, an all-args constructor and NO setters. `@Data` is
//! the mutable form and is only right when a caller can actually write a
//! property.
//!
//! Body instance fields also force `@Data`: `@Value` implies
//! `@AllArgsConstructor` over EVERY field, so a class with body fields would
//! change the constructor arity Kotlin callers use.

use std::fs;
use std::path::Path;
use std::process::Command;

fn emitted(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join(format!("{name}.java"))).unwrap_or_default()
}

#[test]
fn data_class_lombok_annotation_follows_mutability() {
    let root = Path::new("tests/tmp_scratch_lombok_value");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("types.kt"),
        r#"package neutral.val

data class Immutable(val id: String, val count: Int)

data class Mutable(val id: String, var count: Int)

data class WithBody(val id: String) {
    var extra: String? = null
}
"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("types.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let immutable = emitted(root, "Immutable");
    assert!(
        immutable.contains("@Value"),
        "an all-val data class must be @Value:\n{immutable}"
    );
    assert!(
        !immutable.contains("@Data"),
        "an all-val data class must not be mutable @Data:\n{immutable}"
    );

    let mutable = emitted(root, "Mutable");
    assert!(
        mutable.contains("@Data"),
        "a data class with a writable property must stay @Data:\n{mutable}"
    );
    assert!(
        !mutable.contains("@Value"),
        "a data class with a writable property must not be immutable @Value:\n{mutable}"
    );

    let with_body = emitted(root, "WithBody");
    assert!(
        with_body.contains("@Data"),
        "body instance fields change the arity @Value would construct:\n{with_body}"
    );

    let _ = fs::remove_dir_all(root);
}

/// `@Value`/`@Data` carry `@EqualsAndHashCode`, whose generated `equals` covers
/// this class's own fields only: with a real superclass the inherited state has
/// to be folded in, or Lombok warns at every such class. An interface-only
/// supertype must NOT get the call — a supercall to `Object` is itself a Lombok
/// diagnostic, and that mistake cost this project 200 diagnostics once.
#[test]
fn lombok_equals_call_super_only_for_a_real_superclass() {
    let root = Path::new("tests/tmp_scratch_lombok_call_super");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("types.kt"),
        r#"package neutral.valsuper

interface Marker

open class Base(val name: String)

data class Derived(val name: String, val n: Int) : Base(name)

class InterfaceOnly(val name: String) : Marker
"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("types.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let derived = emitted(root, "Derived");
    assert!(
        derived.contains("callSuper = true"),
        "a class with a superclass must fold the inherited state in:\n{derived}"
    );

    let interface_only = emitted(root, "InterfaceOnly");
    assert!(
        !interface_only.contains("callSuper"),
        "an interface-only supertype must not get a supercall:\n{interface_only}"
    );

    let _ = fs::remove_dir_all(root);
}
