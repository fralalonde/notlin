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

data class Immutable(val id: String, val count: Int, val note: String?)

data class Mutable(val id: String, var count: Int, var label: String)

data class WithSecondary(val id: String, val label: String) {
    constructor(label: String) : this(label, label)
}

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
    assert!(
        immutable.contains("@NonNull String id;"),
        "a non-null Kotlin reference must use Lombok @NonNull on the field:\n{immutable}"
    );
    assert!(
        !immutable.contains("@NonNull int count;"),
        "a primitive does not need Lombok @NonNull:\n{immutable}"
    );
    assert!(
        immutable.contains("@Nullable String note;"),
        "a nullable Kotlin reference must stay nullable:\n{immutable}"
    );
    assert!(
        !immutable.contains("public Immutable("),
        "plain @Value classes should let Lombok generate the checked constructor:\n{immutable}"
    );
    assert!(
        !immutable.contains("getId()"),
        "plain @Value classes should let Lombok generate the getter:\n{immutable}"
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
    assert!(
        mutable.contains("@NonNull private final String id;"),
        "explicit Lombok forms must mark non-null final fields:\n{mutable}"
    );
    assert!(
        mutable.contains("@NonNull private String label;"),
        "explicit Lombok forms must mark non-null mutable fields:\n{mutable}"
    );
    assert!(
        mutable.contains("@NonNull String id") && mutable.contains("@NonNull String label"),
        "explicit constructors must enforce Kotlin non-null parameters:\n{mutable}"
    );
    assert!(
        mutable.contains("setLabel(@NonNull String label)"),
        "explicit setters must enforce Kotlin non-null assignments:\n{mutable}"
    );

    let with_secondary = emitted(root, "WithSecondary");
    assert!(
        with_secondary.contains("public WithSecondary(@NonNull String id, @NonNull String label)"),
        "secondary constructors need an explicit checked primary constructor:\n{with_secondary}"
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

#[test]
fn lombok_plain_class_keeps_kotlin_boolean_getter_abi() {
    let root = Path::new("tests/tmp_scratch_lombok_boolean_getter");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("flag.kt"),
        r#"package neutral.flag

class Flag(val enabled: Boolean) {
    fun enabled(): Boolean = enabled
}
"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("flag.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let flag = emitted(root, "Flag");
    assert!(
        flag.contains("public boolean getEnabled()"),
        "Kotlin's Boolean property ABI is getEnabled(), not Lombok's isEnabled():\n{flag}"
    );
    assert!(
        flag.contains("return this.getEnabled();"),
        "same-name function must resolve the property through the Kotlin ABI getter:\n{flag}"
    );

    let _ = fs::remove_dir_all(root);
}
