//! Compact KDoc placed immediately before declarations retained in Kotlin.

/// Format one retention reason as ordinary KDoc. `links` must contain fully
/// qualified type names so IDEs can navigate each `[qualified.name]` target.
pub fn kdoc(code: &str, reason: &str, links: &[String]) -> String {
    let clean = |value: &str| {
        value
            .replace("*/", "* /")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let reason = clean(reason);
    let mut out = format!("/**\n * NOTLIN {code}: {reason}");
    if !links.is_empty() {
        out.push_str("\n * Retained Kotlin dependencies:");
        for link in links {
            out.push_str(&format!("\n * - [{}]", clean(link)));
        }
    }
    out.push_str("\n */\n");
    out
}
