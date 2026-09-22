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
                // Post-rewrite: `…collect(toList()).joinToString(sep)` — the
                // map arm collected before a trailing joinToString member;
                // List.joinToString doesn't exist in Java, so swap the pair
                // for a single joining(sep) collect.
                let java = fix_join_tail(&java);
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
                // Statement-level elvis with void arms: `x?.let{...} ?: y`
                // transpiles to a (null-check ? void : void) ternary which
                // javac rejects as a statement — lift to a real if/else.
                let t = java.trim();
                if t.starts_with('(')
                    && t.ends_with(')')
                    && let Some((cond, rest)) = t[1..t.len() - 1].split_once(" ? ")
                    && let Some((a, b)) = rest.split_once(" : ")
                    && java.contains("System.out.println")
                {
                    // A null-tainted (scope-fn) arm emitted the literal
                    // `null` — javac rejects `if (c) null`; use the other
                    // arm's shape and drop the dead branch.
                    if a.trim() == "null" {
                        out.line(format!("if (!({})) {};", cond, b));
                    } else if b.trim() == "null" {
                        out.line(format!("if ({}) {};", cond, a));
                    } else {
                        out.line(format!("if ({}) {} else {};", cond, a, b));
                    }
                } else if !java.is_empty() {
                    // Emitted fragments may already carry a trailing `;`
                    // (if/else lifters) — avoid `;;`.
                    let jt = java.trim_end().trim_end_matches(';');
                    out.line(format!("{};", jt));
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
        // destructuring `val (a, b) = expr`: multi_variable_declaration
        if kt::child(decl, "variable_declaration").is_none()
            && kt::child(decl, "multi_variable_declaration").is_some()
        {
            self.transpile_local_destructuring(decl, out);
            return;
        }
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

        let mut ty = declared_ty.unwrap_or_else(|| match init {
            Some(i) => self.unit.infer_type(i),
            None => "var".to_string(),
        });

        // Record the local's Java type so later statements in this function
        // get receiver context: `arr.size` -> `arr.length` for arrays, known
        // primitive operands in comparisons, member-call inference, etc.
        // `var x = …` locals: infer the initializer's concrete Java type and
        // record THAT (Java `var` reifies to the initializer type; later
        // member inference needs the concrete shape, not the literal "var").
        let mut record_ty = ty.clone();
        if ty == "var" {
            if let Some(i) = init {
                eprintln!("[dbgK] kind={}", i.kind());
                let concrete = self.unit.infer_type(i);
                eprintln!("[dbgC] concrete={concrete:}");
                if concrete != "var" && concrete != "Object" {
                    record_ty = concrete.clone();
                }
            }
        }
        self.unit.var_types.insert(name.clone(), record_ty.clone());

        eprintln!("[dbgT] name={name} ty={ty:}");
        // Stream-collected containers: Java mapper types are invariant
        // (List<List<Integer>> vs List<List<Object>>) — emit `var` and let
        // the collector infer the precise element shape.
        if ty.contains("List<List<Object>>") || ty.contains("SimpleImmutableEntry<Object, Object>") {
            ty = "var".to_string();
        }
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

    /// Collect (name, java type) pairs from a multi_variable_declaration.
    fn destructuring_components(&self, mvd: tree_sitter::Node) -> Vec<(String, String)> {
        let mut cursor = mvd.walk();
        mvd.children(&mut cursor)
            .filter(|c| c.kind() == "variable_declaration")
            .map(|vd| {
                let name = kt::child(vd, "identifier")
                    .map(|n| self.unit.text(n).to_string())
                    .unwrap_or_else(|| "comp".to_string());
                let ty = kt::child(vd, "user_type")
                    .or_else(|| kt::child(vd, "nullable_type"))
                    .map(|t| kt::java_type_ann(t, self.unit.source, self.unit.annots))
                    .unwrap_or_else(|| "Object".to_string());
                (name, ty)
            })
            .collect()
    }

    /// `val (a, b) = expr`: compiler-generated componentN() extraction can't
    /// be reproduced without the receiver's data shape, so emit one local per
    /// component (first binds the value, the rest null) + N002.
    fn transpile_local_destructuring(&mut self, decl: tree_sitter::Node, out: &mut JavaOut) {
        let Some(mvd) = kt::child(decl, "multi_variable_declaration") else {
            return;
        };
        let comps = self.destructuring_components(mvd);
        if comps.is_empty() {
            self.unit.diags.warn_approx(
                decl,
                self.unit.file,
                "destructuring declaration without components",
            );
            return;
        }
        let init = self.unit.property_initializer(decl);
        let mut e = Expr { unit: self.unit };
        let init_java = init
            .map(|i| e.transpile(i))
            .unwrap_or_else(|| "null".to_string());
        let names: Vec<&str> = comps.iter().map(|(n, _)| n.as_str()).collect();
        // Known data-class receiver? Emit real componentN() extraction via
        // record accessors (bytecode-compatible: records generate accessors,
        // javac compiles them even when accessor names differ from fields).
        // Receivers without a known shape keep the degraded fallback below.
        let init_ty = init.map(|i| self.unit.infer_type(i)).unwrap_or_default();
        let comp_ty = init_ty
            .split('<')
            .next()
            .unwrap_or(&init_ty)
            .trim()
            .to_string();
        if let Some(shape) = self.unit.data_components.get(&comp_ty)
            && shape.len() >= comps.len()
        {
            for (i, (name, _ty)) in comps.iter().enumerate() {
                // Component type comes from the data class's recorded shape,
                // not the destructuring site (which has no type ascription).
                // Records expose components via accessor `name()` directly —
                // `p.comp1()` isn't real Java, so call the component accessor.
                let shape_ty = &shape[i].0;
                let accessor = &shape[i].1;
                out.line(format!(
                    "{} {} = {}.{}();",
                    shape_ty, name, init_java, accessor
                ));
            }
            return;
        }
        // Map.Entry-shaped initializer (`val (k, v) = zipList.first()`):
        // destructures to getKey()/getValue() — bytecode-compatible.
        eprintln!("[dbgD] init_ty={init_ty:?} comps={}", comps.len());
        if (init_ty.starts_with("java.util.AbstractMap.SimpleImmutableEntry<")
            || init_ty.contains("Map.Entry<"))
            && comps.len() == 2
        {
            let generics = &init_ty[init_ty.find('<').unwrap() + 1..init_ty.rfind('>').unwrap()];
            let mut gsplit = generics.splitn(2, ',');
            let kty = gsplit.next().unwrap_or("Object").trim().to_string();
            let vty = gsplit.next().unwrap_or("Object").trim().to_string();
            out.line(format!("{} {} = {}.getKey();", kty, names[0], init_java));
            out.line(format!("{} {} = {}.getValue();", vty, names[1], init_java));
            // register component types for downstream member inference
            self.unit
                .var_types
                .insert(names[0].to_string(), kty.clone());
            self.unit
                .var_types
                .insert(names[1].to_string(), vty.clone());
            return;
        }
        self.unit.diags.warn_approx(
            decl,
            self.unit.file,
            format!(
                "destructuring declaration '({})': componentN() extraction not reproduced; first component binds the value, the rest bind null",
                names.join(", ")
            ),
        );
        for (i, (name, ty)) in comps.iter().enumerate() {
            if i == 0 {
                out.line(format!("{} {} = {};", ty, name, init_java));
            } else {
                out.line(format!("{} {} = null;", ty, name));
            }
        }
    }

    fn transpile_assignment(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) {
        let left = kt::field(stmt, "left");
        let right = kt::field(stmt, "right");
        let op = kt::field(stmt, "operator").map(|o| self.unit.text(o).to_string());
        match (left, right, op) {
            (Some(l), Some(r), Some(op)) => {
                let mut e = Expr { unit: self.unit };
                let r_java = e.transpile(r);
                // Index-assign on a map (`m[k] = v`): rewrite to `m.put(k, v)`
                // — indexing_expression targets can't be assignment vars in
                // Java. Detect via index_expression + trailing '=' kept from
                // transpile_target.
                if let Some(l) = kt::field(stmt, "left")
                    && l.kind() == "index_expression"
                {
                    let mut lcur = l.walk();
                    let named: Vec<_> = l.children(&mut lcur).filter(|c| c.is_named()).collect();
                    // [base, index]
                    let lb = named
                        .first()
                        .map(|b| self.unit.text(*b).to_string())
                        .unwrap_or_default();
                    let li = named
                        .get(1)
                        .map(|i| {
                            let mut ee = Expr { unit: self.unit };
                            ee.transpile(*i)
                        })
                        .unwrap_or_else(|| "null".to_string());
                    out.line(format!("{}.put({}, {});", lb.trim(), li, r_java));
                    return;
                }
                let l_java = e.transpile_target(l);
                if std::mem::replace(&mut self.unit.pending_setter, false) {
                    // setter-call rewrite: `h.late = "x"` -> `h.setLate("x");`
                    out.line(format!("{}{});", l_java, r_java));
                } else {
                    out.line(format!("{} {} {};", l_java, op, r_java));
                }
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
            out.line(format!("return {};", fix_join_tail(&java)));
        }
    }

    fn transpile_if(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) {
        // if_expression structure: if ( condition ) block [else ...]
        let mut children: Vec<tree_sitter::Node> = Vec::new();
        let mut cursor = stmt.walk();
        for c in stmt.children(&mut cursor) {
            children.push(c);
        }
        let cond = kt::field(stmt, "condition").or_else(|| {
            children.iter().copied().find(|c| {
                matches!(
                    c.kind(),
                    "binary_expression" | "is_expression" | "parenthesized_expression"
                )
            })
        });
        // Then-branch: first named child after the condition (single-statement
        // form has no `block`; `control_structure_body` isn't in this grammar).
        let cond_pos = cond
            .and_then(|c| children.iter().position(|k| k.id() == c.id()))
            .unwrap_or(0);
        let then_node = children
            .iter()
            .skip(cond_pos + 1)
            .find(|c| c.is_named() && c.kind() != "else")
            .copied();
        let blocks: Vec<tree_sitter::Node> = then_node.into_iter().collect();

        let cond_java = cond
            .map(|c| {
                let mut e = Expr { unit: self.unit };
                e.transpile(unwrap_parens(c))
            })
            .unwrap_or_else(|| {
                self.unit.diags.warn_approx(
                    stmt,
                    self.unit.file,
                    "if condition not resolved; emitted as true",
                );
                "true".to_string()
            });

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
        // destructuring `for ((a, b) in xs)`: multi_variable_declaration
        if let Some(mvd) = kt::child(stmt, "multi_variable_declaration") {
            self.transpile_for_destructuring(stmt, mvd, out);
            return;
        }
        let var = kt::child(stmt, "variable_declaration")
            .and_then(|v| kt::child(v, "identifier"))
            .map(|n| self.unit.text(n).to_string())
            .unwrap_or_else(|| "i".to_string());
        // The iterable is the named node between `in` and `)`.
        let iterable = Self::find_iterable(stmt);
        let body = kt::child(stmt, "block")
            .or_else(|| kt::child(stmt, "control_structure_body"))
            .or_else(|| single_stmt_body(stmt, iterable));

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

    /// `for ((a, b) in xs)`: first component is the loop variable, the rest
    /// are null locals at the top of the body (componentN() approximated).
    fn transpile_for_destructuring(
        &mut self,
        stmt: tree_sitter::Node,
        mvd: tree_sitter::Node,
        out: &mut JavaOut,
    ) {
        let comps = self.destructuring_components(mvd);
        if comps.is_empty() {
            self.unit.diags.warn_approx(
                stmt,
                self.unit.file,
                "for destructuring without components",
            );
            return;
        }
        let iterable = Self::find_iterable(stmt);
        let body = kt::child(stmt, "block")
            .or_else(|| kt::child(stmt, "control_structure_body"))
            .or_else(|| single_stmt_body(stmt, iterable));
        let names: Vec<&str> = comps.iter().map(|(n, _)| n.as_str()).collect();
        // Known data-class iterable (`for ((x, y) in points)`) where every
        // component is covered by the shape: emit `for (var item : xs)` with
        // real accessor extraction at the top of the body — bytecode-shaped,
        // no N002 needed. Unknown shapes keep the degrade path.
        let elem_ty = iterable
            .map(|i| {
                let it = self.unit.infer_type(i);
                self.unit.elem_type_of(&it)
            })
            .unwrap_or_default();
        if let Some(iter) = iterable
            && self
                .unit
                .data_components
                .get(&elem_ty)
                .is_some_and(|shape| shape.len() >= comps.len())
        {
            let shape = self
                .unit
                .data_components
                .get(&elem_ty)
                .cloned()
                .unwrap_or_default();
            let iter_java = self.translate_iterable(iter, "__notlin_item");
            out.open(format!("for ({}", iter_java));
            if let Some(b) = body {
                for (i, (name, _ty)) in comps.iter().enumerate() {
                    let (sty, accessor) = &shape[i];
                    out.line(format!("{} {} = __notlin_item.{}();", sty, name, accessor));
                }
                self.transpile_body(b, out);
            }
            out.close();
            return;
        }
        self.unit.diags.warn_approx(
            stmt,
            self.unit.file,
            format!(
                "destructuring in 'for (({}))': componentN() extraction not reproduced; non-first components bind null",
                names.join(", ")
            ),
        );
        if let Some(iter) = iterable {
            let iter_java = self.translate_iterable(iter, &comps[0].0);
            out.open(format!("for ({}", iter_java));
            if let Some(b) = body {
                for (name, ty) in &comps[1..] {
                    out.line(format!("{} {} = null;", ty, name));
                }
                self.transpile_body(b, out);
            }
            out.close();
        } else {
            self.unit
                .diags
                .warn_approx(stmt, self.unit.file, "for loop without iterable");
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
        if iter.kind() == "infix_expression"
            && let Some(java) = self.translate_infix_range(iter, var)
        {
            return java;
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
                        "bare step on non-range operand not supported".to_string(),
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
        let cond = kt::field(stmt, "condition");
        let body = kt::child(stmt, "block")
            .or_else(|| kt::child(stmt, "control_structure_body"))
            .or_else(|| single_stmt_body(stmt, cond));
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
        let cond = kt::field(stmt, "condition");
        let body = kt::child(stmt, "block")
            .or_else(|| kt::child(stmt, "control_structure_body"))
            .or_else(|| single_stmt_body(stmt, cond));
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

/// Single-statement loop body: the first named child after the condition
/// (e.g. `while (y < 3) y++` — body is the bare `unary_expression`).
fn single_stmt_body<'t>(
    stmt: tree_sitter::Node<'t>,
    cond: Option<tree_sitter::Node<'t>>,
) -> Option<tree_sitter::Node<'t>> {
    let kids: Vec<tree_sitter::Node<'t>> = stmt.children(&mut stmt.walk()).collect();
    let skip = cond
        .and_then(|c| kids.iter().position(|k| k.id() == c.id()))
        .unwrap_or(0);
    kids.iter().skip(skip + 1).find(|c| c.is_named()).copied()
}

fn unwrap_parens(node: tree_sitter::Node) -> tree_sitter::Node {
    if node.kind() == "parenthesized"
        && let Some(inner) = node.children(&mut node.walk()).find(|c| c.is_named())
    {
        return inner;
    }
    node
}

/// Rewrite `…collect(Collectors.toList()).joinToString(sep)` — the stream
/// arm collected with toList before a trailing joinToString member; the
/// Java List has no joinToString. Merge into one joining(sep) collect.
pub(crate) fn fix_join_tail(java: &str) -> String {
    let marker = ".collect(java.util.stream.Collectors.toList()).joinToString(";
    if let Some(p) = java.find(marker) {
        let close = java[p + marker.len()..]
            .rfind(')')
            .map(|i| i + p + marker.len());
        if let Some(close) = close {
            let sep = java[p + marker.len()..close].trim();
            let mut out = java[..p].to_string();
            if sep.is_empty() {
                out.push_str(".collect(java.util.stream.Collectors.joining())");
            } else {
                out.push_str(&format!(
                    ".collect(java.util.stream.Collectors.joining({}))",
                    sep
                ));
            }
            out.push_str(&java[close + 1..]);
            return out;
        }
    }
    // (b) residual `.joinToString(sep)` tail (collector already joined):
    // strip that member and merge the sep into the upstream join, which is
    // the joinToString emitter's own arg.
    if let Some(j) = java.find(".joinToString(") {
        let close = java[j + ".joinToString(".len()..]
            .rfind(')')
            .map(|i| i + j + ".joinToString(".len());
        if let Some(close) = close {
            let sep = java[j + ".joinToString(".len()..close].trim();
            let mut out = java[..j].to_string();
            // merge the join tail's sep into the upstream joining(...) arg
            if let Some(u) = out.rfind(".collect(java.util.stream.Collectors.joining(") {
                let usep_start = u + ".collect(java.util.stream.Collectors.joining(".len();
                let usep_end = out[usep_start..].rfind("))").map(|i| i + usep_start);
                if let Some(ue) = usep_end {
                    let upstream = out[usep_start..ue].trim();
                    // The joinToString tail's sep equals the upstream
                    // arg in Kotlin source; trust the upstream join.
                    let merged = if upstream.is_empty() {
                        sep.to_string()
                    } else {
                        upstream.to_string()
                    };
                    out.truncate(u);
                    out.push_str(&format!(
                        ".collect(java.util.stream.Collectors.joining({}))",
                        merged
                    ));
                }
            }
            out.push_str(");");
            out.push_str(&java[close + 1..]);
            let cleaned = out.replace(";;", ";");
            return cleaned.trim().to_string();
        }
    }
    java.to_string()
}
