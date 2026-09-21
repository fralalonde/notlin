//! Statement translation.
use crate::transpiler::expr::Expr;
use crate::transpiler::java::JavaOut;
use crate::transpiler::kt;
use crate::transpiler::unit::Unit;

pub struct Stmt<'a, 'u> {
    pub unit: &'a mut Unit<'u>,
}

impl<'a, 'u> Stmt<'a, 'u> {
    pub fn transpile(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) {
        match stmt.kind() {
            "property_declaration" => self.transpile_local_property(stmt, out),
            "assignment" => self.transpile_assignment(stmt, out),
            "return_expression" => self.transpile_return(stmt, out),
            "if_expression" => self.transpile_if(stmt, out),
            "for_statement" => self.transpile_for(stmt, out),
            "while_statement" => self.transpile_while(stmt, out),
            "do_while_statement" => self.transpile_do_while(stmt, out),
            "call_expression" => {
                let mut e = Expr { unit: self.unit };
                let java = e.transpile(stmt);
                out.line(format!("{};", java));
            }
            "block" => {
                out.open("");
                let mut cursor = stmt.walk();
                for child in stmt.children(&mut cursor) {
                    if child.is_named() {
                        self.transpile(child, out);
                    }
                }
                out.close();
            }
            _ => {
                // Fall back to expression statement
                let mut e = Expr { unit: self.unit };
                let java = e.transpile(stmt);
                if !java.is_empty() {
                    out.line(format!("{};", java));
                } else {
                    self.unit.diags.warn_approx(
                        stmt,
                        self.unit.file,
                        format!("statement kind '{}' not translated", stmt.kind()),
                    );
                }
            }
        }
    }

    fn transpile_local_property(&mut self, decl: tree_sitter::Node, out: &mut JavaOut) {
        let is_val = kt::child(decl, "val").is_some();
        let _ = is_val; // locals are always effectively mutable in Java unless final
        let vd = kt::child(decl, "variable_declaration");
        let name = vd
            .and_then(|v| kt::child(v, "identifier"))
            .map(|n| self.unit.text(n).to_string())
            .unwrap_or_else(|| "local".to_string());
        let declared_ty = vd
            .and_then(|v| kt::child(v, "user_type").or_else(|| kt::child(v, "nullable_type")))
            .map(|t| kt::java_type_ann(t, self.unit.source, self.unit.annots));

        let init = self.unit.property_initializer(decl);
        let mut e = Expr { unit: self.unit };
        let init_java = init.map(|i| e.transpile(i));

        let ty = declared_ty.unwrap_or_else(|| match init {
            Some(i) => self.unit.infer_type(i),
            None => "var".to_string(),
        });

        if ty == "var" {
            out.line(format!(
                "var {}{};",
                name,
                init_java.map(|j| format!(" = {}", j)).unwrap_or_default()
            ));
        } else {
            out.line(format!(
                "{} {}{};",
                ty,
                name,
                init_java.map(|j| format!(" = {}", j)).unwrap_or_default()
            ));
        }
    }

    fn transpile_assignment(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) {
        let left = kt::field(stmt, "left");
        let right = kt::field(stmt, "right");
        let op = kt::field(stmt, "operator").map(|o| self.unit.text(o).to_string());
        match (left, right, op) {
            (Some(l), Some(r), Some(op)) => {
                let mut e = Expr { unit: self.unit };
                let l_java = e.transpile_target(l);
                let r_java = e.transpile(r);
                out.line(format!("{} {} {};", l_java, op, r_java));
            }
            _ => {
                self.unit
                    .diags
                    .warn_approx(stmt, self.unit.file, "malformed assignment");
            }
        }
    }

    fn transpile_return(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) {
        let mut cursor = stmt.walk();
        let exprs: Vec<_> = stmt
            .children(&mut cursor)
            .filter(|c| c.is_named())
            .collect();
        let mut e = Expr { unit: self.unit };
        if exprs.is_empty() {
            out.line("return;");
        } else {
            let java = e.transpile(exprs[0]);
            out.line(format!("return {};", java));
        }
    }

    fn transpile_if(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) {
        // if_expression structure: if ( condition ) block [else ...]
        let mut children: Vec<tree_sitter::Node> = Vec::new();
        let mut cursor = stmt.walk();
        for c in stmt.children(&mut cursor) {
            children.push(c);
        }
        let cond = children
            .iter()
            .find(|c| c.kind() == "parenthesized" || c.kind() == "expression")
            .copied();
        let blocks: Vec<tree_sitter::Node> = children
            .iter()
            .filter(|c| c.kind() == "block" || c.kind() == "control_structure_body")
            .copied()
            .collect();

        let cond_java = cond
            .map(|c| {
                let mut e = Expr { unit: self.unit };
                e.transpile(unwrap_parens(c))
            })
            .unwrap_or_else(|| "true".to_string());

        out.open(format!("if ({})", cond_java));
        if let Some(b) = blocks.first() {
            self.transpile_body(*b, out);
        }
        out.close();

        // else / else-if
        let else_idx = children.iter().position(|c| c.kind() == "else");
        if let Some(ei) = else_idx {
            let after = &children[ei + 1..];
            if let Some(else_body) = after.first().copied() {
                if else_body.kind() == "if_expression" {
                    out.buf.push_str("else ");
                    self.transpile_if(else_body, out);
                } else {
                    out.open("else");
                    self.transpile_body(else_body, out);
                    out.close();
                }
            }
        }
    }

    fn transpile_body(&mut self, body: tree_sitter::Node, out: &mut JavaOut) {
        if body.kind() == "block" {
            let mut cursor = body.walk();
            for child in body.children(&mut cursor) {
                if child.is_named() {
                    self.transpile(child, out);
                }
            }
        } else {
            // single statement body
            self.transpile(body, out);
        }
    }

    fn transpile_for(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) {
        let var = kt::child(stmt, "variable_declaration")
            .and_then(|v| kt::child(v, "identifier"))
            .map(|n| self.unit.text(n).to_string())
            .unwrap_or_else(|| "i".to_string());
        // The iterable is the named node between `in` and `)`.
        let iterable = Self::find_iterable(stmt);
        let body = kt::child(stmt, "block").or_else(|| kt::child(stmt, "control_structure_body"));

        match iterable {
            Some(iter) => {
                // range? `a..b` -> for (int i = a; i <= b; i++)  or  until  /  downTo
                let iter_java = self.translate_iterable(iter, &var);
                out.open(format!("for ({}", iter_java));
                if let Some(b) = body {
                    self.transpile_body(b, out);
                }
                out.close();
            }
            None => {
                self.unit
                    .diags
                    .warn_approx(stmt, self.unit.file, "for loop without iterable");
            }
        }
    }

    /// Find the iterable expression in a for_statement: the named node directly
    /// after the `in` keyword token.
    fn find_iterable<'t>(stmt: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
        let mut cursor = stmt.walk();
        let kids: Vec<tree_sitter::Node<'t>> = stmt.children(&mut cursor).collect();
        for (i, c) in kids.iter().enumerate() {
            if c.kind() == "in" {
                return kids.get(i + 1).copied().filter(|n| n.is_named());
            }
        }
        None
    }

    /// Translate a for-iterable into a Java for-header fragment.
    fn translate_iterable(&mut self, iter: tree_sitter::Node, var: &str) -> String {
        // The iterable node may itself be a range_expression or wrap one.
        let range = if iter.kind() == "range_expression" {
            Some(iter)
        } else {
            kt::child(iter, "range_expression")
        };
        if let Some(range) = range {
            return self.translate_range(range, var);
        }
        // downTo/step chains arrive as infix_expression with operator words.
        if iter.kind() == "infix_expression" {
            if let Some(java) = self.translate_infix_range(iter, var) {
                return java;
            }
        }
        // general iterable: for (var x : expr)
        let mut e = Expr { unit: self.unit };
        let java = e.transpile(iter);
        format!("var {} : {})", var, java)
    }

    /// Handle infix_expression range chains: `start downTo end [step n]`,
    /// `start .. end`, `start until end`. Returns a Java for-header fragment.
    /// `step n` is approximated with the matching ++/-- direction (warned).
    fn translate_infix_range(&mut self, expr: tree_sitter::Node, var: &str) -> Option<String> {
        let mut cursor = expr.walk();
        let kids: Vec<_> = expr.children(&mut cursor).collect();
        // Grammar shape: operands and range-function identifiers are ALL named
        // children of infix_expression, e.g.
        //   infix( infix(10, downTo, 0), step, 2 )
        // Collect the operator words present, then the operand nodes in order.
        let mut ops: Vec<String> = Vec::new();
        let mut operands: Vec<tree_sitter::Node> = Vec::new();
        for c in &kids {
            if c.kind() == "identifier" {
                let t = self.unit.text(*c);
                if t == "downTo" || t == "step" || t == "until" {
                    ops.push(t.to_string());
                    continue;
                }
                operands.push(*c);
            } else if c.is_named() {
                operands.push(*c);
            } else if c.kind() != "(" && c.kind() != ")" {
                // unnamed operator token like ..
                let t = self.unit.text(*c).to_string();
                if t == ".." || t == "..<" {
                    ops.push(t);
                }
            }
        }
        if operands.len() < 2 || ops.is_empty() {
            return None;
        }
        let op = ops[0].clone();
        // `step` chains nest: left operand is the base range; step size is
        // dropped with a warning (Java classic loops can't express it cleanly).
        if ops.contains(&"step".to_string()) {
            match operands[0].kind() {
                "infix_expression" => {
                    self.unit.diags.warn_approx(
                        expr,
                        self.unit.file,
                        "step size approximated with +1/--1 direction; custom step sizes dropped",
                    );
                    return self.translate_infix_range(operands[0], var);
                }
                _ => {
                    self.unit.diags.warn_approx(
                        expr,
                        self.unit.file,
                        format!("bare step on non-range operand not supported"),
                    );
                    return None;
                }
            }
        }
        let (loop_op, cond_op) = match op.as_str() {
            "downTo" => ("--", ">="),
            "until" => ("++", "<"),
            ".." => ("++", "<="),
            "..<" => ("++", "<"),
            _ => {
                self.unit.diags.warn_approx(
                    expr,
                    self.unit.file,
                    format!("unsupported range op {:?}", op),
                );
                return None;
            }
        };
        let mut e = Expr { unit: self.unit };
        let lo_java = e.transpile(operands[0]);
        let hi_java = e.transpile(operands[1]);
        Some(format!(
            "int {v} = {lo}; {v} {cond} {hi}; {v}{step})",
            v = var,
            lo = lo_java,
            cond = cond_op,
            hi = hi_java,
            step = loop_op
        ))
    }

    fn translate_range(&mut self, range: tree_sitter::Node, var: &str) -> String {
        let mut cursor = range.walk();
        let kids: Vec<_> = range.children(&mut cursor).collect();
        let lo = kids.iter().find(|c| c.is_named()).copied();
        let hi = kids.iter().rev().find(|c| c.is_named()).copied();
        let op = kids
            .iter()
            .find(|c| !c.is_named() && c.kind() != "(" && c.kind() != ")")
            .map(|c| self.unit.text(*c));
        let mut e = Expr { unit: self.unit };
        let lo_java = lo.map(|l| e.transpile(l)).unwrap_or_default();
        let hi_java = hi.map(|h| e.transpile(h)).unwrap_or_default();
        match op {
            Some("..") => format!(
                "int {} = {}; {} <= {}; {}++)",
                var, lo_java, var, hi_java, var
            ),
            Some("..<") => format!(
                "int {} = {}; {} < {}; {}++)",
                var, lo_java, var, hi_java, var
            ),
            _ => {
                self.unit.diags.warn_approx(
                    range,
                    self.unit.file,
                    format!("unsupported range op {:?}", op),
                );
                format!("int {} = 0; {} < 0; {}++)", var, var, var)
            }
        }
    }

    fn transpile_while(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) {
        let cond = kt::child(stmt, "expression");
        let body = kt::child(stmt, "block").or_else(|| kt::child(stmt, "control_structure_body"));
        let cond_java = cond
            .map(|c| {
                let mut e = Expr { unit: self.unit };
                e.transpile(unwrap_parens(c))
            })
            .unwrap_or_else(|| "true".to_string());
        out.open(format!("while ({})", cond_java));
        if let Some(b) = body {
            self.transpile_body(b, out);
        }
        out.close();
    }

    fn transpile_do_while(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) {
        let cond = kt::child(stmt, "expression");
        let body = kt::child(stmt, "block").or_else(|| kt::child(stmt, "control_structure_body"));
        let cond_java = cond
            .map(|c| {
                let mut e = Expr { unit: self.unit };
                e.transpile(unwrap_parens(c))
            })
            .unwrap_or_else(|| "true".to_string());
        out.open("do");
        if let Some(b) = body {
            self.transpile_body(b, out);
        }
        out.close();
        out.buf.push_str(&format!(
            "{}while ({});\n",
            "    ".repeat(out.indent),
            cond_java
        ));
    }
}

fn unwrap_parens(node: tree_sitter::Node) -> tree_sitter::Node {
    if node.kind() == "parenthesized" {
        if let Some(inner) = node.children(&mut node.walk()).find(|c| c.is_named()) {
            return inner;
        }
    }
    node
}
