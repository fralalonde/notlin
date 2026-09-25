//! Member access and member-call rewrite: property reads -> accessor
//! calls, stdlib-member mapping table, subscript -> get().

use super::Expr;
use crate::transpiler::kt;

impl<'a, 'u> Expr<'a, 'u> {
    pub(crate) fn navigation(&mut self, node: tree_sitter::Node) -> String {
        // Map/Iterable collection ops (`x.filterValues { .. }`,
        // `x.mapKeys { .. }`, `x.associateBy { .. }`) — Java has no such
        // members: lower through stream/entrySet before generic handling.
        let navtext = self.unit.text(node);
        let navtrim = navtext.trim().trim_start_matches('(').to_string();
        let op_member = navtrim.rsplit_once('.').map(|(_, m)| m.trim().to_string());
        if matches!(
            op_member.as_deref(),
            Some("filterValues")
                | Some("mapKeys")
                | Some("associateBy")
                | Some("filterNotNull")
                | Some("filterIsInstance")
        ) {
            // filterNotNull/filterIsInstance have argless or type-arg call
            // shapes; handle them before map_entry_op. Their lambda (when
            // present) passes into the entry-op helper only for the listed
            // Map ops; the iterable ones lower inline here.
            let member = op_member.unwrap_or_default();
            if let Some(nav_base) = node.children(&mut node.walk()).find(|c| c.is_named()) {
                let base_java = self.transpile(nav_base);
                if member == "filterNotNull" {
                    self.unit.diags.warn_approx(
                        node,
                        self.unit.file,
                        "Iterable.filterNotNull lowered to stream().filter(Objects::nonNull).collect(toList())",
                    );
                    self.unit.pending_full_call = true;
                    return format!(
                        "{base_java}.stream().filter(Objects::nonNull).collect(java.util.stream.Collectors.toList())"
                    );
                }
                if member == "filterIsInstance" {
                    // filterIsInstance<T>() — the type argument arrives on
                    // the enclosing call's type_arguments; conservatively
                    // lower without the cast when it is not recoverable.
                    let ty = node
                        .parent()
                        .and_then(|p| kt::child(p, "type_arguments"))
                        .map(|t| self.unit.text(t).to_string())
                        .map(|t| {
                            t.trim_start_matches('<')
                                .trim_end_matches('>')
                                .trim()
                                .to_string()
                        });
                    if let Some(ty) = ty {
                        self.unit.diags.warn_approx(
                            node,
                            self.unit.file,
                            "Iterable.filterIsInstance<T> lowered to stream().filter(x -> x instanceof T).map(x -> (T) x).collect(toList())",
                        );
                        self.unit.pending_full_call = true;
                        return format!(
                            "{base_java}.stream().filter(x -> x instanceof {ty}).map(x -> ({ty}) x).collect(java.util.stream.Collectors.toList())"
                        );
                    }
                }
                return self.map_entry_op(node, &base_java, &member);
            }
        }
        let mut cursor = node.walk();
        let kids: Vec<_> = node.children(&mut cursor).collect();
        // base . member (possibly ?. or ::)
        let base = kids.iter().find(|c| c.is_named()).copied();
        // A supertype member access through `super` (`super.<member>`,
        // optionally qualified `SuperT.super.<member>`) must resolve against
        // the SUPERTYPE'S Java API. The qualifying supertype comes from the
        // AST (`SuperT.super`) or, for a plain `super.<member>`, from the
        // enclosing declaration's supertype list via the workspace index.
        // A TRANSLATED supertype exposes a Lombok/inline default getter, so
        // the interface-legal qualified form `SupName.super.getMember()` is
        // required (plain `super.getMember()` does not compile in an
        // interface default method). A RETAINED-Kotlin supertype has no
        // JVM-visible Java method at all — the caller is tainted instead of
        // emitting broken Java.
        let mut super_owner = self.unit.pending_super_owner.take();
        if let Some(b) = base
            && b.kind() == "super_expression"
            && super_owner.is_none()
        {
            // Plain `super.<member>`: the qualifying supertype is the first
            // supertype of the enclosing declaration, resolved via the
            // workspace index (the index stores each declaration's own
            // supertype list). `current_decl` IS the enclosing declaration
            // for top-level types; nested declarations walk up to their
            // owning class.
            super_owner = self.unit.current_decl.and_then(|d| {
                let class_name = {
                    let mut node = Some(d);
                    let mut found = None;
                    while let Some(n) = node {
                        if n.kind() == "class_declaration" {
                            found = kt::field(n, "name").map(|nm| self.unit.text(nm).to_string());
                            break;
                        }
                        node = kt::parent_of(n);
                    }
                    found
                };
                class_name.and_then(|cn| {
                    self.unit.workspace.and_then(|ws| {
                        ws.declarations().find(|dc| dc.name == cn).and_then(|dc| {
                            dc.supertypes
                                .first()
                                .map(|s| s.split('<').next().unwrap_or(s).trim().to_string())
                        })
                    })
                })
            });
        }
        if let Some(b) = base
            && b.kind() == "super_expression"
            && let Some(owner) = super_owner.clone()
        {
            // A supertype in the SAME FILE translates together with the
            // caller (one unit), so its Java default getter exists even
            // though the index still records its source as Kotlin. An
            // index-qualifying Java declaration or a genuinely translated
            // Kotlin declaration both count; a supertype that stays Kotlin
            // (retained elsewhere / unselected file) does not.
            let ws = self.unit.workspace;
            let same_file = ws.and_then(|w| {
                let declaring = self
                    .unit
                    .workspace_file
                    .as_deref()
                    .unwrap_or(self.unit.file);
                w.source_file(declaring)
            });
            let same_file_hit = same_file.is_some_and(|f| {
                f.declarations.iter().any(|d| {
                    d.name == owner && d.language == crate::workspace::SourceLanguage::Kotlin
                })
            });
            let translated = same_file_hit
                || ws.is_some_and(|w| {
                    w.declarations().any(|d| {
                        d.name == owner && d.language == crate::workspace::SourceLanguage::Java
                    })
                });
            if !translated {
                self.unit.diag_untranslatable(
                    node,
                    format!(
                        "`super.<member>` targets `{owner}`, which remains Kotlin; its JVM accessor is not Java-visible and the caller must stay Kotlin"
                    ),
                );
                return "null".to_string();
            }
        }
        let mut result = base.map(|b| self.transpile(b)).unwrap_or_default();
        if let Some(owner) = super_owner
            && !result.starts_with(&owner)
        {
            result = format!("{owner}.{result}");
        }
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
                            | "stream"
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
                        let base_is_enum = |b: tree_sitter::Node| -> bool {
                            let raw = self.unit.text(b).trim().to_string();
                            let first = raw.split('.').next().unwrap_or("").to_string();
                            if self.unit.enum_types.contains(first.as_str()) {
                                return true;
                            }
                            // `this.getType().name` where getType() or
                            // `type` resolves to an indexed ENUM
                            // declaration (Kotlin or Java): Enum's
                            // accessor is `name()`, not getName().
                            if let Some(ws) = self.unit.workspace {
                                let declaring = self
                                    .unit
                                    .workspace_file
                                    .as_deref()
                                    .unwrap_or(self.unit.file);
                                let recv = raw
                                    .rsplit('.')
                                    .next()
                                    .unwrap_or("")
                                    .trim_end_matches("()")
                                    .trim_start_matches("this.")
                                    .to_string();
                                let ty = ws
                                    .property_type_in_file(declaring, &recv)
                                    .or_else(|| ws.method_return_type_in_file(declaring, &recv))
                                    .or_else(|| {
                                        // The receiver may be `this.<prop>`
                                        // where <prop> is declared INSIDE the
                                        // class this file emits — resolve
                                        // through that class's own members
                                        // (declaration lookup by name,
                                        // independent of file path equalities).
                                        {
                                            // Declaration lookup by NAME:
                                            // pick the class named in the
                                            // raw receiver prefix (e.g.
                                            // `PropertyTagType`), regardless
                                            // of workspace-file path mismatch.
                                            let owner = raw
                                                .trim()
                                                .trim_start_matches("this.")
                                                .rsplit('.')
                                                .nth(1)
                                                .unwrap_or("")
                                                .trim_end_matches("()")
                                                .to_string();
                                            if owner.is_empty() {
                                                // Bare.property receiver: collect
                                                // EVERY indexed member with this
                                                // name and prefer one whose type
                                                // resolves to an indexed ENUM —
                                                // the same member name may exist
                                                // on many types (cross-file
                                                // shadowing), so first-match is
                                                // unreliable.
                                                let mut hit: Option<Option<String>> =
                                                    None;
                                                for cl in ws.declarations() {
                                                    for mm in &cl.members {
                                                        if mm.name != recv {
                                                            continue;
                                                        }
                                                        let bare = mm
                                                            .type_name
                                                            .as_deref()
                                                            .unwrap_or("")
                                                            .split('<')
                                                            .next()
                                                            .unwrap_or("")
                                                            .trim()
                                                            .to_string();
                                                        let enum_typed = bare
                                                            .get(0..1)
                                                            .is_some_and(|c| {
                                                                c.chars()
                                                                    .next()
                                                                    .is_some_and(|c| {
                                                                        c.is_ascii_uppercase()
                                                                    })
                                                            })
                                                            && ws.declarations().any(|d| {
                                                                d.name == bare
                                                                    && d.kind
                                                                        == crate::workspace::DeclarationKind::Enum
                                                            });
                                                        if enum_typed {
                                                            hit = Some(mm.type_name.clone());
                                                            break;
                                                        }
                                                    }
                                                    if hit.is_some() {
                                                        break;
                                                    }
                                                }
                                                hit.unwrap_or(None)
                                            } else if owner.get(0..1).is_some_and(|c| {
                                                c.chars()
                                                    .next()
                                                    .is_some_and(|c| c.is_ascii_uppercase())
                                            }) {
                                                ws.declarations().find_map(|cl| {
                                                    if cl.name != owner {
                                                        return None;
                                                    }
                                                    cl.members.iter().find_map(|mm| {
                                                        if mm.name == recv {
                                                            mm.type_name.clone()
                                                        } else {
                                                            None
                                                        }
                                                    })
                                                })
                                            } else {
                                                None
                                            }
                                        }
                                    })
                                    .map(|t| t.split('<').next().unwrap_or(&t).trim().to_string());
                                if let Some(t) = ty
                                    && ws.declarations().any(|d| {
                                        d.name == t
                                            && d.kind == crate::workspace::DeclarationKind::Enum
                                    })
                                {
                                    return true;
                                }
                            }
                            false
                        };
                        if member_name == "name" && base.map(base_is_enum).unwrap_or(false) {
                            // JDK 25: Enum#name is a private field; the
                            // public accessor is the method `name()`.
                            result.push_str(".name()");
                            continue;
                        }
                        // Data-class record receivers: `q.id` -> `q.id()`
                        // (Java record accessor style, not getter). Nullable
                        // receivers carry the annotation prefix in var_types
                        // (`@Nullable Currency`) — strip it so the record
                        // accessor still fires under `?.`.
                        if base
                            .map(|b| {
                                let t0 = self.unit.text(b).trim().to_string();
                                let ct = self.unit.var_types.get(&t0).cloned();
                                ct.map(|c| {
                                    let bare = c.strip_prefix("@Nullable ").unwrap_or(&c).trim();
                                    self.unit.data_components.contains_key(bare)
                                        && !self.unit.lombok
                                })
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
                        // (N002 recorded at the stream-op site). The guard is
                        // `it` specifically: any OTHER named receiver with a
                        // known type must take the platform accessor below
                        // (`p.name` on a plain class -> `p.getName()`), never
                        // the record-style `name()`.
                        if member_name == "name"
                            && base
                                .map(|b| self.unit.text(b).trim() == "it")
                                .unwrap_or(false)
                            && self
                                .unit
                                .workspace
                                .is_none_or(|ws| ws.find_property_owner("name").is_none())
                        {
                            result.push_str(".name()");
                            continue;
                        }
                        // enum-typed receiver `x.name`: JDK accessor
                        // `name()`, never `getName()` — resolve the base
                        // type from var_types and the workspace index.
                        if member_name == "name" {
                            let base_raw = base
                                .map(|b| self.unit.text(b).trim().to_string())
                                .unwrap_or_default();
                            let base_ty = self
                                .unit
                                .var_types
                                .get(&base_raw)
                                .or(self
                                    .unit
                                    .var_types
                                    .get(base_raw.trim_start_matches("this.")))
                                .cloned();
                            let enum_hit = base_ty
                                .map(|t| {
                                    t.split('<')
                                        .next()
                                        .unwrap_or(&t)
                                        .trim()
                                        .trim_start_matches("@Nullable ")
                                        .trim_end_matches("()")
                                        .to_string()
                                })
                                .and_then(|t0| {
                                    self.unit.workspace.and_then(|ws| {
                                        ws.declarations()
                                            .any(|d| {
                                                d.name == t0
                                                    && d.kind
                                                        == crate::workspace::DeclarationKind::Enum
                                            })
                                            .then_some(t0)
                                    })
                                });
                            if enum_hit.is_some() {
                                result.push_str(".name()");
                                continue;
                            }
                        }
                        let cap: String = member_name
                            .chars()
                            .next()
                            .map(|c| c.to_uppercase().collect::<String>())
                            .unwrap_or_default()
                            + member_name.chars().skip(1).collect::<String>().as_str();
                        // `kClass.java` (KClass -> Class interop) reads as
                        // `getClass()` in Java, never a `getJava()` property.
                        // `receiver.javaClass` is the same interop getter
                        // spelled as a property: `getClass()` is the only
                        // valid Java form.
                        if member_name == "java" {
                            // Kotlin `KClass<T>.java` is already a Java
                            // `Class<T>` after parameter lowering; erase the
                            // interop-only bridge rather than calling
                            // `Class.getClass()`.
                            continue;
                        }
                        if member_name == "javaClass" {
                            if !result.ends_with(".class") {
                                result.push_str(".getClass()");
                            }
                            continue;
                        }

                        result.push_str(&format!(".get{}()", cap));
                    }
                }
            } else if w[1].kind() == "::" {
                // Class/object references and method refs; pass through.
                // `X::class` is the Java class literal `X.class`.
                if self.unit.text(w[2]).trim() == "class" {
                    result.push_str(".class");
                    continue;
                } else if self.unit.text(w[2]).trim() == "java" {
                    // `X.java` where X is a KClass expression: JVM interop
                    // getter is `getClass()`, not `getJava()`.
                    result.push_str(".getClass()");
                    continue;
                }
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
        // Map ops (`m.filterValues { … }`, `m.mapKeys { … }`): Java Map has
        // no such members — lower through the entrySet stream, regardless of
        // receiver complexity.
        let tail_member = raw_trimmed
            .rsplit_once('.')
            .map(|(_, m)| m.trim().to_string());
        if matches!(
            tail_member.as_deref(),
            Some("filterValues") | Some("mapKeys")
        ) {
            let member = tail_member.unwrap_or_default();
            let base = self.transpile(
                node.children(&mut node.walk())
                    .find(|c| c.is_named())
                    .expect("navigation base"),
            );
            return self.map_entry_op(node, &base, &member);
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
                    | "parenthesized_expression"
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
                    // `m.filterValues { v -> pred }` — Java Map has no
                    // filterValues member: lower to entrySet stream + toMap.
                    if member == "filterValues" || member == "mapKeys" {
                        return self.map_entry_op(node, &base_java, &member);
                    }
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
                    // Trailing lambda on the enclosing call: e.g.
                    // `map.values.firstOrNull { … }` mapped here would shadow
                    // call.rs's firstOrNull { pred } rewrite — keep bare.
                    let outer_lambda = node
                        .parent()
                        .filter(|p| p.kind() == "call_expression")
                        .map(|p| {
                            kt::child(p, "lambda_literal").is_some()
                                || kt::child(p, "annotated_lambda").is_some()
                        })
                        .unwrap_or(false);
                    let base_stream_ready = base_java.ends_with(".stream()");
                    // `collection.stream().filter { ... }` is already the
                    // Java Stream API; preserve the direct member rather than
                    // treating Kotlin's Iterable.filter as the receiver.
                    if member == "filter" && base_stream_ready {
                        return format!("{}.filter", base_java);
                    }
                    let jm_first = |jm: &str| -> String {
                        // A mapped form that starts by opening a stream must
                        // not double-stream a receiver that already is one
                        // (`x.stream().first()` chain).
                        if base_stream_ready && let Some(rest) = jm.strip_prefix("stream().") {
                            rest.to_string()
                        } else if base_stream_ready && jm == "stream()" {
                            String::new()
                        } else {
                            jm.to_string()
                        }
                    };
                    return match (kotlin_member_to_java(&member), outer_lambda) {
                        (Some(jm), false) if jm != member => {
                            format!("{}.{}", base_java, jm_first(&jm))
                        }
                        (Some(_), true) => format!("{}.{}", base_java, member),
                        (Some(_), _) => format!("{}.{}", base_java, member),
                        (None, _) if member == "copy" => {
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
                        (None, _) => {
                            // Collection-algebra member calls (`m.plus(x)`,
                            // `m.minus(k)`) have NO Java member form: the
                            // receiver's indexed property type proves a
                            // collection — taint the caller instead of
                            // emitting an unresolvable member.
                            if matches!(member.as_str(), "plus" | "minus" | "times")
                                && let Some(ws) = self.unit.workspace
                                && let Some((_, head)) =
                                    raw_trimmed.rsplit_once(&format!(".{member}"))
                                && let Some(ty) = {
                                    let last = head
                                        .trim()
                                        .rsplit('.')
                                        .next()
                                        .unwrap_or("")
                                        .trim_end_matches("()");
                                    let declaring = self
                                        .unit
                                        .workspace_file
                                        .as_deref()
                                        .unwrap_or(self.unit.file);
                                    ws.property_type_in_file(declaring, last)
                                        .or_else(|| ws.property_type_of_getter(last))
                                }
                                && (ty.contains("Map<")
                                    || ty.contains("List<")
                                    || ty.contains("Set<")
                                    || ty.contains("Collection<")
                                    || ty.contains("Iterable<"))
                            {
                                self.unit.diag_untranslatable(
                                    node,
                                    format!(
                                        "collection `{member}` member call on `{ty}` receiver: stdlib collection algebra has no Java member form; declaration retained in Kotlin"
                                    ),
                                );
                                return raw_trimmed.to_string();
                            }
                            if member == "stream" {
                                let base = raw_trimmed
                                    .rsplit_once('.')
                                    .map(|(b, _)| b)
                                    .unwrap_or(raw_trimmed.as_str());
                                return format!("{}.stream()", base.trim());
                            }
                            // Kotlin property access on an unknown receiver
                            // (`it.name`): if the member is a known
                            // workspace PROPERTY, read it through its Java
                            // getter (`get<Name>()`); emitting
                            // `name()` breaks on every translated
                            // entity whose accessor is `getName()`.
                            if member.as_str() != "name"
                                && !matches!(
                                    member.as_str(),
                                    "map"
                                        | "filter"
                                        | "forEach"
                                        | "flatMap"
                                        | "sorted"
                                        | "distinct"
                                        | "mapNotNull"
                                        | "any"
                                        | "all"
                                        | "none"
                                        | "count"
                                        | "first"
                                        | "last"
                                        | "plus"
                                        | "minus"
                                        | "times"
                                        | "stream"
                                        | "toList"
                                        | "toMap"
                                        | "size"
                                        | "keys"
                                        | "values"
                                        | "entries"
                                        | "joinToString"
                                )
                                && self.unit.workspace.is_some_and(|ws| {
                                    ws.find_property_owner(member.as_str()).is_some()
                                })
                            {
                                let mut chars = member.chars();
                                let getter = format!(
                                    "get{}{}",
                                    chars
                                        .next()
                                        .map(|c| c.to_ascii_uppercase().to_string())
                                        .unwrap_or_default(),
                                    chars.as_str()
                                );
                                return format!("{}.{}()", base_java, getter);
                            }
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
            let outer_lambda = node
                .parent()
                .filter(|p| p.kind() == "call_expression")
                .map(|p| {
                    kt::child(p, "lambda_literal").is_some()
                        || kt::child(p, "annotated_lambda").is_some()
                })
                .unwrap_or(false);
            if let Some(java_member) = kotlin_member_to_java(member) {
                if java_member != member && !outer_lambda {
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
                if outer_lambda {
                    // A trailing lambda is attached to this call — leave the
                    // member name bare so call.rs's lambda-gated arms
                    // (firstOrNull { pred }, etc.) assemble the call.
                    return raw_trimmed.to_string();
                }
                return raw_trimmed.to_string();
            }
            // Unknown member on a receiver: if it resolves to a companion
            // method of a RETAINED Kotlin declaration with type arguments,
            // the call has no Java ABI (reified inline fns inline only at
            // Kotlin call sites) — taint the caller instead of emitting
            // dead Java.
            if self.unit.workspace.is_some()
                && node
                    .parent()
                    .is_some_and(|c| kt::child(c, "type_arguments").is_some())
                && let Some(ws) = self.unit.workspace
                && let Some(owner) = ws.find_static_member_owner(member)
                && owner.language == crate::workspace::SourceLanguage::Kotlin
            {
                self.unit.diag_untranslatable(
                    node,
                    format!(
                        "call `{}.{}` targets a reified/inline companion function of retained Kotlin declaration `{}`; no Java-callable ABI exists",
                        raw_trimmed.rsplit_once(&format!(".{member}")).map(|(b, _)| b.trim().to_string()).unwrap_or_default(), member, owner.name
                    ),
                );
            } else if node.parent().is_some_and(|p| p.kind() == "call_expression")
                && let Some(dot_pos) = raw_trimmed.trim_end_matches('(').rfind('.')
                && let Some(owner_name) = raw_trimmed[..dot_pos].rsplit('.').next().map(str::trim)
                && let Some(ws) = self.unit.workspace
                && let Some(owner) = ws.find_static_member(owner_name, member)
                && owner.language == crate::workspace::SourceLanguage::Kotlin
            {
                // A KClass-formal companion fn takes a KClass at the Java
                // ABI: a `K::class` literal lowered to `K.class` is not a
                // valid Java form for it — taint the caller.
                if node
                    .parent()
                    .and_then(|p| kt::child(p, "value_arguments"))
                    .is_some_and(|va: tree_sitter::Node| {
                        let mut c = va.walk();
                        va.children(&mut c)
                            .filter(|arg| arg.kind() == "value_argument")
                            .any(|arg| {
                                arg.children(&mut arg.walk())
                                    .filter(|x| x.is_named())
                                    .any(|x| self.unit.text(x).contains("::class"))
                            })
                    })
                {
                    self.unit.diag_untranslatable(
                        node,
                        format!(
                            "call `{owner_fallback}.{member}` passes a `KClass` literal into a KClass-formal companion of retained Kotlin declaration `{owner_name}`; no Java-callable literal exists",
                            owner_fallback = raw_trimmed.rsplit_once(&format!(".{member}")).map(|(b, _)| b.trim().to_string()).unwrap_or_default()
                        ),
                    );
                    return raw_trimmed.to_string();
                }
                // Companion fn of a retained Kotlin declaration: Java reaches
                // it through the generated `Companion` holder — plain
                // companion members are not static bridges without
                // @JvmStatic.
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    format!(
                        "companion call on retained Kotlin decl `{}` routed via `Companion.{}`",
                        owner.name, member
                    ),
                );
                // Insert `.Companion` before the member name; call.rs still
                // appends `(args)`.
                if let Some(dot) = raw_trimmed.trim_end_matches('(').rfind('.') {
                    let base = raw_trimmed[..dot].to_string();
                    return format!("{base}.Companion.{member}");
                }
                return format!("Companion.{member}");
            } else {
                // Collection-algebra member calls (`m.plus(x)`,
                // `m.minus(k)`, `s.times(…)`) have NO Java member form: the
                // receiver's indexed property type proves a Map — taint the
                // caller rather than emitting a member javac cannot resolve.
                if matches!(member, "plus" | "minus" | "times")
                    && let Some(ws) = self.unit.workspace
                    && let Some(callee_head) = raw_trimmed.rsplit_once(&format!(".{member}"))
                    && let Some(ty) = {
                        let last = callee_head
                            .1
                            .trim()
                            .rsplit('.')
                            .next()
                            .unwrap_or("")
                            .trim_end_matches("()");
                        let declaring = self
                            .unit
                            .workspace_file
                            .as_deref()
                            .unwrap_or(self.unit.file);
                        ws.property_type_in_file(declaring, last)
                            .or_else(|| ws.property_type_of_getter(last))
                    }
                {
                    // Only collection receivers are unsound: String.plus is
                    // Java `+` concat; user overloads keep their methods.
                    if ty.contains("Map<")
                        || ty.contains("List<")
                        || ty.contains("Set<")
                        || ty.contains("Collection<")
                        || ty.contains("Iterable<")
                    {
                        self.unit.diag_untranslatable(
                            node,
                            format!(
                                "collection `{member}` member call on `{ty}` receiver: stdlib collection algebra has no Java member form; declaration retained in Kotlin"
                            ),
                        );
                        return raw_trimmed.to_string();
                    }
                }
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    format!(
                        "stdlib member `.{}` not mapped; emitted verbatim (verify Java equivalent exists)",
                        member
                    ),
                );
            }
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
        "stream" => Some("stream()"),
        // no-arg collection ops with Java Collection/Stream equivalents.
        // Lambda params `__left`/`__right` can never collide with Kotlin
        // identifiers (Kotlin forbids leading underscores), so `(a, b) -> b`
        // can't shadow user locals named a/b.
        // NOTE: firstOrNull NOT mapped here — with a lambda pred it must
        // route through call.rs's filter(...) form; bare calls hit the
        // fallback passthrough (find symbol error is the least-broken).
        "last" => Some("stream().reduce((__left, __right) -> __right).orElse(null)"),
        // Map-only ops: Java has no such members; lower through entrySet
        // streams. The emit-site wraps these in `entrySet().stream().` +
        // `.collect(Collectors.toMap(...))` when the receiver is a Map.
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
        "append",
    ];
    if SAFE.contains(&member) {
        return Some(member.to_string());
    }
    None
}

/// Map.filterValues lambda with value param — the emitted Java predicate
/// compares `e.getValue()`; the translation engine already renders the body
/// with the value param bound, so the placeholder rename happens in the
/// param arm: `{ v -> body }` becomes `v` kept and used via cast below.
fn _unit_replace_value_placeholder(_body: String) -> String {
    String::new()
}

/// Replace whole-word occurrences of `word` in `text`.
fn replace_whole_word(text: &str, word: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(word) {
        let before_ok = rest[..idx]
            .chars()
            .next_back()
            .map(|c| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(true);
        let after_idx = idx + word.len();
        let after_ok = rest[after_idx..]
            .chars()
            .next()
            .map(|c| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(true);
        if before_ok && after_ok {
            out.push_str(&rest[..idx]);
            out.push_str(replacement);
        } else {
            out.push_str(&rest[..after_idx]);
        }
        rest = &rest[after_idx..];
    }
    out.push_str(rest);
    out
}

impl<'a, 'u> Expr<'a, 'u> {
    /// `map.filterValues { v -> pred }` / `map.mapKeys { k -> f }` — Java Map
    /// has no such members; lower through the entrySet stream.
    pub(crate) fn map_entry_op(
        &mut self,
        node: tree_sitter::Node<'_>,
        base_java: &str,
        member: &str,
    ) -> String {
        let lambda = node.parent().and_then(|p| {
            // trailing lambda lives directly under the call or as the only
            // value_argument
            kt::child(p, "annotated_lambda")
                .and_then(|al| kt::child(al, "lambda_literal"))
                .or_else(|| kt::child(p, "lambda_literal"))
                .or_else(|| {
                    kt::child(p, "value_arguments").and_then(|va| {
                        let mut c = va.walk();
                        va.children(&mut c)
                            .filter(|arg| arg.kind() == "value_argument")
                            .find_map(|arg| {
                                arg.children(&mut arg.walk())
                                    .find(|n| n.kind() == "lambda_literal")
                            })
                    })
                })
        });
        // Without a lambda this call cannot be lowered soundly.
        let Some(l) = lambda else {
            self.unit.diags.warn_approx(
                node,
                self.unit.file,
                format!("Map.{member} requires its filter/map lambda; kept as-is"),
            );
            return format!("{base_java}.{member}");
        };
        let raw = self.transpile(l);
        let trimmed = raw
            .trim()
            .trim_start_matches('{')
            .trim_end_matches('}')
            .trim();
        let (raw_params, body) = trimmed.split_once("->").unwrap_or(("", trimmed));
        let param = raw_params
            .trim()
            .trim_start_matches('(')
            .trim_end_matches(')')
            .trim()
            .to_string();
        let body = body.trim().to_string();
        self.unit.pending_full_call = true;
        match member {
            "filterValues" => {
                let bound = if param.is_empty() {
                    body
                } else {
                    replace_whole_word(&body, &param, "e.getValue()")
                };
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "Map.filterValues lowered to entrySet().stream().filter(...).collect(toMap(getKey, getValue))",
                );
                format!(
                    "{base_java}.entrySet().stream().filter(e -> {bound}).collect(java.util.stream.Collectors.toMap(Map.Entry::getKey, Map.Entry::getValue))"
                )
            }
            "associateBy" => {
                // Iterable.associateBy { k } -> stream().collect(toMap(kfn,
                // v -> v)). The element param (named or `it`) binds to `v`.
                let bound = if param.is_empty() {
                    replace_whole_word(&body, "it", "v")
                } else {
                    replace_whole_word(&body, &param, "v")
                };
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "Iterable.associateBy lowered to stream().collect(toMap(keyfn, v -> v))",
                );
                format!(
                    "{base_java}.stream().collect(java.util.stream.Collectors.toMap(v -> {bound}, v -> v))"
                )
            }
            _ => {
                // mapKeys: pred is a key-to-key remap — body's param slot is
                // the KEY: bind to e.getKey(). The RESULT map keeps the
                // original values.
                let bound = if param.is_empty() {
                    body
                } else {
                    replace_whole_word(&body, &param, "e.getKey()")
                };
                self.unit.diags.warn_approx(
                    node,
                    self.unit.file,
                    "Map.mapKeys lowered to entrySet().stream().collect(toMap(keyfn, Map.Entry::getValue))",
                );
                format!(
                    "{base_java}.entrySet().stream().collect(java.util.stream.Collectors.toMap(e -> {bound}, Map.Entry::getValue))"
                )
            }
        }
    }
}
