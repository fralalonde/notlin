//! Constructor-property metadata must survive explicit Java constructor
//! emission. In particular, Jackson needs to distinguish a data class's
//! canonical property constructor from secondary and default overloads.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root_for(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("notlin-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    root
}

fn write_default_annotation(root: &Path) {
    let package = root.join("neutral/properties");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("Default.java"),
        "package neutral.properties;\n\n\
         import java.lang.annotation.ElementType;\n\
         import java.lang.annotation.Retention;\n\
         import java.lang.annotation.RetentionPolicy;\n\
         import java.lang.annotation.Target;\n\n\
         @Retention(RetentionPolicy.RUNTIME)\n\
         @Target(ElementType.CONSTRUCTOR)\n\
         public @interface Default {}\n",
    )
    .unwrap();
}

fn translate(root: &Path, source: &Path, lombok: bool) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(source.to_str().unwrap());
    if lombok {
        command.arg("--lombok");
    }
    let output = command.output().expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");
}

fn assert_primary_contract(java: &str, primary: &str, other_constructors: &[&str]) {
    let marker = "@java.beans.ConstructorProperties({\"objectId\", \"lookupId\", \"revision\"})";
    assert_eq!(
        java.matches(marker).count(),
        1,
        "the primary property contract must appear exactly once:\n{java}"
    );
    let marker_at = java.find(marker).unwrap();
    let primary_at = java
        .find(primary)
        .unwrap_or_else(|| panic!("missing canonical constructor {primary}:\n{java}"));
    assert!(
        marker_at < primary_at,
        "ConstructorProperties must annotate the canonical constructor:\n{java}"
    );
    let between = &java[marker_at + marker.len()..primary_at];
    assert!(
        between.trim().is_empty(),
        "ConstructorProperties must be attached to the canonical constructor:\n{java}"
    );
    for signature in other_constructors {
        let at = java
            .find(signature)
            .unwrap_or_else(|| panic!("missing secondary/default overload {signature}:\n{java}"));
        assert!(
            at > primary_at,
            "expected the canonical constructor before {signature}:\n{java}"
        );
        assert!(
            !java[at.saturating_sub(marker.len() + 8)..at].contains(marker),
            "secondary/default overload must not receive ConstructorProperties:\n{java}"
        );
    }
    let primary_prefix = &java[..primary_at];
    assert!(
        primary_prefix.contains("@Default")
            || primary_prefix.contains("@neutral.properties.Default"),
        "the source @Default marker must remain on the primary constructor:\n{java}"
    );
}

#[test]
fn lombok_explicit_primary_constructor_keeps_property_metadata_with_overloads() {
    let root = root_for("constructor-properties-lombok");
    write_default_annotation(&root);
    let source = root.join("neutral/properties/GenericId.kt");
    fs::write(
        &source,
        r#"package neutral.properties

import java.util.UUID

data class GenericId @Default @JvmOverloads constructor(
    val objectId: UUID,
    val lookupId: String,
    val revision: Int = 0
) {
    constructor(lookupId: String) : this(UUID.randomUUID(), lookupId)
    override fun toString(): String = "$objectId/$lookupId"
}
"#,
    )
    .unwrap();

    translate(&root, &source, true);
    let java =
        fs::read_to_string(root.join("neutral/properties/GenericId.java")).unwrap_or_default();
    assert_primary_contract(
        &java,
        "public GenericId(UUID objectId, String lookupId, int revision)",
        &[
            "public GenericId(UUID objectId, String lookupId)",
            "public GenericId(String lookupId)",
        ],
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn explicit_data_constructor_keeps_property_metadata_with_overloads() {
    let root = root_for("constructor-properties-explicit");
    write_default_annotation(&root);
    let source = root.join("neutral/properties/ExplicitId.kt");
    fs::write(
        &source,
        r#"package neutral.properties

import java.util.UUID

data class ExplicitId @Default @JvmOverloads constructor(
    val objectId: UUID,
    val lookupId: String,
    var revision: Int = 0
) {
    var label: String = "ready"
    constructor(lookupId: String, revision: Int) : this(UUID.randomUUID(), lookupId, revision)
    override fun toString(): String = "$objectId/$lookupId/$revision/$label"
}
"#,
    )
    .unwrap();

    translate(&root, &source, false);
    let java =
        fs::read_to_string(root.join("neutral/properties/ExplicitId.java")).unwrap_or_default();
    assert_primary_contract(
        &java,
        "public ExplicitId(UUID objectId, String lookupId, int revision)",
        &[
            "public ExplicitId(UUID objectId, String lookupId)",
            "public ExplicitId(String lookupId, int revision)",
        ],
    );
    assert!(
        java.contains("this.label = \"ready\";"),
        "the fixture must exercise explicit Java initialization:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn existing_constructor_properties_annotation_is_not_duplicated() {
    for alias in [false, true] {
        let root = root_for(if alias {
            "constructor-properties-alias"
        } else {
            "constructor-properties-existing"
        });
        write_default_annotation(&root);
        let source = root.join("neutral/properties/AnnotatedId.kt");
        fs::write(
        &source,
        r#"package neutral.properties

import java.beans.ConstructorProperties
import java.util.UUID

data class AnnotatedId @Default @ConstructorProperties(value = ["objectId", "lookupId", "revision"]) @JvmOverloads constructor(
    val objectId: UUID,
    val lookupId: String,
    val revision: Int = 0
) {
    override fun toString(): String = "$objectId/$lookupId/$revision"
}
"#.replace("import java.beans.ConstructorProperties", if alias { "import java.beans.ConstructorProperties as CP" } else { "import java.beans.ConstructorProperties" })
    .replace("@ConstructorProperties(", if alias { "@CP(" } else { "@ConstructorProperties(" }),
    )
    .unwrap();

        translate(&root, &source, true);
        let java = fs::read_to_string(root.join("neutral/properties/AnnotatedId.java"))
            .unwrap_or_default();
        assert_eq!(
            java.matches("@ConstructorProperties(").count()
                + java.matches("@CP(").count()
                + java.matches("@java.beans.ConstructorProperties(").count(),
            1,
            "an explicit source annotation must be preserved without an emitted duplicate:\n{java}"
        );
        let primary = "public AnnotatedId(UUID objectId, String lookupId, int revision)";
        let primary_at = java
            .find(primary)
            .unwrap_or_else(|| panic!("missing canonical constructor:\n{java}"));
        let annotation_at = java
            .find("@ConstructorProperties(")
            .or_else(|| java.find("@CP("))
            .or_else(|| java.find("@java.beans.ConstructorProperties("))
            .unwrap();
        assert!(
            annotation_at < primary_at,
            "the existing annotation must remain attached to the canonical constructor:\n{java}"
        );
        assert!(
            java[..primary_at].contains("@Default"),
            "@Default was not preserved on the primary constructor:\n{java}"
        );
        let _ = fs::remove_dir_all(root);
    }
}
