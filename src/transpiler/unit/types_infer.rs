//! Best-effort type inference and receiver-type helpers.

use super::Unit;
use crate::transpiler::kt;

impl<'a> Unit<'a> {
    /// Element type of a collection/array Java type (`List<X>` -> X, `X[]` ->
    /// X); exposed for destructuring-site extraction in stmt.rs.
    pub(crate) fn elem_type_of(&self, java_ty: &str) -> String {
        elem_type_of(java_ty)
    }

    pub fn infer_type(&mut self, expr: tree_sitter::Node) -> String {
        let t = self.infer_type_inner(expr);
        // Kotlin stdlib `Pair`/`Triple` don't exist in the JDK: the emitted
        // value is a SimpleImmutableEntry, so rewrite the recorded type too
        // (member reads .first/.second key off it).
        if let Some(stripped) = t.strip_prefix("Pair<") {
            format!("java.util.AbstractMap.SimpleImmutableEntry<{}", stripped)
        } else if t == "Triple" {
            "Object".to_string()
        } else {
            t
        }
    }

    fn infer_type_inner(&mut self, expr: tree_sitter::Node) -> String {
        match expr.kind() {
            "identifier" => {
                // Grammar quirk: bare `false`/`true` parses as identifier,
                // not boolean_literal — recognize them here.
                if matches!(self.text(expr).trim(), "true" | "false") {
                    return "boolean".to_string();
                }
                self.var_types
                    .get(self.text(expr).trim())
                    .cloned()
                    .unwrap_or_else(|| "Object".to_string())
            }
            "navigation_expression" => self.infer_navigation(expr),
            "string_literal" => "String".to_string(),
            "number_literal" => {
                let t = self.text(expr);
                if t.contains('.') || t.contains('e') || t.contains('E') {
                    "double".to_string()
                } else {
                    "int".to_string()
                }
            }
            "infix_expression" => {
                // Arithmetic infix inside inference (e.g. `0.5 + 0.25`):
                // widen to double when either operand is a double literal.
                let txt = self.text(expr);
                if txt.contains('.') {
                    "double".to_string()
                } else {
                    "int".to_string()
                }
            }
            "boolean_literal" => "boolean".to_string(),
            "binary_expression" => {
                // Arithmetic ops produce numeric results; comparisons produce boolean.
                let op = kt::field(expr, "operator")
                    .map(|o| self.text(o).to_string())
                    .unwrap_or_default();
                let is_arith = matches!(op.as_str(), "+" | "-" | "*" | "/" | "%");
                // Operator overload: `Pt(1,2) + Pt(3,4)` — operands are
                // constructor/data-class values, not numbers. The operator
                // `fun plus(o: Pt): Pt` returns the receiver's type. Take it
                // from the LEFT operand even without overload knowledge
                // (Kotlin arithmetic on user classes is always via an
                // operator fun whose return is the class in practice).
                let operands_are_user = kt::field(expr, "left")
                    .map(|l| {
                        let c = kt::child(l, "identifier")
                            .or_else(|| {
                                kt::child(expr, "call_expression")
                                    .and_then(|ce| kt::child(ce, "identifier"))
                            })
                            .map(|n| self.text(n).to_string())
                            .unwrap_or_else(|| self.text(l).trim().to_string());
                        let t = self
                            .text(l)
                            .trim()
                            .split('(')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        let _ = c;
                        t.chars().next().is_some_and(|x| x.is_ascii_uppercase())
                    })
                    .unwrap_or(false);
                if is_arith && operands_are_user {
                    let left = kt::field(expr, "left").map(|l| {
                        self.text(l)
                            .trim()
                            .split('(')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string()
                    });
                    if let Some(t) = left {
                        return t;
                    }
                }
                if is_arith {
                    // widen when either operand is a floating literal
                    let has_dot = self
                        .text(expr)
                        .split(op.trim())
                        .any(|side| side.contains('.'));
                    if has_dot {
                        "double".to_string()
                    } else {
                        "int".to_string()
                    }
                } else {
                    "boolean".to_string()
                }
            }
            "call_expression" => {
                // constructor call: Person(...) -> Person
                let callee = kt::child(expr, "identifier")
                    .map(|n| self.text(n).to_string())
                    .unwrap_or_default();
                let targs = kt::child(expr, "type_arguments").map(|t| {
                    // Box Kotlin primitives inside generic args:
                    // `Map<Op, Int>` -> `Map<Op, Integer>`.
                    crate::transpiler::types::box_primitive_generics(self.text(t))
                });
                // primitive array factories: intArrayOf(...) -> int[]
                if !callee.is_empty()
                    && !callee.contains('.')
                    && let Some(prim) = primitive_array_factory(&callee)
                {
                    return prim.to_string();
                }
                if targs.is_none()
                    && matches!(
                        callee.as_str(),
                        "mutableListOf" | "listOf" | "mutableSetOf" | "setOf" | "arrayListOf"
                    )
                {
                    // Infer the element type from integer-literal args (common case)
                    let mut arg_cursor = expr.walk();
                    let all_int = kt::child(expr, "value_arguments")
                        .map(|a| {
                            a.children(&mut arg_cursor)
                                .filter(|c| c.kind() == "value_argument")
                                .all(|c| self.text(c).trim().parse::<i64>().is_ok())
                        })
                        .unwrap_or(false);
                    let elem = if all_int { "Integer" } else { "Object" };
                    let coll_kind = if callee.ends_with("SetOf") || callee == "setOf" {
                        "Set"
                    } else {
                        "List"
                    };
                    return format!("{}<{}>", coll_kind, elem);
                }
                match (callee.as_str(), targs) {
                    ("mutableListOf", Some(t)) => format!("List{}", t),
                    ("mutableListOf", None) => "List<Object>".to_string(),
                    ("mutableMapOf", Some(t)) => format!("Map{}", t),
                    ("mutableMapOf", None) => "Map<Object, Object>".to_string(),
                    ("mutableSetOf", Some(t)) => format!("Set{}", t),
                    ("mutableSetOf", None) => "Set<Object>".to_string(),
                    ("listOf", Some(t)) => format!("List{}", t),
                    ("listOf", None) => "List<Object>".to_string(),
                    ("setOf", Some(t)) => format!("Set{}", t),
                    ("setOf", None) => "Set<Object>".to_string(),
                    ("mapOf", Some(t)) => format!("Map{}", t),
                    ("mapOf", None) => "Map<Object, Object>".to_string(),
                    // user function call: registered return type wins;
                    // NOT a known fn and uppercase => constructor, fall
                    // through to the constructor arm below.
                    _ if !callee.contains('.') && self.fn_rets.contains_key(callee.as_str()) => {
                        self.fn_rets
                            .get(callee.as_str())
                            .cloned()
                            .unwrap_or_default()
                    }
                    _ => {
                        // member call on a receiver: `m.keys()`, `xs.first()`
                        if let Some(nav) = kt::child(expr, "navigation_expression")
                            && let Some((base, member)) = self.nav_base_member(nav)
                        {
                            return self.infer_member_type(base, &member, expr);
                        }
                        // Uppercase callee with no dot: constructor call
                        if !callee.contains('.')
                            && callee
                                .chars()
                                .next()
                                .is_some_and(|c| c.is_ascii_uppercase())
                        {
                            callee
                        } else {
                            self.diag_approx(
                                expr,
                                format!(
                                    "cannot infer type of call `{}`; local emitted as Object",
                                    self.text(expr).trim()
                                ),
                            );
                            "Object".to_string()
                        }
                    }
                }
            }
            _ => "Object".to_string(),
        }
    }

    fn infer_navigation(&mut self, expr: tree_sitter::Node) -> String {
        if let Some((base, member)) = self.nav_base_member(expr) {
            // `Color.RED` / `Registry.CONSTANT`: uppercase base + uppercase
            // member is a static/enum constant whose type IS the base class.
            let base_text = self.text(base).trim().to_string();
            if base.kind() == "identifier"
                && base_text
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase())
                && member
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase())
            {
                return base_text;
            }
            return self.infer_member_type(base, &member, expr);
        }
        "Object".to_string()
    }

    fn infer_member_type(
        &mut self,
        base: tree_sitter::Node,
        member: &str,
        node: tree_sitter::Node,
    ) -> String {
        let recv_ty: Option<String> = match base.kind() {
            "identifier" => self.var_types.get(self.text(base).trim()).cloned(),
            "string_literal" => Some("String".to_string()),
            _ => None,
        };
        // Object/companion member datums recorded at emission: the member
        // name's registered type wins over the unknown path.
        let static_ty = self.static_member_types.get(member).cloned();
        let unknown = |u: &mut Self| {
            u.diag_approx(
                node,
                format!(
                    "cannot infer type of `{}` (member not in known-mapping table); local emitted as Object",
                    member
                ),
            );
            "Object".to_string()
        };
        match member {
            "uppercase" | "lowercase" | "trim" | "toString" => "String".to_string(),
            "size" | "length" | "count" => "int".to_string(),
            "isEmpty" | "isNotEmpty" | "any" | "all" | "none" => "boolean".to_string(),
            "keys" | "keySet" => {
                let key = recv_ty
                    .as_deref()
                    .and_then(|t| type_arg(t, 0))
                    .unwrap_or_else(|| "Object".to_string());
                format!("Set<{}>", key)
            }
            "entries" | "entrySet" => {
                let key = recv_ty
                    .as_deref()
                    .and_then(|t| type_arg(t, 0))
                    .unwrap_or_else(|| "Object".to_string());
                let val = recv_ty
                    .as_deref()
                    .and_then(|t| type_arg(t, 1))
                    .unwrap_or_else(|| "Object".to_string());
                format!("Set<Map.Entry<{}, {}>>", key, val)
            }
            "values" => {
                let val = recv_ty
                    .as_deref()
                    .and_then(|t| type_arg(t, 1))
                    .unwrap_or_else(|| "Object".to_string());
                format!("Collection<{}>", val)
            }
            "first" | "last" | "firstOrNull" | "lastOrNull" => match recv_ty.as_deref() {
                Some(t) => elem_type_of(t),
                None => unknown(self),
            },
            // Stream-approximated collection ops keep the element type
            "map" | "filter" | "flatMap" | "sorted" | "distinct" | "mapNotNull" | "mapIndexed"
            | "filterIndexed" => match recv_ty.as_deref() {
                Some(t) if is_collection_ty(t) => format!("List<{}>", elem_type_of(t)),
                _ => unknown(self),
            },
            "forEach" => match recv_ty.as_deref() {
                Some(t) if is_collection_ty(t) => "void".to_string(),
                _ => unknown(self),
            },
            "joinToString" => "String".to_string(),
            // Collectors-backed grouping ops return a Map, not a List —
            // groupingBy: Map<K, List<T>>; associate/toMap: Map<K, V> with
            // the lambda-entry shape inferred as entries.
            "groupBy" => match recv_ty.as_deref() {
                Some(t) if is_collection_ty(t) => {
                    format!("Map<Object, List<{}>>", elem_type_of(t))
                }
                _ => unknown(self),
            },
            "associate" | "associateBy" | "toMap" => match recv_ty.as_deref() {
                Some(t) if is_collection_ty(t) => "Map<Object, Object>".to_string(),
                _ => unknown(self),
            },
            "sum" => match recv_ty.as_deref() {
                Some(t) if is_collection_ty(t) => "int".to_string(),
                _ => unknown(self),
            },
            "fold" | "reduce" => match recv_ty.as_deref() {
                Some(t) if is_collection_ty(t) => elem_type_of(t),
                _ => unknown(self),
            },
            _ => match static_ty {
                Some(st) => st,
                None => unknown(self),
            },
        }
    }

    pub fn nav_base_member<'t>(
        &self,
        node: tree_sitter::Node<'t>,
    ) -> Option<(tree_sitter::Node<'t>, String)> {
        let mut cursor = node.walk();
        let kids: Vec<tree_sitter::Node<'t>> = node.children(&mut cursor).collect();
        let base = kids.iter().find(|c| c.is_named()).copied()?;
        let member = kids
            .windows(2)
            .filter(|w| w[0].kind() == "." || w[0].kind() == "?.")
            .filter(|w| w[1].kind() == "identifier")
            .map(|w| self.text(w[1]).to_string())
            .next_back();
        member.map(|m| (base, m))
    }

    pub fn receiver_is_array(&self, base: tree_sitter::Node) -> bool {
        // Declared array vars, plus `.values()` on an enum (an array in
        // Java) and any call/text ending in an array-shaped factory.
        if base.kind() == "identifier"
            && self
                .var_types
                .get(self.text(base).trim())
                .is_some_and(|t| t.ends_with("[]"))
        {
            return true;
        }
        let t = self.text(base).trim();
        t.ends_with("values()") && true
    }
}

/// Kotlin primitive-array factories -> Java array type.
pub(crate) fn primitive_array_factory(name: &str) -> Option<&'static str> {
    Some(match name {
        "intArrayOf" => "int[]",
        "longArrayOf" => "long[]",
        "shortArrayOf" => "short[]",
        "byteArrayOf" => "byte[]",
        "doubleArrayOf" => "double[]",
        "floatArrayOf" => "float[]",
        "booleanArrayOf" => "boolean[]",
        "charArrayOf" => "char[]",
        "arrayOf" => "Object[]",
        _ => return None,
    })
}

/// The nth type argument of a generic Java type text
/// (`Map<String, Integer>` -> idx 0 "String", 1 "Integer").
fn type_arg(java_ty: &str, idx: usize) -> Option<String> {
    let open = java_ty.find('<')?;
    let close = java_ty.rfind('>')?;
    if close < open {
        return None;
    }
    let inner = &java_ty[open + 1..close];
    split_top_level(inner, ',')
        .get(idx)
        .map(|s| s.trim().to_string())
}

/// Split on a separator, respecting nested `<...>` (generics).
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut depth: i32 = 0;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            _ if c == sep && depth == 0 => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    parts.push(cur);
    parts
}

/// Element type of a collection/array Java type: `List<Integer>` -> Integer,
/// `int[]` -> int, `Set<String>` -> String; else Object.
fn elem_type_of(java_ty: &str) -> String {
    if java_ty.ends_with("[]") {
        java_ty.trim_end_matches("[]").trim().to_string()
    } else if java_ty.contains('<') {
        type_arg(java_ty, 0).unwrap_or_else(|| "Object".to_string())
    } else {
        "Object".to_string()
    }
}

/// True for Java collection-ish type texts (List/Set/Map/Collection/Iterable
/// plus array suffix), used to gate element-typed inference.
fn is_collection_ty(java_ty: &str) -> bool {
    let t = java_ty.trim();
    t.ends_with("[]")
        || t.starts_with("List<")
        || t.starts_with("Set<")
        || t.starts_with("Map<")
        || t.starts_with("Collection<")
        || t.starts_with("Iterable<")
}
