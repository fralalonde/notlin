//! A generic Kotlin supertype whose abstract members are typed by the
//! SUPERTYPE's own type parameters must not taint implementing classes.
//! Java erasure binds the parameter to whatever the implementor declares:
//! `interface Carrier<T> { fun pick(): T }` is satisfied by
//! `class Impl : Carrier<Widget> { override fun pick(): Widget }`.
//! Previously the raw type-text comparison (T vs Widget) raised N3A2E and
//! retained every implementor of the generic hub. (Members are functions on
//! purpose: a property override would trip the unrelated retained-property
//! interface rule and mask the erasure question.)

use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn generic_supertype_parameter_member_is_erasure_compatible() {
    let root = Path::new("tests/tmp_scratch_genericsup");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("hub.kt"),
        "package neutral.genericsup\n\
         \n\
         interface Payload\n\
         \n\
         interface Carrier<T : Payload> {\n\
         \x20   fun pick(): T\n\
         }\n",
    )
    .unwrap();
    fs::write(
        root.join("impl.kt"),
        "package neutral.genericsup\n\
         \n\
         class Widget : Payload\n\
         \n\
         class WidgetCarrier : Carrier<Widget> {\n\
         \x20   override fun pick(): Widget = Widget()\n\
         }\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("impl.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let out_java = fs::read_to_string(root.join("WidgetCarrier.java")).unwrap_or_default();
    assert!(
        !out_java.is_empty(),
        "implementor of a generic supertype must translate, not be tainted.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("return type is incompatible"),
        "type-parameter member must not raise the ABI guard:\n{stderr}"
    );
    // The retained supertype stays Kotlin, so `super.item`-style accessors on
    // the implementor still cannot call into it — but a plain constructor
    // property override emits its own getter with no super call:
    assert!(
        out_java.contains("class WidgetCarrier"),
        "unexpected Java emission:\n{out_java}"
    );
    let _ = fs::remove_dir_all(root);
}
