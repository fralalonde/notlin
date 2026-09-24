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
        let lambda_arg = lambda_arg.or_else(|| {
            if self.unit.text(node).contains("anyMatch") {
                let mut stack: Vec<tree_sitter::Node> = kids.to_vec();
                while let Some(current) = stack.pop() {
                    if current.kind() == "lambda_literal" {
                        return Some(current);
                    }
                    stack.extend(current.children(&mut current.walk()));
                }
            }
            None
        });

        let mut args: Vec<String> = Vec::new();
        if let Some(an) = args_node {
            let mut inner = an.walk();
            for arg in an.children(&mut inner) {
                if arg.kind() == "value_argument" {
                    let mut ac = arg.walk();
                    let namedkids: Vec<_> = arg
                        .children(&mut ac)
                        .filter(|c| c.is_named())
                        .collect::<Vec<_>>();
                    // `y = 9`: two named kids (identifier + value) and the
                    // raw text carries `=`. Emit `y = 9` so data-class copy
                    // reassembly can substitute by component name.
                    if namedkids.len() == 2 && self.unit.text(arg).contains('=') {
                        let name = self.unit.text(namedkids[0]).trim().to_string();
                        args.push(format!("{} = {}", name, self.transpile(namedkids[1])));
                        continue;
                    }
                    let expr = namedkids
                        .first()
                        .map(|e| {
                            if e.kind() == "spread_expression" {
                                e.children(&mut e.walk())
                                    .find(|c| c.is_named())
                                    .map(|inner| self.transpile(inner))
                                    .unwrap_or_default()
                            } else {
                                self.transpile(*e)
                            }
                        })
                        .unwrap_or_default();
                    args.push(expr);
                } else if arg.kind() == "named_argument" {
                    // `y = 9` — keep as `y = 9` text so downstream maps
                    // (data-class copy reassembly) see both halves.
                    let mut ac = arg.walk();
                    let pair: Vec<_> = arg.children(&mut ac).collect::<Vec<_>>();
                    if pair.len() >= 2 {
                        // identifier, then value
                        let name = self.unit.text(pair[0]).trim().to_string();
                        let vexpr = pair[pair.len() - 1];
                        args.push(format!("{} = {}", name, self.transpile(vexpr)));
                    }
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
            // Map/collection operator members (Map.plus / Map.minus / error
            // only path) emit broken Java when passed through verbatim.
            // Conservative rule: member calls named plus/minus/times on a
            // receiver whose type resolves to a Map taint the caller. A
            // user-defined operator overload keeps its emission (it nests a
            // real method) — detectability lives in the workspace.
            if matches!(member.as_str(), "plus" | "minus" | "times") {
                let recv_text = self.unit.text(base);
                // Receiver type: local inference first, then the file-scoped
                // property/getter lookup (bare `contexts`, `this.getContexts()`,
                // etc.) — cross-file same-name properties must not win over
                // the declaring file's own member.
                let prop = recv_text
                    .trim()
                    .rsplit('.')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches("()")
                    .trim_start_matches("this.");
                let recv_ty = self
                    .infer_operand_type(base)
                    .or_else(|| self.scope_property_type(prop))
                    .or_else(|| {
                        self.scope_property_type(
                            recv_text
                                .trim()
                                .trim_start_matches("this.")
                                .trim_end_matches("()"),
                        )
                    });
                if recv_ty.is_some_and(|ty| {
                    ty.contains("Map<")
                        || ty.contains("List<")
                        || ty.contains("Set<")
                        || ty.contains("Collection<")
                        || ty.contains("Iterable<")
                }) {
                    self.unit.diag_untranslatable(
                        nav,
                        format!(
                            "Map `{member}` member call: collection algebra has no Java member form; declaration retained in Kotlin"
                        ),
                    );
                    return "null".to_string();
                }
            }
            if matches!(
                member.as_str(),
                "filterValues"
                    | "mapKeys"
                    | "associateBy"
                    | "filterNotNull"
                    | "filterIsInstance"
                    | "minus"
            ) {
                let base_java = if base.kind() == "navigation_expression" {
                    self.navigation_call(base)
                } else {
                    self.transpile(base)
                };
                if member == "filterNotNull" {
                    self.unit.diags.warn_approx(
                        nav,
                        self.unit.file,
                        "Iterable.filterNotNull lowered to stream().filter(Objects::nonNull).collect(toList())",
                    );
                    self.unit.pending_full_call = true;
                    return format!(
                        "{base_java}.stream().filter(Objects::nonNull).collect(java.util.stream.Collectors.toList())"
                    );
                }
                if member == "filterIsInstance" {
                    let ty = self.type_arg_of(node).map(|t| {
                        t.trim_start_matches('<')
                            .trim_end_matches('>')
                            .trim()
                            .to_string()
                    });
                    if let Some(ty) = ty {
                        self.unit.diags.warn_approx(
                            nav,
                            self.unit.file,
                            "Iterable.filterIsInstance<T> lowered to stream().filter(x -> x instanceof T).map(x -> (T) x).collect(toList())",
                        );
                        self.unit.pending_full_call = true;
                        return format!(
                            "{base_java}.stream().filter(x -> x instanceof {ty}).map(x -> ({ty}) x).collect(java.util.stream.Collectors.toList())"
                        );
                    }
                }
                if member == "minus" {
                    let arg = node
                        .children(&mut node.walk())
                        .find(|c| c.kind() == "value_arguments")
                        .and_then(|va| {
                            let mut c = va.walk();
                            va.children(&mut c)
                                .find(|a| a.kind() == "value_argument")
                                .and_then(|a| a.children(&mut a.walk()).find(|cc| cc.is_named()))
                                .map(|e| self.transpile(e))
                        });
                    if let Some(arg) = arg {
                        self.unit.diags.warn_approx(
                            nav,
                            self.unit.file,
                            "Iterable.minus(elem) lowered to stream().filter(x -> !Objects.equals(x, elem)).collect(toList())",
                        );
                        self.unit.pending_full_call = true;
                        return format!(
                            "{base_java}.stream().filter(x -> !Objects.equals(x, {arg})).collect(java.util.stream.Collectors.toList())"
                        );
                    }
                }
                return self.map_entry_op(nav, &base_java, &member);
            }
            // Primitive-valued receivers (`.size()`, `.length`, `.count()`)
            // also can't take `.toString()` — same static wrapper path.
            if member == "toString"
                && base.kind() == "navigation_expression"
                && let Some((b, m)) = self.unit.nav_base_member(base)
                && matches!(m.as_str(), "size" | "length" | "count" | "size()")
            {
                let access = if m == "length" {
                    m.to_string()
                } else {
                    format!("{}()", m)
                };
                let recv = format!("{}.{}", self.transpile(b), access);
                self.unit.diags.warn_approx(
                    nav,
                    self.unit.file,
                    "Kotlin primitive `toString(...)` mapped to `String.valueOf(...)`",
                );
                return if args.len() == 1 {
                    format!("Integer.toString({}, {})", recv, args[0])
                } else {
                    format!("String.valueOf({})", recv)
                };
            }
            // Primitive receivers cannot be dereferenced in Java: Kotlin
            // `x.toString(radix)` -> `Integer.toString(x, radix)`,
            // `x.toString()` -> `String.valueOf(x)`, other primitive member
            // calls -> static wrapper when the boxed type has one.
            if base.kind() == "identifier"
                && let Some(bty) = self.unit.var_types.get(self.unit.text(base).trim())
                && matches!(
                    bty.as_str(),
                    "int" | "long" | "short" | "byte" | "double" | "float" | "boolean" | "char"
                )
            {
                let recv = self.transpile(base);
                match member.as_str() {
                    "toString" => {
                        self.unit.diags.warn_approx(
                            nav,
                            self.unit.file,
                            "Kotlin primitive `toString(...)` mapped to the boxed static wrapper (`Integer.toString`/`String.valueOf`)",
                        );
                        return if args.len() == 1 {
                            format!("Integer.toString({}, {})", recv, args[0])
                        } else {
                            format!("String.valueOf({})", recv)
                        };
                    }
                    "toInt" => return recv,
                    "toLong" => return format!("((long) {})", recv),
                    "toDouble" => return format!("((double) {})", recv),
                    "toFloat" => return format!("((float) {})", recv),
                    _ => {}
                }
                // takeIf/takeUnless with a trailing-lambda predicate: no
                // Java deref possible for primitives; lambda-less element
                // read must fall back to a N001 taint instead of a broken
                // `int.takeIf` method reference.
                if let (true, Some(lnode)) = (
                    matches!(member.as_str(), "takeIf" | "takeUnless"),
                    lambda_arg,
                ) {
                    // Kotlin `v.takeIf { it > 0 }` -> `v > 0 ? v : null`
                    // (`takeUnless` -> negation). The subtree beyond the
                    // params/arrow is the predicate expression.
                    let mut body_children: Vec<tree_sitter::Node> = {
                        let mut cur = lnode.walk();
                        lnode
                            .children(&mut cur)
                            .filter(|c| c.is_named() && c.kind() != "lambda_parameters")
                            .collect()
                    };
                    if let Some(pos_arrow) = body_children.iter().position(|c| c.kind() == "->") {
                        body_children = body_children[pos_arrow + 1..].to_vec();
                    }
                    if body_children.len() == 1 {
                        let pred = self.transpile(body_children[0]);
                        let recv_name = self.unit.text(base).trim();
                        let pred = pred.replace("it", recv_name);
                        self.unit.diags.warn_approx(
                            node,
                            self.unit.file,
                            format!(
                                "primitive `.{}` inlined as null-check ternary (no receiver deref in Java)",
                                member
                            ),
                        );
                        return if member == "takeIf" {
                            format!("({} ? {} : null)", pred, recv)
                        } else {
                            format!("(!({}) ? {} : null)", pred, recv)
                        };
                    }
                }
            }
            if self.unit.lombok
                && self.unit.commons_lang
                && member == "toList"
                && self.unit.receiver_is_array(base)
            {
                let receiver = self.transpile(base);
                return format!("org.apache.commons.lang3.ArrayUtils.toList({receiver})");
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
        // Kotlin stdlib `TODO(msg)` has type `Nothing` and always throws
        // (NotImplementedError); Java has no such class. Lower to a THROW
        // expression so statement emitters render it as `throw ...;`
        // instead of `return new TODO(...);` (which references a class
        // that does not exist). Under --commons-lang the throw carries
        // org.apache.commons.lang3.NotImplementedException.
        if callee_java == "TODO" {
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                "Kotlin `TODO(msg)` lowered to a NotImplemented-style throw (it is Nothing-typed, never returns)",
            );
            let msg = args.first().cloned().unwrap_or_default();
            let exception = if self.unit.commons_lang {
                "org.apache.commons.lang3.NotImplementedException".to_string()
            } else {
                "RuntimeException".to_string()
            };
            return if msg.is_empty() {
                format!("throw new {exception}()")
            } else {
                format!("throw new {exception}({msg})")
            };
        }

        // `Receiver.copy(x = 1, z = 2)` on a data-class/record receiver:
        // Java records have no copy() — rebuild: `new Receiver(k, v, …)`
        // with named args substituted in declared component order.
        if let Some(cpos) = callee_java.rfind(".copy")
            && callee_java.ends_with(".copy")
        {
            let recv = callee_java[..cpos].to_string();
            if let Some(ctor) = recv.strip_prefix("new ") {
                let tname = ctor.split('(').next().unwrap_or("").trim().to_string();
                if let Some(comps) = self.unit.data_components.get(&tname).cloned() {
                    // ctor args, in order
                    if let (Some(op), Some(cp)) = (ctor.find('('), ctor.rfind(')')) {
                        let inner = &ctor[op + 1..cp];
                        let mut cargs: Vec<String> = if inner.trim().is_empty() {
                            Vec::new()
                        } else {
                            inner.split(',').map(|s| s.trim().to_string()).collect()
                        };
                        // Named args (`y = 9`) keep their `name = value`
                        // text through `args`; parse it back here.
                        let mut arg_pairs: Vec<(String, String)> = Vec::new();
                        for arg in &args {
                            if let Some(eq) = arg.find(" = ") {
                                arg_pairs.push((
                                    arg[..eq].trim().to_string(),
                                    arg[eq + 3..].trim().to_string(),
                                ));
                            }
                        }
                        for (n, v) in &arg_pairs {
                            if let Some(idx) =
                                comps.iter().position(|(_ct, cn)| cn.trim() == n.trim())
                            {
                                while cargs.len() < comps.len() {
                                    cargs.push(String::new());
                                }
                                cargs[idx] = v.clone();
                            }
                        }
                        let _ = &mut cargs;
                        let full: Vec<String> = comps
                            .iter()
                            .enumerate()
                            .map(|(i, _)| cargs.get(i).cloned().unwrap_or_default())
                            .collect();
                        let full = if full.iter().any(|s| s.is_empty()) {
                            // cannot fill every component positionally —
                            // mark instead of emitting broken Java.
                            self.unit.diags.warn_approx(
                                node,
                                self.unit.file,
                                "data-class `copy` with gaps: component defaults unavailable, entity marked for manual review",
                            );
                            comps
                                .iter()
                                .enumerate()
                                .map(|(i, _)| {
                                    cargs.get(i).cloned().unwrap_or_else(|| "0".to_string())
                                })
                                .collect::<Vec<_>>()
                        } else {
                            full
                        };
                        return format!("new {}({})", tname, full.join(", "));
                    }
                }
            }
        }
        // Collection slice ops on List receivers (Kotlin -> Java streams):
        // take(n) -> stream().limit(n).collect(toList());
        // drop(n) -> stream().skip(n).collect(toList()); chunked(n) ->
        // no direct API — marked below.
        if args.len() == 1 {
            let tail = callee_java.rsplit('.').next().unwrap_or("");
            if matches!(tail, "take" | "drop") && callee_java.rfind('.').is_some() {
                let recv = callee_java[..callee_java.len() - tail.len() - 1]
                    .trim()
                    .to_string();
                let jop = if tail == "take" { "limit" } else { "skip" };
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    format!("Kotlin collection `.{tail}` -> stream().{jop}().collect(toList())"),
                );
                return format!(
                    "{}.stream().{}({}).collect(java.util.stream.Collectors.toList())",
                    recv, jop, args[0]
                );
            }
            // chunked(n): sliding-free partition via IntStream over chunk
            // indices — logic-equal, emits a List<List<T>>.
            if tail == "chunked" && callee_java.rfind('.').is_some() {
                let recv = callee_java[..callee_java.len() - tail.len() - 1]
                    .trim()
                    .to_string();
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "Kotlin `.chunked(n)` -> IntStream partition (List<List<T>>)",
                );
                return format!(
                    "java.util.stream.IntStream.range(0, (int) (({}.stream().count() + {} - 1) / {})).mapToObj(i -> {}.subList(i * {}, Math.min((i + 1) * {}, {}.size()))).collect(java.util.stream.Collectors.toList())",
                    recv, args[0], args[0], recv, args[0], args[0], recv
                );
            }
            // zip(other): pair up positionally with SimpleImmutableEntry
            // values -> List<SimpleImmutableEntry<A,B>> (matches the Kotlin
            // Pair approximation used by `to`).
            if tail == "zip" && callee_java.rfind('.').is_some() {
                let recv = callee_java[..callee_java.len() - tail.len() - 1]
                    .trim()
                    .to_string();
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "Kotlin `.zip(other)` -> IntStream pairwise zip with SimpleImmutableEntry pairs",
                );
                return format!(
                    "java.util.stream.IntStream.range(0, Math.min({}.size(), {}.size())).mapToObj(i -> new java.util.AbstractMap.SimpleImmutableEntry<>({}.get(i), {}.get(i))).collect(java.util.stream.Collectors.toList())",
                    recv, args[0], recv, args[0]
                );
            }
        }

        // `new Regex(p)` -> `Pattern.compile(p)`; matches(input) ->
        // `Pattern.compile(p).matcher(input).matches()`.
        if callee_java == "Regex" && !args.is_empty() {
            return format!("java.util.regex.Pattern.compile({})", args.join(", "));
        }
        if let Some(rpos) = callee_java.rfind(".matches")
            && callee_java.ends_with(".matches")
        {
            let recv = callee_java[..rpos].trim().to_string();
            if recv.starts_with("new Regex(") {
                let inner = recv.trim_start_matches("new Regex(").trim_end_matches(')');
                return format!(
                    "java.util.regex.Pattern.compile({}).matcher({}).matches()",
                    inner, args[0]
                );
            }
            if let Some(inner) = recv
                .strip_prefix("java.util.regex.Pattern.compile(")
                .filter(|_| recv.ends_with(')'))
                .map(|t| &t[..t.len() - 1])
            {
                return format!(
                    "java.util.regex.Pattern.compile({}).matcher({}).matches()",
                    inner, args[0]
                );
            }
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
                        | "sortedBy"
                        | "sortedByDescending"
                        | "mapValues"
                        | "any"
                        | "all"
                        | "none"
                        | "anyMatch"
                        | "allMatch"
                        | "noneMatch"
                        | "count"
                        | "first"
                        | "firstOrNull"
                        | "last"
                        | "lastOrNull"
                        | "joinToString"
                )
            })
            .unwrap_or(false);
        // Scope function with receiver: `x.apply { ... }` / `x.also { ... }`
        // -> IIFE-style static helper? No: Java 25 has no scope functions;
        // nearest logic-preserving form is a nested block with a typed
        // local. Left as an explicit N001 when the callee is a receiver-scope
        // function to avoid silently emitting a method that does not exist.
        // No-lambda collection terminators (sum) must fire before the
        // lambda-gated block.
        let member0 = callee_java.rsplit('.').next().unwrap_or("");
        if member0 == "sum" && lambda_arg.is_none() {
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                "`sum()` approximated with mapToInt(...).sum()",
            );
            return format!(
                "{}.stream().mapToInt(Integer::intValue).sum()",
                callee_java[..callee_java.len() - member0.len()].trim_end_matches('.')
            );
        }

        if lambda_arg.is_some() {
            let member = callee_java.rsplit('.').next().unwrap_or("");
            // Stream-terminator predicates: rewrite to filter(...) before
            // the generic stream path (its full-expression mappings drop
            // the predicate).
            if member == "firstOrNull" {
                let base = callee_java[..callee_java.len() - member.len()]
                    .trim_end_matches('.')
                    .to_string();
                let Some(lnode_fo) = lambda_arg else {
                    return callee_java.clone();
                };
                let pred = self.transpile(lnode_fo);
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "firstOrNull { pred } approximated with filter(...).findFirst().orElse(null)",
                );
                return format!(
                    "{}.stream().filter({}).findFirst().orElse(null)",
                    base,
                    it_subst(&pred)
                );
            }
            if member == "sum" {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "`sum()` approximated with mapToInt(...).sum()",
                );
                return format!(
                    "{}.stream().mapToInt(Integer::intValue).sum()",
                    callee_java[..callee_java.len() - member.len()].trim_end_matches('.')
                );
            }
            if matches!(member, "apply" | "also" | "run" | "with" | "let") {
                self.unit.diag_untranslatable(
                    node,
                    format!(
                        "scope function `.{}` not reproduced: Java has no equivalent; logic must be hand-migrated",
                        member
                    ),
                );
                // Inline comment breaks surrounding expressions; the
                // diagnostic listing carries the message.
                return "null".to_string();
            }
        }
        // A curried fold already assembled its full `stream().reduce(...)`
        // text in navigation_call — take it verbatim and clear it.
        let nav_text = self.unit.pending_nav_text.take();
        let nav_assembled = nav_text.is_some();
        if let (true, Some(lambda)) = (is_stream_op, lambda_arg)
            && !nav_assembled
        {
            let member = callee_java
                .rfind('.')
                .map(|i| &callee_java[i + 1..])
                .unwrap();
            let base = {
                let b = &callee_java[..callee_java.len() - member.len() - 1];
                // Arrays (e.g. `Op.values()`, `chArray`) have no .stream();
                // wrap with Arrays.stream(...).
                if b.ends_with("values()") {
                    format!("java.util.Arrays.stream({})", b)
                } else {
                    b.to_string()
                }
            };
            // Arrays.stream(...) already IS a Stream — no .stream() tail.
            let stream_base =
                if base.starts_with("java.util.Arrays.stream") || base.ends_with(".stream()") {
                    base.clone()
                } else if self
                    .unit
                    .var_types
                    .get(base.trim())
                    .is_some_and(|t| t.starts_with("Map<"))
                {
                    // Kotlin maps stream over their ENTRIES (Map.Entry pairs).
                    format!("{}.entrySet().stream()", base)
                } else {
                    format!("{}.stream()", base)
                };
            let _ = &stream_base;
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
                    "collection op `.{member} {{...}}` approximated with Stream",
                    member = member
                ),
            );
            // joinToString(sep) -> collect(joining(sep)): the separator is
            // the first positional arg; other overloads (prefix/postfix/
            // limit/transform) degrade to joinToString-less collection with
            // a warn via the generic stream path below only when args exist
            if member == "joinToString" && !args.is_empty() {
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    if args.len() == 1 {
                        "Kotlin `joinToString(sep)` mapped to `stream().collect(joining(sep))`"
                    } else {
                        "joinToString with >1 arg (prefix/postfix/limit/transform) approximated as joining(sep); extra args dropped"
                    },
                );
                return format!(
                    "{}.stream().map(Object::toString).collect(java.util.stream.Collectors.joining({}))",
                    base, args[0]
                );
            }
            // forEach terminates the stream: it returns void, so no
            // `.collect(...)` tail (chaining collect after forEach is a
            // compile error and would drop the loop's effect entirely).
            if member == "forEach" {
                return format!("{}.stream().forEach({});", base, self.transpile(lambda));
            }
            // Collectors-backed ops with distinct Java shapes:
            let mapped_lambda = self.transpile(lambda);
            let base_str = callee_java[..callee_java.len() - member.len()]
                .trim_end_matches('.')
                .to_string();
            // Optional receiver (`opt.map { .. }`): Java Optional has its own
            // map/flatMap/filter members — no stream/collect detour; keep the
            // chain on the Optional so terminals like orElse/orElseGet that
            // follow still apply.
            if matches!(
                member,
                "map" | "mapNotNull" | "mapIndexed" | "filter" | "filterIndexed" | "flatMap"
            ) && {
                let recv_name = |s: &String| {
                    s.rsplit('.')
                        .next()
                        .unwrap_or("")
                        .trim_end_matches("()")
                        .trim_start_matches("this.")
                        .to_string()
                };
                let mrti = |name: &str| -> Option<String> {
                    let ws = self.unit.workspace?;
                    let declaring = self
                        .unit
                        .workspace_file
                        .as_deref()
                        .unwrap_or(self.unit.file);
                    ws.method_return_type_in_file(declaring, name)
                };
                // Receiver kind matters: `getX()` is a METHOD call — its
                // declared return type wins; getter-name properties are only
                // a fallback for field-style receivers. A cross-file property
                // with the same name must not shadow the local method.
                mrti(base_str.as_str())
                    .or_else(|| self.scope_property_type(&recv_name(&base_str)))
                    .or_else(|| {
                        mrti(
                            base_str
                                .trim()
                                .trim_start_matches("this.")
                                .trim_end_matches("()")
                                .rsplit('.')
                                .next()
                                .unwrap_or(""),
                        )
                    })
                    .is_some_and(|t| t.contains("Optional<"))
            } {
                return format!("{}.{}({})", base_str, stream_fn, self.transpile(lambda));
            }
            match member {
                // fold(init) { acc, x -> ... } -> reduce(identity, op)
                "fold" => {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "fold approximated with Stream.reduce(identity, op)",
                    );
                    // The lambda maps to `(acc, x) -> body`; reduce wants a
                    // BiFunction — the transpiled form already is one.
                    return format!(
                        "{}.stream().reduce({}, {})",
                        base_str, args[0], mapped_lambda
                    );
                }
                "reduce" => {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "reduce approximated with Stream.reduce(op) (Optional; Kotlin throws — .orElseThrow() added)",
                    );
                    return format!(
                        "{}.stream().reduce({}).orElseThrow()",
                        base_str, mapped_lambda
                    );
                }
                "mapValues" => {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "mapValues approximated with entrySet().stream().collect(toMap(keySet, valueFn))",
                    );
                    // mapped_lambda is `<param> -> body` from the lambda
                    // transpile; toMap wants a bare value fn.
                    let vfn = mapped_lambda
                        .split_once(" -> ")
                        .map(|(_, b)| b.to_string())
                        .unwrap_or_else(|| mapped_lambda.clone())
                        .replace("it.getValue()", "__e.getValue()")
                        .replace("it.value", "__e.getValue()");
                    return format!(
                        "{}.entrySet().stream().collect(java.util.stream.Collectors.toMap(java.util.Map.Entry::getKey, __e -> {}))",
                        base_str, vfn
                    );
                }
                "groupBy" => {
                    // `xs.groupBy { it % 2 }` -> groupingBy(keyFn)
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "groupBy approximated with Collectors.groupingBy (value lists, LinkedHashMap default ordering differs)",
                    );
                    return format!(
                        "{}.collect(java.util.stream.Collectors.groupingBy({}))",
                        base, mapped_lambda
                    );
                }
                "associate" => {
                    // `xs.associate { it to it * 2 }`: the lambda was mapped
                    // to `new SimpleImmutableEntry<>(k, v)` by infix `to` —
                    // convert to toMap(kFn, vFn).
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "associate approximated with Collectors.toMap",
                    );
                    let kv = entry_lambda_to_kv(&mapped_lambda);
                    return format!(
                        "{}.collect(java.util.stream.Collectors.toMap({}, {}))",
                        base, kv.0, kv.1
                    );
                }
                "sortedBy" | "sortedByDescending" => {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "sortedBy approximated with sorted(Comparator.comparing(key))",
                    );
                    return format!(
                        "{}.sorted(java.util.Comparator.comparing({})).collect(java.util.stream.Collectors.toList())",
                        base_str,
                        it_subst(&mapped_lambda)
                    );
                }
                _ => {}
            }
            // Chained stream continuation (`xs.stream().filter {..}.findFirst()`):
            // the base already ends `.stream()` AND the lambda member is a
            // stream-monadic op — the result must stay a Stream so terminal
            // members appended by the compound chain still apply. Emitting a
            // collect here (or double-streaming) breaks the outer chain.
            let continuation = base_str.ends_with(".stream()")
                && matches!(
                    member,
                    "map"
                        | "mapNotNull"
                        | "mapIndexed"
                        | "filter"
                        | "filterIndexed"
                        | "sorted"
                        | "flatMap"
                        | "distinct"
                );
            if continuation {
                let inner = &base_str[..base_str.len() - ".stream()".len()];
                return format!(
                    "{}.stream().{}({})",
                    inner,
                    stream_fn,
                    self.transpile(lambda)
                );
            }
            if matches!(member, "anyMatch" | "allMatch" | "noneMatch") {
                return format!("{}.{}({})", stream_base, stream_fn, self.transpile(lambda));
            }
            return if let Some(sep) = self.unit.pending_join_to_string.take() {
                format!(
                    "{}.{}({}).collect(java.util.stream.Collectors.joining({}))",
                    stream_base,
                    stream_fn,
                    self.transpile(lambda),
                    sep
                )
            } else {
                format!(
                    "{}.{}({}).collect(java.util.stream.Collectors.toList())",
                    stream_base,
                    stream_fn,
                    self.transpile(lambda)
                )
            };
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
            "listOfNotNull" => {
                // Filters nulls: List.of rejects nulls, so stream the args
                // and keep non-nulls.
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "listOfNotNull approximated with Stream.filter(Objects::nonNull) over List.of",
                );
                format!(
                    "Stream.of({}).filter(java.util.Objects::nonNull).collect(java.util.stream.Collectors.toList())",
                    args.join(", ")
                )
            }
            "mutableMapOf" | "mapOf" | "hashMapOf" => {
                if args.is_empty() {
                    "new HashMap<>()".to_string()
                } else {
                    // args are `new SimpleImmutableEntry<>(k, v)` forms from
                    // the `to` infix mapping — Map.ofEntries accepts them
                    // (Map.Entry impl); immutable semantics match mapOf.
                    let entries: Vec<String> = args
                        .iter()
                        .map(|a| {
                            a.trim_start_matches("Map.ofEntries(")
                                .trim_end()
                                .to_string()
                        })
                        .collect();
                    if args.len() == 1 {
                        format!("java.util.Map.ofEntries({})", entries[0])
                    } else {
                        self.unit.diags.warn_approx(
                            node,
                            self.unit.file,
                            "mapOf with multiple entries: Map.ofEntries(e1, e2, ...)",
                        );
                        format!("java.util.Map.ofEntries({})", entries.join(", "))
                    }
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
                // A lone spread arg (`arrayOf(*arr)`) is an array copy:
                // `java.util.Arrays.copyOf(arr, arr.length)` — the literal
                // spread `{*arr}` text is invalid Java.
                let elem_ty = primitive_array_factory(k).unwrap();
                let spread_arg = {
                    // a spread arg was lowered to the bare array text by
                    // transpile()'s spread arm — spot it via the raw subtree
                    let mut found = false;
                    let _cursor = node.walk();
                    if let Some(va) = kt::child(node, "value_arguments") {
                        let mut vcur = va.walk();
                        for argn in va.children(&mut vcur) {
                            if argn.kind() == "value_argument"
                                && argn
                                    .children(&mut argn.walk())
                                    .any(|c| c.kind() == "spread_expression")
                            {
                                found = true;
                            }
                        }
                    }
                    found
                };
                if spread_arg {
                    // A Java array literal cannot spread (`{*a}` is invalid);
                    // the array of the spread parts plus trailing elements is
                    // rebuilt as a concat of flattened element streams. Order
                    // and contents are preserved; shape degrades to Object[]
                    // for `arrayOf` (and is rejected honestly for primitives
                    // via javac on the target).
                    let mut parts: Vec<String> = Vec::new();
                    let _cursor = node.walk();
                    if let Some(va) = kt::child(node, "value_arguments") {
                        let mut vcur = va.walk();
                        for argn in va.children(&mut vcur) {
                            if argn.kind() != "value_argument" {
                                continue;
                            }
                            let mut acur = argn.walk();
                            let ex = match argn.children(&mut acur).find(|c| c.is_named()) {
                                Some(e) => e,
                                None => continue,
                            };
                            if ex.kind() == "spread_expression" {
                                let mut scur = ex.walk();
                                let arr = ex
                                    .children(&mut scur)
                                    .find(|c| c.is_named())
                                    .map(|c| self.transpile(c))
                                    .unwrap_or_else(|| "null".to_string());
                                parts.push(format!("java.util.stream.Stream.of({})", arr));
                            } else {
                                let v = self.transpile(ex);
                                parts.push(format!("java.util.stream.Stream.of({})", v));
                            }
                        }
                    }
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "spread in array factory lowered to a flattened element stream; produces Object[] (primitive element trays need hand migration)",
                    );
                    // Compose flatMap for multi-part spreads:

                    if parts.len() == 1 {
                        let h = parts[0]
                            .strip_prefix("java.util.stream.Stream.of(")
                            .unwrap_or(&parts[0])
                            .trim_end_matches(')');
                        format!("java.util.stream.Stream.of({}).toArray()", h)
                    } else {
                        // Java streams can't concat an element stream with a
                        // bare scalar: wrap each non-spread part in a
                        // one-element Object[] so concat stays homogeneous.
                        // A spread part keeps its own object stream.
                        let strip = |s: &str| {
                            s.strip_prefix("java.util.stream.Stream.of(")
                                .unwrap_or(s)
                                .trim_end_matches(')')
                                .to_string()
                        };
                        let spread_flags = {
                            let mut flags: Vec<bool> = Vec::new();
                            let _cursor = node.walk();
                            if let Some(va) = kt::child(node, "value_arguments") {
                                let mut vcur = va.walk();
                                for argn in va.children(&mut vcur) {
                                    if argn.kind() != "value_argument" {
                                        continue;
                                    }
                                    let mut acur = argn.walk();
                                    let is_spread = argn
                                        .children(&mut acur)
                                        .any(|c| c.kind() == "spread_expression");
                                    flags.push(is_spread);
                                }
                            }
                            flags
                        };
                        let mut chain: String = if spread_flags.first() == Some(&true) {
                            parts[0].clone()
                        } else {
                            format!(
                                "java.util.stream.Stream.of(new Object[]{{{}}})",
                                strip(&parts[0])
                            )
                        };
                        for (i, p) in parts[1..].iter().enumerate() {
                            let tail = if spread_flags.get(i + 1) == Some(&true) {
                                p.clone()
                            } else {
                                format!("java.util.stream.Stream.of(new Object[]{{{}}})", strip(p))
                            };
                            chain = format!("java.util.stream.Stream.concat({}, {})", chain, tail);
                        }
                        format!("{}.toArray()", chain)
                    }
                } else {
                    // `arrayOf(a, b)` style literal: elem_ty ends in `[]`
                    format!(
                        "new {}[]{{{}}}",
                        elem_ty.trim_end_matches("[]"),
                        args.join(", ")
                    )
                }
            }
            _ => {
                // Uppercase callee = constructor call — bare `Foo` or nested
                // `Outer.Inner` both need `new` (data subclasses of a sealed
                // nesting parent are the common case).
                let last_seg = callee_java.rsplit('.').next().unwrap_or("");
                if callee_java == "Triple" {
                    // JDK has no 3-tuple; N001 taint, null emit.
                    self.unit.diag_untranslatable(
                        node,
                        "Triple not translatable: JDK has no 3-tuple type; hand-migrate to a small record",
                    );
                    "null".to_string()
                } else if callee_java
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase())
                    && last_seg
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_uppercase())
                {
                    // Sealed/abstract Kotlin classes route construction
                    // through their companion `operator fun invoke(...)`:
                    // `Surname(...)` is a factory call
                    // (`Owner.invoke(...)`), NOT a constructor — Java
                    // `new Surname(...)` does not compile.
                    let mut ctor_args = args.clone();
                    if let Some(ws) = self.unit.workspace
                        && let Some(decl) = ws.declarations().find(|d| d.name == callee_java)
                    {
                        let properties = decl
                            .members
                            .iter()
                            .filter(|m| m.kind == crate::workspace::MemberKind::Property)
                            .count();
                        if decl.has_default_constructor_parameter
                            && properties == ctor_args.len() + 1
                        {
                            ctor_args.push("null".to_string());
                        }
                    }
                    let base_name = callee_java.rsplit('.').next().unwrap_or("").to_string();
                    if let Some(ws) = self.unit.workspace
                        && ws.find_static_member(&base_name, "invoke").is_some()
                    {
                        self.unit.diags.warn_approx(
                                node,
                                self.unit.file,
                                format!(
                                    "companion `operator fun invoke` on `{base_name}`: factory call routed via `.{base_name}.invoke(...)` (not a constructor)"
                                ),
                            );
                        return format!("{}.invoke({})", callee_java, ctor_args.join(", "));
                    }
                    format!("new {}({})", callee_java, ctor_args.join(", "))
                } else if callee_java.ends_with(')')
                    && (args.is_empty() || (nav_assembled && lambda_arg.is_some()))
                {
                    // Mapped member that is already a complete call expression
                    // (`xs.get(0)`, `xs.stream().findFirst().orElse(null)`):
                    // it IS the call — no `()` wrapper to add. A trailing
                    // lambda argument still appends (fold(idx) { ... }).
                    if let Some(la) = lambda_arg {
                        if callee_java.ends_with(".anyMatch")
                            || callee_java.ends_with(".allMatch")
                            || callee_java.ends_with(".noneMatch")
                        {
                            format!("{}({})", callee_java, args.join(", "))
                        } else if let Some(assembled) = nav_text {
                            // navigation_call already merged the lambda and
                            // assembled `stream().reduce(identity, op)`.
                            let _ = la;
                            assembled
                        } else {
                            format!("{} {}", callee_java, self.transpile(la))
                        }
                    } else {
                        callee_java
                    }
                } else if callee_java.rfind(".joinToString(").is_some() {
                    // trailing joinToString(sep) was re-collected with
                    // joining(sep) upstream; drop the tail entirely.
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "trailing joinToString(sep) dropped: already collected with joining(sep)",
                    );
                    callee_java
                } else if std::mem::replace(&mut self.unit.pending_full_call, false) {
                    // joinToString style: callee mapping already emitted the
                    // full call with args — nothing to append.
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

    /// Callee translation when the type-argument brackets were swallowed into
    /// a binary_expression (`spec<String> { ... }`): translate the callee,
    /// drop the phantom `<Type>` text, and emit a warn explaining the
    /// type-argument loss (Java re-inferable in the common `fun <T>` case).
    pub(crate) fn transpile_callee_generic(
        &mut self,
        callee: tree_sitter::Node,
        type_arg: Option<&str>,
    ) -> String {
        let base = self.transpile_callee(callee);
        if let Some(ty) = type_arg {
            self.unit.diags.warn_approx(
                callee,
                self.unit.file,
                format!(
                    "generic call `{}` with type argument `<{}>` merged from lambda-bracket ambiguity; type argument dropped (Java inference must re-derive it)",
                    base, ty
                ),
            );
        }
        base
    }

    fn type_arg_of(&self, call: tree_sitter::Node) -> Option<String> {
        kt::child(call, "type_arguments").map(|t| self.unit.text(t).to_string())
    }

    pub(crate) fn lambda(&mut self, node: tree_sitter::Node) -> String {
        // lambda_literal: { params -> body }. The body is real AST (named
        // children after `->`) — transpiling it keeps println and other
        // top-level rewrites intact; raw text pass-through loses them.
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        // params
        let mut params_java = String::new();
        // lambda params registered as locals for the body; popped after
        let mut inserted: Vec<String> = Vec::new();
        let mut body_nodes: Vec<tree_sitter::Node> = Vec::new();
        let mut after_arrow = !kids.iter().any(|c| c.kind() == "->");
        for c in kids {
            match c.kind() {
                "lambda_parameters" => {
                    let mut pcur = c.walk();
                    let names: Vec<String> = c
                        .children(&mut pcur)
                        .filter(|vc| vc.kind() == "variable_declaration")
                        .filter_map(|vd| kt::child(vd, "identifier"))
                        .map(|n| self.unit.text(n).to_string())
                        .collect();
                    params_java = names.join(", ");
                    // Lambda params are LOCALS: register them before the body
                    // transpiles, otherwise the body's bare identifier falls
                    // through to workspace property resolution and gets
                    // rewritten to a spurious `this.getX()`.
                    for n in &names {
                        if !self.unit.var_types.contains_key(n) {
                            self.unit
                                .var_types
                                .insert(n.to_string(), "__lambda_param".to_string());
                            inserted.push(n.to_string());
                        }
                    }
                }
                "->" => after_arrow = true,
                k if after_arrow && c.is_named() && k != "{" && k != "}" => {
                    body_nodes.push(c);
                }
                _ => {}
            }
        }
        // body statements -> `\n`-joined expressions (Kotlin lambda bodies
        // here are single expressions in practice)
        let mut body_java = String::new();
        for (i, bn) in body_nodes.iter().enumerate() {
            if i > 0 {
                body_java.push_str("; ");
            }
            body_java.push_str(&self.transpile(*bn));
        }
        for n in &inserted {
            self.unit.var_types.remove(n);
        }
        if body_java.is_empty() {
            body_java = "{}".to_string();
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                "empty lambda body emitted as a no-op block",
            );
        }
        if params_java.is_empty() {
            // `{ it * 2 }`: implicit `it` parameter — the body references it.
            let uses_it = body_nodes
                .iter()
                .any(|bn| Self::body_uses_it(*bn, self.unit.source));
            if uses_it {
                format!("it -> {}", body_java)
            } else {
                format!("() -> {}", body_java)
            }
        } else if params_java.contains(',') {
            // Multi-param lambda: Java requires the paren list
            format!("({}) -> {}", params_java, body_java)
        } else {
            format!("{} -> {}", params_java, body_java)
        }
    }

    /// Does this lambda-body tree reference the implicit `it` parameter?
    fn body_uses_it<'t>(node: tree_sitter::Node<'t>, source: &'t str) -> bool {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if n.kind() == "identifier"
                && n.parent().map(|p| p.kind()) != Some("lambda_parameters")
                && kt::text(n, source) == "it"
            {
                return true;
            }
            let mut cur = n.walk();
            for c in n.children(&mut cur) {
                stack.push(c);
            }
        }
        false
    }
}

fn entry_lambda_to_kv(mapped: &str) -> (String, String) {
    if let Some(start) = mapped.find("SimpleImmutableEntry<>(") {
        let inner_start = start + "SimpleImmutableEntry<>(".len();
        if let Some(end) = mapped.rfind(')') {
            let inner = &mapped[inner_start..end];
            if let Some(comma) = split_top_comma(inner) {
                // The `to` lambda's body referenced the element as `it` —
                // toMap's key/value fns also see the element, so keep the
                // original lambda param name.
                return (
                    format!("__e -> {}", inner[..comma].trim().replace("it", "__e")),
                    format!("__e -> {}", inner[comma + 1..].trim().replace("it", "__e")),
                );
            }
        }
    }
    (mapped.trim().to_string(), "__e -> __e".to_string())
}

/// Find the first top-level comma (not inside parens/angles).
fn split_top_comma(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' | '<' => depth += 1,
            ')' | '>' => depth -= 1,
            ',' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Replace bare `it` identifier in a transpiled lambda body with the
/// element lambda name (used when hoisting a lambda body into filter()).
fn it_subst(pred: &str) -> String {
    pred.replace("it", "__e")
}
