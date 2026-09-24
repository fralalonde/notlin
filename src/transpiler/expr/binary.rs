//! Binary/operator expression translation: elvis handling and
//! primitive-operand heuristics.

use super::Expr;
use crate::transpiler::kt;

impl<'a, 'u> Expr<'a, 'u> {
    fn known_primitive_operand(&self, node: tree_sitter::Node) -> bool {
        if node.kind() == "identifier" {
            self.unit
                .var_types
                .get(self.unit.text(node).trim())
                .is_some_and(|t| is_primitive_type(t))
        } else {
            false
        }
    }

    /// Kotlin infix function calls (`a to b`, `x shr 1`) — the grammar uses
    /// its own infix_expression node, not binary_expression.
    pub(crate) fn infix_expr(&mut self, node: tree_sitter::Node) -> String {
        let mut cursor = node.walk();
        let named: Vec<_> = node
            .children(&mut cursor)
            .filter(|c| c.is_named())
            .collect();
        if named.len() == 3 {
            let op = self.unit.text(named[1]).trim().to_string();
            let op_java = match op.as_str() {
                "shl" => "<<",
                "shr" => ">>",
                "ushr" => ">>>",
                "and" => "&",
                "or" => "|",
                "xor" => "^",
                "rem" => "%",
                _ => op.as_str(),
            };
            let l_java = self.transpile(named[0]);
            let r_java = self.transpile(named[2]);
            if op == "to" {
                // Infix `to` builds a Pair. Closest pure-JDK value:
                // AbstractMap.SimpleImmutableEntry (getKey/getValue mapped
                // at call sites that read .first/.second on Pair-typed vars).
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "infix `to` pair approximated with AbstractMap.SimpleImmutableEntry (getKey/getValue instead of first/second)",
                );
                return format!(
                    "new java.util.AbstractMap.SimpleImmutableEntry<>({}, {})",
                    l_java, r_java
                );
            }
            format!("{} {} {}", l_java, op_java, r_java)
        } else {
            self.unit
                .diags
                .warn_approx(node, self.unit.file, "malformed infix expression");
            self.unit.text(node).to_string()
        }
    }

    pub(crate) fn binary(&mut self, node: tree_sitter::Node) -> String {
        let left = kt::field(node, "left");
        let right = kt::field(node, "right");
        let op = kt::field(node, "operator").map(|o| self.unit.text(o).to_string());

        match (left, right, op) {
            (Some(l), Some(r), Some(op)) => {
                // Grammar collision: a generic call with a trailing lambda
                // (`Host.spec<String> { ... }`) parses as
                // binary_expression(left=nav `Host.spec`, op=`<`,
                // right=`String`) > (right=lambda). Kotlin here means a
                // generic CALL — the `<`/`>` are type-argument brackets, not
                // comparisons. Detect: op is `<`, the left operand ends in an
                // identifier (a callee), the right is a bare type-ish
                // identifier, and the node's raw text closes with `>` before
                // the trailing lambda.
                if op == "<"
                    && let Some(rhs_text) = self.is_generic_call_binary(node)
                {
                    let callee_java = self.transpile_callee_generic(l, Some(rhs_text.as_str()));
                    // Tell the outer `>` binary (the type list's closing
                    // bracket) not to treat this as a comparison.
                    self.unit.pending_generic_call = true;
                    // The trailing lambda belongs to the OUTER call node;
                    // call.rs re-finds it (`annotated_lambda` child) and
                    // appends. Return just the callee.
                    return callee_java;
                }
                // Elvis `?:` arrives as a binary_expression operator in this grammar
                if op == "?:" {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "elvis operator approximated with a null-check ternary",
                    );
                    let l_java = self.transpile(l);
                    let r_java = self.transpile(r);
                    // Always ternary: Optional.ofNullable(...).orElse(...)
                    // boxes and breaks primitive/Int inference in Java
                    // (Kotlin `a ?: 0` returns Int, not Optional<Integer>).
                    // for `a ?: b` Kotlin evaluates `a` twice in the naive
                    // ternary — correct as long as the transpiled `a` isn't
                    // side-effecting (noted as N002).
                    // safe-call lhs already emitted `x != null ? x.m : null`
                    // — collapse into one ternary: x != null ? inner : rhs.
                    let (base, inner) = if let Some(q) = l_java.find(" != null ? ") {
                        let b = l_java[..q].trim().trim_start_matches('(').to_string();
                        if l_java.ends_with(" : null") || l_java.ends_with(" : null)") {
                            let m0 = q + " != null ? ".len();
                            let m1 = if l_java.ends_with(" : null)") {
                                l_java.len() - " : null)".len()
                            } else {
                                l_java.len() - " : null".len()
                            };
                            (b.to_string(), l_java[m0..m1.max(m0)].to_string())
                        } else {
                            (b.clone(), l_java.clone())
                        }
                    } else {
                        (l_java.clone(), l_java.clone())
                    };
                    let _ = &base;
                    return format!("({} != null ? {} : {})", base, inner, r_java);
                }
                // Infix functions: and/or are keywords; others pass through
                let java_op = match op.as_str() {
                    "&&" | "and" => "&&",
                    "||" | "or" => "||",
                    // Kotlin referential equality: Java identity compare.
                    "===" => "==",
                    "!==" => "!=",
                    _ => op.as_str(),
                };
                // Ordered comparisons on non-primitive operands require
                // Comparable in Java (`a > b` is `a.compareTo(b) > 0`).
                // Without type information we keep the operator as-is (correct
                // for primitives, the common case) and warn that object
                // operands need a compareTo-based form.
                let is_ordered_cmp = matches!(java_op, "<" | ">" | "<=" | ">=");
                if is_ordered_cmp {
                    // A trailing-lambda generic call (`spec<String> { ... }`)
                    // arrives as TWO stacked binary_expressions: inner
                    // (`spec < String`) then outer (`(inner) > lambda`). A
                    // real comparison never has a bare type name or lambda as
                    // the right operand with an identifier receiver on the
                    // left — detect the outer shape structurally so the
                    // lambda bracket stays a call, not a compareTo.
                    let lhs_generic_call =
                        l.kind() == "binary_expression" && self.is_generic_call_binary(l).is_some();
                    if lhs_generic_call {
                        let callee_java = self.transpile(l);
                        self.unit.pending_generic_call = false;
                        // The lambda is this OUTER binary's right child —
                        // there is no enclosing call_expression for call.rs
                        // to find it in; attach it here as the call argument.
                        let lambda = (if r.kind() == "lambda_literal" {
                            Some(r)
                        } else {
                            r.children(&mut r.walk())
                                .find(|c| c.kind() == "lambda_literal")
                        })
                        .or_else(|| {
                            kt::child(r, "annotated_lambda")
                                .and_then(|al| kt::child(al, "lambda_literal"))
                        });
                        let callee_java = match (lambda, callee_java.is_empty()) {
                            (Some(lam), false) => {
                                if callee_java.ends_with(')') {
                                    callee_java
                                } else {
                                    format!("{}({})", callee_java, self.transpile(lam))
                                }
                            }
                            _ => callee_java,
                        };
                        return callee_java;
                    }
                    let lhs_text = self.unit.text(l).trim();
                    let rhs_text = self.unit.text(r).trim();
                    let either_primitive = lhs_text.parse::<i64>().is_ok()
                        || lhs_text.parse::<f64>().is_ok()
                        || rhs_text.parse::<i64>().is_ok()
                        || rhs_text.parse::<f64>().is_ok()
                        || is_likely_primitive(lhs_text)
                        || is_likely_primitive(rhs_text)
                        || self.known_primitive_operand(l)
                        || self.known_primitive_operand(r);
                    if !either_primitive {
                        self.unit.diags.warn_approx(
                            node,
                            self.unit.file,
                            "ordered comparison on non-primitive operands requires Comparable; emitted a.compareTo(b) form",
                        );
                        let l_java = self.transpile(l);
                        let r_java = self.transpile(r);
                        let cmp = format!("{}.compareTo({})", l_java, r_java);
                        return match java_op {
                            "<" => format!("{} < 0", cmp),
                            ">" => format!("{} > 0", cmp),
                            "<=" => format!("{} <= 0", cmp),
                            _ => format!("{} >= 0", cmp),
                        };
                    }
                }
                // Kotlin `==` is structural equals for objects; Java `==` is
                // identity. `===` (referential) reached here already rewritten
                // to java_op "==" / "!=", but MUST keep identity semantics —
                // track the original operator separately.
                let referential = matches!(op.as_str(), "===" | "!==");
                let op_java = if java_op == "==" || java_op == "!=" {
                    let lhs_text = self.unit.text(l);
                    // Primitive when a literal, or a known primitive param/local.
                    let is_primitive = is_likely_primitive(lhs_text)
                        || self
                            .unit
                            .var_types
                            .get(lhs_text.trim())
                            .is_some_and(|t| is_primitive_type(t));
                    if referential {
                        // Referential equality keeps Java identity regardless
                        // of operand type.
                        let rhs = self.transpile(r);
                        let lhs = self.transpile(l);
                        return format!("({} {} {})", lhs, java_op, rhs);
                    }
                    if !is_primitive {
                        let rhs = self.transpile(r);
                        let lhs = self.transpile(l);
                        if java_op == "==" {
                            return format!("Objects.equals({}, {})", lhs, rhs);
                        } else {
                            return format!("!Objects.equals({}, {})", lhs, rhs);
                        }
                    }
                    java_op
                } else {
                    java_op
                };
                let l_java = self.transpile(l);
                let r_java = self.transpile(r);
                // Operator overloads: if either operand is a user-class
                // value (constructor call or class-typed local), `a + b`
                // is Kotlin's `operator fun plus(o)` — emit `a.plus(b)`.
                let l_ty = self.infer_operand_type(l);
                let r_ty = self.infer_operand_type(r);
                let user_ty = [l_ty.as_deref(), r_ty.as_deref()]
                    .into_iter()
                    .flatten()
                    // "Object" is an UNKNOWN operand (entry generics lost),
                    // not a user class with an overloaded `plus` — and it
                    // must not route `k + v` to `k.plus(v)`; the
                    // Object+Object string-concat arm below handles it.
                    .find(|t| !is_primitive_type(t) && *t != "String" && *t != "Object");
                let is_arith = matches!(op.as_str(), "+" | "-" | "*" | "/" | "%");
                if is_arith && let Some(ty) = user_ty {
                    let mname = match op.as_str() {
                        "+" => "plus",
                        "-" => "minus",
                        "*" => "times",
                        "/" => "div",
                        "%" => "rem",
                        _ => "",
                    };
                    // Kotlin stdlib collection algebra (`Map + Map`,
                    // `Map - k`) has NO Java member and no sound
                    // expression-position lowering: the result needs a
                    // fresh collection + putAll/remove. Not a user operator
                    // overload — taint instead of a broken call.
                    // (List/Set plus hold their pre-existing approximations;
                    // narrowing here keeps the retention claim tight.)
                    if is_arith
                        && matches!(op.as_str(), "+" | "-")
                        && (ty.contains("Map<")
                            || ty.contains("List<")
                            || ty.contains("Set<")
                            || ty.contains("Collection<")
                            || ty.contains("Iterable<"))
                    {
                        self.unit.diag_untranslatable(
                            node,
                            format!(
                                "collection `{}` on a `{}` operand: stdlib collection algebra has no Java expression form; declaration retained in Kotlin",
                                op, ty
                            ),
                        );
                        let _ = mname;
                        // syntactically inert placeholder, mirroring the
                        // scope-function taint return
                        return "null".to_string();
                    }
                    if !mname.is_empty() {
                        self.unit.diags.warn_approx(
                            node,
                            self.unit.file,
                            format!(
                                "`operator+` overload: `{} a {} b` emitted as `a.{}(b)` (Kotlin operator name)",
                                ty, ty, mname
                            ),
                        );
                        return format!("{}.{}({})", l_java, mname, r_java);
                    }
                }
                if op == "to" {
                    // Infix `to` builds a Pair. Closest pure-JDK value:
                    // AbstractMap.SimpleImmutableEntry — getKey/getValue map
                    // to first/second reads with a member rewrite; record
                    // the pair semantics with an approximation warning.
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "infix `to` pair approximated with AbstractMap.SimpleImmutableEntry (getKey/getValue instead of first/second)",
                    );
                    return format!(
                        "new java.util.AbstractMap.SimpleImmutableEntry<>({}, {})",
                        l_java, r_java
                    );
                }
                // Unknown/Object-typed operands for `+`: the Kotlin `+` on
                // an unknown pair is a String concat in the practical case
                // (entry destructure then println). `"" + a + b` coerces.
                if is_arith
                    && op == "+"
                    // receivers are plain identifiers, not String/prim
                    // typed (=Object), and not literals: only then the
                    // Kotlin plus target is unknowable.
                    && l.kind() == "identifier"
                    && r.kind() == "identifier"
                    && self.unit.var_types.get(l_java.trim())
                        .is_some_and(|t| t == "Object")
                    && self.unit.var_types.get(r_java.trim())
                        .is_some_and(|t| t == "Object")
                {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "binary `+` on Object-typed operands -> string concat (`\"\" + a + b`); Kotlin plus-selection unverifiable here",
                    );
                    return format!("(\"\" + {} + {})", l_java, r_java);
                }
                format!("{} {} {}", l_java, op_java, r_java)
            }
            _ => {
                self.unit
                    .diags
                    .warn_approx(node, self.unit.file, "malformed binary expression");
                self.unit.text(node).to_string()
            }
        }
    }
}

impl<'a, 'u> Expr<'a, 'u> {
    /// Detects the `callee < Type` binary shape produced when a generic call
    /// with a trailing lambda is parsed; returns the phantom type-argument
    /// text when shaped. Structural (no transpilation, no side effects).
    fn is_generic_call_binary(&self, node: tree_sitter::Node) -> Option<String> {
        let left = kt::field(node, "left")?;
        let right = kt::field(node, "right")?;
        let op = kt::field(node, "operator").map(|o| self.unit.text(o).trim().to_string())?;
        if op != "<" {
            return None;
        }
        let lhs_text = self.unit.text(left).trim();
        let rhs_text = self.unit.text(right).trim();
        let callee_shaped = lhs_text
            .rsplit('.')
            .next()
            .map(|s| {
                s.chars()
                    .next()
                    .map(|c| c.is_ascii_lowercase() || c == '_')
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let type_shaped = rhs_text
            .chars()
            .next()
            .map(|c| c.is_ascii_uppercase())
            .unwrap_or(false)
            && !rhs_text.contains('(')
            && !lhs_text.contains('(')
            && !self.unit.var_types.contains_key(rhs_text);
        if callee_shaped && type_shaped {
            Some(rhs_text.to_string())
        } else {
            None
        }
    }

    /// Java type of an operand (for operator-overload detection): known
    /// locals via var_types; constructor calls via callee name; else None.
    /// File-scoped property lookup used by call/nav receivers: an indexed
    /// property named exactly `prop` in THIS unit's declaring file beats
    /// cross-file name collisions, then falls back to getter-shape and
    /// workspace-wide matches.
    pub(crate) fn scope_property_type(&self, prop: &str) -> Option<String> {
        let ws = self.unit.workspace?;
        let declaring = self
            .unit
            .workspace_file
            .as_deref()
            .unwrap_or(self.unit.file);
        let in_file = ws.property_type_in_file(declaring, prop);
        let getter = ws.property_type_of_getter(prop);
        in_file.or(getter)
    }

    pub(crate) fn infer_operand_type(&self, node: tree_sitter::Node) -> Option<String> {
        let t = self.unit.text(node).trim().to_string();
        if let Some(vt) = self.unit.var_types.get(&t) {
            if is_primitive_type(vt) || vt == "String" {
                return None;
            }
            return Some(vt.clone());
        }
        // member access `x.getFoo()` / `getFoo()` / bare `foo`: the indexed
        // property type of the getter's backing field
        if let Some(ws) = self.unit.workspace {
            let getter = t
                .rsplit_once('.')
                .map(|(_, last)| last.trim_end_matches("()").trim().to_string())
                .unwrap_or_else(|| t.clone());
            if getter
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
                && let Some(ty) = ws.property_type_of_getter(&getter)
            {
                if !is_primitive_type(&ty) && ty != "String" && ty != "Object" {
                    return Some(ty);
                }
                return None;
            }
            // bare field name: property type straight from the index
            if getter
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase())
                && let Some(ty) = ws
                    .bare_property_type(&getter)
                    .or_else(|| ws.property_type_of_getter(&getter))
            {
                if !is_primitive_type(&ty) && ty != "String" && ty != "Object" {
                    return Some(ty);
                }
                return None;
            }
        }
        // constructor call: `Pt(1, 2)` -> first word before '('
        let head = t.split('(').next().unwrap_or("").trim().to_string();
        if head.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && !is_primitive_type(&head)
        {
            return Some(head);
        }
        None
    }
}

fn is_likely_primitive(expr_text: &str) -> bool {
    // crude heuristic: numeric literal or known primitive-typed identifier
    let t = expr_text.trim();
    t.parse::<i64>().is_ok()
        || t.parse::<f64>().is_ok()
        || t == "true"
        || t == "false"
        || t.ends_with('L')
        || t.ends_with('f')
        || t.ends_with('F')
}

/// True for Java primitive type names (as emitted by map_type_name).
fn is_primitive_type(ty: &str) -> bool {
    matches!(
        ty,
        "int" | "long" | "short" | "byte" | "double" | "float" | "boolean" | "char"
    )
}
