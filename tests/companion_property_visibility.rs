use clap::Parser;
use notlin::workspace::SourceIndex;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

#[test]
fn companion_property_is_public_for_retained_kotlin_consumers() {
    let source = "class Gate {\n    companion object {\n        val DEFAULT: Int = 7\n    }\n}\n";
    let cli = notlin::cli::Cli::parse_from(["notlin", "--lombok", "Gate.kt"]);
    let (files, errors, _warnings, _coverage) =
        notlin::transpiler::transpile(source, &PathBuf::from("Gate.kt"), &cli);
    assert_eq!(errors, 0);
    let output = files
        .into_iter()
        .map(|(_, content)| content)
        .collect::<String>();
    assert!(
        output.contains("public static final int DEFAULT = 7;"),
        "companion property must be directly visible to retained Kotlin:\n{output}"
    );
}

#[test]
fn companion_property_reads_use_the_public_static_field() {
    let source = "class Gate {\n    companion object {\n        val DEFAULT: Int = 7\n    }\n}\nclass Consumer {\n    fun value(): Int = Gate.DEFAULT\n}\n";
    let cli = notlin::cli::Cli::parse_from(["notlin", "--lombok", "Gate.kt"]);
    let (files, errors, _warnings, _coverage) =
        notlin::transpiler::transpile(source, &PathBuf::from("Gate.kt"), &cli);
    assert_eq!(errors, 0);
    let output = files
        .into_iter()
        .map(|(_, content)| content)
        .collect::<String>();
    assert!(
        output.contains("Gate.DEFAULT"),
        "companion reads must use the public static field:\n{output}"
    );
    assert!(
        !output.contains("Gate.getDEFAULT()"),
        "a retained Kotlin owner has no generated Java getter:\n{output}"
    );
}

#[test]
fn retained_kotlin_interface_companion_reads_use_companion_getter() {
    let root =
        std::env::temp_dir().join(format!("notlin-retained-companion-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Contract.kt"),
        "interface Contract {\n    companion object {\n        val DEFAULT: Int = 7\n    }\n}\n",
    )
    .unwrap();
    let consumer = "class Consumer { fun value(): Int = Contract.DEFAULT }\n";
    let consumer_path = root.join("Consumer.kt");
    fs::write(&consumer_path, consumer).unwrap();
    let cli =
        notlin::cli::Cli::parse_from(["notlin", "--root", root.to_str().unwrap(), "Consumer.kt"]);
    let index = SourceIndex::discover(&root).unwrap();
    let retained = HashSet::from(["Contract".to_string()]);
    let (files, errors, _warnings, _coverage) = notlin::transpiler::transpile_with_workspace_hint(
        consumer,
        &consumer_path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
        Some(&retained),
        true,
    );
    assert_eq!(errors, 0);
    let output = files
        .into_iter()
        .map(|(_, content)| content)
        .collect::<String>();
    assert!(
        output.contains("Contract.Companion.getDEFAULT()"),
        "{output}"
    );
    let _ = fs::remove_dir_all(root);
}
