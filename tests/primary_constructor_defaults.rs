use std::fs;
use std::path::Path;
use std::process::Command;

/// `@JvmOverloads` on a primary constructor gives Java callers one overload
/// for every omitted trailing default. The generated Java must replace the
/// Kotlin annotation with real delegating constructors instead of retaining
/// an unresolved `@JvmOverloads` annotation.
#[test]
fn jvm_overloads_primary_constructor_emits_every_trailing_default_overload() {
    let root = Path::new("tests/tmp_scratch_primary_ctor_defaults");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Endpoint.kt"),
        "package neutral.defaults\n\ndata class Endpoint @JvmOverloads constructor(\n    val host: String,\n    val port: Int = 8080,\n    val secure: Boolean = false\n)\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--lombok", "--in-place"])
        .arg(root.join("Endpoint.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let java = fs::read_to_string(root.join("Endpoint.java")).unwrap_or_default();

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        !root.join("Endpoint.kt").exists(),
        "supported @JvmOverloads declaration remained Kotlin:\n{stderr}"
    );
    assert!(
        java.contains("public Endpoint(String host, int port)")
            && java.contains("this(host, port, false);"),
        "missing one-default constructor overload:\n{java}"
    );
    assert!(
        java.contains("public Endpoint(String host)") && java.contains("this(host, 8080, false);"),
        "missing two-default constructor overload:\n{java}"
    );
    assert!(
        !java.contains("@JvmOverloads"),
        "Kotlin-only annotation leaked into generated Java:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn jvm_overloads_plain_primary_constructor_emits_every_trailing_default_overload() {
    let root = Path::new("tests/tmp_scratch_plain_primary_ctor_defaults");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Client.kt"),
        "package neutral.defaults\n\nclass Client @JvmOverloads constructor(\n    val address: String,\n    val retries: Int = 3,\n    val tracing: Boolean = false\n)\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--lombok", "--in-place"])
        .arg(root.join("Client.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let java = fs::read_to_string(root.join("Client.java")).unwrap_or_default();

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        java.contains("public Client(String address, int retries)")
            && java.contains("this(address, retries, false);"),
        "missing one-default plain-class constructor overload:\n{java}"
    );
    assert!(
        java.contains("public Client(String address)") && java.contains("this(address, 3, false);"),
        "missing two-default plain-class constructor overload:\n{java}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn jvm_overloads_default_expression_reads_prior_constructor_parameter_directly() {
    let root = Path::new("tests/tmp_scratch_primary_ctor_default_reference");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Message.kt"),
        "package neutral.defaults\n\ndata class Message @JvmOverloads constructor(\n    val name: String,\n    val text: String = \"hello \" + name\n)\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--lombok", "--in-place"])
        .arg(root.join("Message.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let java = fs::read_to_string(root.join("Message.java")).unwrap_or_default();

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        java.contains("this(name, \"hello \" + name);") && !java.contains("this.getName()"),
        "constructor default must use the prior parameter, not this before construction:\n{java}"
    );

    let _ = fs::remove_dir_all(root);
}
