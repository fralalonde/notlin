//! Expression translation: Kotlin expression tree -> Java expression text.
use crate::transpiler::kt;
use crate::transpiler::unit::Unit;

pub struct Expr<'a, 'u> {
    pub unit: &'a mut Unit<'u>,
}

impl<'a, 'u> Expr<'a, 'u> {
    /// True if the identifier node is a known primitive-typed variable in the
    /// current translation scope (params/local decls recorded in var_types).
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

    pub fn transpile(&mut self, node: tree_sitter::Node) -> String {
        match node.kind() {
            "string_literal" => self.string_literal(node),
            "number_literal" | "boolean_literal" | "hex_literal" | "long_literal"
            | "real_literal" => self.unit.text(node).to_string(),
            "identifier" => self.unit.text(node).to_string(),
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

    /// Like transpile, but for assignment targets (no fallback to `null`).
    pub fn transpile_target(&mut self, node: tree_sitter::Node) -> String {
        match node.kind() {
            "identifier" | "navigation_expression" => self.transpile(node),
            _ => self.transpile(node),
        }
    }

    fn string_literal(&mut self, node: tree_sitter::Node) -> String {
        // Reconstruct from children, converting $name and ${expr} to concatenation.
        let raw = self.unit.text(node);
        if !raw.contains('$') {
            // plain string: escape and return
            return format!("{:?}", raw.trim_matches('"').replace("\\\"", "\""));
        }

        // Interpolated string: split into parts. The grammar gives us
        // string_content pieces; `$` and `${...}` markers.
        let mut out = String::new();
        let mut parts: Vec<String> = Vec::new();
        let mut current = String::new();
        let inner = raw
            .trim_start_matches('"')
            .trim_end_matches('"')
            .to_string();
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '$'
                && let Some(&next) = chars.peek()
            {
                if next == '{' {
                    // ${expr}
                    if !current.is_empty() {
                        parts.push(format!("{:?}", current));
                        current = String::new();
                    }
                    let mut expr = String::new();
                    chars.next(); // consume '{'
                    let mut depth = 1;
                    for ec in chars.by_ref() {
                        if ec == '{' {
                            depth += 1;
                        } else if ec == '}' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        expr.push(ec);
                    }
                    parts.push(format!("({})", expr));
                    continue;
                } else if next.is_alphabetic() || next == '_' {
                    // $identifier
                    if !current.is_empty() {
                        parts.push(format!("{:?}", current));
                        current = String::new();
                    }
                    let mut ident = String::new();
                    while let Some(&ic) = chars.peek() {
                        if ic.is_alphanumeric() || ic == '_' {
                            ident.push(ic);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    parts.push(ident);
                    continue;
                }
            }
            current.push(c);
        }
        if !current.is_empty() {
            parts.push(format!("{:?}", current));
        }
        if parts.is_empty() {
            out.push_str("\"\"");
        } else {
            out.push_str(&parts.join(" + "));
        }
        out
    }

    fn navigation(&mut self, node: tree_sitter::Node) -> String {
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        // base . member (possibly ?. or ::)
        let base = kids.iter().find(|c| c.is_named()).copied();
        let mut result = base.map(|b| self.transpile(b)).unwrap_or_default();
        for w in kids.windows(3) {
            if w[1].kind() == "." || w[1].kind() == "?." {
                if w[1].kind() == "?." {
                    self.unit.diags.warn_approx(
                        w[1],
                        self.unit.file,
                        "safe-call `?.` approximated as plain `.`; NPEs possible",
                    );
                }
                if w[2].is_named() {
                    // Kotlin properties become Java accessor calls: `x.age` ->
                    // `x.getAge()` (user classes), `x.length` -> `x.length()`
                    // (builtin). A trailing call (`x.foo(...)`) is handled by the
                    // call handler, which passes the member through unchanged.
                    let member_name = if w[2].kind() == "identifier" {
                        self.unit.text(w[2]).to_string()
                    } else {
                        self.transpile(w[2])
                    };
                    if w[2].kind() != "identifier" {
                        result.push_str(&format!(".{}", member_name));
                    } else if matches!(
                        member_name.as_str(),
                        "length"
                            | "size"
                            | "isEmpty"
                            | "isNotEmpty"
                            | "keys"
                            | "values"
                            | "entries"
                    ) {
                        // property-like reads -> Java accessor calls; keys/
                        // entries have different Java names (Map API)
                        let java_member: String = match member_name.as_str() {
                            "keys" => "keySet()".to_string(),
                            "entries" => "entrySet()".to_string(),
                            other => format!("{}()", other),
                        };
                        if matches!(member_name.as_str(), "size" | "length")
                            && base
                                .map(|b| self.unit.receiver_is_array(b))
                                .unwrap_or(false)
                        {
                            // Kotlin arrays expose `size`, Java exposes the
                            // `length` field — no parens for a field read.
                            result.push_str(".length");
                        } else {
                            result.push_str(&format!(".{}", java_member));
                        }
                    } else if member_name
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_uppercase())
                    {
                        // Uppercase member: class ref / static member (Registry.INSTANCE)
                        result.push_str(&format!(".{}", member_name));
                    } else {
                        // user-defined property read -> getter call
                        let cap: String = member_name
                            .chars()
                            .next()
                            .map(|c| c.to_uppercase().collect::<String>())
                            .unwrap_or_default()
                            + member_name.chars().skip(1).collect::<String>().as_str();
                        result.push_str(&format!(".get{}()", cap));
                    }
                }
            } else if w[1].kind() == "::" {
                // Class/object references and method refs; pass through.
            }
        }
        // Fallback: if windows didn't yield members, join verbatim
        if result.is_empty() {
            result = self.unit.text(node).replace("?.", ".");
        }
        result
    }

    fn call(&mut self, node: tree_sitter::Node) -> String {
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();

        // callee may be identifier or navigation_expression
        let callee = kids.iter().find(|c| c.is_named()).copied();
        let args_node = kids.iter().find(|c| c.kind() == "value_arguments").copied();
        // trailing lambda: direct lambda_literal or wrapped in annotated_lambda
        let lambda_arg = kids
            .iter()
            .find(|c| c.kind() == "lambda_literal")
            .copied()
            .or_else(|| {
                kids.iter()
                    .find(|c| c.kind() == "annotated_lambda")
                    .and_then(|al| kt::child(*al, "lambda_literal"))
            });

        let mut args: Vec<String> = Vec::new();
        if let Some(an) = args_node {
            let mut inner = an.walk();
            for arg in an.children(&mut inner) {
                if arg.kind() == "value_argument" {
                    let mut ac = arg.walk();
                    let expr = arg
                        .children(&mut ac)
                        .find(|c| c.is_named())
                        .map(|e| self.transpile(e))
                        .unwrap_or_default();
                    args.push(expr);
                }
            }
        }
        if let Some(l) = lambda_arg {
            args.push(self.transpile(l));
        }

        // Navigation callee with type context: array `.size`/`.length` and
        // same-file extension call sites are rewritten BEFORE the generic
        // callee path so private messages don't fire for them.
        if let Some(nav) = kids
            .iter()
            .find(|c| c.kind() == "navigation_expression")
            .copied()
            && let Some((base, member)) = self.unit.nav_base_member(nav)
        {
            if self.unit.receiver_is_array(base) && matches!(member.as_str(), "size" | "length") {
                // Kotlin `arr.size()`/`arr.size` -> Java `arr.length`.
                return format!("{}.length", self.transpile(base));
            }
            if let Some(_recv_ty) = self.unit.extension_fns.get(member.as_str()) {
                // `x.f(...)` for a same-file extension -> static `f(x, ...)`.
                let recv_java = self.transpile(base);
                self.unit.diags.warn_approx(
                        nav,
                        self.unit.file,
                        format!(
                            "extension call site `{base}.{member}(...)` rewritten to static `{member}({base}, ...)` (same-file approximation; caller-visible signature change)",
                            base = self.unit.text(base).trim(),
                        ),
                    );
                return format!(
                    "{}({})",
                    member,
                    std::iter::once(recv_java)
                        .chain(args.into_iter())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }

        let callee_java = callee.map(|c| self.transpile_callee(c)).unwrap_or_default();

        // println -> System.out.println
        if callee_java == "println" {
            return format!("System.out.println({})", args.join(", "));
        }
        if callee_java == "print" {
            return format!("System.out.print({})", args.join(", "));
        }

        // Kotlin collection operations with a trailing lambda -> Stream API
        let is_stream_op = callee_java
            .rfind('.')
            .map(|i| &callee_java[i + 1..])
            .map(|m| {
                matches!(
                    m,
                    "map"
                        | "filter"
                        | "forEach"
                        | "flatMap"
                        | "sorted"
                        | "distinct"
                        | "mapNotNull"
                        | "filterIndexed"
                        | "mapIndexed"
                        | "associate"
                        | "groupBy"
                        | "fold"
                        | "reduce"
                        | "foldIndexed"
                        | "any"
                        | "all"
                        | "none"
                        | "count"
                        | "first"
                        | "firstOrNull"
                        | "last"
                        | "lastOrNull"
                        | "joinToString"
                )
            })
            .unwrap_or(false);
        if let (true, Some(lambda)) = (is_stream_op, lambda_arg) {
            let member = callee_java
                .rfind('.')
                .map(|i| &callee_java[i + 1..])
                .unwrap();
            let base = &callee_java[..callee_java.len() - member.len() - 1];
            let stream_fn = match member {
                "map" | "mapNotNull" | "mapIndexed" => "map",
                "filter" | "filterIndexed" => "filter",
                "forEach" => "forEach",
                "sorted" => "sorted",
                other => other,
            };
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                format!(
                    "collection op `.{} {{...}}` approximated with Stream",
                    member
                ),
            );
            return format!(
                "{}.stream().{}({}).collect(java.util.stream.Collectors.toList())",
                base,
                stream_fn,
                self.transpile(lambda)
            );
        }

        // mutableListOf<String>(...) etc. -> new ArrayList<>()
        match callee_java.as_str() {
            "mutableListOf" | "arrayListOf" | "listOf" => {
                let elem_ty = self.type_arg_of(node);
                let _ = elem_ty;
                if args.is_empty() {
                    "new ArrayList<>()".to_string()
                } else {
                    format!("new ArrayList<>(List.of({}))", args.join(", "))
                }
            }
            "mutableMapOf" | "mapOf" | "hashMapOf" => {
                if args.is_empty() {
                    "new HashMap<>()".to_string()
                } else {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "mapOf with entries approximated as empty HashMap",
                    );
                    "new HashMap<>()".to_string()
                }
            }
            "mutableSetOf" | "setOf" | "hashSetOf" => {
                if args.is_empty() {
                    "new HashSet<>()".to_string()
                } else {
                    format!("new HashSet<>(Set.of({}))", args.join(", "))
                }
            }
            "emptyList" => "List.of()".to_string(),
            "emptyMap" => "Map.of()".to_string(),
            "emptySet" => "Set.of()".to_string(),
            _ => {
                // Uppercase callee with no dot = constructor call
                if !callee_java.contains('.')
                    && callee_java
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_uppercase())
                {
                    format!("new {}({})", callee_java, args.join(", "))
                } else {
                    format!("{}({})", callee_java, args.join(", "))
                }
            }
        }
    }

    fn transpile_callee(&mut self, node: tree_sitter::Node) -> String {
        match node.kind() {
            "identifier" => self.unit.text(node).to_string(),
            "navigation_expression" => self.navigation_call(node),
            _ => self.transpile(node),
        }
    }

    /// navigation used as a call target: keep dots, strip ?., and rewrite
    /// Kotlin-stdlib method names to their Java counterparts; anything not in
    /// the known-mapping set warns (call-site portability risk).
    fn navigation_call(&mut self, node: tree_sitter::Node) -> String {
        let raw = self.unit.text(node).replace("?.", ".");
        // member call: `.name(...)`
        let mut raw_trimmed = raw.trim().to_string();
        if let Some(r) = &self.unit.ext_receiver_name {
            // `this.x` inside an extension body refers to the receiver param.
            if raw_trimmed.starts_with("this.") {
                raw_trimmed = format!("{}{}", r, &raw_trimmed[4..]);
            }
        }
        // Compound receiver (itself a call/index/nav chain): the base must be
        // translated as an expression — raw-text surgery would leave inner
        // extension call sites verbatim (`s.shout().lowercase` would stay
        // `s.shout().toLowerCase` instead of `shout(s).toLowerCase`).
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        let base = kids.iter().find(|c| c.is_named()).copied();
        if let Some(b) = base {
            let compound = matches!(
                b.kind(),
                "call_expression"
                    | "navigation_expression"
                    | "indexing_expression"
                    | "parenthesized"
                    | "if_expression"
                    | "when_expression"
            );
            if compound {
                let member = kids
                    .windows(2)
                    .filter(|w| w[0].kind() == "." || w[0].kind() == "?.")
                    .filter(|w| w[1].kind() == "identifier")
                    .map(|w| self.unit.text(w[1]).to_string())
                    .next_back();
                if let Some(member) = member {
                    let base_java = self.transpile(b);
                    return match kotlin_member_to_java(&member) {
                        Some(jm) if jm != member => format!("{}.{}", base_java, jm),
                        Some(_) => format!("{}.{}", base_java, member),
                        None => {
                            self.unit.diags.warn_approx(
                                node,
                                self.unit.file,
                                format!(
                                    "stdlib member `.{}` not mapped; emitted verbatim (verify Java equivalent exists)",
                                    member
                                ),
                            );
                            format!("{}.{}", base_java, member)
                        }
                    };
                }
            }
        }
        if let Some(dot) = raw_trimmed.rfind('.') {
            let member_end = raw_trimmed[dot + 1..]
                .find('(')
                .map(|i| dot + 1 + i)
                .unwrap_or(raw_trimmed.len());
            let member = &raw_trimmed[dot + 1..member_end];
            if let Some(java_member) = kotlin_member_to_java(member) {
                if java_member != member {
                    return format!(
                        "{}{}",
                        &raw_trimmed[..dot + 1],
                        raw_trimmed[dot + 1..].replacen(member, &java_member, 1)
                    );
                }
                return raw_trimmed.to_string();
            }
            // Unknown member on a receiver: warn, pass through
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                format!(
                    "stdlib member `.{}` not mapped; emitted verbatim (verify Java equivalent exists)",
                    member
                ),
            );
        }
        raw_trimmed.to_string()
    }

    fn type_arg_of(&self, call: tree_sitter::Node) -> Option<String> {
        kt::child(call, "type_arguments").map(|t| self.unit.text(t).to_string())
    }

    fn binary(&mut self, node: tree_sitter::Node) -> String {
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

    fn elvis(&mut self, node: tree_sitter::Node) -> String {
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
            format!("java.util.Optional.ofNullable({}).orElse({})", lhs, rhs)
        } else {
            "null".to_string()
        }
    }

    fn lambda(&mut self, node: tree_sitter::Node) -> String {
        // lambda_literal: { params -> body }
        let raw = self.unit.text(node);
        // strip braces
        let inner = raw.trim().trim_start_matches('{').trim_end_matches('}');
        let (params, body) = match inner.split_once("->") {
            Some((p, b)) => (p.trim(), b.trim()),
            None => ("", inner.trim()),
        };
        // params like `a, b` or typed `a: Int`
        let params_java = params
            .split(',')
            .map(|p| p.split(':').next().unwrap_or(p).trim().to_string())
            .filter(|p| !p.is_empty() && p != "it")
            .collect::<Vec<_>>()
            .join(", ");
        let body_java = self.transpile_body_text(node, body);
        if params_java.is_empty() {
            // `{ it * 2 }`: implicit `it` parameter — the body references it.
            if body_java.contains("it") {
                format!("it -> {}", body_java)
            } else {
                format!("() -> {}", body_java)
            }
        } else {
            format!("{} -> {}", params_java, body_java)
        }
    }

    /// Translate a plain-text lambda body (best-effort; single expression).
    /// `node` backs the lambda for diagnostic positions.
    fn transpile_body_text(&mut self, node: tree_sitter::Node, body: &str) -> String {
        // For now: pass through simple expressions, warn otherwise.
        if body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._()<>[]\" ',:+-*/%=!&|?".contains(c))
            && !body.contains("val ")
            && !body.contains("var ")
        {
            body.replace("?.", ".").replace('$', "")
        } else {
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                format!("complex lambda body passed through raw: {}", body),
            );
            body.replace("?.", ".").replace('$', "")
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
        let subject = kids
            .iter()
            .find(|c| c.is_named() && c.kind() != "when_entry")
            .copied();
        let entries: Vec<_> = kids
            .iter()
            .filter(|c| c.kind() == "when_entry")
            .copied()
            .collect();
        let subject_java = subject.map(|s| self.transpile(s)).unwrap_or_default();

        let mut ternary = String::new();
        for entry in entries.iter().rev() {
            let mut ec = entry.walk();
            let e_kids: Vec<_> = entry.children(&mut ec).collect();
            let condition = e_kids.iter().find(|c| c.is_named()).copied();
            let result = e_kids.iter().filter(|c| c.is_named()).nth(1).copied();
            let result_java = result
                .map(|r| self.transpile(r))
                .unwrap_or_else(|| "null".to_string());
            let cond_java = match condition {
                Some(c) if self.unit.text(c) == "else" => "true".to_string(),
                Some(c) => {
                    let cj = self.transpile(c);
                    if subject.is_some() && cj != "true" {
                        format!("Objects.equals({}, {})", subject_java, cj)
                    } else {
                        cj
                    }
                }
                None => "true".to_string(),
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
        } else {
            ternary
        }
    }

    fn indexing(&mut self, node: tree_sitter::Node) -> String {
        // list[0] -> list.get(0)
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        let base = kids.iter().find(|c| c.is_named()).copied();
        let indices: Vec<_> = kids
            .iter()
            .filter(|c| c.kind() == "indexing_suffix")
            .flat_map(|s| {
                s.children(&mut s.walk())
                    .filter(|c| c.is_named())
                    .collect::<Vec<_>>()
            })
            .collect();
        let base_java = base.map(|b| self.transpile(b)).unwrap_or_default();
        if indices.len() == 1 {
            let idx = self.transpile(indices[0]);
            format!("{}.get({})", base_java, idx)
        } else {
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                "multi-index expressions not supported",
            );
            base_java
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
        let kids: Vec<_> = node.children(&mut cursor).collect();
        // Condition: the `condition` field if present, else the first is/binary
        // expression child before `else`.
        let cond_node = kids
            .iter()
            .find(|c| {
                cursor.field_name() == Some("condition")
                    || c.kind() == "is_expression"
                    || c.kind() == "binary_expression"
                    || c.kind() == "parenthesized"
            })
            .copied();
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

/// Kotlin stdlib member -> Java counterpart. None = not recognized as
/// stdlib (user-defined methods pass through unmapped).
fn kotlin_member_to_java(member: &str) -> Option<String> {
    let mapped: Option<&str> = match member {
        "uppercase" => Some("toUpperCase"),
        "lowercase" => Some("toLowerCase"),
        "keys" => Some("keySet"),
        "entries" => Some("entrySet"),
        // no-arg collection ops with Java Collection/Stream equivalents
        "firstOrNull" => Some("stream().findFirst().orElse(null)"),
        "last" => Some("stream().reduce((a, b) -> b).orElse(null)"),
        "reversed" => Some("reversed()"),
        "count" => Some("size()"),
        _ => None,
    };
    if let Some(m) = mapped {
        return Some(m.to_string());
    }
    // Same-spelling names that exist in Java: safe pass-through, no warn
    const SAFE: &[&str] = &[
        "trim",
        "size",
        "isEmpty",
        "values",
        "length",
        "put",
        "stream",
        "iterator",
        "hashCode",
        "toString",
        "equals",
        "compareTo",
        "contains",
        "indexOf",
        "lastIndexOf",
        "startsWith",
        "endsWith",
        "substring",
        "replace",
        "split",
        "chars",
        "get",
        "containsKey",
        "containsValue",
        "remove",
        "clear",
        "add",
    ];
    if SAFE.contains(&member) {
        return Some(member.to_string());
    }
    None
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

/// True for Java primitive type names (as emitted by map_type_name).
fn is_primitive_type(ty: &str) -> bool {
    matches!(
        ty,
        "int" | "long" | "short" | "byte" | "double" | "float" | "boolean" | "char"
    )
}
