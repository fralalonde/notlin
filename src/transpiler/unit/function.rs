//! Function emission: params, receivers, type parameters, bodies.

use super::Unit;
use crate::transpiler::expr::Expr;
use crate::transpiler::java::JavaOut;
use crate::transpiler::kt;

impl<'src, 'tree> Unit<'src, 'tree> {
    /// Name of the nearest enclosing class/interface declaration, if this
    /// function is a class-body member.
    fn enclosing_class_name(&self, decl: tree_sitter::Node) -> Option<String> {
        let mut node = kt::parent_of(decl);
        while let Some(n) = node {
            if n.kind() == "class_declaration" {
                return kt::field(n, "name").map(|nm| self.text(nm).to_string());
            }
            node = kt::parent_of(n);
        }
        None
    }

    fn function_parameter_types(&self, decl: tree_sitter::Node) -> Vec<String> {
        let Some(parameters) = kt::child(decl, "function_value_parameters") else {
            return Vec::new();
        };
        parameters
            .children(&mut parameters.walk())
            .filter(|parameter| parameter.kind() == "parameter")
            .filter_map(|parameter| {
                parameter
                    .children(&mut parameter.walk())
                    .find(|child| {
                        matches!(
                            child.kind(),
                            "user_type" | "nullable_type" | "function_type" | "parenthesized_type"
                        )
                    })
                    .map(|ty| self.text(ty).trim().to_string())
            })
            .collect()
    }

    /// Detects an `override fun f` whose return differs from a Kotlin
    /// supertype. Java-compatible covariance is allowed only when that
    /// supertype is selected and will translate in the same fixpoint; retaining
    /// the Kotlin interface beside a Java implementation can crash kotlinc's
    /// fake-override lowering.
    fn covariant_iface_return_conflict(&self, decl: tree_sitter::Node) -> Option<String> {
        let workspace = self.workspace?;
        let class_name = self.enclosing_class_name(decl)?;
        let is_override = kt::child(decl, "modifiers")
            .map(|m| self.text(m).contains("override"))
            .unwrap_or(false);
        if !is_override {
            return None;
        }
        let fname = kt::field(decl, "name")
            .map(|n| self.text(n).to_string())
            .unwrap_or_default();
        let declaring = self.workspace_file.as_deref().unwrap_or(self.file);
        let owner = workspace
            .source_file(declaring)?
            .declarations
            .iter()
            .find(|candidate| candidate.name == class_name)?;
        let parameter_types = self.function_parameter_types(decl);
        let conflicts = workspace.retained_supertype_member_conflicts_in_file(
            declaring,
            &owner.supertypes,
            &class_name,
        );
        for conflict in conflicts.into_iter().filter(|conflict| {
            conflict.member_name == fname
                && conflict.kind == crate::workspace::MemberKind::Method
                && conflict.parameter_types == parameter_types
                && conflict.inherited_type != conflict.implementation_type
        }) {
            match conflict.classification {
                crate::workspace::MemberConflictClass::SupertypeTypeParameter => continue,
                crate::workspace::MemberConflictClass::JavaCovariantReturn => {}
                _ => return Some(conflict.inherited_type),
            }
            let Some(supertype) =
                workspace.resolve_type(workspace.source_file(declaring)?, &conflict.supertype)
            else {
                return Some(conflict.inherited_type);
            };
            if supertype.language == crate::workspace::SourceLanguage::Java {
                continue;
            }
            let Some(retained) = self.retained_hint else {
                return Some(conflict.inherited_type);
            };
            if workspace.declaration_retained(supertype, retained) {
                return Some(conflict.inherited_type);
            }
            let Some(supertype_file) = workspace.declaration_source_file(supertype) else {
                return Some(conflict.inherited_type);
            };
            if !workspace.is_selected(&supertype_file.path, self.translation_roots) {
                return Some(conflict.inherited_type);
            }
        }
        None
    }

    pub(crate) fn transpile_function(
        &mut self,
        decl: tree_sitter::Node,
        in_class: bool,
        out: &mut JavaOut,
    ) {
        self.transpile_function_opts(decl, in_class, false, false, out)
    }

    pub(crate) fn transpile_function_opts(
        &mut self,
        decl: tree_sitter::Node,
        _in_class: bool,
        make_static: bool,
        is_main: bool,
        out: &mut JavaOut,
    ) {
        // Covariant-override guard: a Kotlin data class often narrows an
        // interface function's return type (`override fun with(...): Impl`).
        // The translated Java class then overrides the Kotlin interface
        // member with a different JVM descriptor family, and kotlinc's
        // fake-override synthesis can crash resolving it. If a known
        // supertype declares this member with a different type, keep the
        // declaration in Kotlin.
        self.current_function_name = None;
        self.current_function_returns_value = false;
        self.current_function_return_type = None;
        if self.is_property_repair_bridge(decl) {
            return;
        }
        if let Some(_iface_type) = self.covariant_iface_return_conflict(decl) {
            self.diag_untranslatable(
                decl,
                "override narrows a supertype function return type; retained in Kotlin",
            );
            return;
        }
        // Each declaration is its own translation scope: params and locals
        // must not leak from a previously emitted function (var_types
        // persists on Unit across top-level and member declarations).
        // Exceptions re-seeded below: enum ctor params (fields visible to
        // every enum body method) and the current extension receiver.
        self.var_types.clear();
        self.flow_smart_casts.clear();
        for (fname, fty) in std::mem::take(&mut self.pending_field_types) {
            self.var_types.insert(fname, fty);
        }
        let name = kt::field(decl, "name")
            .map(|n| self.text(n).to_string())
            .unwrap_or_else(|| "anon".to_string());
        self.current_function_name = Some(name.clone());
        let visibility = self.visibility_of(decl);
        let function_annotations: Vec<String> = kt::child(decl, "modifiers")
            .map(|modifiers| {
                modifiers
                    .children(&mut modifiers.walk())
                    .filter(|modifier| {
                        matches!(modifier.kind(), "annotation" | "annotated_expression")
                    })
                    .filter_map(|annotation| self.transpile_declaration_annotation(annotation))
                    .filter(|annotation| !is_kotlin_jvm_annotation(annotation))
                    .collect()
            })
            .unwrap_or_default();
        // Kotlin override methods can narrow a nullable supertype return (for
        // example, `String?` to `String`). Java's unannotated override loses
        // that contract to Kotlin callers, which merge the inherited nullable
        // declaration back into the lookup. Preserve the source's explicit,
        // non-null reference return as JVM nullability metadata.
        let has_override_modifier = kt::child(decl, "modifiers").is_some_and(|modifiers| {
            self.text(modifiers)
                .split_whitespace()
                .any(|part| part == "override")
        });

        // function_modifier children (suspend/operator/infix/tailrec/
        // external/inline...): suspend changes semantics, external can't be
        // a java method body; both must be flagged, the rest warn.
        let mut is_external = false;
        if let Some(mods) = kt::child(decl, "modifiers") {
            let mut mcur = mods.walk();
            for m in mods.children(&mut mcur) {
                if m.kind() != "function_modifier" {
                    continue;
                }
                let mut icur = m.walk();
                for f in m.children(&mut icur) {
                    let word = self.text(f).trim().to_string();
                    match word.as_str() {
                        "suspend" => {
                            self.diag_untranslatable(
                                f,
                                "Kotlin suspend function requires coroutine lowering; retained in Kotlin",
                            );
                            return;
                        }
                        "external" => {
                            // JNI-shaped; bodyless native method is closest
                            self.diag_approx(f, "Kotlin `external` emitted as JNI `native` method");
                            is_external = true;
                        }
                        // `operator` changes Kotlin call syntax only. Its JVM
                        // member name and signature are the declared method,
                        // which Java emits directly without approximation.
                        "operator" => {}
                        "infix" | "tailrec" => {
                            self.diag_approx(
                                f,
                                format!("Kotlin function modifier `{}` has no Java counterpart; emitted as a plain method", word),
                            );
                        }
                        "inline" => {
                            // Java can't inline functions; harmless no-op
                            self.diag_approx(f, "Kotlin `inline` dropped (JIT inlines anyway)");
                        }
                        _ => {
                            self.diag_untranslatable(
                                f,
                                format!("function modifier not supported: {}", word),
                            );
                        }
                    }
                }
            }
        }

        // Type parameters `fun <T> f(...)` -> Java generics `<T extends Bound>`;
        // `reified` has no Java counterpart and taints the declaration.
        let mut type_params = String::new();
        if let Some(tp) = kt::child(decl, "type_parameters") {
            let mut cursor = tp.walk();
            let mut parts: Vec<String> = Vec::new();
            for t in tp.children(&mut cursor) {
                if t.kind() != "type_parameter" {
                    continue;
                }
                let id = kt::child(t, "identifier");
                let bound = kt::child(t, "user_type").or_else(|| kt::child(t, "nullable_type"));
                match (id, bound) {
                    (Some(id), Some(bound)) => {
                        let id_text = self.text(id).to_string();
                        let bound = kt::java_type_ann(bound, self.source, self.annots);
                        if bound == "Object" || bound == "Any" {
                            parts.push(id_text);
                        } else {
                            parts.push(format!("{} extends {}", id_text, bound));
                        }
                    }
                    (Some(id), None) => {
                        // unbounded type param: `<T>` is valid Java
                        parts.push(self.text(id).to_string());
                    }
                    (None, _) => {
                        self.diag_untranslatable(t, "type parameter without a name");
                    }
                }
                // reified has no Java counterpart (inline-only); approximate
                for m in t.children(&mut t.walk()) {
                    if m.kind() == "type_parameter_modifiers" {
                        let txt = self.text(m).trim().to_string();
                        if txt.contains("reified") {
                            // A reified parameter only works if its ONE use
                            // is `T::class.java` — the Java ABI then becomes
                            // `Class<T>` explicitly passed at the call site.
                            // Any other use (calling `T.class` in the body,
                            // `T()` construction...) cannot exist in Java and
                            // taints, because emitting `<T extends …> f()`
                            // with a body using `T.class` is invalid Java.
                            let type_name =
                                id.map(|n| self.text(n).to_string()).unwrap_or_default();
                            let body_uses_type_literal_elsewhere = self
                                .text(decl)
                                .contains(&format!("{}::class.java", type_name));
                            let body_uses_param_in_any_way = self.text(decl).contains(&type_name);
                            // The ONLY translatable use of a reified param is
                            // exactly one `T::class.java` inlining; anything
                            // beyond that (`as T`, construction, reflection on
                            // T) taints the declaration.
                            let uses_beyond_literal =
                                body_uses_param_in_any_way && !body_uses_type_literal_elsewhere;
                            if !uses_beyond_literal {
                                self.diag_approx(
                                    m,
                                    "reified type parameter has no Java counterpart; emitted without it",
                                );
                            } else {
                                self.diag_untranslatable(
                                    decl,
                                    "reified type parameter used beyond `T::class.java` inlining has no Java form; declaration retained in Kotlin",
                                );
                                return;
                            }
                        } else if !txt.is_empty() {
                            self.diag_untranslatable(
                                m,
                                format!("type-parameter modifier not supported: {}", txt),
                            );
                        }
                    }
                }
            }
            if !parts.is_empty() {
                type_params = format!("<{}> ", parts.join(", "));
            }
        }

        // return type: positional — the first type-ish named child after
        // function_value_parameters (grammar has no return_type field).
        let mut ret = "void".to_string();
        // `override fun toString() = ...` overrides Any.toString(): String —
        // Kotlin infers the return type from the override; Java needs it
        // spelled out or javac sees an unrelated void toString().
        let fname_raw = kt::field(decl, "name")
            .map(|n| self.source[n.start_byte()..n.end_byte()].to_string())
            .unwrap_or_default();
        let mut explicit_ret = false;
        {
            let mut cursor = decl.walk();
            let kids: Vec<_> = decl.children(&mut cursor).collect();
            let mut after_params = false;
            for k in kids {
                if k.kind() == "function_value_parameters" {
                    after_params = true;
                    continue;
                }
                if after_params && k.is_named() {
                    if fname_raw == "toString" {
                        ret = "String".to_string();
                    }
                    match k.kind() {
                        "user_type" | "nullable_type" | "function_type" | "type"
                        | "parenthesized_type" => {
                            ret = kt::java_type_ann(k, self.source, self.annots);
                            explicit_ret = true;
                        }
                        _ => {}
                    }
                    break;
                }
            }
        }
        // Register the return type so `val x = fname()` call sites infer
        // (Pair.first -> getKey() and friends need fn-receiver context).
        // Expression-body infer when no explicit type: Kotlin infers from
        // the body (`= when ... -> int` would otherwise emit void and
        // break `return`). Number literal -> int; string -> String;
        // else keep void with an N002 note at the body site.
        if ret == "void"
            && !explicit_ret
            && let Some(fv) = decl
                .children(&mut decl.walk())
                .find(|c| c.kind() == "function_body")
        {
            let is_expr_body = fv.children(&mut fv.walk()).any(|c| c.kind() == "=");
            if is_expr_body {
                let body_expr = fv.children(&mut fv.walk()).find(|c| {
                    c.is_named()
                        && c.kind() != "="
                        && !matches!(c.kind(), "line_comment" | "block_comment")
                });
                let inferred = body_expr.map(|be| {
                    match be.kind() {
                        "number_literal" => Some("int"),
                        "string_literal" | "interpolated_string" => Some("String"),
                        "true" | "false" => Some("boolean"),
                        // when/if over numbers: pick the arm literal kinds
                        "when_expression" | "if_expression" | "binay" => {
                            let mut has_num = false;
                            let mut has_str = false;
                            let mut stack = vec![be];
                            while let Some(n) = stack.pop() {
                                match n.kind() {
                                    "number_literal" => has_num = true,
                                    "string_literal" | "interpolated_string" => has_str = true,
                                    _ => {}
                                }
                                let mut wc = n.walk();
                                for c in n.children(&mut wc) {
                                    stack.push(c);
                                }
                            }
                            if has_num && !has_str {
                                Some("int")
                            } else if has_str && !has_num {
                                Some("String")
                            } else {
                                None
                            }
                        }
                        // `Type(...)` — an expression-bodied function that only
                        // constructs a value still needs its return type: Kotlin
                        // infers the constructed type, and defaulting to `void`
                        // emits `return new Type(x);` inside a void method, which
                        // javac rejects with "unexpected return value" and turns
                        // every call site into "'void' type not allowed here".
                        "call_expression" => {
                            let text = self.text(be);
                            let head = text.split('(').next().unwrap_or("").trim();
                            let segment = head.rsplit('.').next().unwrap_or(head);
                            let end = segment
                                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                                .unwrap_or(segment.len());
                            let name = &segment[..end];
                            // Kotlin's `TODO()` is a function, not a type.
                            if !name.is_empty()
                                && name != "TODO"
                                && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                            {
                                Some(crate::transpiler::types::map_type_name(name))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                    .map(|s| s.to_string())
                });
                if let Some(t) = inferred.flatten() {
                    ret = t;
                }
            }
        }
        self.fn_rets.insert(fname_raw.clone(), ret.clone());

        // parameters
        let mut params: Vec<String> = Vec::new(); // signature fragments "ty name"
        let mut param_names: Vec<String> = Vec::new();
        let mut param_defaults: Vec<Option<tree_sitter::Node>> = Vec::new();
        if let Some(fvp) = kt::child(decl, "function_value_parameters") {
            // Default values (`= expr`) sit between/before parameters as
            // siblings inside function_value_parameters. Kotlin binds `= x`
            // to the parameter that immediately precedes it.
            let mut cursor = fvp.walk();
            let kids: Vec<tree_sitter::Node> = fvp.children(&mut cursor).collect();
            let mut last_param: Option<tree_sitter::Node> = None;
            let mut defaults: Vec<(tree_sitter::Node, tree_sitter::Node)> = Vec::new(); // (param, expr)
            for (i, k) in kids.iter().enumerate() {
                match k.kind() {
                    "parameter" => last_param = Some(*k),
                    "=" => {
                        // default value binds to the preceding parameter; the
                        // expression is the next named child of the list
                        if let Some(prev) = last_param
                            && let Some(expr) = kids.get(i + 1).copied().filter(|n| n.is_named())
                        {
                            defaults.push((prev, expr));
                        }
                    }
                    _ => {}
                }
            }
            let mut prev_modifiers: Option<tree_sitter::Node> = None;
            for k in &kids {
                match k.kind() {
                    "parameter_modifiers" => prev_modifiers = Some(*k),
                    "parameter" => {
                        let modifiers = prev_modifiers.take();
                        let is_vararg = modifiers
                            .map(|m| self.text(m).contains("vararg"))
                            .unwrap_or(false);
                        let annotations = modifiers
                            .map(|m| {
                                m.children(&mut m.walk())
                                    .filter(|modifier| {
                                        matches!(
                                            modifier.kind(),
                                            "annotation" | "annotated_expression"
                                        )
                                    })
                                    .filter_map(|annotation| {
                                        self.transpile_declaration_annotation(annotation)
                                    })
                                    .filter(|annotation| !is_kotlin_jvm_annotation(annotation))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        let pname = kt::child(*k, "identifier")
                            .map(|n| self.text(n).to_string())
                            .unwrap_or_else(|| "arg".to_string());
                        let pty = kt::child(*k, "user_type")
                            .or_else(|| kt::child(*k, "nullable_type"))
                            .map(|t| {
                                let java = kt::java_parameter_type(t, self.source, self.annots);
                                crate::transpiler::types::covariant_readonly_parameter(
                                    self.text(t),
                                    &java,
                                )
                            })
                            .unwrap_or_else(|| "Object".to_string());
                        if is_vararg {
                            params.push(format!(
                                "{}{}{}... {}",
                                annotations.join(" "),
                                if annotations.is_empty() { "" } else { " " },
                                pty,
                                pname,
                            ));
                        } else {
                            params.push(format!(
                                "{}{}{} {}",
                                annotations.join(" "),
                                if annotations.is_empty() { "" } else { " " },
                                pty,
                                pname,
                            ));
                        }
                        param_names.push(pname.clone());
                        param_defaults.push(
                            defaults
                                .iter()
                                .find(|(p, _)| p.id() == k.id())
                                .map(|(_, d)| *d),
                        );
                        self.var_types.insert(pname, pty.clone());
                    }
                    _ => {}
                }
            }
        }
        // Extension receiver (`fun String.shout()`): the grammar puts the
        // receiver type as a bare user_type before the function name. Emit
        // it as the first parameter; `this` in the body refers to it.
        let mut prev_receiver: Option<String> = None;
        {
            // Extension receiver: a bare user_type before the `name` field.
            // Collect (field, kind) per child in one synced cursor walk.
            let mut fcur = decl.walk();
            let mut fields: Vec<(Option<String>, String, tree_sitter::Node)> = Vec::new();
            if fcur.goto_first_child() {
                loop {
                    fields.push((
                        fcur.field_name().map(|s| s.to_string()),
                        fcur.node().kind().to_string(),
                        fcur.node(),
                    ));
                    if !fcur.goto_next_sibling() {
                        break;
                    }
                }
            }
            let name_pos = fields
                .iter()
                .position(|(f, _, _)| f.as_deref() == Some("name"));
            let recv = fields.iter().enumerate().find_map(|(i, (f, k, n))| {
                let is_type = k == "user_type" || k == "nullable_type";
                if !is_type || f.is_some() {
                    return None;
                }
                match name_pos {
                    // receiver: unfielded type strictly before the name
                    Some(np) if i < np => Some(*n),
                    _ => None,
                }
            });
            if let Some(recv_ty) = recv {
                let recv_java = kt::java_type_ann(recv_ty, self.source, self.annots);
                self.var_types
                    .insert("__receiver__".to_string(), recv_java.clone());
                params.insert(0, format!("{} __receiver__", recv_java));
                if make_static {
                    // Same-file statics: call sites `x.f(...)` can be
                    // rewritten to `f(x, ...)` (see expr.rs call handling).
                    self.extension_fns.insert(name.clone(), recv_java);
                }
                prev_receiver = self.ext_receiver_name.replace("__receiver__".to_string());
                self.diag_approx(
                    decl,
                    "extension function: receiver emitted as first parameter; call sites `x.f()` become `f(x)` in the same file",
                );
            }
        }
        if is_main && params.is_empty() {
            params.push("String[] args".to_string());
            // Kotlin `fun main()` implicitly takes Array<String> args.
            self.var_types
                .insert("args".to_string(), "String[]".to_string());
        }

        let is_static = if make_static { "static " } else { "" };
        let is_getter_function = fname_raw.strip_prefix("get").is_some_and(|suffix| {
            suffix
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_uppercase())
        });
        let non_null_override_return = has_override_modifier
            && is_getter_function
            && explicit_ret
            && !ret.starts_with('@')
            && !matches!(
                ret.as_str(),
                "void"
                    | "boolean"
                    | "byte"
                    | "short"
                    | "int"
                    | "long"
                    | "float"
                    | "double"
                    | "char"
            );
        let has_body = kt::child(decl, "function_body").is_some();
        // Inside an interface: bodyless stays implicit, with body -> default
        let in_interface = kt::parent_of(decl)
            .and_then(|p| kt::parent_of(p))
            .map(|gp| gp.children(&mut gp.walk()).any(|c| c.kind() == "interface"))
            .unwrap_or(false);
        // `external` fun has no JVM body; emit `native` and skip the body.
        let abstract_kw = if is_external {
            "native "
        } else if has_body {
            if in_interface { "default " } else { "" }
        } else {
            "abstract "
        };
        let has_body = has_body && !is_external;
        for annotation in function_annotations {
            out.line(annotation);
        }
        if non_null_override_return
            && let Some(annotation) = crate::transpiler::types::non_null_annotation(self.annots)
        {
            out.line(annotation);
        }
        if !has_body {
            // Bodyless: signature-only (abstract / interface method)
            self.ext_receiver_name = prev_receiver;
            out.line(format!(
                "{}{}{}{}{} {}({});",
                visibility,
                is_static,
                abstract_kw,
                type_params,
                ret,
                name,
                params.join(", ")
            ));
            return;
        }
        out.open(format!(
            "{}{}{}{}{} {}({})",
            visibility,
            is_static,
            abstract_kw,
            type_params,
            ret,
            name,
            params.join(", ")
        ));

        // body
        self.current_function_returns_value = ret != "void";
        self.current_function_return_type = Some(ret.clone());
        if let Some(fb) = kt::child(decl, "function_body") {
            let reflective_locals = self.text(fb).contains(".getConstructor(") && {
                let mut stack = vec![fb];
                let mut found = false;
                while let Some(node) = stack.pop() {
                    if node.kind() == "property_declaration" {
                        found = true;
                        break;
                    }
                    stack.extend(
                        node.children(&mut node.walk())
                            .filter(|child| child.is_named()),
                    );
                }
                found
            };
            if reflective_locals {
                out.open("try");
                self.transpile_function_body(fb, out, ret != "void");
                out.close_then("catch (Exception e)");
                out.line("throw new RuntimeException(e);");
                out.close();
            } else {
                self.transpile_function_body(fb, out, ret != "void");
            }
        }
        out.close();
        self.current_function_returns_value = false;
        self.current_function_return_type = None;

        // Kotlin default parameters -> Java overloads. Only a defaulted
        // *suffix* is expressible as overloads (a defaulted middle param
        // can't be skipped without named arguments); each suffix overload
        // delegates to the full method filling the omitted defaults.
        let trailing_defaults: Vec<(String, tree_sitter::Node)> = {
            let mut td: Vec<(String, tree_sitter::Node)> = Vec::new();
            for i in (0..param_names.len()).rev() {
                match param_defaults.get(i).copied().flatten() {
                    Some(d) => td.push((param_names[i].clone(), d)),
                    None => break,
                }
            }
            td.reverse();
            td
        };
        if has_body {
            let defaulted: Vec<&str> = param_names
                .iter()
                .zip(param_defaults.iter())
                .filter(|(_, d)| d.is_some())
                .map(|(n, _)| n.as_str())
                .collect();
            if !defaulted.is_empty() && trailing_defaults.is_empty() {
                // Defaults exist but none form a trailing suffix: no overload
                // can stand in — callers must pass these explicitly.
                self.diag_approx(
                    decl,
                    format!(
                        "default parameter value(s) on '{}' (params: {}) have no Java counterpart — callers must pass them explicitly",
                        name,
                        defaulted.join(", ")
                    ),
                );
            } else if !trailing_defaults.is_empty() {
                // One N002 per function (not per param), as user policy.
                self.diag_approx(
                    decl,
                    format!(
                        "default parameter values on '{}' approximated by synthesizing {} Java overload(s); named-argument and mid-parameter skipping semantics not reproduced",
                        name,
                        trailing_defaults.len()
                    ),
                );
                let n = params.len();
                let m = trailing_defaults.len();
                for k in 1..=m {
                    let sig = params[..n - k].join(", ");
                    let mut call_args: Vec<String> = param_names[..n - k].to_vec();
                    for (_, dnode) in &trailing_defaults[m - k..] {
                        let mut e = Expr { unit: self };
                        call_args.push(e.transpile(*dnode));
                    }
                    out.blank();
                    out.open(format!(
                        "{}{}{}{} {}({})",
                        visibility, is_static, type_params, ret, name, sig
                    ));
                    if ret == "void" {
                        out.line(format!("{}({});", name, call_args.join(", ")));
                    } else {
                        out.line(format!("return {}({});", name, call_args.join(", ")));
                    }
                    out.close();
                }
            }
        }
        self.ext_receiver_name = prev_receiver;
    }

    pub(crate) fn transpile_function_body(
        &mut self,
        fb: tree_sitter::Node,
        out: &mut JavaOut,
        returns_value: bool,
    ) {
        let previous_returns_value = self.current_function_returns_value;
        self.current_function_returns_value = returns_value;
        let mut cursor = fb.walk();
        for child in fb.children(&mut cursor) {
            if child.kind() == "block" {
                let mut inner = child.walk();
                for stmt in child.children(&mut inner) {
                    if stmt.is_named()
                        && stmt.kind() != "{"
                        && stmt.kind() != "}"
                        && !matches!(stmt.kind(), "line_comment" | "block_comment")
                    {
                        self.transpile_statement(stmt, out);
                    }
                }
            } else if child.is_named()
                && child.kind() != "="
                && !matches!(child.kind(), "line_comment" | "block_comment")
            {
                // expression body: `= expr` -> `return expr;`
                let mut e = Expr { unit: self };
                let java = e.transpile(child);
                let java = self.coerce_optional_stream_return(java);
                let java = crate::transpiler::stmt::fix_join_tail(&java);
                let trimmed = java.trim_start();
                // `= if (…) … else …` / `= when (…)` cannot become a ternary
                // when the branches are blocks: the emitted text is then a
                // STATEMENT whose branches already carry their own `return`.
                // Prefixing another one yields `return if (…) { return …; }`,
                // which is not Java at all.
                let statement_shaped = matches!(child.kind(), "if_expression" | "when_expression")
                    && (trimmed.starts_with("if (") || trimmed.starts_with("switch ("))
                    && java.contains("return ");
                if trimmed.starts_with("throw ") {
                    out.line(format!("{};", java));
                } else if statement_shaped {
                    out.line(java);
                } else if returns_value {
                    out.line(format!("return {};", java));
                } else {
                    out.line(format!("{};", java));
                }
            }
        }
        self.current_function_returns_value = previous_returns_value;
    }

    pub(crate) fn coerce_optional_stream_return(&self, java: String) -> String {
        let Some(inner) = self
            .current_function_return_type
            .as_deref()
            .and_then(|ty| ty.strip_prefix("Optional<"))
            .and_then(|ty| ty.strip_suffix('>'))
        else {
            return java;
        };
        if java.ends_with(".findAny()") || java.ends_with(".findFirst()") {
            format!("(Optional<{inner}>) (Optional<?>) ({java})")
        } else {
            java
        }
    }
}

fn is_kotlin_jvm_annotation(annotation: &str) -> bool {
    [
        "JvmOverloads",
        "JvmName",
        "JvmStatic",
        "JvmField",
        "JvmSuppressWildcards",
        "JvmSynthetic",
    ]
    .iter()
    .any(|jvm_only| annotation.contains(jvm_only))
}
