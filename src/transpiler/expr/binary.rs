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
                        "elvis operator approximated with Optional.ofNullable(...).orElse(...)",
                    );
                    let l_java = self.transpile(l);
                    let r_java = self.transpile(r);
                    // `lhs ?: println(...)`-style void arms: orElse(void) is
                    // illegal Java — fall back to a null-check ternary.
                    if r_java.starts_with("System.out.println")
                        || l_java.starts_with("System.out.println")
                    {
                        return format!("({} != null ? {} : {})", l_java, l_java, r_java);
                    }
                    return format!(
                        "java.util.Optional.ofNullable({}).orElse({})",
                        l_java, r_java
                    );
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

    pub(crate) fn elvis(&mut self, node: tree_sitter::Node) -> String {
        self.unit.diags.warn_approx(
            node,
            self.unit.file,
            "elvis operator approximated with Optional.ofNullable(...).orElse(...)",
        );
        let mut cursor = node.walk();
        let kids: Vec<_> = node
            .children(&mut cursor)
            .filter(|c| c.is_named())
            .collect();
        if kids.len() >= 2 {
            let lhs = self.transpile(kids[0]);
            let rhs = self.transpile(kids[1]);
            // orElse of a void/println arm is illegal Java. A ternary is
            // always valid (works for void-as-statement contexts too when
            // emitted inside an expression-only parent? no — statement
            // parents get it via when/if rules). The correct semantics for
            // `x ?: y` is a ternary on truthy-only-for-refs: use the plain
            // null-check ternary for string/nullable shapes.
            if rhs.contains("void")
                || rhs.starts_with("System.out.println")
                || lhs.starts_with("System.out.println")
            {
                format!("({} != null ? {} : {})", lhs.trim_end(), lhs, rhs)
            } else {
                format!("java.util.Optional.ofNullable({}).orElse({})", lhs, rhs)
            }
        } else {
            "null".to_string()
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
