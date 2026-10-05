//! JPA entity annotations commonly combine Kotlin array literals with nested
//! annotations. The Java spelling needs brace arrays and `@` on each nested
//! annotation; keeping only the outer annotation text is not compilable Java.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join(format!("notlin-entity-annotations-{}", std::process::id()))
}

fn run_notlin(root: &Path, input: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(input)
        .output()
        .expect("run notlin CLI");
    assert!(
        output.status.success(),
        "notlin failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn jpa_annotation_arrays_lower_nested_values_to_java_syntax() {
    let root = fixture_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let input = root.join("Entities.kt");
    fs::write(
        &input,
        r#"package neutral.jpa

annotation class Entity
annotation class Index(val name: String, val columnList: String)
annotation class UniqueConstraint(val columnNames: Array<String>)
annotation class Table(
    val indexes: Array<Index> = [],
    val uniqueConstraints: Array<UniqueConstraint> = []
)

@Entity
@Table(
    indexes = [
        Index(name = "ENTRY_OWNER_IDX", columnList = "OWNER_ID"),
    ],
    uniqueConstraints = [
        UniqueConstraint(columnNames = ["TENANT_ID", "ENTRY_ID"]),
    ],
)
class Entry(val tenantId: String, val entryId: String)
"#,
    )
    .unwrap();

    let stderr = run_notlin(&root, &input);
    let entry = fs::read_to_string(root.join("Entry.java")).unwrap();
    assert!(
        entry.contains("@Entity"),
        "entity marker was lost:\n{entry}"
    );
    assert!(
        entry.contains("@Index(name = \"ENTRY_OWNER_IDX\", columnList = \"OWNER_ID\")"),
        "nested index annotation must be prefixed with @:\n{entry}"
    );
    assert!(
        entry.contains("@UniqueConstraint(columnNames = {\"TENANT_ID\", \"ENTRY_ID\"})"),
        "nested unique constraint and its array must use Java syntax:\n{entry}"
    );
    assert!(
        !entry.contains("indexes = [") && !entry.contains("columnNames = ["),
        "Kotlin array brackets remain in emitted Java:\n{entry}"
    );
    assert!(
        !stderr.contains("N04DC"),
        "unexpected annotation retention:\n{stderr}"
    );

    let _ = fs::remove_dir_all(root);
}
