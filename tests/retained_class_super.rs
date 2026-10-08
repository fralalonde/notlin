use std::fs;
use std::path::Path;
use std::process::Command;

fn run_workspace(root: &Path) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn workspace(name: &str) -> std::path::PathBuf {
    let root = Path::new("tests").join(format!("tmp_{name}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("jakarta/persistence")).unwrap();
    root
}

fn write_mapped_superclass_stub(root: &Path) {
    fs::write(
        root.join("jakarta/persistence/MappedSuperclass.java"),
        "package jakarta.persistence;\npublic @interface MappedSuperclass {}\n",
    )
    .unwrap();
}

#[test]
fn mapped_superclass_with_jvm_overloads_accepts_exact_typed_super_call() {
    let root = workspace("mapped_superclass_ctor_compatible");
    write_mapped_superclass_stub(&root);
    fs::write(
        root.join("model.kt"),
        r#"package fixture
import jakarta.persistence.MappedSuperclass

@MappedSuperclass
abstract class Parent @kotlin.jvm.JvmOverloads constructor(
    val id: String,
    val version: Int = 0
) {
    suspend fun keptInKotlin() {}
}

abstract class Child(id: String, token: String) : Parent(id)
"#,
    )
    .unwrap();

    let (success, stderr) = run_workspace(&root);
    assert!(success, "notlin failed:\n{stderr}");
    let child = fs::read_to_string(root.join("Child.java")).unwrap_or_default();
    assert!(
        child.contains("extends Parent") && child.contains("super(id)"),
        "an exact typed call to the JVM overload should let the Java child extend the retained abstract parent:\n{child}\n{stderr}"
    );
    let kotlin = fs::read_to_string(root.join("model.kt")).unwrap();
    assert!(kotlin.contains("abstract class Parent"), "{kotlin}");
    assert!(
        !kotlin.contains("abstract class Child"),
        "the child should leave the Kotlin snapshot after its superclass proof succeeds:\n{kotlin}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn omitted_parent_default_without_jvm_overloads_is_not_assumed_to_exist_in_java() {
    let root = workspace("mapped_superclass_ctor_no_overloads");
    write_mapped_superclass_stub(&root);
    fs::write(
        root.join("model.kt"),
        r#"package fixture
import jakarta.persistence.MappedSuperclass

@MappedSuperclass
abstract class Parent constructor(
    val id: String,
    val version: Int = 0
) {
    suspend fun keptInKotlin() {}
}

abstract class Child(id: String) : Parent(id)
"#,
    )
    .unwrap();

    let (success, stderr) = run_workspace(&root);
    assert!(success, "notlin failed:\n{stderr}");
    assert!(
        !root.join("Child.java").exists(),
        "Kotlin default arguments do not create a Java overload without @JvmOverloads"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn mismatched_constructor_parameter_types_do_not_prove_java_super_call_compatibility() {
    let root = workspace("mapped_superclass_ctor_type_mismatch");
    write_mapped_superclass_stub(&root);
    fs::write(
        root.join("model.kt"),
        r#"package fixture
import jakarta.persistence.MappedSuperclass

@MappedSuperclass
abstract class Parent @kotlin.jvm.JvmOverloads constructor(
    val id: String,
    val version: Int = 0
) {
    suspend fun keptInKotlin() {}
}

abstract class Child(id: Int) : Parent(id)
"#,
    )
    .unwrap();

    let (success, stderr) = run_workspace(&root);
    assert!(success, "notlin failed:\n{stderr}");
    assert!(
        !root.join("Child.java").exists(),
        "a same-arity but differently typed super call must remain conservative"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn same_package_annotation_cannot_impersonate_mapped_superclass() {
    let root = workspace("mapped_superclass_annotation_shadow");
    fs::write(
        root.join("model.kt"),
        r#"package fixture
annotation class MappedSuperclass

@MappedSuperclass
abstract class Parent @kotlin.jvm.JvmOverloads constructor(
    val id: String,
    val version: Int = 0
) {
    suspend fun keptInKotlin() {}
}

abstract class Child(id: String) : Parent(id)
"#,
    )
    .unwrap();

    let (success, stderr) = run_workspace(&root);
    assert!(success, "notlin failed:\n{stderr}");
    assert!(
        !root.join("Child.java").exists(),
        "a domain annotation with the same simple name must not authorize a superclass proof"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn same_package_annotation_cannot_impersonate_jvm_overloads() {
    let root = workspace("jvm_overloads_annotation_shadow");
    write_mapped_superclass_stub(&root);
    fs::write(
        root.join("model.kt"),
        r#"package fixture
import jakarta.persistence.MappedSuperclass

annotation class JvmOverloads

@MappedSuperclass
abstract class Parent @JvmOverloads constructor(
    val id: String,
    val version: Int = 0
) {
    suspend fun keptInKotlin() {}
}

abstract class Child(id: String) : Parent(id)
"#,
    )
    .unwrap();

    let (success, stderr) = run_workspace(&root);
    assert!(success, "notlin failed:\n{stderr}");
    assert!(
        !root.join("Child.java").exists(),
        "a domain annotation with the same simple name must not prove a JVM overload exists"
    );
    let _ = fs::remove_dir_all(root);
}
