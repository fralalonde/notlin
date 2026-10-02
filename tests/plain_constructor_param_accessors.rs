//! A plain (non-property) primary-constructor parameter declares no Kotlin
//! member, so it must not get a Java accessor. Emitting one produces a getter
//! reading a field that was never written — javac rejects it ("cannot find
//! symbol x"), and when the parameter name matches an inherited property's name
//! the body resolves to that property's backing field instead ("id has private
//! access in Base").

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn plain_constructor_parameters_get_no_accessors() {
    let root = Path::new("tests/tmp_scratch_plain_param_accessors");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("entity.kt"),
        "package neutral.plainparam\n\
         \n\
         open class Base(open var id: String)\n\
         \n\
         class Derived(id: String) : Base(id)\n\
         \n\
         class Mixed(var name: String, marker: String) : Base(name)\n",
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let derived = fs::read_to_string(root.join("Derived.java")).unwrap_or_default();
    let mixed = fs::read_to_string(root.join("Mixed.java")).unwrap_or_default();
    assert!(
        derived.contains("public Derived(String id)"),
        "the constructor still takes every parameter, Derived.java:\n{derived}"
    );
    assert!(
        !derived.contains("getId()"),
        "a plain parameter declares no accessor, Derived.java:\n{derived}"
    );
    assert!(
        mixed.contains("getName()") && mixed.contains("setName("),
        "a property parameter keeps its accessors, Mixed.java:\n{mixed}"
    );
    assert!(
        !mixed.contains("getMarker()"),
        "the non-property parameter of the same constructor must not get one, Mixed.java:\n{mixed}"
    );
    let _ = fs::remove_dir_all(root);
}
