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
                        let mut ident = String::new();
                        let mut rest = raw;
                        for (i, c) in raw.char_indices() {
                            if i == 0 || c.is_alphanumeric() || c == '_' {
                                if i > 0 && !c.is_alphanumeric() && c != '_' {
                                    rest = &raw[i..];
                                    break;
                                }
                                ident.push(c);
                            } else {
                                rest = &raw[i..];
                                break;
                            }
                        }
                        parts.push(ident);
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
