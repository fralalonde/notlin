//! Member access and member-call rewrite: property reads -> accessor
//! calls, stdlib-member mapping table, subscript -> get().

use super::Expr;

impl<'a, 'u> Expr<'a, 'u> {
    pub(crate) fn navigation(&mut self, node: tree_sitter::Node) -> String {
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

    pub(crate) fn navigation_call(&mut self, node: tree_sitter::Node) -> String {
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
                    if java_member.contains('(') {
                        // Full-call mapping (`stream().findFirst().orElse(null)`,
                        // `reversed()`): the mapped text is the whole member
                        // expression — emit as-is so the caller's `()` wrapper
                        // doesn't produce `...orElse(null)()`.
                        return format!("{}{}", &raw_trimmed[..dot + 1], java_member);
                    }
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

    pub(crate) fn indexing(&mut self, node: tree_sitter::Node) -> String {
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
}

/// Kotlin stdlib member -> Java counterpart. None = not recognized as
/// stdlib (user-defined methods pass through unmapped).
fn kotlin_member_to_java(member: &str) -> Option<String> {
    let mapped: Option<&str> = match member {
        "uppercase" => Some("toUpperCase"),
        "lowercase" => Some("toLowerCase"),
        "keys" => Some("keySet"),
        "entries" => Some("entrySet"),
        // no-arg collection ops with Java Collection/Stream equivalents.
        // Lambda params `__left`/`__right` can never collide with Kotlin
        // identifiers (Kotlin forbids leading underscores), so `(a, b) -> b`
        // can't shadow user locals named a/b.
        "firstOrNull" => Some("stream().findFirst().orElse(null)"),
        "last" => Some("stream().reduce((__left, __right) -> __right).orElse(null)"),
        "lastOrNull" => Some("stream().reduce((__left, __right) -> __right).orElse(null)"),
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
