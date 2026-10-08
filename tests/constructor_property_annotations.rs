//! Kotlin puts persistence and metadata annotations on the CONSTRUCTOR
//! PROPERTY:
//!
//!     class Ref(val id: String, @Binding(IKind::class) var kind: IKind)
//!
//! `class_params` collected only (is_property, is_mutable, name, java_type), so
//! the annotations were discarded and the field emitter wrote a naked field.
//! For the ORM that is not cosmetic: an interface-typed field with no `@Type`
//! is treated as a basic Java value, and Hibernate cannot infer a JDBC
//! representation for it — a startup failure, not a compile error.

use std::fs;
use std::path::Path;
use std::process::Command;

fn emitted(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join(format!("{name}.java"))).unwrap_or_default()
}

#[test]
fn annotated_immutable_data_class_uses_fields_instead_of_record_components() {
    let root = Path::new("tests/tmp_scratch_annotated_data_class");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Weight.kt"),
        r#"package neutral.weight

data class Weight(
    @Column(name = "WEIGHT")
    val value: java.math.BigDecimal,
    @Column(name = "WEIGHT_UNIT")
    val unit: String,
)
"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("Weight.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let java = emitted(root, "Weight");
    assert!(
        java.contains("public class Weight"),
        "annotated properties need independently annotatable fields:\n{java}"
    );
    assert!(!java.contains("record Weight"), "{java}");
    for (annotation, field) in [
        ("@Column(name = \"WEIGHT\")", "BigDecimal value;"),
        ("@Column(name = \"WEIGHT_UNIT\")", "String unit;"),
    ] {
        let annotation_at = java.find(annotation).unwrap_or_else(|| {
            panic!("expected {annotation} to survive on the generated field:\n{java}")
        });
        let field_at = java
            .find(field)
            .unwrap_or_else(|| panic!("expected {field}:\n{java}"));
        assert!(
            annotation_at < field_at && field_at - annotation_at < 80,
            "{java}"
        );
    }

    let _ = fs::remove_dir_all(root);
}

#[test]
fn constructor_property_annotations_land_on_the_generated_field() {
    let root = Path::new("tests/tmp_scratch_ctor_annotations");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("types.kt"),
        r#"package neutral.ctorann

interface IKind

class Ref(
    val id: String,
    @Binding(IKind::class)
    @JoinColumn(name = "kind_id")
    var kind: IKind,
)
"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("types.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let java = emitted(root, "Ref");
    let field = java
        .find("private IKind kind;")
        .unwrap_or_else(|| panic!("expected the property to become a field:\n{java}"));

    // Both annotations must sit immediately above THAT field.
    let binding = java
        .find("@Binding(IKind.class)")
        .unwrap_or_else(|| panic!("a `Name::class` argument must lower to `Name.class`:\n{java}"));
    let join = java
        .find("@JoinColumn(name = \"kind_id\")")
        .unwrap_or_else(|| panic!("a Java annotation must pass through verbatim:\n{java}"));
    assert!(
        binding < field && join < field,
        "the annotations must annotate the field, not float elsewhere:\n{java}"
    );

    // The un-annotated property must not collect them.
    // `val id` is immutable, so the field carries `final`.
    let id_field = java
        .find("String id;")
        .unwrap_or_else(|| panic!("expected the un-annotated property to become a field:\n{java}"));
    assert!(
        !java[id_field.saturating_sub(120)..id_field].contains("@Binding"),
        "annotations must attach to their own property only:\n{java}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn parameter_only_annotations_stay_on_the_java_constructor_parameter() {
    let root = Path::new("tests/tmp_scratch_ctor_parameter_annotations");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Parent.java"),
        "package neutral.ctorparam;\nimport java.lang.annotation.*;\n@Target(ElementType.PARAMETER)\npublic @interface Parent { String value(); }\n",
    )
    .unwrap();
    fs::write(
        root.join("types.kt"),
        r#"package neutral.ctorparam

data class Ref(
    @Parent("site")
    val id: String,
)
"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root)
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let java = emitted(root, "Ref");
    assert!(
        java.contains("public Ref(@Parent(\"site\") @NonNull String id)"),
        "a PARAMETER-only annotation must follow Kotlin's use-site selection:\n{java}"
    );
    let field = java
        .find("String id;")
        .unwrap_or_else(|| panic!("expected id field:\n{java}"));
    assert!(
        !java[field.saturating_sub(80)..field].contains("@Parent"),
        "a PARAMETER-only annotation is invalid on a Java field:\n{java}"
    );

    let _ = fs::remove_dir_all(root);
}

/// Annotations on a property written in the CLASS BODY (not the constructor
/// parameter list) reach the field the same way, and `@get:` follows the same
/// rule: onto the generated getter when one is emitted, onto the field when it
/// is not. Dropping it is never an option — the annotation is the metadata the
/// ORM or the serialiser reads.
#[test]
fn body_property_annotations_follow_the_getter_rule() {
    let root = Path::new("tests/tmp_scratch_body_property_annotations");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("types.kt"),
        r#"package neutral.bodyann

interface IKind

class OnField(val id: String) {
    @Binding(IKind::class)
    var kind: IKind? = null
}

class OnGetter(val id: String) {
    @get:Binding(IKind::class)
    var kind: IKind? = null
}

class DeclaredGetter(val id: String) {
    @get:Binding(IKind::class)
    var kind: IKind? = null

    fun getKind(): IKind? = kind
}
"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("types.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let field = emitted(root, "OnField");
    let kind_at = field
        .find("IKind kind")
        .unwrap_or_else(|| panic!("expected the kind field:\n{field}"));
    let mark = field
        .find("@Binding(IKind.class)")
        .unwrap_or_else(|| panic!("a body-property annotation must survive:\n{field}"));
    assert!(
        mark < kind_at && kind_at - mark < 60,
        "it belongs on ITS field, not just somewhere in the class:\n{field}"
    );

    let getter = emitted(root, "OnGetter");
    let getter_at = getter
        .find("public @Nullable IKind getKind()")
        .unwrap_or_else(|| panic!("expected the generated getter:\n{getter}"));
    let mark = getter
        .find("@Binding(IKind.class)")
        .unwrap_or_else(|| panic!("`@get:` must not be dropped:\n{getter}"));
    assert!(
        mark < getter_at && getter_at - mark < 60,
        "`@get:` belongs on the getter itself:\n{getter}"
    );

    // The class declares its own `getKind()`, so no getter is generated and a
    // `@get:` annotation would have nowhere to land: it stays on the field.
    let declared = emitted(root, "DeclaredGetter");
    assert!(
        declared.contains("@Binding(IKind.class)"),
        "`@get:` must not be dropped when the getter is user-declared:\n{declared}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn positional_jpa_column_name_is_named_for_java() {
    let root = Path::new("tests/tmp_scratch_positional_column");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Entity.kt"),
        r#"package neutral.column

data class Entity(@Column("CLASS") val kind: String)
"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("Entity.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let java = emitted(root, "Entity");
    assert!(java.contains("@Column(name = \"CLASS\")"), "{java}");
    assert!(!java.contains("@Column(\"CLASS\")"), "{java}");

    let _ = fs::remove_dir_all(root);
}
