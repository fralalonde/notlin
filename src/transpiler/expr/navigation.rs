//! Member access and member-call rewrite: property reads -> accessor
//! calls, stdlib-member mapping table, subscript -> get().

use super::Expr;
use crate::transpiler::kt;

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
                    // proper safe-call: `x?.m` -> `x == null ? null : x.m`
                    // (or a ternary on the whole nav text if in a value
                    // context). Textual: wrap the segment.
                    self.unit.diags.warn_approx(
                        w[1],
                        self.unit.file,
                        "safe-call `?.` -> null-check ternary",
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
                        // Uppercase member: class ref / static member
                        // (Registry.INSTANCE), or a companion-object member
                        // read through the outer class: `Use.MAX` ->
                        // `Use.getMAX()` because companion vals became static
                        // fields with accessors.
                        let base_text = base
                            .map(|b| self.unit.text(b).trim().to_string())
                            .unwrap_or_default();
                        if let Some(accessor) = self.unit.companion_members.get(&member_name) {
                            // known companion member name: getter call
                            let _ = &base_text;
                            result.push_str(&format!(".{}", accessor));
                        } else {
                            result.push_str(&format!(".{}", member_name));
                        }
                    } else {
                        // user-defined property read -> getter call; Pair/
                        // Entry receivers map first/second to the JDK
                        // SimpleImmutableEntry accessors emitted by `to`.
                        // JDK package paths (java.util.List.of) are NOT
                        // property reads — pass the segment verbatim.
                        let base_text0 = base
                            .map(|b| self.unit.text(b).trim().to_string())
                            .unwrap_or_default();
                        if base_text0 == "java"
                            || base_text0.starts_with("java.")
                            || base_text0 == "javax"
                            || base_text0.starts_with("javax.")
                        {
                            result.push_str(&format!(".{}", member_name));
                            continue;
                        }
                        let recv_ty = base
                            .and_then(|b| {
                                self.unit.var_types.get(self.unit.text(b).trim()).cloned()
                            })
                            .unwrap_or_default();
                        if recv_ty.contains("Pair<") || recv_ty.contains("Entry<") {
                            let jfn = if member_name == "first" {
                                "getKey()"
                            } else if member_name == "second" {
                                "getValue()"
                            } else {
                                ""
                            };
                            if !jfn.is_empty() {
                                result.push_str(&format!(".{}", jfn));
                                continue;
                            }
                        }
                        // `name` on an enum constant: Enum#name is public,
                        // BUT data-class records define `id` style accessors;
                        // Kotlin `val name` on a user type maps to getName()
                        // while java.lang.Enum constants expose `name` —
                        // so route enum-typed receivers to .name only when
                        // the base is a known enum type.
                        if member_name == "name"
                            && base
                                .map(|b| {
                                    let raw = self.unit.text(b).trim().to_string();
                                    let first = raw.split('.').next().unwrap_or("").to_string();
                                    self.unit.enum_types.contains(first.as_str())
                                })
                                .unwrap_or(false)
                        {
                            // JDK 25: Enum#name is a private field; the
                            // public accessor is the method `name()`.
                            result.push_str(".name()");
                            continue;
                        }
                        // Data-class record receivers: `q.id` -> `q.id()`
                        // (Java record accessor style, not getter).
                        if base
                            .map(|b| {
                                let t0 = self.unit.text(b).trim().to_string();
                                let ct = self.unit.var_types.get(&t0).cloned();
                                ct.map(|c| self.unit.data_components.contains_key(&c))
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false)
                        {
                            result.push_str(&format!(".{}()", member_name));
                            continue;
                        }
                        // `it.name` on an untyped lambda param: the common
                        // case is enum/string name access -> `name()` (JDK
                        // enum accessor). Strings don't have `name`, but a
                        // Kotlin `val name` user prop would have been a
                        // getter — this context is the LIMITED-subset case
                        // (N002 recorded at the stream-op site).
                        if member_name == "name" {
                            result.push_str(".name()");
                            continue;
                        }
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
        // safe-call: if the source had any `?.`, wrap the whole nav in a
        // null-check ternary: `a?.b` -> `a != null ? a.b : null`.
        // (Do it AFTER windoing so the inner members are already mapped.)
        if self.unit.text(node).contains("?.") {
            // re-derive base text: everything before the last `?.`
            let raw = self.unit.text(node).replace("?.", ".");
            // base = first segment; member chain = the rest
            if let Some(q) = raw.find('.') {
                let (b, m) = raw.split_at(q);
                let m = &m[1..];
                let mut res = format!("{} != null ? {}.{} : null", b, b, m);
                // if the member chain already carries an accessor method
                // applied (getter etc.) the rewritten form here might be
                // stale — leave the current result as-is; the ternary wrap
                // only applies when the whole raw nav is what came out.
                if result.contains(b) {
                    res = format!("{} != null ? {} : null", b, result);
                }
                result = res;
            }
        }
        // Mid-chain joinToString: the stream arm terminated with
        // `collect(toList())`; swap the tail for joining(sep) so the chain
        // compiles (Kotlin List has no joinToString; this is the
        // collector-level rewrite).
        if result.rfind(".joinToString(").is_some()
            && let Some(j) = result.rfind(".joinToString(")
        {
            let head = result[..j].to_string();
            let _ = head;
            let raw = self.unit.text(node);
            let tail = raw[raw.rfind(".joinToString").unwrap_or(0)..]
                .trim()
                .trim_start_matches(".joinToString")
                .trim();
            let inner = tail.trim_start_matches('(').trim_end_matches(')').trim();
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                "joinToString after collect -> collect(joining())",
            );
            if inner.is_empty() {
                return format!("{}.collect(java.util.stream.Collectors.joining())", head);
            }
            return format!(
                "{}.collect(java.util.stream.Collectors.joining({}))",
                head, inner
            );
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
        // `…collect(toList()).joinToString(sep)` — the map arm terminated
        // the stream early; swap the tail for joining(sep) over the same
        // base (List.joinToString does not exist in Java).
        // `…map { … }.joinToString(sep)`: the sibling call is the terminal
        // joinToString — tell call.rs's map arm to choose joining(sep)
        // instead of its default toList() collect.
        if let Some(j) = raw_trimmed.rfind(".joinToString") {
            let _tail = raw_trimmed[j..].trim();
            if base_text(raw_trimmed[..j].to_string().as_str())
                || raw_trimmed.contains(".map ")
                || raw_trimmed.contains(".map {")
            {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "terminal joinToString after map{…} -> collect(joining(sep))",
                );
                // args live on the sibling call node, like curried fold
                let args = node
                    .parent()
                    .filter(|p| p.kind() == "call_expression")
                    .and_then(|p| kt::child(p, "value_arguments"))
                    .map(|va| {
                        let mut ac = va.walk();
                        va.children(&mut ac)
                            .filter(|c| c.kind() == "value_argument")
                            .filter_map(|a| a.children(&mut a.walk()).find(|c| c.is_named()))
                            .map(|e| {
                                let mut ee = Expr { unit: self.unit };
                                ee.transpile(e)
                            })
                            .collect::<Vec<_>>()
                    })
                    .filter(|a| !a.is_empty())
                    .map(|a| a.join(", "))
                    .unwrap_or_default();
                let inner = args;
                self.unit.pending_join_to_string = (!inner.is_empty()).then_some(inner.to_string());
                // strip the joinToString suffix from the callee we return
                // and swallow its (args) — call.rs must not re-emit them.
                raw_trimmed = raw_trimmed[..j].to_string();
                self.unit.pending_full_call = true;
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
                    // `chained.sorted()` (no args) on a mid-stream List —
                    // Kotlin sorted() returns a NEW sorted list; the Java
                    // List API has no equivalent member.
                    if member == "sorted" {
                        self.unit.diags.warn_approx(
                            node,
                            self.unit.file,
                            "List.sorted() approximated as stream().sorted().collect(toList())",
                        );
                        self.unit.pending_full_call = true;
                        return format!(
                            "{}.stream().sorted().collect(java.util.stream.Collectors.toList())",
                            base_java
                        );
                    }
                    // `x.first()`: collection -> stream(); String -> charAt(0)
                    if member == "first" && base_java.trim() == "it" {
                        self.unit.diags.warn_approx(
                            node,
                            self.unit.file,
                            "String.first() inferred: charAt(0) (N002)",
                        );
                        return format!("{}.charAt(0)", base_java);
                    }
                    if member == "first"
                        && self
                            .unit
                            .var_types
                            .get(self.unit.text(b).trim())
                            .map(|t| t.contains("String"))
                            .unwrap_or(false)
                    {
                        return format!("{}.charAt(0)", base_java);
                    }
                    // Curried stream ops: fold(0){lambda}. Identity arg +
                    // lambda both belong here — assemble stream reduce
                    // immediately (call.rs must not re-emit).
                    if matches!(member.as_str(), "fold" | "foldIndexed") {
                        let outer = node.parent().map(|mut p| {
                            loop {
                                if p.kind() == "call_expression"
                                    && kt::child(p, "annotated_lambda").is_some()
                                {
                                    break p;
                                }
                                p = match p.parent() {
                                    Some(q) => q,
                                    None => break p,
                                };
                            }
                        });
                        let inner = node.parent().filter(|p| p.kind() == "call_expression");
                        let identity = inner
                            .and_then(|p| kt::child(p, "value_arguments"))
                            .and_then(|va| {
                                va.children(&mut va.walk())
                                    .find(|c| c.kind() == "value_argument")
                            })
                            .and_then(|arg0| arg0.children(&mut arg0.walk()).find(|c| c.is_named()))
                            .map(|e| self.transpile(e))
                            .unwrap_or_else(|| "null".to_string());
                        self.unit.diags.warn_approx(
                            node,
                            self.unit.file,
                            format!(
                                "fold approximated with Stream.reduce(identity={}, op)",
                                identity
                            ),
                        );
                        let lambda = outer.and_then(|p| {
                            kt::child(p, "lambda_literal").or_else(|| {
                                kt::child(p, "annotated_lambda")
                                    .and_then(|al| kt::child(al, "lambda_literal"))
                            })
                        });
                        if let Some(l) = lambda {
                            let lam = self.transpile(l);
                            let assembled =
                                format!("{}.stream().reduce({}, {})", base_java, identity, lam);
                            self.unit.pending_nav_text = Some(assembled.clone());
                            return assembled;
                        }
                    }
                    return match kotlin_member_to_java(&member) {
                        Some(jm) if jm != member => format!("{}.{}", base_java, jm),
                        Some(_) => format!("{}.{}", base_java, member),
                        None if member == "copy" => {
                            // Hand the FULL callee text (receiver + `.copy`)
                            // to call.rs — its copy arm rebuilds the ctor
                            // with named-arg substitution using
                            // data_components.
                            self.unit.diags.warn_approx(
                                node,
                                self.unit.file,
                                "data-class `copy` -> record ctor reassembly with named args",
                            );
                            format!("{}.{}", base_java, member)
                        }
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
            // Pair.first/.second on a Pair/Entry-typed receiver: the `to`
            // approximation uses AbstractMap.SimpleImmutableEntry, whose
            // accessors are getKey()/getValue().
            if matches!(member, "first" | "second")
                && let Some(b) = base
                && b.kind() == "identifier"
                && self
                    .unit
                    .var_types
                    .get(self.unit.text(b).trim())
                    .is_some_and(|t| {
                        // Only direct Pair/Entry receivers: a List of
                        // entries (`List<Entry<K,V>>`) takes `.first()`
                        // as a collection op, not an accessor.
                        (t.starts_with("Pair<")
                            || t.starts_with("java.util.AbstractMap.SimpleImmutableEntry<")
                            || t.contains("Map.Entry")
                            || t.starts_with("Triple"))
                            && !t.starts_with("List<")
                    })
            {
                let jfn = if member == "first" {
                    "getKey()"
                } else {
                    "getValue()"
                };
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    format!(
                        "`.{member}` on Pair approximated as `{}` on SimpleImmutableEntry",
                        jfn
                    ),
                );
                return format!("{}.{}", self.transpile(b), jfn);
            }
            // `first()` on a known List-typed receiver: Java has no `first`;
            // `get(0)` is the List API closest in semantics. The warn stays
            // because on an empty list Java throws IndexOutOfBoundsException
            // while Kotlin throws NoSuchElementException.
            if member == "first"
                && let Some(b) = base
                && b.kind() == "identifier"
                && self
                    .unit
                    .var_types
                    .get(self.unit.text(b).trim())
                    .is_some_and(|t| t.starts_with("List<"))
            {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "Kotlin `first()` mapped to Java `get(0)`: throws IndexOutOfBoundsException instead of NoSuchElementException on an empty list",
                );
                return format!("{}.get(0)", self.transpile(b));
            }
            // `it.first()` inside a lambda (base not a typed var — lambda
            // param): the overwhelmingly common case is String.first() ->
            // first char.
            if member == "first"
                && let Some(b) = base
                && b.kind() == "identifier"
                && self.unit.text(b).trim() == "it"
            {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "String.first() inferred: charAt(0) (N002)",
                );
                return format!("{}.charAt(0)", self.transpile(b));
            }
            // `x.joinToString("")` / no-arg: stream().collect(joining()).
            // The generic member machinery has no lambda-less branch.
            if member == "joinToString" {
                let base = &raw_trimmed[..dot];
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "joinToString -> stream().map(toString).collect(joining())",
                );
                return format!(
                    "{}.stream().map(Object::toString).collect(java.util.stream.Collectors.joining())",
                    base
                );
            }
            // `…collect(toList()).joinToString(sep)` — the map arm
            // terminated early with toList; swap the tail for
            // joining(sep) over the still-open stream.
            if member == "joinToString"
                && let Some(t) = raw_trimmed[..dot]
                    .strip_suffix(".collect(java.util.stream.Collectors.toList())")
            {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "joinToString after collect(toList()) -> collect(joining())",
                );
                let raw_args = raw_trimmed[dot..]
                    .trim_start_matches(".joinToString")
                    .trim();
                let sep = raw_args
                    .strip_prefix('(')
                    .map(|a| a.strip_suffix(')').unwrap_or(a).trim().to_string())
                    .unwrap_or_default();
                return if sep.is_empty() {
                    format!("{}.collect(java.util.stream.Collectors.joining())", t)
                } else {
                    format!(
                        "{}.collect(java.util.stream.Collectors.joining({}))",
                        t, sep
                    )
                };
            }
            // `it.<prop>` inside a lambda (param type unknown): `.name()`
            // covers the common enum-names mapping case (String.tname has
            // none); N002-note rather than guessing a getter.
            if base
                .map(|b| self.unit.text(b).trim() == "it")
                .unwrap_or(false)
            {
                // name (String/enum) handled below via cap-getter fallback
            }
            // joinToString(sep) -> stream().collect(joining(sep)): need the
            // call's args from the AST — the raw path runs inside call.rs
            // AFTER callee translation, so handle it there via a marker or
            // here by re-reading args from the node tree.
            if member == "joinToString" {
                // extract args from this navigation's enclosing call — walk
                // the tree here instead: the call_expression's value_arguments
                if let Some(call) = node
                    .parent()
                    .filter(|p| p.kind() == "call_expression")
                    .and_then(|p| kt::child(p, "value_arguments"))
                {
                    let mut acur = call.walk();
                    let args: Vec<String> = call
                        .children(&mut acur)
                        .filter(|a| a.kind() == "value_argument")
                        .filter_map(|a| a.children(&mut a.walk()).find(|x| x.is_named()))
                        .map(|e| self.transpile(e))
                        .collect();
                    if !args.is_empty() {
                        self.unit.diags.warn_approx(
                            node,
                            self.unit.file,
                            if args.len() == 1 {
                                "Kotlin `joinToString(sep)` mapped to `stream().collect(joining(sep))`; element toString used"
                            } else {
                                "joinToString with >1 arg (prefix/postfix/limit/transform) approximated as joining(sep); extra args dropped"
                            },
                        );
                        let base_java = self
                            .unit
                            .text(node)
                            .split('.')
                            .next()
                            .unwrap_or("xs")
                            .to_string();
                        // base may itself be compound; use node text minus suffix
                        let _ = base_java;
                        self.unit.pending_full_call = true;
                        return format!(
                            "{}.stream().map(Object::toString).collect(java.util.stream.Collectors.joining({}))",
                            &raw_trimmed[..dot],
                            args[0]
                        );
                    }
                }
            }
            // Uppercase member could be a nested-type constructor
            // (`State.Running(7)`) OR an object/companion member read with
            // call syntax (`Registry.register("x")` — register is a static
            // METHOD on the object's Java class). Only treat it as a
            // constructor when the outer name is itself uppercase (a type);
            // lowercase outer (`Registry`) is a value/instance reference.
            if member
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
                && !member.ends_with(')')
                && raw_trimmed[..dot]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase())
            {
                let outer = &raw_trimmed[..dot];
                return format!("new {}.{}", outer, member);
            }
            // Curried stream ops: `xs.fold(0) { acc, x -> ... }` — the arg
            // list belongs to fold and the lambda arrives at the OUTER call.
            // Return `base.member` bare so call.rs's stream arm assembles
            // stream().reduce(identity, lambda).
            if matches!(
                member,
                "fold"
                    | "foldIndexed"
                    | "reduce"
                    | "sortedBy"
                    | "sortedByDescending"
                    | "groupBy"
                    | "mapValues"
            ) {
                let base = &raw_trimmed[..dot];
                self.unit.pending_full_call = true;
                return format!("{}.{}", base, member);
            }
            // Kotlin `List.sorted()` (no args) has no direct List member in
            // Java; sort a fresh stream pass and collect.
            if member == "sorted" {
                let base = &raw_trimmed[..dot];
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "List.sorted() approximated as stream().sorted().collect(toList())",
                );
                return format!(
                    "{}.stream().sorted().collect(java.util.stream.Collectors.toList())",
                    base
                );
            }
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
        // Both shapes: `indexing_expression` wraps the index in an
        // `indexing_suffix` node; `index_expression` (tree-sitter-kotlin-ng)
        // puts base and index as direct children with bracket punctuators
        // between them.
        // Both shapes: `indexing_expression` wraps each index in an
        // `indexing_suffix` node; `index_expression` (tree-sitter-kotlin-ng)
        // puts base and index(es) as direct children with bracket
        // punctuators between them.
        let mut indices: Vec<_> = kids
            .iter()
            .filter(|c| c.kind() == "indexing_suffix")
            .flat_map(|s| {
                s.children(&mut s.walk())
                    .filter(|c| c.is_named())
                    .collect::<Vec<_>>()
            })
            .collect();
        if indices.is_empty() {
            // index_expression shape: named children after the first are
            // the indices (base is kids[0]).
            indices = kids
                .iter()
                .skip(1)
                .filter(|c| c.is_named())
                .copied()
                .collect();
        }
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
fn base_text(s: &str) -> bool {
    // the nav ends with `.map`/`.filter`/… operator that owns the lambda
    s.contains(".map ")
        || s.contains(".map{")
        || s.rfind(".map").is_some()
        || s.rfind(".filter").is_some()
}

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
        // NOTE: firstOrNull NOT mapped here — with a lambda pred it must
        // route through call.rs's filter(...) form; bare calls hit the
        // fallback passthrough (find symbol error is the least-broken).
        "last" => Some("stream().reduce((__left, __right) -> __right).orElse(null)"),
        "lastOrNull" => Some("stream().reduce((__left, __right) -> __right).orElse(null)"),
        "reversed" => Some("reversed()"),
        "count" => Some("size()"),
        "first" => Some("stream().findFirst().orElseThrow()"),
        // firstOrNull { pred } handled in call.rs (needs the lambda pred);
        // bare firstOrNull() uses the Optional-friendly form below.
        "firstOrNull" => Some("stream().findFirst().orElse(null)"),
        // joinToString(sep) needs the sep argument — handled upstream in the
        // call path where args are available, not by this name table.
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
