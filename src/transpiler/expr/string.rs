//! String literal translation, incl. `$name` / `${expr}` interpolation.

use super::Expr;

impl<'a, 'u> Expr<'a, 'u> {
    pub(crate) fn string_literal(&mut self, node: tree_sitter::Node) -> String {
        // Walk the grammar's own children. Two forms:
        //  - `${expr}`: a real `interpolation` node — transpile its expression
        //    child so member access/calls keep working.
        //  - `$ident`: the grammar gives `string_content "$"` followed by
        //    `string_content` holding the identifier text (or the rest of the
        //    piece). `$` followed by an identifier-start char pushes that
        //    identifier; any other `$` (template-literal edge) passes through.
        let mut cursor = node.walk();
        let mut parts: Vec<String> = Vec::new();
        let mut dollar_pending = false;
        for child in node.children(&mut cursor) {
            match child.kind() {
                "string_content" => {
                    let raw = self.unit.text(child);
                    if raw.is_empty() {
                        continue;
                    }
                    if raw == "$" {
                        // `$` + identifier continues in the next piece —
                        // defer emission until we see the name.
                        dollar_pending = true;
                        continue;
                    }
                    if dollar_pending
                        && raw
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_alphabetic() || c == '_')
                    {
                        let ident_end = raw
                            .char_indices()
                            .take_while(|(i, c)| *i == 0 || c.is_alphanumeric() || *c == '_')
                            .map(|(i, c)| i + c.len_utf8())
                            .last()
                            .unwrap_or(0);
                        let ident = raw[..ident_end].to_string();
                        let rest = &raw[ident_end..];
                        // The interpolated name splices the implicit `this`
                        // receiver around that identifier: `"... $name ..."`
                        // reads `this.getName()`, NOT a bare unqualified
                        // symbol (which javac rejects on interfaces).
                        let interped = self.transpile_identifier_text(&ident);
                        parts.push(interped);
                        if !rest.is_empty() {
                            parts.push(format!("{:?}", rest));
                        }
                        dollar_pending = false;
                        continue;
                    }
                    if dollar_pending {
                        parts.push("\\\"$\\\"".to_string());
                        dollar_pending = false;
                    }
                    parts.push(format!("{:?}", raw));
                }
                "interpolation" => {
                    dollar_pending = false;
                    let mut icur = child.walk();
                    let expr_node = child
                        .children(&mut icur)
                        .find(|c| c.is_named() && c.kind() != "interpolation");
                    if let Some(expr_node) = expr_node {
                        let java = self.transpile(expr_node);
                        // parenthesize: interpolation splices into a `+`
                        // chain; `x + 1` unparenthesized would change the
                        // expression's meaning (string concat vs arithmetic).
                        parts.push(format!("({})", java));
                    }
                }
                "escape_sequence" => {
                    dollar_pending = false;
                    // The node text IS the source escape (e.g. `\n`, `\\`)
                    // and Java honours the same escapes — quote it, no
                    // re-escaping (Rust {:?} would double every slash).
                    let t = self.unit.text(child);
                    parts.push(format!("\"{}\"", t));
                }
                _ => {}
            }
        }
        if dollar_pending {
            parts.push("\"$\"".to_string());
        }
        if parts.is_empty() {
            "\"\"".to_string()
        } else {
            parts.join(" + ")
        }
    }
}

impl<'a, 'u> Expr<'a, 'u> {
    /// Transpile a bare identifier spliced by string interpolation with the
    /// same rules as a real `identifier` expression node: property accessors
    /// on the implicit `this` (`"...$name..." -> "..." + this.getName()`).
    pub(crate) fn transpile_identifier_text(&mut self, ident: &str) -> String {
        if self.unit.current_object.as_deref() == Some(ident) {
            return format!("{}.INSTANCE", ident);
        }
        if !self.unit.var_types.contains_key(ident)
            && self.unit.var_types.is_empty()
            && let Some(getter) = self.unit.self_getters.get(ident)
        {
            return format!("this.{}()", getter);
        }
        if !self.unit.var_types.contains_key(ident)
            && let Some(owner) = self
                .unit
                .workspace
                .and_then(|w| w.find_property_owner(ident))
        {
            let _ = owner;
            let mut cap = ident.to_string();
            if let Some(first) = cap.chars().next() {
                cap = first.to_uppercase().collect::<String>() + &cap[1..];
            }
            return format!("this.get{}()", cap);
        }
        ident.to_string()
    }
}
