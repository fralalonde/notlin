//! Expression translation: Kotlin expression tree -> Java expression text.

mod binary;
mod call;
mod navigation;
mod string;

use crate::transpiler::kt;
use crate::transpiler::unit::Unit;

pub struct Expr<'a, 'u> {
    pub unit: &'a mut Unit<'u>,
}

impl<'a, 'u> Expr<'a, 'u> {
    pub fn transpile(&mut self, node: tree_sitter::Node) -> String {
        match node.kind() {
            "string_literal" => self.string_literal(node),
            "number_literal" | "boolean_literal" | "hex_literal" | "long_literal"
            | "real_literal" => {
                let raw = self.unit.text(node).trim().to_string();
                // Kotlin unsigned suffixes (u/U/L/UL/uL) have no Java
                // equivalent for the plain literal; strip to the base type.
                if raw.ends_with("uL")
                    || raw.ends_with("UL")
                    || raw.ends_with("Lu")
                    || raw.ends_with("LU")
                {
                    // unsigned markers vanish in Java
                    raw.trim_end_matches(['u', 'U', 'L']).to_string()
                } else if raw.ends_with('L') {
                    raw.trim_end_matches('L').to_string()
                } else {
                    raw
                }
            }
            "identifier" => {
                let name = self.unit.text(node).trim().to_string();
                // Bare self-reference inside an object body: Kotlin resolves
                // `Registry` to the singleton; Java needs `Registry.INSTANCE`.
                if self.unit.current_object.as_deref() == Some(name.as_str()) {
                    format!("{}.INSTANCE", name)
                } else {
                    name
                }
            }
            // `this` inside an extension function body refers to the receiver
            // parameter (emitted as a regular first param, so `this` must map
            // to it in the static method's body).
            "this_expression" => self
                .unit
                .ext_receiver_name
                .clone()
                .unwrap_or_else(|| "this".to_string()),
            "navigation_expression" => self.navigation(node),
            "call_expression" => self.call(node),
            "infix_expression" => self.infix_expr(node),
            "binary_expression" => self.binary(node),
            "parenthesized" => {
                let inner = node
                    .children(&mut node.walk())
                    .find(|c| c.is_named())
                    .map(|c| self.transpile(c))
                    .unwrap_or_default();
                format!("({})", inner)
            }
            "elvis_expression" => self.elvis(node),
            "range_expression" => {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "range used as a value has no direct Java equivalent",
                );
                let mut cursor = node.walk();
                let kids: Vec<String> = node
                    .children(&mut cursor)
                    .filter(|c| c.is_named())
                    .map(|c| self.transpile(c))
                    .collect();
                format!("List.of({})", kids.join(", "))
            }
            "lambda_literal" => self.lambda(node),
            "when_expression" => self.when_expr(node),
            "is_expression" => {
                // `x is T` -> `x instanceof T` (primitives use their boxed type;
                // smart-cast narrowing is not emitted — caller may need a cast)
                let mut cursor = node.walk();
                let kids: Vec<_> = node.children(&mut cursor).collect();
                let target = kids.iter().find(|c| c.is_named()).copied();
                let ty = kids
                    .iter()
                    .filter(|c| c.is_named())
                    .nth(1)
                    .map(|t| box_primitive(&kt::java_type(*t, self.unit.source)))
                    .unwrap_or_else(|| "Object".to_string());
                match target {
                    Some(t) => {
                        let t_java = self.transpile(t);
                        format!("{} instanceof {}", t_java, ty)
                    }
                    None => "false".to_string(),
                }
            }
            "indexing_expression" => self.indexing(node),
            "jump_expression" => self.jump(node),
            "if_expression" => self.if_expr(node),
            _ => {
                // Last resort: try to copy verbatim text if it's plausible Java,
                // else emit null and warn.
                let text = self.unit.text(node);
                if text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._()<>[]{}\" ',:+-*/%=!&|?;".contains(c))
                    && !text.contains("val")
                    && !text.contains("var")
                    && !text.contains("fun ")
                {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        format!("expression kind '{}' passed through verbatim", node.kind()),
                    );
                    text.to_string()
                } else {
                    self.unit.diag_untranslatable(
                        node,
                        format!("expression kind '{}' not supported", node.kind()),
                    );
                    "null /* notlin: unsupported */".to_string()
                }
            }
        }
    }

    pub fn transpile_target(&mut self, node: tree_sitter::Node) -> String {
        match node.kind() {
            "identifier" => {
                let name = self.unit.text(node).trim().to_string();
                // Bare self-reference inside an object body: Kotlin resolves
                // `Registry` to the singleton; Java needs `Registry.INSTANCE`.
                if self.unit.current_object.as_deref() == Some(name.as_str()) {
                    format!("{}.INSTANCE", name)
                } else {
                    name
                }
            }
            "navigation_expression" => {
                // Assignment target: the property-read path emits getter calls
                // (`h.late = "x"` -> `h.getLate() = "x"`, illegal Java). A
                // known class property rewrites to its setter; unknown/local
                // targets pass through.
                let raw = self.unit.text(node).replace("?.", ".");
                if let Some(dot) = raw.rfind('.') {
                    let member = &raw[dot + 1..];
                    if member
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_lowercase())
                        && let Some(setter) = self.unit.class_props.get(member)
                    {
                        if setter.is_empty() {
                            self.unit.diag_untranslatable(
                                node,
                                format!(
                                    "assignment to `{}` is rejected in Kotlin (val or `private set`); declaration taints",
                                    member
                                ),
                            );
                        } else {
                            self.unit.pending_setter = true;
                            return format!("{}{}(", &raw[..dot + 1], setter);
                        }
                    }
                }
                self.transpile(node)
            }
            _ => self.transpile(node),
        }
    }

    fn when_expr(&mut self, node: tree_sitter::Node) -> String {
        self.unit.diags.warn_approx(
            node,
            self.unit.file,
            "when-expression approximated; check ternary output",
        );
        // when (subject) { branch -> expr, ... }  =>  ternary chain (single branch for now)
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        // The subject is inside a when_subject wrapper (with paren tokens):
        // find its first named expression child so `xs.size` TRANSPILES (its
        // property access -> xs.size()) instead of raw text pass-through.
        let subject = kids
            .iter()
            .find(|c| c.kind() == "when_subject")
            .and_then(|ws| ws.children(&mut ws.walk()).find(|c| c.is_named()))
            .or_else(|| {
                kids.iter()
                    .find(|c| c.is_named() && c.kind() != "when_entry")
                    .copied()
            });
        let entries: Vec<_> = kids
            .iter()
            .filter(|c| c.kind() == "when_entry")
            .copied()
            .collect();
        let subject_java = subject.as_ref().map(|s| self.transpile(*s));
        let subject_java = subject_java.unwrap_or_default();
        let mut ternary = String::new();
        for entry in entries.iter().rev() {
            let mut ec = entry.walk();
            let e_kids: Vec<_> = entry.children(&mut ec).collect();
            let e_named: Vec<_> = e_kids.iter().filter(|c| c.is_named()).copied().collect();
            let result = e_named.last().copied();
            let conditions: Vec<_> = e_named[..e_named.len().saturating_sub(1)].to_vec();
            let result_java = result
                .map(|r| self.transpile(r))
                .unwrap_or_else(|| "null".to_string());
            // Conditions joined with OR: `0, 1 ->` means `x==0 || x==1`;
            // `in 2..9 ->` is a range test over the when subject; `else`
            // is the fallthrough arm.
            let mut cond_parts: Vec<String> = Vec::new();
            // Smart-cast type when any condition is a `is T` test:
            // member accesses on the subject in this arm need (T) subject.
            let mut cast_type: Option<String> = None;
            for c in &conditions {
                let text = self.unit.text(*c);
                if text == "else" {
                    cond_parts.push("true".to_string());
                } else if c.kind() == "range_test" {
                    cond_parts.push(self.range_test_cond(*c, &subject_java));
                } else if c.kind() == "type_test" {
                    // `is State.Running` -> subject instanceof State.Running.
                    // Kotlin smart-casts the subject to the tested type, so
                    // member accesses on the subject in the arm must see a
                    // narrowed receiver: force a cast in the emitted arm.
                    let mut tcur = c.walk();
                    let ty = c
                        .children(&mut tcur)
                        .find(|t| t.is_named())
                        .map(|t| self.unit.text(t).trim().replace(" ", ""))
                        .unwrap_or_default();
                    cond_parts.push(format!("{} instanceof {}", subject_java, ty));
                    cast_type = Some(ty.clone());
                } else {
                    let cj = self.transpile(*c);
                    if subject.is_some() && cj != "true" {
                        cond_parts.push(format!("Objects.equals({}, {})", subject_java, cj));
                    } else {
                        cond_parts.push(cj);
                    }
                }
            }
            let cond_java = if cond_parts.is_empty() {
                "true".to_string()
            } else {
                cond_parts.join(" || ")
            };
            // Apply the smart cast AFTER conditions loop filled cast_type:
            // member reads on the subject in this arm need the narrowed type
            // (Kotlin smart-cast semantics).
            let result_java = match (&cast_type, subject) {
                (Some(ty), Some(_)) => {
                    // wrap the cast around the RECEIVER: ((T) s).getPid(),
                    // not (T) s.getPid() which casts the whole call result.
                    result_java.replace(
                        &format!("{}.", subject_java),
                        &format!("(({}) {}).", ty, subject_java),
                    )
                }
                _ => result_java,
            };
            ternary = if ternary.is_empty() {
                result_java.clone()
            } else {
                format!("{} ? {} : {}", cond_java, result_java, ternary)
            };
            if entries.len() == 1 {
                ternary = result_java;
            }
        }
        if ternary.is_empty() {
            "null".to_string()
        } else if ternary.contains("System.out.println") && ternary.contains(" : ") {
            // when-as-statement with void arms: a ternary over void calls is
            // illegal Java — real if/else chain preserves the logic.
            if let Some((c, rest)) = ternary.split_once(" ? ")
                && let Some((a, b)) = rest.split_once(" : ")
            {
                return format!("if ({}) {}; else {};", c, a, b);
            }
            ternary
        } else {
            ternary
        }
    }

    /// `x in lo..hi` when-condition -> `x >= lo && x <= hi` (`!in` negated).
    /// The subject is the when-expression's subject, not part of the node.
    fn range_test_cond(&mut self, cond: tree_sitter::Node, subject: &str) -> String {
        let mut cursor = cond.walk();
        let kids: Vec<_> = cond.children(&mut cursor).collect();
        let negate = kids.iter().any(|c| self.unit.text(*c).trim() == "!in");
        let range = kids.iter().find(|c| c.is_named()).copied();
        let Some(range) = range else {
            return "true".to_string();
        };
        let mut rc = range.walk();
        let rkids: Vec<_> = range.children(&mut rc).filter(|c| c.is_named()).collect();
        if rkids.len() != 2 {
            // not a plain a..b range — degrade to a true condition
            return "true".to_string();
        }
        let lo = self.transpile(rkids[0]);
        let hi = self.transpile(rkids[1]);
        if subject.is_empty() {
            return "true".to_string();
        }
        let in_range = format!("{} >= {} && {} <= {}", subject, lo, subject, hi);
        if negate {
            format!("!({})", in_range)
        } else {
            in_range
        }
    }

    fn jump(&mut self, node: tree_sitter::Node) -> String {
        let mut cursor = node.walk();
        let kids: Vec<_> = node
            .children(&mut cursor)
            .filter(|c| c.is_named())
            .collect();
        let kind = self.unit.text(node);
        if kind.starts_with("break") {
            "break".to_string()
        } else if kind.starts_with("continue") {
            "continue".to_string()
        } else {
            // return with expression (in expression context)
            kids.first().map(|e| self.transpile(*e)).unwrap_or_default()
        }
    }

    fn if_expr(&mut self, node: tree_sitter::Node) -> String {
        // if (c) a else b -> ternary (single-level)
        self.unit.diags.warn_approx(
            node,
            self.unit.file,
            "if-expression approximated as ternary",
        );
        let mut cursor = node.walk();
        let kids: Vec<tree_sitter::Node> = node.children(&mut cursor).collect();
        // Condition: first named child between the `(` and `)` tokens —
        // the if-condition can be ANY expression kind (call, binary, is,
        // paren). Field-based detection misses call conditions.
        let cond_node: Option<tree_sitter::Node> = {
            let mut c2 = node.walk();
            let all: Vec<_> = node.children(&mut c2).collect();
            let mut inner = Vec::new();
            let mut depth = 0;
            for c in all {
                if c.kind() == "(" {
                    depth = 1;
                    continue;
                }
                if depth == 1 {
                    if c.kind() == ")" {
                        break;
                    }
                    if c.is_named() {
                        inner.push(c);
                    }
                }
            }
            inner.first().copied()
        };
        let cond = cond_node.map(|c| {
            let inner = unwrap_paren_node(c);
            self.transpile(inner)
        });
        // If the condition is `x is T`, smart-cast means the then-branch
        // must cast x back to T in Java.
        let cast_ty = cond_node
            .map(|c| unwrap_paren_node(c))
            .filter(|c| c.kind() == "is_expression")
            .and_then(|c| {
                c.children(&mut c.walk())
                    .filter(|k| k.is_named())
                    .nth(1)
                    .map(|t| box_primitive(&kt::java_type(t, self.unit.source)))
            });
        // Branches: named children after the condition. When the condition is
        // a field-named node we skip only that node; else skip all up to `else`.
        let cond_id = cond_node.map(|c| c.id());
        let branches: Vec<_> = kids
            .iter()
            .filter(|c| c.is_named() && c.kind() != "is_expression" && Some(c.id()) != cond_id)
            .copied()
            .collect();
        match (cond.clone(), branches.len()) {
            (Some(c), 2) => {
                let a = self.transpile(branches[0]);
                let b = self.transpile(branches[1]);
                // Smart-cast repair: cast the then-branch to the is-type if the
                // then-branch is the same expression the is-check tested.
                let a = match (&cast_ty, branches[0].kind()) {
                    (Some(ty), "identifier") => format!("(({}) {})", ty, a),
                    _ => a,
                };
                // Both arms are void calls (println etc.)? A statement-level
                // ternary over void calls is illegal Java — emit real if/else.
                let arms_void =
                    a.contains("System.out.println") || b.contains("System.out.println");
                if arms_void {
                    return format!(
                        "if ({}) {{\n{};\n}} else {{\n{};\n}}",
                        c,
                        a.trim_matches('(').trim_matches(')'),
                        b
                    );
                }
                format!("({} ? {} : {})", c, a, b)
            }
            _ => {
                // Fallback: emit first/last named children as ternary arms
                if let (Some(c), 1) = (cond, branches.len()) {
                    let a = self.transpile(branches[0]);
                    format!("({} ? {} : null)", c, a)
                } else {
                    "null".to_string()
                }
            }
        }
    }
}

fn unwrap_paren_node(node: tree_sitter::Node) -> tree_sitter::Node {
    if node.kind() == "parenthesized"
        && let Some(inner) = node.children(&mut node.walk()).find(|c| c.is_named())
    {
        return inner;
    }
    node
}

/// Map primitive Java types to their boxed forms (for instanceof/casts).
fn box_primitive(ty: &str) -> String {
    match ty {
        "int" => "Integer".to_string(),
        "long" => "Long".to_string(),
        "short" => "Short".to_string(),
        "byte" => "Byte".to_string(),
        "double" => "Double".to_string(),
        "float" => "Float".to_string(),
        "boolean" => "Boolean".to_string(),
        "char" => "Character".to_string(),
        other => other.to_string(),
    }
}
