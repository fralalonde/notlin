//! Conservative snapshots of Lombok accessor configuration.
//!
//! Only the absence of every ancestor `lombok.config` proves that Lombok's
//! default accessor rules apply. Existing files are opaque: this module does
//! not attempt to interpret Lombok configuration syntax.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigObservation {
    Absent,
    Present(Vec<u8>),
    Unreadable,
}

/// Cached, fail-closed proof that default Lombok accessor configuration is in
/// effect for Java source paths.
#[derive(Debug, Default)]
pub(crate) struct DefaultLombokConfiguration {
    observations: BTreeMap<PathBuf, ConfigObservation>,
    checked_directories: HashSet<PathBuf>,
}

impl DefaultLombokConfiguration {
    /// Return true only if every ancestor directory has no `lombok.config`.
    /// Observations, including absence, are cached so later filesystem changes
    /// cannot silently change the proof within one planning run.
    pub(crate) fn supports_default_accessors(&mut self, java_file: &Path) -> bool {
        let Some(absolute) = absolute_path(java_file) else {
            return false;
        };
        let Some(path) = canonicalize_existing_ancestor(&absolute) else {
            return false;
        };
        let mut directory = if path.is_dir() {
            path
        } else {
            let Some(parent) = path.parent() else {
                return false;
            };
            parent.to_path_buf()
        };

        loop {
            if !self.inspect_directory(&directory) {
                return false;
            }
            let Some(parent) = directory.parent() else {
                break;
            };
            if parent == directory {
                break;
            }
            directory = parent.to_path_buf();
        }
        true
    }

    /// Return the observed bytes or absence for every readable config path
    /// consulted so far. Unreadable paths are intentionally omitted.
    pub(crate) fn snapshots(&self) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        self.observations
            .iter()
            .filter_map(|(path, observation)| match observation {
                ConfigObservation::Absent => Some((path.clone(), None)),
                ConfigObservation::Present(bytes) => Some((path.clone(), Some(bytes.clone()))),
                ConfigObservation::Unreadable => None,
            })
            .collect()
    }

    fn inspect_directory(&mut self, directory: &Path) -> bool {
        let directory = lexical_normalize(directory);
        if !self.checked_directories.insert(directory.clone()) {
            return self
                .observations
                .get(&directory.join("lombok.config"))
                .is_some_and(|observation| matches!(observation, ConfigObservation::Absent));
        }

        let config = directory.join("lombok.config");
        let observation = match std::fs::metadata(&config) {
            Ok(metadata) if metadata.is_file() => match std::fs::read(&config) {
                Ok(bytes) => ConfigObservation::Present(bytes),
                Err(_) => ConfigObservation::Unreadable,
            },
            Ok(_) => ConfigObservation::Unreadable,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ConfigObservation::Absent,
            Err(_) => ConfigObservation::Unreadable,
        };
        let is_absent = matches!(observation, ConfigObservation::Absent);
        self.observations.insert(config, observation);
        is_absent
    }
}

fn absolute_path(path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    Some(lexical_normalize(&absolute))
}

/// Canonicalize the deepest existing ancestor, then append any missing path
/// components. This supports planned output files that do not exist yet.
fn canonicalize_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut existing = path;
    let mut suffix = Vec::new();
    loop {
        match std::fs::canonicalize(existing) {
            Ok(canonical) => {
                let mut result = canonical;
                for component in suffix.iter().rev() {
                    result.push(component);
                }
                return Some(lexical_normalize(&result));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing.file_name()?;
                suffix.push(name.to_os_string());
                existing = existing.parent()?;
            }
            Err(_) => return None,
        }
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    let mut parents = 0usize;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    parents += 1;
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    for _ in 0..parents {
        normalized.push("..");
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("notlin-lombok-config-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            let canonical = fs::canonicalize(&path).unwrap();
            Self(canonical)
        }

        fn file(&self, relative: &str) -> PathBuf {
            self.0.join(relative)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let temp = fs::canonicalize(std::env::temp_dir()).unwrap();
            if self.0.starts_with(&temp)
                && self
                    .0
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("notlin-lombok-config-"))
            {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn missing_ancestor_configs_prove_defaults_and_are_cached() {
        let fixture = Fixture::new();
        let nested = fixture.file("a/b");
        fs::create_dir_all(&nested).unwrap();
        let mut proof = DefaultLombokConfiguration::default();

        assert!(proof.supports_default_accessors(&nested.join("Generated.java")));
        let snapshots = proof.snapshots();
        assert!(
            snapshots
                .iter()
                .any(|(path, value)| { path == &nested.join("lombok.config") && value.is_none() })
        );

        fs::write(nested.join("lombok.config"), b"config.stopBubbling = true").unwrap();
        assert!(proof.supports_default_accessors(&nested.join("Generated.java")));
        assert!(
            proof
                .snapshots()
                .iter()
                .any(|(path, value)| path == &nested.join("lombok.config") && value.is_none())
        );
    }

    #[test]
    fn any_present_ancestor_config_disables_default_proof() {
        let fixture = Fixture::new();
        let nested = fixture.file("a/b");
        fs::create_dir_all(&nested).unwrap();
        let config = fixture.file("a/lombok.config");
        fs::write(&config, b"lombok.accessors.fluent = true").unwrap();
        let mut proof = DefaultLombokConfiguration::default();

        assert!(!proof.supports_default_accessors(&nested.join("Generated.java")));
        assert!(proof.snapshots().iter().any(|(path, value)| path == &config
            && value.as_deref() == Some(&b"lombok.accessors.fluent = true"[..])));
    }

    #[test]
    fn similarly_named_files_are_not_configuration() {
        let fixture = Fixture::new();
        let nested = fixture.file("a");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("lombok.config.txt"), b"anything").unwrap();
        let mut proof = DefaultLombokConfiguration::default();

        assert!(proof.supports_default_accessors(&nested.join("Generated.java")));
        assert!(
            !proof
                .snapshots()
                .iter()
                .any(|(path, _)| path.file_name().is_some_and(|name| name != "lombok.config"))
        );
    }

    #[test]
    fn unreadable_config_fails_closed_without_snapshot_bytes() {
        let fixture = Fixture::new();
        let config = fixture.file("lombok.config");
        fs::create_dir(&config).unwrap();
        let mut proof = DefaultLombokConfiguration::default();

        assert!(!proof.supports_default_accessors(&fixture.file("Generated.java")));
        assert!(!proof.snapshots().iter().any(|(path, _)| path == &config));
    }

    #[test]
    fn non_existing_output_path_uses_existing_parent_chain() {
        let fixture = Fixture::new();
        let output = fixture.file("not-yet/created/Generated.java");
        let mut proof = DefaultLombokConfiguration::default();

        assert!(proof.supports_default_accessors(&output));
        assert!(proof.snapshots().iter().any(|(path, value)| {
            path == &fixture.file("not-yet/created/lombok.config") && value.is_none()
        }));
    }

    #[cfg(windows)]
    #[test]
    fn directory_named_lombok_config_fails_closed() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.file("lombok.config")).unwrap();
        let mut proof = DefaultLombokConfiguration::default();

        assert!(!proof.supports_default_accessors(&fixture.file("Generated.java")));
        assert!(
            !proof
                .snapshots()
                .iter()
                .any(|(path, _)| { path == &fixture.file("lombok.config") })
        );
    }
}
