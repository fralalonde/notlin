//! Compact Javadocs placed immediately before declarations retained in Kotlin.

/// Format one retention reason as ordinary Javadoc. `links` must contain fully
/// qualified Java type names so IDEs can navigate each `{@link ...}` target.
pub fn javadoc(code: &str, reason: &str, links: &[String]) -> String {
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
            out.push_str(&format!("\n * - {{@link {}}}", clean(link)));
        }
    }
    out.push_str("\n */\n");
    out
}
