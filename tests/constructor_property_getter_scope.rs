//! A class member must win over a same-named imported top-level property when
//! a computed getter is lowered, while method parameters and companion scope
//! keep their own Kotlin name resolution.

use std::fs;
use std::path::Path;
use std::process::Command;

fn translate(root: &Path, lombok: bool) -> String {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Global.kt"),
        "package unrelated\nval data: String = \"global\"\n",
    )
    .unwrap();
    fs::write(
        root.join("Box.kt"),
        r#"package boundary
import unrelated.data

data class Box<T>(var data: T) {
    val payload: T get() = data
    fun echo(data: T): T = data
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("CompanionBox.kt"),
        r#"package boundary
import unrelated.data

class CompanionBox(var data: String) {
    companion object {
        val importedData: String get() = data
    }
}
"#,
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command.args([
        "--root",
        root.to_str().unwrap(),
        "--in-place",
        "--annotations",
        "none",
    ]);
    if lombok {
        command.arg("--lombok");
    }
    let output = command.arg(root).output().expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let remaining_source = fs::read_to_string(root.join("Box.kt")).unwrap_or_default();
    let report = fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        output.status.success(),
        "notlin failed:\n{stderr}\nremaining Kotlin:\n{remaining_source}\ncoverage reports:\n{report}"
    );
    let java = fs::read_to_string(root.join("Box.java")).unwrap_or_default();
    assert!(
        !java.is_empty(),
        "notlin retained Box.kt without producing Box.java; stderr:\n{stderr}\nremaining Kotlin:\n{remaining_source}\ncoverage reports:\n{report}"
    );
    java
}

#[test]
fn constructor_property_binds_before_imported_global_without_capturing_shadows() {
    let scratch = Path::new("tests/tmp_scratch_constructor_property_scope");
    let _ = fs::remove_dir_all(scratch);
    let plain = translate(&scratch.join("plain"), false);
    let lombok = translate(&scratch.join("lombok"), true);

    for (mode, java) in [("plain", plain.as_str()), ("lombok", lombok.as_str())] {
        let payload = java
            .split("getPayload()")
            .nth(1)
            .unwrap_or("")
            .split('}')
            .next()
            .unwrap_or("");
        assert!(
            payload.contains("return this.getData();") || payload.contains("return this.data;"),
            "{mode} computed getter must bind the generic constructor property, not the imported global:\n{java}"
        );
        assert!(
            !java.contains("import unrelated.data.*;"),
            "{mode} Java must not render a top-level property import as a package wildcard:\n{java}"
        );
        assert!(
            java.contains("echo(T data)") && java.contains("return data;"),
            "{mode} method parameter must shadow the constructor property:\n{java}"
        );
    }
    // The companion has no instance receiver; its imported top-level `data`
    // must not be rebound to CompanionBox's constructor field. Since cross-file
    // bare property references are not lowered, conservative retention is the
    // correct boundary behavior.
    assert!(
        scratch.join("plain").join("CompanionBox.kt").exists(),
        "plain companion case should remain Kotlin rather than bind outer instance state"
    );
    assert!(
        scratch.join("lombok").join("CompanionBox.kt").exists(),
        "Lombok companion case should remain Kotlin rather than bind outer instance state"
    );
    let _ = fs::remove_dir_all(scratch);
}

#[test]
fn enclosing_member_call_wins_over_imported_unrelated_top_level_function() {
    let scratch = Path::new("tests/tmp_scratch_member_call_scope");
    let _ = fs::remove_dir_all(scratch);
    fs::create_dir_all(scratch).unwrap();
    fs::write(
        scratch.join("Global.kt"),
        "package unrelated\nfun getData(): String = \"global\"\nfun String.unusedExtension(): String = this\nclass Token\nfun Token(value: Int): Token = Token()\n",
    )
    .unwrap();
    fs::write(
        scratch.join("Box.kt"),
        r#"package boundary
import unrelated.getData
import unrelated.getData as globalGetData
import unrelated.unusedExtension
import unrelated.Token

class Box(@JvmField val data: String) {
    fun getData(): String = data
    val payload: String get() = getData()
    val globalPayload: String get() = globalGetData()
    fun token(value: Token): Token = value
}
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args([
            "--root",
            scratch.to_str().unwrap(),
            "--in-place",
            "--annotations",
            "none",
        ])
        .arg(scratch)
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");
    let java = fs::read_to_string(scratch.join("Box.java")).unwrap();
    assert!(
        java.contains("import unrelated.Token;"),
        "a classifier/factory collision must preserve the Java type import:\n{java}"
    );
    let payload = java
        .split("getPayload()")
        .nth(1)
        .unwrap_or("")
        .split('}')
        .next()
        .unwrap_or("");
    assert!(
        payload.contains("getData()") && !payload.contains("Global.getData()"),
        "the enclosing member call must win over the unrelated imported top-level function:\n{java}"
    );
    let global_payload = java
        .split("getGlobalPayload()")
        .nth(1)
        .unwrap_or("")
        .split('}')
        .next()
        .unwrap_or("");
    assert!(
        global_payload.contains("Global.getData()") && !global_payload.contains("globalGetData()"),
        "an explicit import alias should resolve to the actual facade method:\n{java}"
    );
    assert!(
        !java.contains("import unrelated.getData")
            && !java.contains("import unrelated.Global.getData")
            && !java.contains("import unrelated.unusedExtension"),
        "the imported top-level function is emitted through its qualified facade, not a Java import:\n{java}"
    );
    let _ = fs::remove_dir_all(scratch);
}
