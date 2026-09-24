use std::fs;
use std::path::Path;
use std::process::Command;

/// A translated method using reflective instantiation
/// (`clazz.getConstructor(...).newInstance(...)`) must compile in Java:
/// Kotlin treats the checked reflective exceptions as unchecked, so the
/// generated body wraps the reflective call in try/catch rethrowing a
/// RuntimeException instead of changing the method's signature with
/// `throws` (pre-existing Java callers have fixed signatures).
#[test]
fn reflective_getconstructor_wraps_checked_exceptions() {
    let root = Path::new("tests/tmp_scratch_reflect");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("m.kt"),
        "package p\n\nabstract class Widget(val objectId: java.util.UUID, val lookupId: String)\n\nclass WidgetId(override var objectId: UUID, var lookupId: String) : Widget(objectId, lookupId)\n\nclass Holder(\n    var objectId: UUID,\n    var lookupId: String\n) {\n    fun <T : Widget> toWidgetClass(clazz: Class<out T>): T {\n        return clazz.getConstructor(UUID::class.java, String::class.java).newInstance(objectId, lookupId)\n    }\n}\n",
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("m.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let java = fs::read_to_string(root.join("Holder.java")).unwrap_or_default();
    assert!(
        !java.is_empty(),
        "Holder.java must be generated:\n(m.kt emitted nothing)"
    );
    assert!(
        !java.contains("throws NoSuchMethodException"),
        "checked reflective exceptions must be wrapped, not declared:\n{java}"
    );
    assert!(
        java.contains("getConstructor"),
        "reflective instantiation must be kept:\n{java}"
    );
    // the wrap shape: try { return ... } catch (Exception e) { throw new RuntimeException(e); }
    assert!(
        java.contains("throw new RuntimeException(e)"),
        "reflective call must be guarded with a rethrow wrapper:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}
