//! Call-expression translation: callee rewriting, stdlib/collection
//! dispatch, trailing lambdas.

use super::Expr;
use crate::transpiler::kt;
use crate::transpiler::unit::primitive_array_factory;

impl<'a, 'u> Expr<'a, 'u> {
    pub(crate) fn call(&mut self, node: tree_sitter::Node) -> String {
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
            k if primitive_array_factory(k).is_some() => {
                // Kotlin array literal factories -> Java array literals:
                // `intArrayOf(1, 2)` -> `new int[]{1, 2}`, `arrayOf(...)` ->
                // `new Object[]{...}` (reuses the inference table).
                let elem = primitive_array_factory(k).unwrap().trim_end_matches("[]");
                format!("new {}[]{{{}}}", elem, args.join(", "))
            }
            _ => {
                // Uppercase callee with no dot = constructor call
                if !callee_java.contains('.')
                    && callee_java
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_uppercase())
                {
                    format!("new {}({})", callee_java, args.join(", "))
                } else if callee_java.ends_with(')') && args.is_empty() {
                    // Mapped member that is already a complete call expression
                    // (`xs.get(0)`, `xs.stream().findFirst().orElse(null)`):
                    // it IS the call — no `()` wrapper to add.
                    callee_java
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

    fn type_arg_of(&self, call: tree_sitter::Node) -> Option<String> {
        kt::child(call, "type_arguments").map(|t| self.unit.text(t).to_string())
    }

    pub(crate) fn lambda(&mut self, node: tree_sitter::Node) -> String {
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
}
