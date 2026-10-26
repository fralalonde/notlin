//! What the run tells the helper process to do at launch.
//!
//! Two halves matter and both are child-only (this process never mutates its own
//! environment):
//!
//! * `NOTLIN_KOTLIN_SOURCES` / `NOTLIN_CLASSPATH` come from the run's
//!   configuration, canonicalized to the helper's source *roots*;
//! * `--kotlin` is passed only to a full jar that actually has sources to index
//!   — the helper treats the flag without sources as a startup failure, and a
//!   slim jar has no Kotlin code to enable.
//!
//! This file has nothing configured when it starts (its own process), so the
//! "no sources" half of the matrix lives here; `facts_helper_launch_configured`
//! covers the other half.

use notlin::facts::client::FactsClient;
use notlin::facts::runtime::specifies_full_jar;
use std::ffi::OsStr;
use std::path::Path;

fn args_of(jar: &str) -> Vec<String> {
    FactsClient::jar_command(Path::new(jar))
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

#[test]
fn full_jar_is_detected_by_name_or_override() {
    let full = Path::new("/x/notlin-jvm-facts-full.jar");
    let slim = Path::new("/x/notlin-jvm-facts.jar");
    assert!(specifies_full_jar(full, None));
    assert!(!specifies_full_jar(slim, None));
    // A renamed jar: the override decides, in both directions.
    assert!(specifies_full_jar(slim, Some(OsStr::new("1"))));
    for off in ["0", "false", "off", "no", ""] {
        assert!(!specifies_full_jar(full, Some(OsStr::new(off))), "{off}");
    }
}

#[test]
fn no_configured_sources_means_no_kotlin_flag() {
    if std::env::var_os("NOTLIN_KOTLIN_SOURCES").is_some() {
        eprintln!("NOTLIN_KOTLIN_SOURCES is exported; the no-sources half is covered elsewhere");
        return;
    }
    // The full jar without sources runs as a Track A helper. The client must
    // skip Kotlin queries instead of waiting for an unsupported response.
    assert!(!args_of("/x/notlin-jvm-facts-full.jar").contains(&"--kotlin".to_string()));
    assert!(!args_of("/x/notlin-jvm-facts.jar").contains(&"--kotlin".to_string()));
}
