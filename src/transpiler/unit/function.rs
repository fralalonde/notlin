//! Function emission: params, receivers, type parameters, bodies.

use super::Unit;
use crate::transpiler::expr::Expr;
use crate::transpiler::java::JavaOut;
use crate::transpiler::kt;

impl<'a> Unit<'a> {
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
        let name = kt::field(decl, "name")
            .map(|n| self.text(n).to_string())
            .unwrap_or_else(|| "anon".to_string());
        let visibility = self.visibility_of(decl);

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
                            self.diags.warn_approx(
                                f,
                                self.file,
                                "Kotlin `suspend` compiled to a plain blocking method; coroutine semantics lost",
                            );
                        }
                        "external" => {
                            // JNI-shaped; bodyless native method is closest
                            self.diags.warn_approx(
                                f,
                                self.file,
                                "Kotlin `external` emitted as JNI `native` method",
                            );
                            is_external = true;
                        }
                        "operator" | "infix" | "tailrec" => {
                            self.diags.warn_approx(
                                f,
                                self.file,
                                format!("Kotlin function modifier `{}` has no Java counterpart; emitted as a plain method", word),
                            );
                        }
                        "inline" => {
                            // Java can't inline functions; harmless no-op
                            self.diags.warn_approx(
                                f,
                                self.file,
                                "Kotlin `inline` dropped (JIT inlines anyway)",
                            );
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
                            self.diags.warn_approx(
                                m,
                                self.file,
                                "reified type parameter has no Java counterpart; emitted without it",
                            );
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
                    match k.kind() {
                        "user_type" | "nullable_type" | "function_type" | "type"
                        | "parenthesized_type" => {
                            ret = kt::java_type_ann(k, self.source, self.annots);
                        }
                        _ => {}
                    }
                    break;
                }
            }
        }

        // parameters
        let mut params: Vec<String> = Vec::new();
        if let Some(fvp) = kt::child(decl, "function_value_parameters") {
            // Default values (`= expr`) sit between/before parameters as
            // siblings inside function_value_parameters. Kotlin binds `= x`
            // to the parameter that immediately precedes it.
            let mut cursor = fvp.walk();
            let kids: Vec<tree_sitter::Node> = fvp.children(&mut cursor).collect();
            let mut last_param: Option<tree_sitter::Node> = None;
            for k in &kids {
                match k.kind() {
                    "parameter" => last_param = Some(*k),
                    "=" => {
                        // default value binds to the preceding parameter
                        if let Some(prev) = last_param.take() {
                            let pname2 = kt::child(prev, "identifier")
                                .map(|n| self.text(n).to_string())
                                .unwrap_or_default();
                            self.diags.warn_approx(
                                prev,
                                self.file,
                                format!(
                                    "default parameter value on '{}' has no Java counterpart (caller must pass it explicitly)",
                                    pname2
                                ),
                            );
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
                        let is_vararg = prev_modifiers
                            .take()
                            .map(|m| self.text(m).contains("vararg"))
                            .unwrap_or(false);
                        let pname = kt::child(*k, "identifier")
                            .map(|n| self.text(n).to_string())
                            .unwrap_or_else(|| "arg".to_string());
                        let pty = kt::child(*k, "user_type")
                            .or_else(|| kt::child(*k, "nullable_type"))
                            .map(|t| kt::java_type_ann(t, self.source, self.annots))
                            .unwrap_or_else(|| "Object".to_string());
                        if is_vararg {
                            params.push(format!("{}... {}", pty, pname));
                        } else {
                            params.push(format!("{} {}", pty, pname));
                        }
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
                self.diags.warn_approx(
                    decl,
                    self.file,
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
        if !has_body {
            // Bodyless: signature-only (abstract / interface method)
            self.ext_receiver_name = prev_receiver;
            out.line(format!(
                "{}{}{}{}{} {}({});",
                visibility,
                is_static,
                type_params,
                abstract_kw,
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
            type_params,
            abstract_kw,
            ret,
            name,
            params.join(", ")
        ));

        // body
        if let Some(fb) = kt::child(decl, "function_body") {
            self.transpile_function_body(fb, out);
        }
        out.close();
        self.ext_receiver_name = prev_receiver;
    }

    pub(crate) fn transpile_function_body(&mut self, fb: tree_sitter::Node, out: &mut JavaOut) {
        let mut cursor = fb.walk();
        for child in fb.children(&mut cursor) {
            if child.kind() == "block" {
                let mut inner = child.walk();
                for stmt in child.children(&mut inner) {
                    if stmt.is_named() && stmt.kind() != "{" && stmt.kind() != "}" {
                        self.transpile_statement(stmt, out);
                    }
                }
            } else if child.is_named() && child.kind() != "=" {
                // expression body: `= expr` -> `return expr;`
                let mut e = Expr { unit: self };
                let java = e.transpile(child);
                out.line(format!("return {};", java));
            }
        }
    }
}
