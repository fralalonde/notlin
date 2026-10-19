use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn generic_copy_captures_receiver_once_and_arguments_in_source_order() {
    let root = Path::new("tests/tmp_data_class_copy_lowering");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Box.kt"),
        "package sample\n\ndata class Box<T>(val value: T, val label: Int) {\n    fun replace(next: T): Box<T> {\n        var count = 0\n        return copy(label = count++, value = next)\n    }\n    fun relabel(): Box<T> {\n        var count = 0\n        return copy(label = count++)\n    }\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Box.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let java = fs::read_to_string(root.join("Box.java")).unwrap_or_default();

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        java.contains("Function<Box<T>, java.util.function.Function<Integer, java.util.function.Function<T, Box<T>>>>")
            && java.contains(".apply(this)"),
        "copy receiver should be evaluated once with its established generic type:\n{java}"
    );
    let first = java.find(".apply(count++)").unwrap();
    let second = java.find(".apply(next)").unwrap();
    assert!(
        first < second,
        "copy arguments must retain Kotlin source order:\n{java}"
    );
    let marker = java.find("__notlinCopyReceiver").unwrap();
    let suffix = java[marker + "__notlinCopyReceiver".len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    assert!(
        java.contains(&format!(
            "new Box<>(__notlinCopyArg{suffix}_1, __notlinCopyArg{suffix}_0)"
        )),
        "named copy arguments should be placed by component name:\n{java}"
    );
    assert!(
        java.contains(".value(), __notlinCopyArg"),
        "omitted components should come from the receiver accessor:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn omitted_copy_component_with_user_accessor_retains_only_unsafe_owner() {
    for (mode, owner, accessor) in [
        ("plain", "Guarded", "item"),
        ("lombok", "LombokGuarded", "isReady"),
    ] {
        let root =
            Path::new("tests").join(format!("tmp_copy_accessor_{mode}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("Models.kt"),
            r#"package sample
data class Guarded<T>(@JvmField val item: T) {
    fun getItem(): T = item
    fun item(): T = item
    fun duplicate(): Guarded<T> = copy()
    fun replaced(next: T): Guarded<T> = copy(item = next)
}
data class LombokGuarded(@JvmField val isReady: Boolean) {
    fun isReady(): Boolean = false
    fun duplicate(): LombokGuarded = copy()
}
class Safe<T> {
    fun replaced(original: Guarded<T>, next: T): Guarded<T> = original.copy(item = next)
}
"#,
        )
        .unwrap();

        let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
        command.arg("--root").arg(&root).arg("--in-place");
        if mode == "lombok" {
            command.arg("--lombok");
        }
        let output = command
            .arg(root.join("Models.kt"))
            .output()
            .expect("run notlin");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "notlin failed:\n{stderr}");

        let kotlin = fs::read_to_string(root.join("Models.kt")).unwrap();
        assert!(
            kotlin.contains(&format!("data class {owner}"))
                && kotlin.contains(&format!("fun {accessor}")),
            "the owner with a custom omitted-component accessor should remain Kotlin ({mode}):\n{kotlin}\n{stderr}"
        );
        let safe_java = fs::read_to_string(root.join("Safe.java")).unwrap_or_default();
        assert!(
            safe_java.contains("new Guarded<>(") || safe_java.contains("new Guarded("),
            "fully supplied components should still lower when no getter is needed ({mode}):\n{safe_java}\n{stderr}"
        );
        let _ = fs::remove_dir_all(root);
    }
}
