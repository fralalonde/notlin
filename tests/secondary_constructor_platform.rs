use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "notlin-secondary-platform-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn uuid_factory_delegation_uses_the_proven_platform_contract() {
    for lombok in [false, true] {
        let scratch = Scratch::new();
        fs::write(
            scratch.0.join("Token.kt"),
            "package secondary.platform\nimport java.util.UUID\nclass Token(val id: UUID, val key: String) {\n    constructor(key: String) : this(UUID.randomUUID(), key)\n}\n",
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
        command
            .arg("--root")
            .arg(&scratch.0)
            .args(["--annotations", "none", "--in-place"]);
        if lombok {
            command.arg("--lombok");
        }
        let result = command.arg(&scratch.0).output().unwrap();
        assert!(result.status.success(), "{result:?}");
        let java = fs::read_to_string(scratch.0.join("Token.java")).unwrap_or_default();
        assert!(
            java.contains("this(UUID.randomUUID(), key);"),
            "{java}\n{result:?}"
        );
        assert!(java.contains("Token(UUID id, String key)"), "{java}");
        assert!(!scratch.0.join("Token.kt").exists(), "{result:?}");
    }
}

#[test]
fn custom_uuid_factory_does_not_gain_platform_constructor_proof() {
    let scratch = Scratch::new();
    fs::write(
        scratch.0.join("Token.kt"),
        "package secondary.platform\nobject UUID {\n    fun randomUUID(): String = \"custom\"\n}\nclass Token(val id: String, val key: String) {\n    constructor(key: String) : this(UUID.randomUUID(), key)\n}\n",
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&scratch.0)
        .args(["--annotations", "none", "--in-place"])
        .arg(&scratch.0)
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert!(!scratch.0.join("Token.java").exists(), "{result:?}");
    assert!(scratch.0.join("Token.kt").exists(), "{result:?}");
}

#[test]
fn uuid_instance_extensions_are_not_static_constructor_factories() {
    for receiver in ["source", "UUID"] {
        let scratch = Scratch::new();
        fs::write(scratch.0.join("Token.kt"), format!(
            "package secondary.platform\nimport java.util.UUID\nfun UUID.randomUUID(): UUID = UUID(0, 0)\nclass Token(val id: UUID) {{\n    constructor({receiver}: UUID, marker: Int) : this({receiver}.randomUUID())\n}}\n"
        )).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_notlin"))
            .arg("--root")
            .arg(&scratch.0)
            .args(["--annotations", "none", "--in-place"])
            .arg(&scratch.0)
            .output()
            .unwrap();
        assert!(result.status.success(), "{result:?}");
        assert!(
            !scratch.0.join("Token.java").exists(),
            "{receiver}: {result:?}"
        );
        assert!(
            scratch.0.join("Token.kt").exists(),
            "{receiver}: {result:?}"
        );
    }
}
