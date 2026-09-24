use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::SourceIndex;
use std::fs;
use std::path::PathBuf;

/// Companion fns on a RETAINED Kotlin declaration are not Java statics:
/// plain ones route via `Owner.Companion.member(...)`; reified inline ones
/// taint the caller. Java owners with the same simple name and a true
/// static are routed as plain statics (Java wins the lookup).
#[test]
fn companion_calls_route_through_companion_holder() {
    let root = std::env::temp_dir().join(format!("notlin-companion-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    fs::write(
        root.join("Owner.kt"),
        "package neutral.kind\ninterface Owner {\n    companion object {\n        fun of(alias: String): String = alias\n    }\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("Reified.kt"),
        "package neutral.kind\ninterface Reified {\n    companion object {\n        inline fun <reified T : Any> make(): T = throw NotImplementedError()\n    }\n}\n",
    )
    .unwrap();

    let caller = "package neutral.kind\nenum class Kind(val id: String) {\n    A(Owner.of(\"a\")),\n    B(\"b\")\n}\n";
    fs::write(root.join("Kind.kt"), caller).unwrap();

    let cli = Cli::parse_from(vec![
        "notlin",
        "--root",
        root.to_string_lossy().as_ref(),
        "Kind.kt",
    ]);
    let index = SourceIndex::discover(&root).unwrap();
    let (files, errors, _warnings, _cov) = transpiler::transpile_with_workspace(
        caller,
        &PathBuf::from("Kind.kt"),
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    let kind = files
        .iter()
        .find(|(n, _)| n == "Kind.java")
        .map(|(_, c)| c.as_str())
        .expect("Kind.java emitted");
    assert!(
        kind.contains("Owner.Companion.of(\"a\")"),
        "plain companion call must route via Companion: {kind}"
    );
    let _ = fs::remove_dir_all(root);
}
#[test]
fn java_owner_wins_companion_routing() {
    // Same simple name owned by BOTH a Java decl (true static `of`) and a
    // Kotlin decl (companion `of`): the caller compiles against the Java
    // static — no Companion hop.
    let root = std::env::temp_dir().join(format!("notlin-companion-java-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    fs::write(
        root.join("Owner.kt"),
        "package neutral.kind\ninterface Owner {\n    companion object {\n        fun of(x: Int): Int = x\n    }\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("Owner2.java"),
        "package neutral.kind;\n\npublic final class Owner {\n    public static Integer of(Integer x) {\n        return x;\n    }\n}\n",
    )
    .unwrap();
    let caller = "package neutral.kind\nenum class Kind(val id: Int) {\n    A(Owner.of(1))\n}\n";
    fs::write(root.join("Kind.kt"), caller).unwrap();

    let cli = Cli::parse_from(vec![
        "notlin",
        "--root",
        root.to_string_lossy().as_ref(),
        "Kind.kt",
    ]);
    let index = SourceIndex::discover(&root).unwrap();
    let (files, errors, _warnings, _cov) = transpiler::transpile_with_workspace(
        caller,
        &PathBuf::from("Kind.kt"),
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    if let Some(kind) = files
        .iter()
        .find(|(n, _)| n == "Kind.java")
        .map(|(_, c)| c.as_str())
    {
        assert!(
            !kind.contains("Owner.Companion"),
            "Java owner must not route through Companion: {kind}"
        );
    }
    let _ = fs::remove_dir_all(root);
}
