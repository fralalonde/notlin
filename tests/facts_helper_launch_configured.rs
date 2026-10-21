//! The configured half of the helper launch contract: once the run knows its
//! Kotlin sources and classpath, the *full* jar is launched with `--kotlin` and
//! the child gets both environment entries. The slim jar never gets the flag.

use notlin::facts::client::FactsClient;
use notlin::facts::runtime;
use std::fs;
use std::path::{Path, PathBuf};

fn args_of(jar: &str) -> Vec<String> {
    FactsClient::jar_command(Path::new(jar))
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

fn env_of(jar: &str, name: &str) -> Option<PathBuf> {
    FactsClient::jar_command(Path::new(jar))
        .get_envs()
        .find(|(key, _)| *key == name)
        .and_then(|(_, value)| value.map(PathBuf::from))
}

#[test]
fn configured_sources_enable_kotlin_only_for_the_full_jar() {
    let root = std::env::temp_dir().join(format!("notlin-launch-{}", std::process::id()));
    let nested = root.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let first = nested.join("One.kt");
    let second = nested.join("Two.kt");
    fs::write(&first, "package a\n").unwrap();
    fs::write(&second, "package a\n").unwrap();
    let jars = root.join("jars");
    fs::create_dir_all(&jars).unwrap();

    // Two files in one directory are one source root; a directory entry is
    // itself a root. Deduplicated, in input order.
    runtime::configure_kotlin_sources([first.clone(), second.clone(), nested.clone()]);
    runtime::configure_classpath([jars.clone()]);

    let full = args_of("/x/notlin-jvm-facts-full.jar");
    assert!(full.contains(&"--kotlin".to_string()), "{full:?}");
    let slim = args_of("/x/notlin-jvm-facts.jar");
    assert!(!slim.contains(&"--kotlin".to_string()), "{slim:?}");

    let sources = env_of("/x/notlin-jvm-facts-full.jar", "NOTLIN_KOTLIN_SOURCES")
        .expect("child carries the source roots");
    let canonical_nested = fs::canonicalize(&nested).unwrap();
    assert_eq!(sources, canonical_nested);

    let classpath = env_of("/x/notlin-jvm-facts-full.jar", "NOTLIN_CLASSPATH")
        .expect("child carries the configured classpath");
    let entries = std::env::split_paths(&classpath).collect::<Vec<_>>();
    assert_eq!(entries, vec![jars.clone()]);
}
