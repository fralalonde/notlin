use notlin::workspace::SourceIndex;
use std::fs;

/// Build-output mirrors (target/build dirs like `build/k2j/...`) must not be
/// indexed as Java sources:它们 shadow migrated sources and break companion
/// owner resolution.
#[test]
fn build_directories_are_not_indexed() {
    let root = std::env::temp_dir().join(format!("notlin-build-hygiene-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src/main/java/neutral")).unwrap();
    fs::create_dir_all(root.join("build/k2j/neutral")).unwrap();
    fs::write(
        root.join("src/main/java/neutral/Kind.kt"),
        "package neutral\ninterface Kind {\n    companion object {\n        fun of(x: Int): Int = x\n    }\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("build/k2j/neutral/Kind.java"),
        "package neutral;\npublic final class Kind {\n    public static Integer of(Integer x) {\n        return x;\n    }\n}\n",
    )
    .unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let build_javas = index
        .java_files()
        .filter(|f| f.path.starts_with(root.join("build")))
        .count();
    assert_eq!(build_javas, 0, "build/ output must not enter the index");
    // the same-name declaration from the build mirror must not resolve as Java
    let owner = index.find_static_member("Kind", "of");
    assert!(
        owner
            .map(|d| d.language != notlin::workspace::SourceLanguage::Java)
            .unwrap_or(true),
        "no build/network Java twin of {owner:?} allowed to win resolution"
    );
    let _ = fs::remove_dir_all(root);
}
