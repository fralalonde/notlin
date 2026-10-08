//! String literal translation, incl. `$name` / `${expr}` interpolation.

use super::Expr;

impl<'a, 'src, 'tree> Expr<'a, 'src, 'tree> {
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
                    let content = if dollar_pending {
                        dollar_pending = false;
                        format!("${raw}")
                    } else {
                        raw.to_string()
                    };
                    self.push_string_content(&content, node, &mut parts, &mut dollar_pending);
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

    fn push_string_content(
        &mut self,
        content: &str,
        anchor: tree_sitter::Node,
        parts: &mut Vec<String>,
        dollar_pending: &mut bool,
    ) {
        let mut rest = content;
        while let Some(dollar) = rest.find('$') {
            if dollar > 0 {
                parts.push(format!("{:?}", &rest[..dollar]));
            }
            let after = &rest[dollar + 1..];
            let Some(first) = after.chars().next() else {
                *dollar_pending = true;
                return;
            };
            if !(first.is_alphabetic() || first == '_') {
                parts.push("\"$\"".to_string());
                rest = after;
                continue;
            }
            let ident_end = after
                .char_indices()
                .take_while(|(index, character)| {
                    *index == 0 || character.is_alphanumeric() || *character == '_'
                })
                .map(|(index, character)| index + character.len_utf8())
                .last()
                .unwrap_or(first.len_utf8());
            parts.push(self.transpile_identifier_text(&after[..ident_end], anchor));
            rest = &after[ident_end..];
        }
        if !rest.is_empty() {
            parts.push(format!("{:?}", rest));
        }
    }
}

impl<'a, 'src, 'tree> Expr<'a, 'src, 'tree> {
    /// Transpile a bare identifier spliced by string interpolation with the
    /// same rules as a real `identifier` expression node: property accessors
    /// on the implicit `this` (`"...$name..." -> "..." + this.getName()`).
    pub(crate) fn transpile_identifier_text(
        &mut self,
        ident: &str,
        reference: tree_sitter::Node,
    ) -> String {
        if self.unit.current_object.as_deref() == Some(ident) {
            return format!("{}.INSTANCE", ident);
        }
        if !self.unit.var_types.contains_key(ident)
            && let Some(getter) = self.unit.self_getters.get(ident)
        {
            return format!("this.{}()", getter);
        }
        if !self.unit.var_types.contains_key(ident)
            && let Some(literal) = self.inline_same_file_string_const(ident, reference)
        {
            return literal;
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

    pub(crate) fn inline_same_file_string_const(
        &self,
        ident: &str,
        reference: tree_sitter::Node,
    ) -> Option<String> {
        let provider = self.unit.semantic_provider?;
        let file = self
            .unit
            .workspace_file
            .as_deref()
            .unwrap_or(self.unit.file);
        crate::semantics::same_file_string_const_literal(
            self.unit.source,
            file,
            ident,
            reference,
            provider,
        )
    }
}
