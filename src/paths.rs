//! Paths as they should read in output: relative to the translation root.
//!
//! Everything notlin prints — diagnostics, log lines, the run summary, the
//! retention table — goes through [`display`], so a path never depends on where
//! the tool was invoked from, on the canonicalized `\\?\` form Windows hands
//! back, or on the drive letter of the checkout.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// The translation root, once set, is the base every printed path is relative
/// to. Set it before any output is produced.
pub fn set_root(root: &Path) {
    let _ = ROOT.set(root.to_path_buf());
}

/// The translation root, if one was set.
pub fn root() -> Option<&'static Path> {
    ROOT.get().map(PathBuf::as_path)
}

/// Render a path for humans: relative to the translation root, without the
/// Windows verbatim prefix, with forward slashes.
///
/// A path outside the root stays absolute (the caller deliberately reached
/// out of the workspace); a path already relative is printed as given.
pub fn display(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    let text = path.to_string_lossy();
    // `\\?\D:\work\mod\src\x.kt` is what `canonicalize` and `WalkDir` produce;
    // strip it so the root comparison below can match at all.
    let stripped = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let path = Path::new(stripped);
    let relative = root()
        .and_then(|root| path.strip_prefix(root).ok())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|cwd| path.strip_prefix(cwd).ok())
        });
    match relative {
        Some(relative) if !relative.as_os_str().is_empty() => {
            relative.to_string_lossy().replace('\\', "/")
        }
        _ => stripped.replace('\\', "/"),
    }
}
