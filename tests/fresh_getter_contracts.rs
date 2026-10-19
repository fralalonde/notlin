use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "notlin-fresh-getter-contracts-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        for (name, source) in files {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        Self(root)
    }

    fn migrate(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_notlin"))
            .current_dir(&self.0)
            .arg("--in-place")
            .arg(&self.0)
            .output()
            .unwrap()
    }

    fn java(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.0.join(name)).ok()
    }

    fn has_java(&self, name: &str) -> bool {
        self.java(name).is_some()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "notlin failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn source_still_exists(root: &Path, path: &str) -> bool {
    root.join(path).is_file()
}

#[test]
fn fresh_generic_default_getter_chain_translates_while_suspend_leaf_stays_kotlin() {
    let fixture = Fixture::new(&[
        ("Base.kt", "package sample\nopen class Base {}\n"),
        (
            "Root.kt",
            "package sample\ninterface KotlinRoot {\n    fun getId(): Base\n}\n",
        ),
        (
            "GenericRoot.kt",
            "package sample\ninterface GenericRoot<K : Base> : KotlinRoot {\n    override fun getId(): K\n}\n",
        ),
        (
            "Provider.kt",
            "package sample\ninterface Provider<K : Base> : GenericRoot<K> {\n    fun getPayload(): K\n    override fun getId(): K {\n        return getPayload()\n    }\n}\n",
        ),
        (
            "Child.kt",
            "package sample\nclass Child(private val payload: Base) : Provider<Base> {\n    override fun getPayload(): Base {\n        return payload\n    }\n    suspend fun retain(): Int {\n        return 1\n    }\n}\n",
        ),
    ]);

    let output = fixture.migrate();
    assert_success(&output);
    assert!(fixture.has_java("KotlinRoot.java"));
    assert!(fixture.has_java("GenericRoot.java"));
    assert!(fixture.has_java("Provider.java"));
    assert!(fixture.has_java("Base.java"));
    assert!(source_still_exists(&fixture.0, "Child.kt"));
    assert!(!fixture.has_java("Child.java"));
}

#[test]
fn fresh_java_util_wildcard_keeps_uuid_default_method_translatable() {
    let fixture = Fixture::new(&[
        (
            "UuidContract.kt",
            "package sample\nimport java.util.*\ninterface UuidContract {\n    fun getObjectId(): UUID\n}\n",
        ),
        (
            "UuidRoot.kt",
            "package sample\nimport java.util.*\ninterface UuidRoot : UuidContract {\n    override fun getObjectId(): UUID {\n        return UUID.randomUUID()\n    }\n}\n",
        ),
        (
            "UuidChild.kt",
            "package sample\nclass UuidChild : UuidRoot {\n    suspend fun retain(): Int {\n        return 1\n    }\n}\n",
        ),
    ]);

    let output = fixture.migrate();
    assert_success(&output);
    let java = fixture
        .java("UuidRoot.java")
        .expect("fresh Kotlin root should translate");
    assert!(
        java.contains("UUID"),
        "UUID signature should be preserved: {java}"
    );
    assert!(java.contains("import org.jetbrains.annotations.Nullable;"));
    assert!(!java.contains("import org.jetbrains.annotations.*;"));
    assert!(fixture.0.join("UuidChild.kt").is_file());
}

#[test]
fn fresh_java_util_wildcard_keeps_string_property_default_translatable() {
    let fixture = Fixture::new(&[
        (
            "TextContract.kt",
            "package sample\nimport java.util.*\ninterface TextContract {\n    val label: String\n}\n",
        ),
        (
            "TextRoot.kt",
            "package sample\nimport java.util.*\ninterface TextRoot : TextContract {\n    override val label: String\n        get() = \"label\"\n}\n",
        ),
        (
            "TextChild.kt",
            "package sample\nclass TextChild : TextRoot {\n    suspend fun retain(): Int {\n        return 1\n    }\n}\n",
        ),
    ]);

    let output = fixture.migrate();
    assert_success(&output);
    let java = fixture
        .java("TextRoot.java")
        .expect("fresh Kotlin root should translate");
    assert!(
        java.contains("String"),
        "String property type should survive: {java}"
    );
    assert!(
        java.contains("getLabel"),
        "property should emit its getter: {java}"
    );
    assert!(fixture.0.join("TextChild.kt").is_file());
}

#[test]
fn ambiguous_wildcard_type_identity_keeps_the_kotlin_contract() {
    let fixture = Fixture::new(&[
        ("left/Token.kt", "package left\nclass Token {}\n"),
        ("right/Token.kt", "package right\nclass Token {}\n"),
        (
            "TokenContract.kt",
            "package sample\nimport left.*\nimport right.*\ninterface TokenContract {\n    fun getToken(): Token\n}\n",
        ),
        (
            "TokenRoot.kt",
            "package sample\nimport left.*\nimport right.*\ninterface TokenRoot : TokenContract {\n    fun getPayload(): Token\n    override fun getToken(): Token {\n        return getPayload()\n    }\n}\n",
        ),
        (
            "TokenChild.kt",
            "package sample\nclass TokenChild : TokenRoot {\n    suspend fun retain(): Int {\n        return 1\n    }\n}\n",
        ),
    ]);

    let output = fixture.migrate();
    assert_success(&output);
    assert!(fixture.0.join("TokenRoot.kt").is_file());
    assert!(!fixture.has_java("TokenRoot.java"));
}

#[test]
fn fresh_property_covariance_does_not_accept_a_different_return_type() {
    let fixture = Fixture::new(&[(
        "Contracts.kt",
        "package sample\n\
             open class Base {}\n\
             class Narrow : Base() {}\n\
             interface Root {\n\
                 val item: Base\n\
                     get() = Base()\n\
             }\n\
             interface Provider : Root {\n\
                 override val item: Narrow\n\
                     get() = Narrow()\n\
             }\n\
             class Child : Provider {\n\
                 suspend fun retain(): Int {\n\
                     return 1\n\
                 }\n\
             }\n",
    )]);

    let output = fixture.migrate();
    assert_success(&output);
    assert!(fixture.0.join("Contracts.kt").is_file());
    assert!(!fixture.has_java("Contracts.java"));
}

#[test]
fn fresh_interface_comparison_through_declared_properties_translates() {
    let fixture = Fixture::new(&[
        (
            "Keyed.kt",
            "package sample\ninterface Keyed : Comparable<Keyed> {\n val key: String\n override fun compareTo(other: Keyed): Int = this.key.compareTo(other.key)\n}\n",
        ),
        (
            "Entry.kt",
            "package sample\ndata class Entry(override val key: String) : Keyed\n",
        ),
    ]);
    let output = fixture.migrate();
    assert_success(&output);
    let keyed = fixture
        .java("Keyed.java")
        .expect("property comparison interface translates");
    assert!(keyed.contains("compareTo("), "{keyed}");
    assert!(
        fixture.has_java("Entry.java"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
