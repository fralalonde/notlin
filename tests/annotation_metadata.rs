use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_CASE: AtomicUsize = AtomicUsize::new(0);

fn scratch() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "notlin-annotation-metadata-{}-{}",
        std::process::id(),
        NEXT_CASE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn translate(root: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Annotations.kt"))
        .output()
        .expect("run notlin");
    assert!(
        output.status.success(),
        "notlin failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn canonical_annotation_metadata_maps_and_unrepresentable_target_is_retained() {
    let root = scratch();
    fs::write(
        root.join("Annotations.kt"),
        r#"package generic.annotations

@Target(AnnotationTarget.CLASS, AnnotationTarget.ANNOTATION_CLASS)
@Retention(AnnotationRetention.RUNTIME)
@MustBeDocumented
annotation class Marker(val value: String = "default")

annotation class DefaultRetention

@Target(AnnotationTarget.PROPERTY)
annotation class PropertyOnly

@Marker("ok")
class Marked
"#,
    )
    .unwrap();

    let stderr = translate(&root);
    let marker = fs::read_to_string(root.join("Marker.java")).unwrap_or_default();
    assert!(marker.contains("@java.lang.annotation.Target({java.lang.annotation.ElementType.TYPE, java.lang.annotation.ElementType.ANNOTATION_TYPE})"), "{marker}\n{stderr}");
    assert!(
        marker.contains(
            "@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)"
        ),
        "{marker}"
    );
    assert!(
        marker.contains("@java.lang.annotation.Documented"),
        "{marker}"
    );

    let default_retention =
        fs::read_to_string(root.join("DefaultRetention.java")).unwrap_or_default();
    assert!(
        default_retention.contains(
            "@java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)"
        ),
        "{default_retention}"
    );

    let remaining = fs::read_to_string(root.join("Annotations.kt")).unwrap_or_default();
    assert!(
        remaining.contains("annotation class PropertyOnly"),
        "Kotlin-only PROPERTY target must stay in Kotlin:\n{remaining}\n{stderr}"
    );
    assert!(!root.join("PropertyOnly.java").exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn same_named_source_annotation_is_not_mistaken_for_kotlin_target() {
    let root = scratch();
    fs::write(
        root.join("Annotations.kt"),
        r#"package generic.annotations

annotation class Target(val value: String)

@Target("custom")
annotation class Shadowed
"#,
    )
    .unwrap();

    let stderr = translate(&root);
    let emitted = fs::read_to_string(root.join("Shadowed.java")).unwrap_or_default();
    assert!(
        emitted.contains("@Target(\"custom\")"),
        "recovered declaration must preserve its custom annotation identity/value:\n{emitted}\n{stderr}"
    );
    assert!(
        !emitted.contains("java.lang.annotation.Target") && !emitted.contains("ElementType"),
        "a same-named source annotation is not Kotlin Target metadata:\n{emitted}"
    );
    assert!(stderr.contains("2 declarations"), "{stderr}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn custom_metadata_is_emitted_for_an_established_annotation_declaration() {
    let root = scratch();
    fs::write(
        root.join("Annotations.kt"),
        r#"package generic.annotations
annotation class Target(val value: String)
@Target("custom")
annotation class Marked(val code: String)
"#,
    )
    .unwrap();
    let stderr = translate(&root);
    let java = fs::read_to_string(root.join("Marked.java")).unwrap_or_default();
    assert!(java.contains("@Target(\"custom\")"), "{java}\n{stderr}");
    assert!(!java.contains("java.lang.annotation.ElementType"), "{java}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn metadata_import_aliases_preserve_retention_without_duplicate_annotations() {
    let root = scratch();
    fs::write(
        root.join("Annotations.kt"),
        r#"package generic.annotations
import java.lang.annotation.Retention as Lifetime
import java.lang.annotation.RetentionPolicy
import kotlin.annotation.Target as Destination
import kotlin.annotation.Retention as Keep

annotation class Lifetime(val value: String)

@Lifetime(RetentionPolicy.SOURCE)
annotation class SourceOnly(val value: String)

@Destination(kotlin.annotation.AnnotationTarget.CLASS)
@Keep()
annotation class RuntimeOnly(val value: String)
"#,
    )
    .unwrap();
    let stderr = translate(&root);
    let source_only = fs::read_to_string(root.join("SourceOnly.java")).unwrap_or_default();
    assert!(
        source_only.contains("RetentionPolicy.SOURCE"),
        "{source_only}\n{stderr}"
    );
    assert!(
        !source_only.contains("RetentionPolicy.RUNTIME"),
        "{source_only}"
    );
    assert_eq!(
        source_only.matches("Retention(").count(),
        1,
        "{source_only}"
    );
    let runtime = fs::read_to_string(root.join("RuntimeOnly.java")).unwrap_or_default();
    assert!(
        runtime.contains("ElementType.TYPE") && runtime.contains("RetentionPolicy.RUNTIME"),
        "{runtime}\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn bodyless_meta_annotations_recover_with_siblings_in_one_physical_file() {
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let root = scratch();
    let _cleanup = Cleanup(root.clone());
    fs::write(
        root.join("Annotations.kt"),
        r#"package generic.annotations

@Target(AnnotationTarget.CLASS, AnnotationTarget.ANNOTATION_CLASS)
annotation class Meta

@Meta
@Target(AnnotationTarget.CLASS)
annotation class Marker

@Marker
class Receiver

const val TOKEN: Int = 7
"#,
    )
    .unwrap();
    let stderr = translate(&root);
    for name in ["Meta", "Marker", "Receiver"] {
        assert!(
            root.join(format!("{name}.java")).is_file(),
            "missing {name}.java:\n{stderr}"
        );
    }
    let meta = fs::read_to_string(root.join("Meta.java")).unwrap();
    let marker = fs::read_to_string(root.join("Marker.java")).unwrap();
    assert!(meta.contains("public @interface Meta"), "{meta}");
    assert!(meta.contains("ElementType.ANNOTATION_TYPE"), "{meta}");
    assert!(
        marker.contains("@Meta") && marker.contains("public @interface Marker"),
        "{marker}"
    );
    assert!(marker.contains("RetentionPolicy.RUNTIME"), "{marker}");
    assert!(!stderr.contains("N47BC"), "{stderr}");
}
