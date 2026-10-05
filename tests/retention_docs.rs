use std::{fs, process::Command};

#[test]
fn multiline_diagnostics_stay_inside_javadoc() {
    let comment = notlin::retention_docs::javadoc(
        "N04DC",
        "annotation: `@Table(\r\n name = \"X\",\n indexes = []\n)`; retained",
        &[],
    );
    assert!(comment.starts_with("/**\n * NOTLIN N04DC:"));
    assert!(comment.ends_with("\n */\n"));
    assert!(comment.contains("name = \"X\""));
    assert_eq!(
        comment,
        notlin::retention_docs::javadoc(
            "N04DC",
            "annotation: `@Table(\n name = \"X\",\n indexes = []\n)`; retained",
            &[],
        )
    );
}

#[test]
fn dependency_javadoc_uses_fully_qualified_links() {
    let comment = notlin::retention_docs::javadoc(
        "N2142",
        "an interface subtype remains Kotlin.",
        &[
            "com.onomatic.tes.coreapi.device.IDevice".into(),
            "com.onomatic.tes.config.device.DeviceEntity".into(),
        ],
    );
    assert!(comment.contains("{@link com.onomatic.tes.coreapi.device.IDevice}"));
    assert!(comment.contains("{@link com.onomatic.tes.config.device.DeviceEntity}"));
    assert!(!comment.contains("markdown"));
}

#[test]
fn marker_interface_translates_beside_retained_implementor_and_documents_the_blocker() {
    let root = std::env::temp_dir().join(format!("notlin-marker-docs-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Marker.kt"), "package p\ninterface Marker\n").unwrap();
    fs::write(
        root.join("Kept.kt"),
        "package p\nvalue class Kept(val raw: Int) : Marker\n",
    )
    .unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_notlin"))
            .args(["--root", root.to_str().unwrap(), "--in-place"])
            .arg(&root)
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(root.join("Marker.java").exists());
    let kept = fs::read_to_string(root.join("Kept.kt")).unwrap();
    assert!(kept.contains("/**\n * NOTLIN NF7FA:"), "{kept}");
    assert!(!root.join(".notlin/retention.md").exists());
    assert!(!root.join(".notlin/blockers").exists());
    assert!(run().status.success());
    assert_eq!(fs::read_to_string(root.join("Kept.kt")).unwrap(), kept);
    fs::remove_dir_all(root).unwrap();
}
