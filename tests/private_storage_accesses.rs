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
            "notlin-private-storage-accesses-{}-{id}",
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

fn write_fixture(root: &Path, base: &str, child: Option<&str>) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Contract.kt"),
        "package sample\ninterface Contract { var id: String }\n",
    )
    .unwrap();
    fs::write(root.join("Base.kt"), base).unwrap();
    if let Some(child) = child {
        fs::write(root.join("Child.kt"), child).unwrap();
    }
}

fn translate(root: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(root)
        .args(["--in-place", "--annotations", "none"])
        .arg(root)
        .output()
        .expect("run notlin")
}

const SUPPORTED_BASE: &str = r#"package sample
@Target(AnnotationTarget.FIELD)
@Retention(AnnotationRetention.RUNTIME)
annotation class Marker(val value: String)

open class Base(@field:Marker("select id from table") override var id: String) : Contract {
    suspend fun keepBase() {}

    fun readOuter(): String = id
    fun writeOuter(next: String) { id = next }

    fun nestedScope(next: String): String {
        fun localRead(): String {
            val id = next
            return id
        }
        return localRead() + ":" + this.id
    }
}
"#;

const SUPPORTED_CHILD: &str = r#"package sample
class Child(override var id: String) : Base(id) {
    suspend fun keepChild() {}

    fun readChild(): String = id
    fun writeChild(next: String) { id = next }
}
"#;

#[test]
fn open_storage_bridge_preserves_scopes_annotation_and_child_override() {
    let scratch = Scratch::new();
    write_fixture(&scratch.0, SUPPORTED_BASE, Some(SUPPORTED_CHILD));

    let output = translate(&scratch.0);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(scratch.0.join("Contract.java").is_file());
    assert!(!scratch.0.join("Contract.kt").exists());
    let base = fs::read_to_string(scratch.0.join("Base.kt")).unwrap();
    let child = fs::read_to_string(scratch.0.join("Child.kt")).unwrap();

    for source in [&base, &child] {
        assert!(source.contains("private var id: String"), "{source}");
        assert!(source.contains("fun getId(): String"), "{source}");
        assert!(source.contains("fun setId(value: String)"), "{source}");
    }
    assert!(
        base.contains("@field:Marker(\"select id from table\")"),
        "{base}"
    );
    assert!(
        base.contains("val id = next\n            return id"),
        "the nested local shadow remains local:\n{base}"
    );
    assert!(
        base.contains("this.getId()"),
        "the unshadowed receiver uses virtual dispatch:\n{base}"
    );
    assert!(
        base.contains("setId(next)"),
        "the unshadowed write uses virtual dispatch:\n{base}"
    );
    assert!(child.contains("override fun getId(): String"), "{child}");
    assert!(
        child.contains("override fun setId(value: String)"),
        "{child}"
    );
}

#[test]
fn unsupported_private_storage_access_shapes_retain_the_contract() {
    let cases = [
        (
            "compound-write",
            r#"package sample
open class Base(override var id: String) : Contract {
    suspend fun keepCompound() {}
    fun append(suffix: String) { id += suffix }
}
"#,
        ),
        (
            "member-extension",
            r#"package sample
open class Base(override var id: String) : Contract {
    suspend fun keepExtension() {}
    fun String.withId(): String = "$this:$id"
}
"#,
        ),
        (
            "callable-reference",
            r#"package sample
open class Base(override var id: String) : Contract {
    suspend fun keepReference() {}
    fun propertyReference() = this::id
}
"#,
        ),
    ];

    for (name, base) in cases {
        let scratch = Scratch::new();
        let root = scratch.0.join(name);
        write_fixture(&root, base, None);

        let output = translate(&root);
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            root.join("Contract.kt").is_file(),
            "{name} must keep its Kotlin contract available"
        );
        assert!(
            !root.join("Contract.java").exists(),
            "{name} must not emit a partial Java contract"
        );
        assert!(
            root.join("Base.kt").is_file() && !root.join("Base.java").exists(),
            "{name} should retain its intentionally suspended owner"
        );
    }
}
