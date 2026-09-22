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
                    _ => op.as_str(),
                };
                // Ordered comparisons on non-primitive operands require
                // Comparable in Java (`a > b` is `a.compareTo(b) > 0`).
                // Without type information we keep the operator as-is (correct
                // for primitives, the common case) and warn that object
                // operands need a compareTo-based form.
                let is_ordered_cmp = matches!(java_op, "<" | ">" | "<=" | ">=");
                if is_ordered_cmp {
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
                // Kotlin `==` is structural equals for objects; Java `==` is identity.
                let op_java = if java_op == "==" || java_op == "!=" {
                    let lhs_text = self.unit.text(l);
                    // Primitive when a literal, or a known primitive param/local.
                    let is_primitive = is_likely_primitive(lhs_text)
                        || self
                            .unit
                            .var_types
                            .get(lhs_text.trim())
                            .is_some_and(|t| is_primitive_type(t));
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
                    .find(|t| !is_primitive_type(t) && *t != "String");
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

impl Expr<'_, '_> {
    /// Java type of an operand (for operator-overload detection): known
    /// locals via var_types; constructor calls via callee name; else None.
    fn infer_operand_type(&self, node: tree_sitter::Node) -> Option<String> {
        let t = self.unit.text(node).trim().to_string();
        if let Some(vt) = self.unit.var_types.get(&t) {
            if is_primitive_type(vt) || vt == "String" {
                return None;
            }
            return Some(vt.clone());
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

/// Operand resolves to a primitive/String via literal — used to gate the
/// Object + string-concat rewrite. This is only a text-level gate; the
/// var_types lookup happens through the Expr method form in binary().
fn is_primitive_or_string_operand_text(
    e: &tree_sitter::Node,
) -> bool {
    let t = format!("{:?}", e.kind());
    let _ = t;
    false // text-level unreachable; the var_types check is authoritative
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
