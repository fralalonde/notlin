use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn function_and_parameter_annotations_survive_on_java_methods() {
    let root = Path::new("tests/tmp_scratch_function_annotations");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Query.java"),
        "import java.lang.annotation.*; @Target(ElementType.METHOD) @interface Query { String value(); }",
    )
    .unwrap();
    fs::write(
        root.join("Param.java"),
        "import java.lang.annotation.*; @Target(ElementType.PARAMETER) @interface Param { String value(); }",
    )
    .unwrap();
    fs::write(
        root.join("Repository.kt"),
        r#"interface Repository {
    @JvmStatic
    @Query("""SELECT item
        FROM Item item
        WHERE item.name = :name""")
    fun find(@Param("name") name: String): String?
}
"#,
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Repository.kt").to_str().unwrap())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "notlin failed:\n{stderr}");

    let java = fs::read_to_string(root.join("Repository.java")).unwrap();
    assert!(
        java.contains(
            "@Query(\"SELECT item\\n        FROM Item item\\n        WHERE item.name = :name\")"
        ),
        "Kotlin raw annotation strings must become valid Java strings:\n{java}"
    );
    assert!(
        java.contains("find(@Param(\"name\") String name)"),
        "parameter annotations must stay attached to their parameters:\n{java}"
    );
    assert!(
        !java.contains("@JvmStatic"),
        "Kotlin JVM control annotations have no Java declaration to emit:\n{java}"
    );

    let _ = fs::remove_dir_all(root);
}
