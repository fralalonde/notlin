//! Unrelated workspace properties must not capture lexical bindings.
use std::{fs, path::Path, process::Command};

fn translate(root: &Path) -> String {
    if root.join("out").exists() {
        fs::remove_dir_all(root.join("out")).unwrap();
    }
    let result = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(root)
        .args([
            "--jvm-facts=off",
            "--allow-approximations",
            "-o",
            "out",
            "Exercise.kt",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fs::read_to_string(root.join("out/Exercise.java")).unwrap()
}

#[test]
fn loop_binding_does_not_depend_on_unrelated_workspace_properties() {
    let root = std::env::temp_dir().join(format!("notlin-identifier-loop-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Exercise.kt"), "package neutral.loops\nfun exercise() {\n    for (cursor in 0..2) {\n        println(cursor)\n    }\n}\n").unwrap();
    let baseline = translate(&root);
    assert!(
        baseline.contains("System.out.println(cursor);"),
        "{baseline}"
    );
    fs::write(
        root.join("Unrelated.kt"),
        "package neutral.unrelated\nclass Unrelated(val cursor: Int)\n",
    )
    .unwrap();
    let indexed = translate(&root);
    fs::remove_dir_all(root).unwrap();
    assert_eq!(
        indexed, baseline,
        "adding an unrelated property changed loop-variable binding"
    );
}

#[test]
fn interpolated_loop_binding_does_not_become_an_implicit_property() {
    let root =
        std::env::temp_dir().join(format!("notlin-identifier-template-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Exercise.kt"), "package neutral.templates\nfun exercise() {\n    for (cursor in 0..2) {\n        println(\"count=$cursor\")\n    }\n}\n").unwrap();
    let baseline = translate(&root);
    fs::write(
        root.join("Unrelated.kt"),
        "package neutral.other\nclass Unrelated(val cursor: Int)\n",
    )
    .unwrap();
    let indexed = translate(&root);
    fs::remove_dir_all(root).unwrap();
    assert_eq!(
        indexed, baseline,
        "string templates must respect loop-variable binding too"
    );
}
