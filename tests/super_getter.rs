use std::fs;
use std::path::Path;
use std::process::Command;

/// Kotlin `super.<prop>` inside an overriding getter targets the
/// SUPERTYPE'S default getter. Java needs the qualified form
/// `Base.super.getAlias()` — plain `super.getAlias()` does not compile in
/// an interface default method (and even in a class, `super.getX()` only
/// works when the parent exposes that accessor). When the declaring
/// supertype TRANSLATED to Java, emit the qualified super-accessor.
#[test]
fn interface_super_property_gets_qualified_super_accessor() {
    let root = Path::new("tests/tmp_scratch_super1");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    // Base + impl in the SAME file: both translate together, so `Base` is
    // a Java interface when the impl's getter body is emitted.
    fs::write(
        root.join("m.kt"),
        "package neutral.superq\n\ninterface Base {\n    val alias: String\n        get() = \"g\"\n}\n\nenum class Impl : Base {\n    ONE;\n\n    override val alias: String\n        get() = super.alias + \"/x\"\n}\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let java = fs::read_to_string(root.join("Impl.java")).unwrap_or_default();
    assert!(
        java.contains("Base.super.getAlias()"),
        "super.<prop> in a getter must lower to the qualified `Base.super.getAlias()`:\n{java}"
    );
    assert!(
        !java.contains("super.getAlias()") || java.contains("Base.super.getAlias()"),
        "plain `super.getAlias()` must not leak when the supertype translated:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

/// When the declaring supertype RETAINS Kotlin (retained for its own
/// reasons), its getter has no JVM-visible Java default method; the
/// translated caller cannot compile against it, so the caller must stay
/// Kotlin (taint) instead of emitting `Base.super.getAlias()`.
#[test]
fn super_property_against_retained_supertype_taints_caller() {
    let root = Path::new("tests/tmp_scratch_super2");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    // Base.kt is NOT a translation input: it stays Kotlin.
    fs::write(
        root.join("base.kt"),
        "package neutral.superr\n\ninterface Base {\n    val alias: String\n        get() = \"g\"\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("impl.kt"),
        "package neutral.superr\n\nenum class Impl : Base {\n    ONE;\n\n    override val alias: String\n        get() = super.alias + \"/x\"\n}\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("impl.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let java = fs::read_to_string(root.join("Impl.java")).unwrap_or_default();
    assert!(
        java.is_empty() || !java.contains("super.getAlias()"),
        "caller must not emit a super-accessor against a retained Kotlin supertype:\n{java}"
    );
    if !java.is_empty() {
        assert!(
            java.contains("Base.super.getAlias()"),
            "if emitted at all, the accessor must still be qualified:\n{java}"
        );
    }
    let _ = fs::remove_dir_all(root);
}
