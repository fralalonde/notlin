use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn property_interfaces_and_selected_descendants_translate() {
    let root = Path::new("tests/tmp_scratch_property_interfaces");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    for (file, source) in [
        (
            "state.kt",
            "package neutral.interfaces\ninterface IRuntimeState\n",
        ),
        (
            "state_aware.kt",
            "package neutral.interfaces\ninterface IStateAware { val state: IRuntimeState }\n",
        ),
        (
            "widget_state_aware.kt",
            "package neutral.interfaces\ninterface IWidgetStateAware : IStateAware\n",
        ),
        (
            "enabled.kt",
            "package neutral.interfaces\ninterface IEnabledAware { val enabled: Boolean }\n",
        ),
        (
            "base.kt",
            "package neutral.interfaces\nabstract class Base : IWidgetStateAware, IEnabledAware\n",
        ),
        (
            "widget.kt",
            "package neutral.interfaces\nclass Widget(override val state: IRuntimeState, override val enabled: Boolean) : Base()\n",
        ),
    ] {
        fs::write(root.join(file), source).unwrap();
    }

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root)
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");
    for declaration in [
        "IRuntimeState",
        "IStateAware",
        "IWidgetStateAware",
        "IEnabledAware",
        "Base",
        "Widget",
    ] {
        assert!(
            root.join(format!("{declaration}.java")).exists(),
            "{declaration} stayed Kotlin:\n{stderr}"
        );
    }
    fs::remove_dir_all(root).unwrap();
}
