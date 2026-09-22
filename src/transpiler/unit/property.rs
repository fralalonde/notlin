//! Property emission: fields, accessors, lateinit, delegates, statics.

use super::Unit;
use super::capitalize;
use crate::transpiler::expr::Expr;
use crate::transpiler::java::JavaOut;
use crate::transpiler::kt;

impl<'a> Unit<'a> {
    pub(crate) fn transpile_property(&mut self, decl: tree_sitter::Node, out: &mut JavaOut) {
        self.transpile_property_opts(decl, out, false, None)
    }

    /// Emit a property as instance members (`make_static=false`) or as static
    /// members of the enclosing class (`make_static=true`, used for companion
    /// objects, top-level properties and object singletons). `owner` is the
    /// enclosing Java class name, needed by static setters (`this.x = x` is
    /// illegal in a static method).
    pub(crate) fn transpile_property_opts(
        &mut self,
        decl: tree_sitter::Node,
        out: &mut JavaOut,
        make_static: bool,
        owner: Option<&str>,
    ) {
        // Fresh scope per property: getter/setter bodies must not see locals
        // declared while a previous property was translated.
        self.var_types.clear();
        let is_val = kt::child(decl, "val").is_some();
        let vd = kt::child(decl, "variable_declaration");
        let name = vd
            .and_then(|v| kt::child(v, "identifier"))
            .map(|n| self.text(n).to_string())
            .unwrap_or_else(|| "prop".to_string());
        let ty = vd
            .and_then(|v| kt::child(v, "user_type").or_else(|| kt::child(v, "nullable_type")))
            .map(|t| kt::java_type_ann(t, self.source, self.annots));
        let visibility = self.visibility_of(decl);

        // Destructuring class property `val (a, b) = expr`: field + accessor
        // shapes don't apply; flag it instead of emitting a junk `prop` field.
        if vd.is_none() && kt::child(decl, "multi_variable_declaration").is_some() {
            self.diag_approx(
                decl,
                format!(
                    "destructuring class property '{}': componentN() extraction not reproduced; emitted as a single backing field",
                    name
                ),
            );
            return;
        }

        // lateinit: Kotlin promises non-null after init, Java fields start
        // null — flag the null-init gap.
        if kt::child(decl, "modifiers")
            .map(|m| self.text(m).contains("lateinit"))
            .unwrap_or(false)
        {
            self.diag_approx(
                decl,
                format!(
                    "lateinit var '{}': Java fields default to null — no non-null enforcement (reading before init returns null instead of throwing)",
                    name
                ),
            );
        }

        // Delegated property (`by lazy {}` / `by observable` / ...): the
        // delegate expression becomes the initializer; lazy semantics are
        // approximated by an eager initializer + getter (functional superset,
        // minor imbalance).
        let delegate = kt::child(decl, "property_delegate");
        if let Some(delim) = delegate {
            let dtext = self.text(delim);
            if dtext.contains("lazy") {
                // initializer is the lambda body after `lazy { ... }`
                let expr = delim
                    .children(&mut delim.walk())
                    .find(|c| c.kind() == "call_expression")
                    .and_then(|ce| {
                        ce.children(&mut ce.walk())
                            .find(|c| c.kind() == "annotated_lambda")
                    })
                    .and_then(|al| kt::child(al, "lambda_literal"))
                    .and_then(|ll| ll.children(&mut ll.walk()).find(|c| c.is_named()));
                let _cap = capitalize(&name);
                match expr {
                    Some(body) => {
                        // if the body is a lambda (collection literal), take its
                        // statements into the initializer best-effort
                        let mut e = Expr { unit: self };
                        let java = e.transpile(body);
                        let tyy = ty.clone().unwrap_or_else(|| "Object".to_string());
                        if java.contains("LAMBDA") || body.kind() == "lambda_literal" {
                            out.line(format!("private {} {} = null;", tyy, name));
                            self.diag_approx(
                                delim,
                                format!(
                                    "`by lazy {{ ... }}` for '{}' emitted as `= null` + warning (lambda body not representable in field initializer); body reads: {}",
                                    name, self.text(body).trim()
                                ),
                            );
                        } else {
                            out.line(format!("private {} {} = {};", tyy, name, java));
                            self.diag_approx(
                                delim,
                                format!(
                                    "`by lazy` for '{}' emitted as eager initializer (memoization not reproduced)",
                                    name
                                ),
                            );
                        }
                        return;
                    }
                    None => {
                        self.diag_untranslatable(delim, "delegate `lazy` without body");
                        return;
                    }
                }
            } else {
                if let Some(dn) = self.current_decl {
                    let label = self.decl_labels.get(&dn.id()).cloned().unwrap_or_default();
                    self.taint_decl(&label);
                }
                self.diag_untranslatable(
                    delim,
                    format!("property delegate not supported: {}", dtext.trim()),
                );
                return;
            }
        }

        let getter = kt::child(decl, "getter");
        let setter = kt::child(decl, "setter");
        let initializer = kt::child(decl, "initializer"); // hmm: may not exist; handle '=' expr below

        // Determine the type: declared or inferred from initializer
        let ty = match ty {
            Some(t) => t,
            None => {
                // inferred: from initializer expression (best-effort: Object unless literal)
                let init = self.property_initializer(decl);
                match init {
                    Some(init_node) => self.infer_type(init_node),
                    None => "Object".to_string(),
                }
            }
        };

        // Custom getter/setter bodies
        let getter_body: Option<tree_sitter::Node> =
            getter.as_ref().and_then(|g| kt::child(*g, "function_body"));
        let setter_body: Option<tree_sitter::Node> =
            setter.as_ref().and_then(|s| kt::child(*s, "function_body"));

        // Field (skip if it's purely a getter property with no backing field use;
        // we can't tell yet, so emit backing field unless there's no initializer
        // and no setter and a custom getter — heuristic).
        let backing = initializer.is_some() || setter_body.is_some() || getter.is_none();
        // `const val` -> static final; otherwise static only when requested
        // (companion/top-level/object properties).
        let is_const = kt::child(decl, "modifiers")
            .map(|m| self.text(m).contains("const"))
            .unwrap_or(false);
        let static_kw = if is_const {
            "static final "
        } else if make_static && is_val {
            // Kotlin `val` is final; keep the field final in the static
            // mirror so bytecode-level immutability semantics match.
            "static final "
        } else if make_static {
            "static "
        } else {
            ""
        };
        if backing {
            let init_java = self.property_initializer(decl).map(|init| {
                let mut e = Expr { unit: self };
                e.transpile(init)
            });
            match init_java {
                Some(java) => out.line(format!("private {}{} {} = {};", static_kw, ty, name, java)),
                None => out.line(format!("private {}{} {};", static_kw, ty, name)),
            }
        }

        // getter — but not if the user declared their own accessor-method
        // of the same name in the class body (would collide in Java).
        let cap = capitalize(&name);
        let getter_name = format!("get{}", cap);
        let setter_name = format!("set{}", cap);
        let mut conflicts: Vec<String> = Vec::new();
        if let Some(body) = kt::parent_of(decl).filter(|p| p.kind() == "class_body") {
            let mut cursor = body.walk();
            for member in body.children(&mut cursor) {
                if member.kind() == "function_declaration"
                    && let Some(mname) = kt::field(member, "name")
                {
                    conflicts.push(self.text(mname).to_string());
                }
            }
        }
        let has_getter_method = conflicts.contains(&getter_name);
        let has_setter_method = conflicts.contains(&setter_name);

        if !has_getter_method {
            out.open(format!(
                "{}{}{} get{}()",
                visibility,
                if make_static { "static " } else { "" },
                ty,
                cap
            ));
            match getter_body {
                Some(gb) => {
                    // body: `= expr` or `{ ... }`
                    let mut cursor = gb.walk();
                    for c in gb.children(&mut cursor) {
                        if c.kind() == "block" {
                            let mut inner = c.walk();
                            for s in c.children(&mut inner) {
                                if s.is_named() {
                                    self.transpile_statement(s, out);
                                }
                            }
                        } else if c.is_named() && c.kind() != "=" {
                            let mut e = Expr { unit: self };
                            let java = e.transpile(c);
                            out.line(format!("return {};", java));
                        }
                    }
                }
                None => out.line(format!("return {};", name)),
            }
            out.close();
        }

        if !is_val && !has_setter_method {
            // Kotlin `private set`: the setter exists but is private;
            // otherwise its visibility matches the property's.
            let set_vis = if setter
                .as_ref()
                .and_then(|s| kt::child(*s, "modifiers"))
                .map(|m| self.text(m).contains("private"))
                .unwrap_or(false)
            {
                "private ".to_string()
            } else {
                visibility.clone()
            };
            out.blank();
            out.open(format!(
                "{}{}void set{}({} {})",
                set_vis,
                if make_static { "static " } else { "" },
                cap,
                ty,
                name
            ));
            match setter_body {
                Some(sb) => {
                    let mut cursor = sb.walk();
                    for c in sb.children(&mut cursor) {
                        if c.kind() == "block" {
                            let mut inner = c.walk();
                            for s in c.children(&mut inner) {
                                if s.is_named() {
                                    self.transpile_statement(s, out);
                                }
                            }
                        } else if c.is_named() && c.kind() != "=" {
                            self.diag_untranslatable(c, "expression setters not yet supported");
                        }
                    }
                }
                None => {
                    // `this.x = x` is illegal in a static method; qualify the
                    // field with the owner class name instead.
                    match owner {
                        Some(o) if make_static => out.line(format!("{}.{} = {};", o, name, name)),
                        _ => out.line(format!("this.{} = {};", name, name)),
                    }
                }
            }
            out.close();
        }
    }

    pub fn property_initializer<'t>(
        &self,
        decl: tree_sitter::Node<'t>,
    ) -> Option<tree_sitter::Node<'t>> {
        let mut cursor = decl.walk();
        let children: Vec<tree_sitter::Node<'t>> = decl.children(&mut cursor).collect();
        for (i, c) in children.iter().enumerate() {
            if c.kind() == "=" {
                return children.get(i + 1).copied().filter(|n| n.is_named());
            }
        }
        None
    }

    pub(crate) fn transpile_toplevel_property(
        &mut self,
        decl: tree_sitter::Node,
        out: &mut JavaOut,
        owner: &str,
    ) {
        // same as transpile_property but static, owned by the file-level
        // utility class (`owner` qualifies static setter bodies).
        self.transpile_property_opts(decl, out, true, Some(owner));
    }
}
