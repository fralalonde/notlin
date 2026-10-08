//! CLI regression for in-place migration idempotence: stripping neighboring
//! declarations must not make a previously retained annotated interface
//! newly translatable only on a later invocation.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture_root() -> PathBuf {
    std::env::temp_dir().join(format!("notlin-idempotence-{}", std::process::id()))
}

fn run_notlin(root: &Path, input: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(input.parent().unwrap())
        .output()
        .expect("run notlin CLI")
}

fn source_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|ext| ext == "kt" || ext == "java")
        {
            files.insert(path.file_name().unwrap().into(), fs::read(path).unwrap());
        }
    }
    files
}

#[test]
fn second_in_place_invocation_preserves_source_tree() {
    let root = fixture_root();
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let input = root.join("Types.kt");
    fs::write(
        &input,
        r#"package neutral.mapping

interface Plain {
    val code: String
}

@TypeInfo(
    use = TypeInfo.Id.NAME,
    include = TypeInfo.As.PROPERTY,
    property = "kind")
@SubTypes(
    SubTypes.Type(value = Leaf::class),
    SubTypes.Type(value = Other::class)
)
interface Tagged {
    val code: String
    val fallback: String
        get() = "none"
}

class Leaf
class Other
class Holder(val tag: Tagged)

class Converter {
    fun convert(value: String?): String? = value?.let { it.trim() }
}

@JsonNaming(PropertyNamingStrategies.UpperCamelCaseStrategy::class)
data class Request(val value: String)
"#,
    )
    .unwrap();

    let first = run_notlin(&root, &input);
    assert!(
        first.status.success(),
        "first notlin invocation failed:\n{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let after_first = source_tree(&root);

    let second = run_notlin(&root, &input);
    assert!(
        second.status.success(),
        "second notlin invocation failed:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let after_second = source_tree(&root);
    assert_eq!(
        after_second.keys().collect::<Vec<_>>(),
        after_first.keys().collect::<Vec<_>>(),
        "second invocation changed the .kt/.java source file set"
    );
    assert_eq!(
        after_second,
        after_first,
        "second invocation changed source bytes; stderr:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn blocker_comment_does_not_split_a_class_annotation_block() {
    let root = fixture_root().with_extension("class-annotations");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let input = root.join("Entities.kt");
    let outside = root.join("Outside.kt");
    fs::write(
        &input,
        r#"package neutral.jpa

import jakarta.persistence.*
import java.io.Serializable

interface Hub
value class Prior(val raw: Int)

@Entity
@Table(name = "ENTRIES", indexes = [Index(columnList = "owner, lang")])
@IdClass(EntryId::class)
data class Entry(@Id var owner: String, @Id var lang: String)

data class EntryId(var owner: String? = null, var lang: String? = null) : Serializable
"#,
    )
    .unwrap();
    fs::write(
        &outside,
        r#"package neutral.jpa

value class External(val raw: Int) : Hub
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(&input)
        .output()
        .expect("run notlin CLI");
    assert!(
        output.status.success(),
        "notlin invocation failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let residue = fs::read_to_string(&input).unwrap();
    assert!(!residue.contains("data class Entry"), "{residue}");
    let entry = fs::read_to_string(root.join("Entry.java")).unwrap();
    assert!(entry.contains("@Table(name = \"ENTRIES\""), "{entry}");
    assert!(entry.contains("@IdClass(EntryId.class)"), "{entry}");
    assert!(entry.contains("class Entry"), "{entry}");
    assert!(
        root.join("EntryId.java").exists(),
        "the independent ID class should still translate"
    );

    let after_first = source_tree(&root);
    let second = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(&input)
        .output()
        .expect("run notlin CLI a second time");
    assert!(
        second.status.success(),
        "second notlin invocation failed:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        source_tree(&root),
        after_first,
        "second invocation changed the retained annotated entity"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn nested_class_annotation_wrappers_preserve_every_annotation_and_argument() {
    let root = fixture_root().with_extension("nested-class-annotations");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let input = root.join("Reference.kt");
    let source = r#"package sample.persistence

import jakarta.persistence.Entity
import jakarta.persistence.Table
import org.hibernate.annotations.Immutable
import org.hibernate.annotations.SQLRestriction

@Entity
@Table(name = "RESOURCE_REF")
@Immutable
@SQLRestriction("tenant_id <> 'ignored' and active = true")
class ResourceReference(val code: String)

interface Repository<T>
interface ResourceRepository : Repository<ResourceReference>
"#;
    let ast = notlin::transpiler::dump_ast(source);
    assert!(
        ast.matches("annotated_expression").count() >= 4,
        "fixture must exercise the nested annotation-wrapper parser shape:\n{ast}"
    );
    fs::write(&input, source).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(&input)
        .output()
        .expect("run notlin CLI");
    assert!(
        output.status.success(),
        "notlin invocation failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let java = fs::read_to_string(root.join("ResourceReference.java")).unwrap();
    for annotation in [
        "@Entity",
        "@Table(name = \"RESOURCE_REF\")",
        "@Immutable",
        "@SQLRestriction(\"tenant_id <> 'ignored' and active = true\")",
    ] {
        assert!(java.contains(annotation), "missing {annotation}:\n{java}");
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn explicit_empty_directory_is_a_successful_noop() {
    let root = fixture_root().with_extension("empty");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(&root)
        .output()
        .expect("run notlin CLI");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = fs::remove_dir_all(root);
}
