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
                } else if !self.unit.var_types.contains_key(&name)
                    && self.unit.var_types.is_empty()
                    && let Some(getter) = self.unit.self_getters.get(&name)
                {
                    // Not a local/param in this scope: a bare identifier
                    // naming a property member of the enclosing type reads
                    // through its accessor (implicit `this`).
                    format!("this.{}()", getter)
                } else if !self.unit.var_types.contains_key(&name)
                    && let Some(owner) = self
                        .unit
                        .workspace
                        .and_then(|w| w.find_property_owner(&name))
                {
                    // Indexed property owner: same accessor shape whether
                    // the owner translated or stayed Kotlin. Conservative
                    // shape: only fire when no local of that name exists.
                    let _ = owner;
                    let mut cap = name.clone();
                    if let Some(first) = cap.chars().next() {
                        cap = first.to_uppercase().collect::<String>() + &cap[1..];
                    }
                    format!("this.get{}()", cap)
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
            // `super<T>` qualifier: Java drops the explicit supertype — an
            // interface super-access is just `T.super.member` (a Kotlin class
            // super-access is the plain-`super` form, which falls through).
            "super_expression" => self.transpile_super_qualifier(node),
            "navigation_expression" => self.navigation(node),
            "call_expression" => self.call(node),
            "infix_expression" => self.infix_expr(node),
            "binary_expression" => self.binary(node),
            "parenthesized_expression" | "parenthesized" => {
                let inner = node
                    .children(&mut node.walk())
                    .find(|c| c.is_named())
                    .map(|c| self.transpile(c))
                    .unwrap_or_default();
                format!("({})", inner)
            }
            // elvis_expression: use binary()'s unified ternary rewrite
            "elvis_expression" => self.binary(node),
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
            "indexing_expression" | "index_expression" => self.indexing(node),
            "jump_expression" => self.jump(node),
            "throw_expression" => self.throw_expr(node),
            "if_expression" => self.if_expr(node),
            // Kotlin vararg spread `*expr`. Java has no spread syntax, so the
            // leaked `*` would be a syntax error — never pass the operator
            // through. The practical cases:
            //   `arrayOf(*arr)`  (spread as the only/first factory arg) is an
            //     array copy — handled in call.rs's array-factory arm;
            //   everything else passes the array itself and records the
            //     approximation (the callee must accept the array).
            "spread_expression" => {
                let mut cursor = node.walk();
                let inner = node
                    .children(&mut cursor)
                    .find(|c| c.is_named())
                    .map(|c| self.transpile(c))
                    .unwrap_or_else(|| "null".to_string());
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "vararg spread `*expr` lowered to the array itself (Java has no spread at call sites; callee must accept the array)",
                );
                inner
            }
            "as_expression" => self.as_expr(node),
            "unary_expression" => self.unary_expr(node),
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
                    // Inside the setter for the SAME property
                    // (`fun setEquipmentSet(v) { this.equipmentSet = ... }`),
                    // `this.member = ...` is a FIELD write — rewriting it to
                    // `this.setMember(...)` re-enters the setter (infinite
                    // recursion) and flips the parameter type.
                    let in_own_setter =
                        self.unit.current_function_name.as_deref().is_some_and(|f| {
                            f.starts_with("set")
                                && f[3..]
                                    .chars()
                                    .next()
                                    .map(|c| c.to_ascii_lowercase().to_string())
                                    .map(|l| format!("{l}{}", &f[4..]))
                                    .is_some_and(|prop| prop == member)
                        });
                    if in_own_setter {
                        // plain field write: emit the raw `this.member` text
                        return raw;
                    }
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
            "when-expression lowered to switch or if/else",
        );
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
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
        let subject_java = subject
            .as_ref()
            .map(|s| self.transpile(*s))
            .unwrap_or_default();
        let arms: Vec<(Vec<tree_sitter::Node>, Option<tree_sitter::Node>)> = entries
            .iter()
            .map(|entry| {
                let e_named: Vec<_> = entry
                    .children(&mut entry.walk())
                    .filter(|c| c.is_named())
                    .collect();
                let result = e_named.last().copied();
                let conditions = e_named[..e_named.len().saturating_sub(1)].to_vec();
                (conditions, result)
            })
            .collect();
        if let Some(lowered) = self.when_switch(subject, &subject_java, &arms) {
            return lowered;
        }
        self.when_if_else(node, subject, &subject_java, &arms)
    }

    fn when_switch(
        &mut self,
        subject: Option<tree_sitter::Node>,
        subject_java: &str,
        arms: &[(Vec<tree_sitter::Node>, Option<tree_sitter::Node>)],
    ) -> Option<String> {
        if subject.is_none() || subject_java.is_empty() || arms.is_empty() {
            return None;
        }
        let mut cases = Vec::new();
        let mut default_arm = None;
        for (conditions, result) in arms {
            let is_else = conditions.is_empty()
                || conditions
                    .iter()
                    .all(|c| self.unit.text(*c).trim() == "else");
            if is_else {
                default_arm = Some(*result);
                continue;
            }
            let mut labels = Vec::new();
            for condition in conditions {
                labels.push(self.switch_case_label(*condition)?);
            }
            cases.push((labels, *result));
        }
        if cases.is_empty() {
            return None;
        }
        let mut out = format!("switch ({subject_java}) {{\n");
        for (labels, result) in cases {
            out.push_str(&format!(
                "case {} -> {};\n",
                labels.join(", "),
                self.when_arm_body(result)
            ));
        }
        out.push_str(&format!(
            "default -> {};\n}}",
            default_arm
                .flatten()
                .map(|result| self.when_arm_body(Some(result)))
                .unwrap_or_else(|| "throw new IllegalStateException()".to_string())
        ));
        Some(out)
    }

    fn switch_case_label(&self, node: tree_sitter::Node) -> Option<String> {
        match node.kind() {
            "number_literal" | "string_literal" => Some(self.unit.text(node).trim().to_string()),
            "navigation_expression" => node
                .children(&mut node.walk())
                .filter(|child| child.kind() == "identifier")
                .last()
                .map(|child| self.unit.text(child).trim().to_string())
                .filter(|label| !label.is_empty()),
            "identifier" => {
                let label = self.unit.text(node).trim().to_string();
                (!label.is_empty() && label != "else").then_some(label)
            }
            _ => None,
        }
    }

    fn when_arm_body(&mut self, result: Option<tree_sitter::Node>) -> String {
        match result {
            Some(node) if node.kind() == "throw_expression" => self.throw_expr(node),
            Some(node) => self.transpile(node),
            None => "null".to_string(),
        }
    }

    fn when_if_else(
        &mut self,
        node: tree_sitter::Node,
        subject: Option<tree_sitter::Node>,
        subject_java: &str,
        arms: &[(Vec<tree_sitter::Node>, Option<tree_sitter::Node>)],
    ) -> String {
        let yield_return = node
            .parent()
            .is_some_and(|parent| matches!(parent.kind(), "return_expression" | "function_body"));
        let mut out = String::new();
        let mut emitted_branch = false;
        for (conditions, result) in arms {
            let is_else = conditions.is_empty()
                || conditions
                    .iter()
                    .all(|c| self.unit.text(*c).trim() == "else");
            let mut cast_type = None;
            let mut cond_parts = Vec::new();
            for condition in conditions {
                let text = self.unit.text(*condition);
                if text.trim() == "else" {
                    continue;
                } else if condition.kind() == "range_test" {
                    cond_parts.push(self.range_test_cond(*condition, subject_java));
                } else if condition.kind() == "type_test" {
                    let ty = condition
                        .children(&mut condition.walk())
                        .find(|child| child.is_named())
                        .map(|child| self.unit.text(child).trim().replace(' ', ""))
                        .unwrap_or_default();
                    cond_parts.push(format!("{subject_java} instanceof {ty}"));
                    cast_type = Some(ty);
                } else {
                    let rendered = self.transpile(*condition);
                    if subject.is_some() && rendered != "true" {
                        cond_parts.push(format!("Objects.equals({subject_java}, {rendered})"));
                    } else {
                        cond_parts.push(rendered);
                    }
                }
            }
            let mut body = self.when_arm_body(*result);
            if let (Some(ty), Some(_)) = (&cast_type, subject) {
                body = body.replace(
                    &format!("{subject_java}."),
                    &format!("(({}) {}).", ty, subject_java),
                );
            }
            let body = if body.trim_start().starts_with("throw ") {
                format!("{body};")
            } else if yield_return {
                format!("return {body};")
            } else {
                format!("{body};")
            };
            if is_else {
                out.push_str(&format!("else {{\n{body}\n}}"));
            } else if !emitted_branch {
                let cond = if cond_parts.is_empty() {
                    "true".to_string()
                } else {
                    cond_parts.join(" || ")
                };
                out.push_str(&format!("if ({cond}) {{\n{body}\n}}"));
                emitted_branch = true;
            } else {
                let cond = cond_parts.join(" || ");
                out.push_str(&format!(" else if ({cond}) {{\n{body}\n}}"));
            }
        }
        if out.is_empty() {
            "null".to_string()
        } else {
            out
        }
    }

    /// A `super_expression` (`super`, or a qualified `SuperType.super`):
    /// the qualifying supertype matters for member access policy
    /// (`super.<member>` must become `SupName.super.member` in Java). The
    /// AST qualifier is authoritative when present; otherwise the
    /// enclosing declaration's supertype list supplies it via the index
    /// (`navigation()` resolves and taints). Pure text here: any
    /// workspace-policy decision happens in `navigation()`.
    fn transpile_super_qualifier(&mut self, node: tree_sitter::Node) -> String {
        match kt::child(node, "user_type") {
            Some(ty) => {
                let name = kt::java_type(ty, self.unit.source).trim().to_string();
                self.unit.pending_super_owner = Some(name.clone());
                name
            }
            None => "super".to_string(),
        }
    }

    fn throw_expr(&mut self, node: tree_sitter::Node) -> String {
        let ctor = node
            .children(&mut node.walk())
            .find(|child| child.is_named())
            .map(|child| self.transpile(child))
            .unwrap_or_else(|| "RuntimeException()".to_string());
        if ctor.trim_start().starts_with("new ") {
            format!("throw {ctor}")
        } else {
            format!("throw new {ctor}")
        }
    }

    /// `x as T` -> `(T) x`; `x as? T` -> a null-safe ternary cast. Kotlin `as`
    /// on a receiver that is smart-cast-safe is always a hard cast in Java.
    fn as_expr(&mut self, node: tree_sitter::Node) -> String {
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        let left = kids.iter().find(|c| c.is_named()).copied();
        let ty = kids
            .iter()
            .filter(|c| c.is_named())
            .nth(1)
            .map(|t| box_primitive(&kt::java_type(*t, self.unit.source)))
            .unwrap_or_else(|| "Object".to_string());
        let nullable = kids.iter().any(|c| self.unit.text(*c).trim() == "as?");
        let Some(left) = left else {
            self.unit
                .diag_untranslatable(node, "`as` cast with no subject expression".to_string());
            return "null".to_string();
        };
        let l_java = self.transpile(left);
        if nullable {
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                format!("`as?` cast to `{ty}` lowered to instanceof-check cast"),
            );
            format!(
                "({} instanceof {} ? ({}) {} : null)",
                l_java, ty, ty, l_java
            )
        } else {
            format!("(({}) {})", ty, l_java)
        }
    }

    /// `expr!!` (branching not-null assertion) and Java-expressible postfix
    /// unary forms (`++`/`-`/`+`/`!`). `!!` has no Java operator; the honest
    /// lowering is `Objects.requireNonNull(expr)` so NPE timing is preserved.
    fn unary_expr(&mut self, node: tree_sitter::Node) -> String {
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        let is_not_null = kids
            .iter()
            .any(|c| c.kind() == "!!" || self.unit.text(*c).trim() == "!!");
        if is_not_null {
            let arg = kids
                .iter()
                .find(|c| c.kind() == "argument" || c.is_named())
                .copied();
            let Some(arg) = arg else {
                self.unit.diag_untranslatable(
                    node,
                    "`!!` assertion with no subject expression".to_string(),
                );
                return "null".to_string();
            };
            let a_java = self.transpile(arg);
            if a_java.contains("?") || a_java.contains(" instanceof ") {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "`!!` on a lowered (ternary/instanceof) expression: null-check is skipped inside the cast",
                );
                return a_java;
            }
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                "`!!` lowered to Objects.requireNonNull (throws NPE at the same point)",
            );
            return format!("java.util.Objects.requireNonNull({})", a_java);
        }
        // Other postfix/prefix unary forms: map operator -> text directly.
        let mut op = String::new();
        let mut arg_text = None;
        for c in &kids {
            if c.is_named() {
                arg_text = Some(self.transpile(*c));
            } else {
                let t = self.unit.text(*c).trim().to_string();
                match t.as_str() {
                    "-" | "+" | "!" | "++" | "--" => op = t,
                    _ => {}
                }
            }
        }
        let Some(arg_text) = arg_text else {
            return "null".to_string();
        };
        if op.is_empty() {
            arg_text
        } else if op == "++" || op == "--" {
            format!("{}{}", arg_text, op)
        } else {
            format!("{}{}", op, arg_text)
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
