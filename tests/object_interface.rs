use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn kotlin_object_implements_translated_interface() {
    let root = Path::new("tests/tmp_scratch_object_interface");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("model.kt"),
        "package fixture.objectinterface\n\ninterface Contract\n\nobject Singleton : Contract\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("model.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");

    let java = fs::read_to_string(root.join("Singleton.java")).unwrap();
    assert!(
        java.contains("final class Singleton implements Contract"),
        "Kotlin object must implement an interface, not extend it:\n{java}"
    );
    assert!(!java.contains("extends Contract"), "invalid Java:\n{java}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn object_interface_property_is_an_instance_getter() {
    let root = Path::new("tests/tmp_scratch_object_interface_property");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("model.kt"),
        "package fixture.objectinterface\n\ninterface Contract { val type: String }\n\nobject Singleton : Contract {\n    override val type: String\n        get() = \"singleton\"\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("model.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");

    let java = fs::read_to_string(root.join("Singleton.java")).unwrap();
    assert!(java.contains("public String getType()"), "{java}");
    assert!(!java.contains("static String getType()"), "{java}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn object_override_function_is_an_instance_method() {
    let root = Path::new("tests/tmp_scratch_object_override_function");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("model.kt"),
        "package fixture.objectinterface\n\
         interface Contract {\n    fun getType(): String\n}\n\
         object Singleton : Contract {\n    override fun getType(): String = \"singleton\"\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("model.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");

    let java = fs::read_to_string(root.join("Singleton.java")).unwrap();
    assert!(java.contains("public String getType()"), "{java}");
    assert!(!java.contains("static String getType()"), "{java}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn repaired_object_property_getter_is_an_instance_method_after_read_resolve() {
    let root = Path::new("tests/tmp_scratch_repaired_object_property");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("model.kt"),
        "package fixture.objectinterface\n\
         import kotlin.reflect.KClass\n\
         @Target(AnnotationTarget.CLASS) annotation class Read(val using: KClass<*>)\n\
         class Reader\nclass MessageType\nobject ApplicationInit\n\
         interface MessageData { val type: MessageType }\n\
         @Read(using = Reader::class)\n\
         object MessageException : MessageData {\n\
             private fun readResolve(): Any = ApplicationInit\n\
             override val type: MessageType get() = MessageType()\n\
         }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.to_str().unwrap())
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");

    let java = fs::read_to_string(root.join("MessageException.java")).unwrap();
    assert!(java.contains("public MessageType getType()"), "{java}");
    assert!(!java.contains("static MessageType getType()"), "{java}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn translated_java_uses_instance_field_for_imported_retained_kotlin_object() {
    let root = Path::new("tests/tmp_scratch_retained_object_reference");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.join("provider")).unwrap();
    fs::create_dir_all(root.join("client")).unwrap();
    fs::write(
        root.join("provider/StartupState.kt"),
        "package fixture.provider\nobject StartupState\n",
    )
    .unwrap();
    let client = root.join("client/Reader.kt");
    fs::write(
        &client,
        "package fixture.client\nimport fixture.provider.StartupState\n\
         class Reader { private fun readResolve(): Any = StartupState }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args([
            "--root",
            root.to_str().unwrap(),
            "--annotations",
            "none",
            "--in-place",
        ])
        .arg(&client)
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");

    assert!(root.join("provider/StartupState.kt").is_file());
    let java = fs::read_to_string(root.join("client/Reader.java")).unwrap();
    assert!(
        java.contains("return StartupState.INSTANCE;"),
        "a Java method must read both retained and generated Kotlin objects via INSTANCE:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
