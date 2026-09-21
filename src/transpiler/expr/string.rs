//! String literal translation, incl. `$name` / `${expr}` interpolation.

use super::Expr;

impl<'a, 'u> Expr<'a, 'u> {
    pub(crate) fn string_literal(&mut self, node: tree_sitter::Node) -> String {
        // Reconstruct from children, converting $name and ${expr} to concatenation.
        let raw = self.unit.text(node);
        if !raw.contains('$') {
            // plain string: escape and return
            return format!("{:?}", raw.trim_matches('"').replace("\\\"", "\""));
        }

        // Interpolated string: split into parts. The grammar gives us
        // string_content pieces; `$` and `${...}` markers.
        let mut out = String::new();
        let mut parts: Vec<String> = Vec::new();
        let mut current = String::new();
        let inner = raw
            .trim_start_matches('"')
            .trim_end_matches('"')
            .to_string();
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '$'
                && let Some(&next) = chars.peek()
            {
                if next == '{' {
                    // ${expr}
                    if !current.is_empty() {
                        parts.push(format!("{:?}", current));
                        current = String::new();
                    }
                    let mut expr = String::new();
                    chars.next(); // consume '{'
                    let mut depth = 1;
                    for ec in chars.by_ref() {
                        if ec == '{' {
                            depth += 1;
                        } else if ec == '}' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        expr.push(ec);
                    }
                    parts.push(format!("({})", expr));
                    continue;
                } else if next.is_alphabetic() || next == '_' {
                    // $identifier
                    if !current.is_empty() {
                        parts.push(format!("{:?}", current));
                        current = String::new();
                    }
                    let mut ident = String::new();
                    while let Some(&ic) = chars.peek() {
                        if ic.is_alphanumeric() || ic == '_' {
                            ident.push(ic);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    parts.push(ident);
                    continue;
                }
            }
            current.push(c);
        }
        if !current.is_empty() {
            parts.push(format!("{:?}", current));
        }
        if parts.is_empty() {
            out.push_str("\"\"");
        } else {
            out.push_str(&parts.join(" + "));
        }
        out
    }
}
