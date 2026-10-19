use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "notlin-inherited-bare-property-write-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_fixture(root: &Path) {
    fs::write(
        root.join("Contract.kt"),
        "package sample\ninterface Contract { var label: String }\n",
    )
    .unwrap();
    fs::write(
        root.join("Base.kt"),
        "package sample\nopen class Base(override var label: String) : Contract\n",
    )
    .unwrap();
    fs::write(
        root.join("Derived.kt"),
        r#"package sample
class Derived(initial: String) : Base(initial) {
    fun update(next: String) { label = next }

    fun localShadow(next: String): String {
        var label = next
        label = "local"
        return label
    }
}
"#,
    )
    .unwrap();
}

#[test]
fn inherited_bare_var_write_uses_setter_and_preserves_local_shadow() {
    let scratch = Scratch::new();
    write_fixture(&scratch.0);

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&scratch.0)
        .args(["--in-place", "--annotations", "none"])
        .arg(&scratch.0)
        .output()
        .expect("run notlin");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let derived = fs::read_to_string(scratch.0.join("Derived.java")).unwrap();
    assert!(
        derived.contains("this.setLabel(next);"),
        "{derived}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!derived.contains("this.label = next"), "{derived}");
    assert!(derived.contains("String label = next;"), "{derived}");
    assert!(derived.contains("label = \"local\";"), "{derived}");
}

#[test]
fn inherited_setter_fact_uses_parent_imports_and_rejects_private_constructor_properties() {
    let scratch = Scratch::new();
    let parent = scratch.0.join("parent");
    let sample = scratch.0.join("sample");
    fs::create_dir_all(&parent).unwrap();
    fs::create_dir_all(&sample).unwrap();
    fs::write(
        parent.join("Contract.kt"),
        "package parent.api\ninterface Contract { var label: String }\n",
    )
    .unwrap();
    fs::write(
        parent.join("Base.kt"),
        "package parent.api\nopen class Base(override var label: String) : Contract\n",
    )
    .unwrap();
    fs::write(
        sample.join("Contract.kt"),
        "package sample\ninterface Contract { val label: Int }\n",
    )
    .unwrap();
    fs::write(
        sample.join("Derived.kt"),
        "package sample\nimport parent.api.Base\nclass Derived(value: String) : Base(value)\n",
    )
    .unwrap();
    fs::write(
        parent.join("Hidden.kt"),
        "package parent.api\nopen class Hidden(private var token: String)\n",
    )
    .unwrap();
    fs::write(
        sample.join("PrivateChild.kt"),
        "package sample\nimport parent.api.Hidden\nclass PrivateChild : Hidden(\"x\")\n",
    )
    .unwrap();

    let index = notlin::workspace::SourceIndex::discover(&scratch.0).unwrap();
    assert_eq!(
        index.inherited_mutable_property_setter_in_file(
            &sample.join("Derived.kt"),
            "Derived",
            "label"
        ),
        Some("setLabel".into())
    );
    assert_eq!(
        index.inherited_mutable_property_setter_in_file(
            &sample.join("PrivateChild.kt"),
            "PrivateChild",
            "token"
        ),
        None
    );
}

#[test]
fn inherited_setter_fact_rejects_applicable_overloads() {
    let scratch = Scratch::new();
    for (name, declaration) in [
        ("Api", "interface Api { var payload: Any }"),
        (
            "Base",
            "open class Base(override var payload: Any) : Api {}",
        ),
        (
            "Overloaded",
            "class Overloaded : Base(\"seed\") { fun setPayload(input: String) {} }",
        ),
        (
            "Defaulted",
            "class Defaulted : Base(\"seed\") { fun setPayload(input: String, revision: Int = 0) {} }",
        ),
        (
            "Disjoint",
            "class Disjoint : Base(\"seed\") { fun setPayload(input: String, revision: Int) {} }",
        ),
    ] {
        fs::write(
            scratch.0.join(format!("{name}.kt")),
            format!("package sample\n{declaration}\n"),
        )
        .unwrap();
    }
    let index = notlin::workspace::SourceIndex::discover(&scratch.0).unwrap();
    assert_eq!(index.declarations().count(), 5, "{:?}", index.files);
    for owner in ["Overloaded", "Defaulted"] {
        assert_eq!(
            index.inherited_mutable_property_setter_in_file(
                &scratch.0.join(format!("{owner}.kt")),
                owner,
                "payload"
            ),
            None,
            "{owner}"
        );
    }
    assert_eq!(
        index.inherited_mutable_property_setter_in_file(
            &scratch.0.join("Disjoint.kt"),
            "Disjoint",
            "payload"
        ),
        Some("setPayload".into())
    );
}
