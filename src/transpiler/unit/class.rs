//! Class/object/interface/record emission.

use super::Unit;
use super::capitalize;
use crate::diagnostics::DiagnosticKind;
use crate::transpiler::expr::Expr;
use crate::transpiler::java::JavaOut;
use crate::transpiler::kt;

/// One companion fn captured for the nested `Companion` bridge.
struct BridgeSig {
    signature: String,
    call: String,
}

impl<'a> Unit<'a> {
    pub(crate) fn collect_type_relations(&mut self, root: tree_sitter::Node<'a>) {
        let mut stack: Vec<tree_sitter::Node<'a>> = vec![root];
        while let Some(n) = stack.pop() {
            for c in n.children(&mut n.walk()) {
                stack.push(c);
            }
            // Top-level function names for `is_untranslated_file_function`:
            // a translated declaration must not emit a bare call to a file
            // function that remained Kotlin.
            if n.kind() == "function_declaration"
                && n.parent().is_some_and(|p| p.kind() == "source_file")
                && let Some(nm) = kt::field(n, "name")
            {
                let fname = self.text(nm).to_string();
                // A top-level function referenced by retained Kotlin source
                // will itself remain Kotlin (the loose-decl pass runs after
                // the type loop, too late for callee checks) — pre-mark it so
                // translated bodies taint instead of emitting bare calls.
                let retained = self.workspace_requires_top_level_retention(&fname);
                if retained {
                    self.retained_file_functions.insert(fname.clone());
                }
                self.top_level_functions.insert(fname);
            }
            if n.kind() != "class_declaration" {
                continue;
            }
            let Some(nm) = kt::field(n, "name") else {
                continue;
            };
            let tname = self.text(nm).to_string();
            let is_sealed = kt::child(n, "modifiers")
                .map(|m| self.text(m).contains("sealed"))
                .unwrap_or(false);
            if is_sealed {
                self.sealed_types.insert(tname.clone());
            }
            // Data classes: capture primary-constructor params as record
            // components for destructuring-site extraction (`val (a, b) = p`).
            let is_data = kt::child(n, "modifiers")
                .map(|m| self.text(m).contains("data"))
                .unwrap_or(false);
            if is_data && let Some(pvs) = kt::child(n, "primary_constructor") {
                let mut comps: Vec<(String, String)> = Vec::new();
                // Structure: primary_constructor > class_parameters > class_parameter
                let mut pvs_cur = pvs.walk();
                let cparams = pvs
                    .children(&mut pvs_cur)
                    .find(|c| c.kind() == "class_parameters");
                let params: Vec<tree_sitter::Node> = cparams
                    .map(|cp| {
                        cp.children(&mut cp.walk())
                            .filter(|c| c.is_named() && c.kind() == "class_parameter")
                            .collect()
                    })
                    .unwrap_or_default();
                for pm in params {
                    let mut pm_cur = pm.walk();
                    let named: Vec<tree_sitter::Node> =
                        pm.children(&mut pm_cur).filter(|c| c.is_named()).collect();
                    let pname = named
                        .iter()
                        .find(|c| c.kind() == "identifier")
                        .map(|x| self.text(*x).to_string())
                        .unwrap_or_default();
                    let ptype = named
                        .iter()
                        .find(|c| c.kind() == "user_type" || c.kind().ends_with("_type"))
                        .map(|t| kt::java_type(*t, self.source))
                        .unwrap_or_else(|| "Object".to_string());
                    if !pname.is_empty() {
                        comps.push((ptype, pname));
                    }
                }
                if !comps.is_empty() {
                    self.data_components.insert(tname.clone(), comps);
                }
            }
            if let Some(sup) = self.superclass_name(n) {
                self.subclass_map.entry(sup).or_default().push(tname);
            }
        }
    }

    fn superclass_name(&self, decl: tree_sitter::Node<'a>) -> Option<String> {
        let dc = kt::child(decl, "delegation_specifiers")?;
        let spec = dc
            .children(&mut dc.walk())
            .find(|s| s.kind() == "delegation_specifier")?;
        let ci = spec
            .children(&mut spec.walk())
            .find(|c| c.kind() == "constructor_invocation")?;
        let ut = ci
            .children(&mut ci.walk())
            .find(|c| c.kind() == "user_type")?;
        let mut last: Option<tree_sitter::Node<'a>> = None;
        for ch in ut.children(&mut ut.walk()) {
            if ch.kind() == "identifier" {
                last = Some(ch);
            }
        }
        last.map(|n| self.text(n).to_string())
    }

    fn decl_contains(&self, decl: tree_sitter::Node<'a>, name: &str) -> bool {
        let mut stack: Vec<tree_sitter::Node<'a>> = vec![decl];
        while let Some(n) = stack.pop() {
            for c in n.children(&mut n.walk()) {
                stack.push(c);
            }
            if n.id() == decl.id()
                || !matches!(n.kind(), "class_declaration" | "object_declaration")
            {
                continue;
            }
            if let Some(nm) = kt::field(n, "name")
                && self.text(nm) == name
            {
                return true;
            }
        }
        false
    }

    pub(crate) fn transpile_type_decl_set(
        &mut self,
        out: &mut JavaOut,
        package: &str,
        imports: &[String],
        f: impl FnOnce(&mut Self, &mut JavaOut),
    ) {
        // Provenance header: every generated .java records which .kt produced
        // it (in-place migration trims the .kt, so the pair must stay matchable).
        // Forward slashes only: a raw backslash inside a Java comment is not a
        // unicode escape today, but `\c`-style prefixes would be rejected as
        // illegal escapes by javac on any path containing one.
        out.line(format!(
            "// NOTLIN: generated from {} — do not edit by hand while the source .kt exists",
            self.file.display().to_string().replace('\\', "/")
        ));
        if !package.is_empty() {
            out.line(format!("package {};", package));
            out.blank();
        }
        for imp in imports {
            if !imp.is_empty() {
                out.line(format!("import {};", imp));
            }
        }
        // Under --lombok the emitted @Data/@AllArgsConstructor need their
        // imports; user-declared lombok imports (Lombok-flagged source) may
        // already provide some — add only what's missing, exactly once.
        if self.lombok {
            for want in ["lombok.Data", "lombok.AllArgsConstructor"] {
                if !imports.iter().any(|i| i == want) {
                    out.line(format!("import {};", want));
                }
            }
        }
        // The generated body uses ArrayList/HashMap/HashSet/List/Map/Set from
        // stdlib collections; java.util.* covers them all in one line.
        out.line("import java.util.*;");
        out.line("import java.util.stream.Stream;");
        out.blank();
        if let Some(pkg) = crate::transpiler::types::nullable_import(self.annots) {
            out.line(format!("import {}.*;", pkg));
            out.blank();
        }
        f(self, out);
    }

    /// Lower one declaration annotation to Java text, or None when the
    /// annotation must stay in Kotlin. Java-native pass-through rules:
    ///   - the annotation NAME resolves to a pre-existing Java declaration in
    ///     the workspace index, OR the workspace is absent/unresolvable
    ///     (single-file probes; the target build proves the classpath) —
    ///   - the ARGUMENTS contain no Kotlin-only syntax: `::class` references,
    ///     string templates, `[]` array arguments, lambdas, or named-value
    ///     forms the annotation node renders with Kotlin spellings.
    ///
    /// The use-site target (`@get:` / `@field:`) is dropped: Java annotates
    /// the element itself, and notlin's primary-constructor properties are
    /// fields with Lombok accessors.
    pub(crate) fn transpile_declaration_annotation(
        &self,
        node: tree_sitter::Node,
    ) -> Option<String> {
        // Wrapper shape: an annotated_expression node (grammar quirk) carries
        // the annotation in a child plus, sometimes, a sibling
        // parenthesized_expression holding the argument list
        // (`@JsonSubTypes(...)`); when the arguments live INSIDE the
        // annotation's own constructor_invocation (`@JsonTypeInfo(use = ...)`)
        // the sibling is absent. Descend to the annotation for the parts and
        // prefer the in-node argument text.
        let (annotation_node, argument_text) = if node.kind() == "annotated_expression" {
            let annotation = kt::child(node, "annotation");
            let args = kt::child(node, "parenthesized_expression")
                .map(|p| self.text(p).to_string())
                .unwrap_or_default();
            let in_node_args = annotation
                .and_then(|a| kt::child(a, "constructor_invocation"))
                .and_then(|a| kt::child(a, "value_arguments"))
                .map(|a| self.text(a).to_string())
                .unwrap_or_default();
            let args = if in_node_args.is_empty() {
                args
            } else {
                in_node_args
            };
            (annotation, args)
        } else {
            (
                Some(node),
                kt::child(node, "constructor_invocation")
                    .and_then(|a| kt::child(a, "value_arguments"))
                    .map(|a| self.text(a).to_string())
                    .unwrap_or_default(),
            )
        };
        let annotation_node = annotation_node?;
        // The annotation name: the `constructor_invocation`'s `user_type`
        // (handles `com.example.Mapping`); a bare `user_type` directly under
        // the annotation node (no arguments, e.g. `@NotNull`) is the
        // fallback. The use-site target child is skipped by kind.
        let invocation_user_type = kt::child(annotation_node, "constructor_invocation")
            .and_then(|a| kt::child(a, "user_type"));
        let direct_user_type = annotation_node
            .children(&mut annotation_node.walk())
            .find(|c| matches!(c.kind(), "user_type" | "identifier"));
        let name = invocation_user_type
            .or(direct_user_type)
            .map(|c| self.text(c).to_string())?;
        // Kotlin-only argument shapes taint immediately — except a
        // `Name::class` class literal, which rewrites to `Name.class` in Java
        // when `Name` resolves to a Java-visible declaration (checked below);
        // an unknown or Kotlin-only name keeps the taint.
        if argument_text.contains("${")
            || argument_text.contains('[')
            || argument_text.contains('{')
        {
            return None;
        }
        if argument_text.contains("::class") {
            let mut lowered = argument_text.clone();
            for part in argument_text.split(['(', ',', ')']) {
                let trimmed = part.trim();
                if let Some(token) = trimmed.strip_suffix("::class") {
                    let class_name = token.split_whitespace().next_back().unwrap_or(token);
                    let simple = class_name.rsplit('.').next().unwrap_or(class_name);
                    let java_visible = self.workspace.is_some_and(|ws| {
                        ws.declarations().any(|d| {
                            d.name == simple
                                && d.kind != crate::workspace::DeclarationKind::Annotation
                        })
                    });
                    if !java_visible {
                        return None;
                    }
                    lowered = lowered.replace(
                        &format!("{class_name}::class"),
                        &format!("{class_name}.class"),
                    );
                }
            }
            if lowered.contains("::class") {
                return None;
            }
            return Some(
                format!(
                    "@{name}{}",
                    brace_wrap_unnamed_nested_annotations(&prefix_nested_annotations(&lowered))
                )
                .trim_end()
                .to_string(),
            );
        }
        // A workspace-provable Kotlin annotation type taints: the annotation
        // declaration itself stays Kotlin (or translates separately, but the
        // Java side cannot reference a Kotlin-only element).
        if let Some(workspace) = self.workspace
            && workspace.declarations().any(|decl| {
                decl.name == name && decl.kind == crate::workspace::DeclarationKind::Annotation
            })
            && !workspace.annotation_is_selected(&name, self.translation_roots)
        {
            return None;
        }
        // Rebuild from the parts: a use-site target (`@get:X(...)` -> `@X(...)`)
        // is dropped and the arguments ride on the bare name; the wrapper
        // shape (annotated_expression) holds its arguments separately from
        // the annotation node, so `text` is not usable there.
        // Both shapes pass ONLY the argument list (never the annotation name)
        // to the transforms: brace-wrapping the name would move it inside the
        // braces.
        let arguments =
            brace_wrap_unnamed_nested_annotations(&prefix_nested_annotations(&argument_text));
        let rebuilt = format!("@{name}{arguments}");
        // Kotlin wildcard/star imports and `!` nullability assertions never
        // appear here (parsed as value arguments), but a trailing semicolon
        // or whitespace from multi-annotation lines would break Java.
        Some(rebuilt.trim_end().to_string())
    }

    fn workspace_requires_kotlin_retention(&self, name: &str) -> bool {
        let Some(workspace) = self.workspace else {
            return false;
        };
        let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
        let Some(source_file) = workspace.source_file(indexed_path) else {
            return false;
        };
        let Some(target) = source_file
            .declarations
            .iter()
            .find(|declaration| declaration.name == name)
        else {
            return false;
        };
        workspace.has_unselected_kotlin_subtype(target, self.translation_roots)
            // Subtype rule, two modes:
            // - No fixpoint hint (single-file mode): retain on ANY Kotlin
            //   subtype — conservative, cannot know what translates later.
            // - With hint (fixpoint pass): retain only when a Kotlin subtype
            //   is ITSELF retained for an intrinsic reason; a retained
            //   Kotlin implementor cannot implement a translated-away
            //   supertype (enum entries ABI, KClass...). Monotone: seeds
            //   (intrinsically tainted decls) never shrink, so iteration
            //   reaches the least fixpoint.
            || (target.kind == crate::workspace::DeclarationKind::Interface
                && match self.retained_hint.as_ref() {
                    Some(retained) => workspace.has_retained_kotlin_subtype(target, retained),
                    None => workspace.has_kotlin_subtype(target),
                })
            || (target.has_default_constructor_parameter
                && workspace.has_kotlin_reference(indexed_path, name))
            || workspace.narrows_nullable_kotlin_property(source_file, target)
            || (self.in_place
                && workspace.inherits_retained_kotlin_property_interface(source_file, target))
            || workspace.property_smart_cast_used_by_kotlin(indexed_path, target)
    }

    /// A Kotlin companion `operator fun invoke` gives the enclosing type a
    /// class-call ABI (`Type(...)`) that Java cannot represent: Kotlin binds
    /// that syntax to a Java constructor after migration, never to a static
    /// factory. Only residual Kotlin callers make this a retention boundary.
    fn has_companion_operator_invoke(&self, decl: tree_sitter::Node) -> bool {
        let Some(body) = kt::child(decl, "class_body") else {
            return false;
        };
        let mut stack: Vec<tree_sitter::Node> = body
            .children(&mut body.walk())
            .filter(|node| node.kind() == "companion_object")
            .collect();
        while let Some(node) = stack.pop() {
            if node.kind() == "function_declaration"
                && kt::field(node, "name").is_some_and(|name| self.text(name) == "invoke")
                && self.text(node).contains("operator")
            {
                return true;
            }
            stack.extend(node.children(&mut node.walk()));
        }
        false
    }

    pub(crate) fn transpile_type_decl(&mut self, decl: tree_sitter::Node, out: &mut JavaOut) {
        // `KClass<T>` type references lower to Java `Class<T>` (kt.rs /
        // types.rs interop mapping). That ABI change is only compatible
        // when no RESIDUAL Kotlin file consumes this declaration: a
        // retained Kotlin caller passing `X::class` (KClass) to the
        // translated Java `Class` overload no longer compiles, and
        // override/property type agreement breaks. The index proves it:
        // retention when residual Kotlin references the declaration,
        // translation when only Java (or nobody) does — Kotlin callers ad
        // apt via `X::class.java`, which is legal against a `Class` param.
        if self.text(decl).contains("KClass") {
            let name = kt::field(decl, "name")
                .map(|n| self.text(n).to_string())
                .unwrap_or_default();
            let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
            if self
                .workspace
                .map(|w| w.has_external_kotlin_reference(indexed_path, &name))
                .unwrap_or(true)
            {
                self.diag_untranslatable(
                    decl,
                    "declaration references kotlin.reflect.KClass consumed by residual Kotlin; retained in Kotlin",
                );
                return;
            }
        }
        let mut is_data = false;
        let mut is_sealed = false;
        let mut is_enum = false;
        let mut is_annotation = false;
        let is_fun_interface = decl
            .children(&mut decl.walk())
            .any(|child| child.kind() == "fun");
        // Kotlin `interface` parses as class_declaration with an unnamed
        // `interface` keyword child.
        let is_interface = decl
            .children(&mut decl.walk())
            .any(|c| c.kind() == "interface");
        let mut modifiers = String::new();
        let mut annotations: Vec<String> = Vec::new();
        // Java-native annotations hoisted from a preceding top-level
        // annotated_expression wrapper (grammar quirk) ride first.
        if let Some(hoisted) = self.hoisted_annotations.remove(&decl.id()) {
            annotations.extend(hoisted);
        }
        if let Some(mods) = kt::child(decl, "modifiers") {
            let mut cursor = mods.walk();
            for m in mods.children(&mut cursor) {
                match m.kind() {
                    "annotation" => {
                        if let Some(text) = self.transpile_declaration_annotation(m) {
                            annotations.push(text);
                        } else {
                            self.diag_untranslatable(
                                m,
                                "declaration annotation is retained in Kotlin",
                            );
                        }
                    }
                    "class_modifier" => {
                        let mut inner = m.walk();
                        for cm in m.children(&mut inner) {
                            match cm.kind() {
                                "data" => is_data = true,
                                "open" | "abstract" | "sealed" => {
                                    let word = self.text(cm).trim();
                                    if word == "sealed" {
                                        is_sealed = true;
                                    }
                                    modifiers.push_str(word);
                                    modifiers.push(' ');
                                }
                                "enum" => is_enum = true,
                                "annotation" => is_annotation = true,
                                "companion" | "inline" | "value" | "expect" | "actual"
                                | "external" | "inner" => {
                                    self.diag_untranslatable(
                                        cm,
                                        format!("class modifier not supported: {}", self.text(cm)),
                                    );
                                }
                                _ => {}
                            }
                        }
                    }
                    "visibility_modifier" | "inheritance_modifier" => {
                        // handled below via text
                    }
                    _ => {}
                }
            }
        }
        if is_fun_interface {
            annotations.push("@FunctionalInterface".to_string());
        }
        // Kotlin permits repeating the same annotation on one declaration
        // (a stack of `@X(...)` lines); plain Java needs `@Repeatable` on the
        // annotation type, which notlin cannot verify — taint instead of
        // emitting an uncompilable duplicate.
        {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for annotation in &annotations {
                let annotation_name = annotation
                    .trim_start_matches('@')
                    .split(['(', ' ', '\n'])
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if !seen.insert(annotation_name.clone()) {
                    self.diag_untranslatable(
                        decl,
                        format!(
                            "annotation {annotation_name} is repeated on this declaration; Java needs @Repeatable which cannot be verified"
                        ),
                    );
                    return;
                }
            }
        }
        // visibility: Kotlin default = public; java default = package-private
        // We emit `public ` for Kotlin public (default) and nothing for private etc.
        let visibility = self.visibility_of(decl);

        let name = kt::field(decl, "name")
            .map(|n| self.text(n).to_string())
            .unwrap_or_else(|| "Anonymous".to_string());
        let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
        if self.has_companion_operator_invoke(decl)
            && self
                .workspace
                .map(|workspace| workspace.has_external_kotlin_reference(indexed_path, &name))
                .unwrap_or(true)
        {
            self.diag_untranslatable(
                decl,
                "companion operator `invoke` is consumed by residual Kotlin; Java constructors cannot preserve the Kotlin class-call ABI",
            );
            return;
        }
        if self.workspace_requires_kotlin_retention(&name) {
            self.diag_untranslatable(
                decl,
                "workspace Kotlin implementation requires this declaration to remain Kotlin",
            );
            return;
        }

        if decl.kind() == "object_declaration" {
            self.transpile_object(decl, &name, &visibility, &annotations, out);
            return;
        }
        if is_annotation {
            self.transpile_annotation_decl(decl, &name, &visibility, out);
            return;
        }
        if is_enum {
            self.transpile_enum(
                decl,
                &name,
                &visibility,
                &modifiers,
                is_sealed,
                &annotations,
                out,
            );
            return;
        }
        // primary constructor parameters -> fields + constructor
        // Class type parameters `class Gen<T : Bound>(...)` must be emitted or
        // field/ctor references to them won't resolve (P0 audit finding).
        let mut type_params = String::new();
        if let Some(tp) = kt::child(decl, "type_parameters") {
            let mut cursor = tp.walk();
            let mut parts: Vec<String> = Vec::new();
            for t in tp.children(&mut cursor) {
                if t.kind() != "type_parameter" {
                    continue;
                }
                if let Some(id) = kt::child(t, "identifier") {
                    let id_text = self.text(id).to_string();
                    match kt::child(t, "user_type").or_else(|| kt::child(t, "nullable_type")) {
                        Some(bound) => {
                            let b = kt::java_type_ann(bound, self.source, self.annots);
                            if b == "Object" || b == "Any" {
                                parts.push(id_text);
                            } else {
                                parts.push(format!("{} extends {}", id_text, b));
                            }
                        }
                        None => parts.push(id_text),
                    }
                }
                // variance/reified modifiers inside type_parameter: untranslatable
                for extra in t.children(&mut t.walk()) {
                    if extra.kind() == "type_parameter_modifiers" {
                        self.diag_untranslatable(
                            extra,
                            format!(
                                "type-parameter modifier '{}' has no Java counterpart",
                                self.text(extra).trim()
                            ),
                        );
                    }
                }
            }
            if !parts.is_empty() {
                type_params = format!("<{}> ", parts.join(", "));
            }
        }
        let params = self.class_params(decl);

        // superclass / interfaces
        // The grammar wraps each supertype in `delegation_specifier`
        // (constructor_invocation / explicit_delegation / user_type).
        let mut extends = String::new();
        let mut superclass: Option<String> = None;
        let mut super_ctor_args: Option<String> = None;
        if let Some(dc) = kt::child(decl, "delegation_specifiers") {
            let mut parts: Vec<String> = Vec::new();
            let mut cursor = dc.walk();
            for spec in dc.children(&mut cursor) {
                if spec.kind() != "delegation_specifier" {
                    continue;
                }
                // unwrap: take the first named inner node
                let inner = spec.children(&mut spec.walk()).find(|c| c.is_named());
                let Some(inner) = inner else {
                    continue;
                };
                match inner.kind() {
                    // `: Parent(args)` — constructor invocation => class
                    // superclass with ctor ARGUMENTS that must be forwarded
                    // (`: RuntimeException(message)`); dropping them makes
                    // the Java parent's required ctor unreachable.
                    "constructor_invocation" => {
                        if let Some(ut) = inner
                            .children(&mut inner.walk())
                            .find(|c| c.kind() == "user_type")
                        {
                            // raw text keeps Kotlin spellings (`Any`); run
                            // through the Kotlin->Java type-name mapping.
                            let t_j = kt::java_type(ut, self.source).replace(" ", "");
                            parts.push(format!("class:{}", t_j));
                            let mut args_collected: Vec<String> = Vec::new();
                            if let Some(va) = inner
                                .children(&mut inner.walk())
                                .find(|c| c.kind() == "value_arguments")
                            {
                                let mut acur = va.walk();
                                for a in va
                                    .children(&mut acur)
                                    .filter(|c| c.kind() == "value_argument")
                                {
                                    let expr_node = a
                                        .children(&mut a.walk())
                                        .find(|c| c.is_named())
                                        .unwrap_or(a);
                                    let raw_expr = self.text(expr_node).trim().to_string();
                                    let mut e = Expr { unit: self };
                                    let rendered = if expr_node.kind() == "navigation_expression" {
                                        let raw = raw_expr.as_str();
                                        let mut pieces = raw.split('.');
                                        let base = pieces.next().unwrap_or(raw).trim();
                                        let member = pieces.next().unwrap_or("").trim();
                                        if params.iter().any(|(_, pname, _)| pname == base)
                                            && !member.is_empty()
                                        {
                                            format!("{}.get{}()", base, capitalize(member))
                                        } else {
                                            e.transpile(expr_node)
                                        }
                                    } else if expr_node.kind() == "call" {
                                        e.transpile(expr_node)
                                    } else {
                                        self.text(expr_node).trim().to_string()
                                    };
                                    let head = rendered.split('(').next().unwrap_or("");
                                    let rendered = if rendered.ends_with(')')
                                        && head
                                            .chars()
                                            .next()
                                            .is_some_and(|c| c.is_ascii_uppercase())
                                        && !rendered.starts_with("new ")
                                    {
                                        format!("new {}", rendered)
                                    } else {
                                        rendered
                                    };
                                    args_collected.push(rendered);
                                }
                            }
                            if !args_collected.is_empty() {
                                superclass = Some(t_j.clone());
                                super_ctor_args = Some(args_collected.join(", "));
                            }
                        }
                    }
                    // `: Greeter by Parent2()` — delegation: implement the
                    // interface; the `by` delegate is approximated (warned).
                    "explicit_delegation" => {
                        if let Some(ut) = inner
                            .children(&mut inner.walk())
                            .find(|c| c.kind() == "user_type")
                        {
                            parts.push(format!("iface:{}", self.text(ut).replace(" ", "")));
                        }
                        self.diag_approx(
                            inner,
                            "interface delegation `by` has no Java counterpart; emitted as plain implements",
                        );
                    }
                    // `: Greeter` — bare supertype; can't tell class vs
                    // interface without cross-file metadata, assume interface
                    "user_type" | "nullable_type" => {
                        let t = self.text(inner).replace(" ", "");
                        if !t.starts_with("@") {
                            parts.push(format!("iface:{}", t));
                        }
                    }
                    other => {
                        self.diag_untranslatable(
                            inner,
                            format!("supertype form not supported: {}", other),
                        );
                    }
                }
            }
            if !parts.is_empty() {
                // constructor_invocation => extends; everything else =>
                // implements (a class superclass must come first, which the
                // Kotlin grammar guarantees).
                let classes: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| p.strip_prefix("class:"))
                    .collect();
                let ifaces: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| p.strip_prefix("iface:"))
                    .collect();
                if let Some(c) = classes.first() {
                    superclass = Some(c.to_string());
                }
                let mut j = String::new();
                if let Some(c) = classes.first() {
                    j.push_str(&format!(" extends {c}"));
                }
                if !ifaces.is_empty() {
                    if is_interface {
                        j.push_str(&format!(" extends {}", ifaces.join(", ")));
                    } else {
                        j.push_str(&format!(" implements {}", ifaces.join(", ")));
                    }
                }
                if !j.is_empty() {
                    extends = j;
                }
            }
        }

        // Sealed classes: Java needs a `permits` clause for direct subclasses
        // that land in other files; same-file nested subclasses share this
        // compilation unit and need none. With no in-file subclasses at all,
        // `sealed` has no legal Java form — fall back to a plain class.
        let mut permits = String::new();
        if is_sealed {
            let subs = self
                .subclass_map
                .get(name.as_str())
                .cloned()
                .unwrap_or_default();
            if subs.is_empty() {
                modifiers = modifiers.replace("sealed ", "");
                self.diag_approx(
                    decl,
                    format!(
                        "sealed class '{}': no subclasses declared in this file; emitted as a plain class (sealed restriction lost)",
                        name
                    ),
                );
            } else {
                let top: Vec<String> = subs
                    .iter()
                    .filter(|s| !self.decl_contains(decl, s))
                    .cloned()
                    .collect();
                if !top.is_empty() {
                    permits = format!(" permits {}", top.join(", "));
                }
            }
        }

        let kind_word = if is_interface {
            "interface"
        } else if is_data {
            "record"
        } else {
            "class"
        };

        if is_interface {
            // Interfaces declared type parameters too (`interface
            // IActivityUpdatedEvent<T : Activity>`) — emit them or the
            // `T` references in extends/implies clauses won't resolve.
            // Java-native declaration annotations pass through verbatim
            // (collected during the modifiers scan above).
            for annotation in &annotations {
                out.line(annotation.clone());
            }
            out.open(format!(
                "{}{}interface {}{}{}",
                visibility, modifiers, name, type_params, extends
            ));
            if let Some(body) = kt::child(decl, "class_body") {
                let mut cursor = body.walk();
                for member in body.children(&mut cursor) {
                    match member.kind() {
                        "function_declaration" => {
                            self.transpile_function(member, true, out);
                            out.blank();
                        }
                        "property_declaration" => {
                            // interface property: abstract accessor unless a
                            // getter/setter body is declared -> default method
                            let vd = kt::child(member, "variable_declaration");
                            if let Some(vd) = vd {
                                let ident = kt::child(vd, "identifier");
                                if let Some(ident) = ident {
                                    let pname = self.text(ident).to_string();
                                    let pty = kt::child(vd, "user_type")
                                        .or_else(|| kt::child(vd, "nullable_type"))
                                        .map(|t| kt::java_type_ann(t, self.source, self.annots))
                                        .unwrap_or_else(|| "Object".to_string());
                                    let cap = capitalize(&pname);
                                    let getter = kt::child(member, "getter");
                                    match getter.and_then(|g| kt::child(g, "function_body")) {
                                        Some(gb) => {
                                            // Fresh scope for the accessor body:
                                            // locals must not leak into later
                                            // interface members.
                                            self.var_types.clear();
                                            out.open(format!("default {} get{}()", pty, cap));
                                            self.transpile_function_body(gb, out);
                                            out.close();
                                        }
                                        None => out.line(format!("{} get{}();", pty, cap)),
                                    }
                                    if kt::child(member, "val").is_none() {
                                        let setter = kt::child(member, "setter");
                                        match setter.and_then(|s| kt::child(s, "function_body")) {
                                            Some(sb) => {
                                                self.var_types.clear();
                                                out.open(format!(
                                                    "default void set{}({} value)",
                                                    cap, pty
                                                ));
                                                self.transpile_function_body(sb, out);
                                                out.close();
                                            }
                                            None => {
                                                out.line(format!("void set{}({} value);", cap, pty))
                                            }
                                        }
                                    }
                                    out.blank();
                                }
                            }
                        }
                        ";" | "{" | "}" => {}
                        _ => {
                            if member.is_named() {
                                self.diag_untranslatable(
                                    member,
                                    format!("interface member not supported: {}", member.kind()),
                                );
                            }
                        }
                    }
                }
            }
            out.close();
            return;
        }
        if is_data && !params.is_empty() && self.lombok {
            // --lombok: data class -> @Data class with mutable fields
            // (@Data generates equals/hashCode/toString/getters/setters;
            // var properties keep their setters semantically).
            out.line("@Data");
            out.line("@AllArgsConstructor");
            out.blank();
            // Java-native declaration annotations pass through verbatim
            // (collected during the modifiers scan above).
            for annotation in &annotations {
                out.line(annotation.clone());
            }
            let tp = type_params.trim_end();
            out.open(format!(
                "{}{}class {}{}{}",
                visibility, modifiers, name, tp, extends
            ));
            for (is_val, fname, ftype) in &params {
                // final val fields: @Data omits the setter automatically
                let final_kw = if *is_val { "final " } else { "" };
                out.line(format!("private {}{} {};", final_kw, ftype, fname));
            }
            for (_, fname, ftype) in &params {
                if ftype == "boolean" {
                    out.line(format!(
                        "public boolean get{}() {{ return {}; }}",
                        capitalize(fname),
                        fname
                    ));
                }
            }
            self.emit_jvm_overloads_primary_constructors(decl, &name, &params, out);
            out.blank();
            if let Some(body) = kt::child(decl, "class_body") {
                self.transpile_class_body(body, out);
            }
            out.close();
            return;
        }
        if is_data && !params.is_empty() {
            // record: parameters become record components. Records are
            // immutable — a data class with any `var` component loses setter
            // semantics, which is a semantic drop, so without --lombok the
            // declaration is TAINTED (stays in the .kt, warns N001) instead
            // of silently emitting a broken translation.
            if params.iter().any(|(is_val, _, _)| !*is_val) {
                // --lombok data-class path is handled above with an early
                // return, so `self.lombok` cannot be true here.
                debug_assert!(!self.lombok);
                if let Some(dn) = self.current_decl {
                    let label = self.decl_labels.get(&dn.id()).cloned().unwrap_or_default();
                    self.taint_decl(&label);
                }
                self.diag_untranslatable(
                    decl,
                    "data class has `var` components; Java records are immutable — use --lombok for a mutable @Data class",
                );
                return;
            }
            let comps: Vec<String> = params
                .iter()
                .map(|(_, n, t)| format!("{} {}", t, n))
                .collect();
            // Java records can't extend anything. A data class with a
            // superclass can't be a record — default to a final class with
            // explicit fields + accessors (signature-identical to the
            // record's: final fields, equals/hashCode/toString inherited or
            // approximated). With `extends` present, emit that form.
            // Type parameters must ride along on every shape — a record
            // `data class FindQ<T : IObj>(...)` needs `record FindQ<T>(...)`
            // or every `T` reference inside fails to resolve.
            let tp = type_params.trim_end(); // "<T> " / ""
            for annotation in &annotations {
                out.line(annotation.clone());
            }
            if extends.is_empty() {
                out.open(format!(
                    "{}record {}{}({})",
                    visibility,
                    name,
                    tp.trim_end(),
                    comps.join(", ")
                ));
            } else {
                let inner = if extends.is_empty() {
                    format!("{}final class {}{}", visibility, name, tp)
                } else {
                    // must nest inside `extends X` clause: nested class
                    // inheritance in one line
                    // static nested: no enclosing instance needed at `new` —
                    // but only legal INSIDE a class (top-level classes reject
                    // the `static` modifier).
                    let static_kw = if decl.parent().is_some_and(|p| p.kind() == "class_body") {
                        "static "
                    } else {
                        ""
                    };
                    format!(
                        "{}{}final class {}{} {}",
                        visibility, static_kw, name, tp, extends
                    )
                };
                out.open(inner);
                out.blank();
                for (is_val, fname, ftype) in &params {
                    let final_kw = if *is_val { "final " } else { "" };
                    out.line(format!("private {}{} {};", final_kw, ftype, fname));
                }
                out.blank();
                out.open(format!("public {}({})", name, comps.join(", ")));
                for (_, fname, _) in &params {
                    out.line(format!("this.{} = {};", fname, fname));
                }
                out.close();
                out.blank();
                for (_is_val, fname, ftype) in &params {
                    let cap = capitalize(fname);
                    out.open(format!("public {} get{}()", ftype, cap));
                    out.line(format!("return {};", fname));
                    out.close();
                }
                out.blank();
            }
            // record members: body content after the header (overrides etc.)
            if let Some(body) = kt::child(decl, "class_body") {
                let mut cursor = body.walk();
                for member in body.children(&mut cursor) {
                    if member.kind() == "function_declaration" {
                        out.blank();
                        self.transpile_function(member, false, out);
                    } else if member.kind() == "property_declaration" {
                        // Custom properties inside a data class: backing
                        // field + accessors are legal record members — do
                        // NOT taint the record for these.
                        out.blank();
                        self.transpile_property(member, out);
                    } else if member.is_named() && !matches!(member.kind(), ";" | "{" | "}") {
                        self.diag_untranslatable(
                            member,
                            format!("record member not supported: {}", member.kind()),
                        );
                    }
                }
            }
            out.close();
        } else {
            // Java places type params after the class name: `class Name<T>`.
            let tp = type_params.trim_end(); // "<T>" or "" (no space needed before '{')
            // Subclasses of a file-sealed type must be final in Java (Kotlin
            // classes are final by default unless open/abstract/sealed).
            let final_kw = if is_sealed || modifiers.contains("abstract") {
                ""
            } else if let Some(sp) = &superclass {
                if self.sealed_types.contains(sp) {
                    "final "
                } else {
                    ""
                }
            } else {
                ""
            };
            // --lombok: hand-rolled accessors/equals/hashCode/toString become
            // Lombok annotations placed BEFORE the class declaration.
            if self.lombok && !params.is_empty() {
                out.line("@Data");
                // @AllArgsConstructor's synthesized ctor collides with the
                // explicit super-forwarding ctor emitted below — only
                // annotate when the explicit one is not being written.
                if super_ctor_args.is_none() {
                    out.line("@AllArgsConstructor");
                }
                out.blank();
            }
            // Java-native declaration annotations pass through verbatim
            // (collected during the modifiers scan above).
            for annotation in &annotations {
                out.line(annotation.clone());
            }
            out.open(format!(
                "{}{}{}{} {}{}{}{}",
                visibility, final_kw, modifiers, kind_word, name, tp, extends, permits
            ));
            // fields (final for val: @Data skips the setter on a final field)
            for (is_val, fname, ftype) in &params {
                let final_kw = if *is_val { "final " } else { "" };
                out.line(format!("private {}{} {};", final_kw, ftype, fname));
            }
            if !params.is_empty() {
                out.blank();
            }
            // constructor (redundant under --lombok: AllArgsConstructor)
            if let Some(sargs) = &super_ctor_args {
                // superclass ctor needs arguments: emit an explicit ctor
                // forwarding them (`: Parent("template")` -> super("template")).
                // @AllArgsConstructor's generated ctor cannot express the
                // super-call, so the explicit one is required regardless.
                out.open(format!("public {}({})", name, {
                    params
                        .iter()
                        .map(|(_, n, t)| format!("{} {}", t, n))
                        .collect::<Vec<_>>()
                        .join(", ")
                }));
                out.line(format!("super({});", sargs));
                for (_, fname, _) in &params {
                    out.line(format!("this.{} = {};", fname, fname));
                }
                out.close();
                out.blank();
            }
            if !params.is_empty() && !self.lombok && super_ctor_args.is_none() {
                out.open(format!("public {}({})", name, {
                    params
                        .iter()
                        .map(|(_, n, t)| format!("{} {}", t, n))
                        .collect::<Vec<_>>()
                        .join(", ")
                }));
                for (_, fname, _) in &params {
                    out.line(format!("this.{} = {};", fname, fname));
                }
                out.close();
                out.blank();
            }
            self.emit_jvm_overloads_primary_constructors(decl, &name, &params, out);
            // accessors (skipped under --lombok: @Data generates them)
            if !self.lombok {
                for (is_val, fname, ftype) in &params {
                    let cap = capitalize(fname);
                    out.open(format!("public {} get{}()", ftype, cap));
                    out.line(format!("return {};", fname));
                    out.close();
                    if !*is_val {
                        out.blank();
                        out.open(format!("public void set{}({} {})", cap, ftype, fname));
                        out.line(format!("this.{} = {};", fname, fname));
                        out.close();
                    }
                    out.blank();
                }
            } // body members
            if let Some(body) = kt::child(decl, "class_body") {
                self.transpile_class_body(body, out);
            }
            out.close();
        }
    }

    fn transpile_annotation_decl(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        visibility: &str,
        out: &mut JavaOut,
    ) {
        let params = self.class_params(decl);
        let defaults = self.class_param_defaults(decl);
        out.open(format!("{visibility}@interface {name}"));
        for ((_, param_name, param_type), default) in params.into_iter().zip(defaults) {
            let suffix = default
                .map(|value| format!(" default {value}"))
                .unwrap_or_default();
            out.line(format!("{param_type} {param_name}(){suffix};"));
        }
        out.close();
    }

    /// Primary constructor parameters -> (is_val, name, java_type). Shared by
    /// the class/enum/record paths so ctor-param handling stays in one place.
    fn class_params(&mut self, decl: tree_sitter::Node) -> Vec<(bool, String, String)> {
        kt::child(decl, "primary_constructor")
            .and_then(|pc| kt::child(pc, "class_parameters"))
            .map(|cps| {
                let mut cursor = cps.walk();
                cps.children(&mut cursor)
                    .filter(|c| c.kind() == "class_parameter")
                    .filter_map(|cp| {
                        let is_val = kt::child(cp, "val").is_some();
                        let ident = kt::child(cp, "identifier")?;
                        let ty = kt::child(cp, "user_type")
                            .or_else(|| kt::child(cp, "nullable_type"))
                            .or_else(|| kt::child(cp, "function_type"))
                            .or_else(|| kt::child(cp, "parenthesized_type"));
                        let ty_java = match ty {
                            Some(t) => {
                                let t_java = kt::java_type_ann(t, self.source, self.annots);
                                if t_java == crate::transpiler::types::FUNCTION_TYPE_PLACEHOLDER {
                                    self.diag_untranslatable(
                                        t,
                                        format!(
                                            "function type on param '{}' has no Java counterpart (functional-interface mapping not implemented)",
                                            self.text(ident)
                                        ),
                                    );
                                    "Object".to_string()
                                } else {
                                    t_java
                                }
                            }
                            None => {
                                // Param with unrecognized type shape: flag it
                                // instead of silently dropping the field.
                                self.diags.push(crate::diagnostics::Diagnostic {
                                    severity: crate::diagnostics::Severity::Warning,
                                    kind: DiagnosticKind::Approximated,
                                    message: format!(
                                        "primary-ctor param '{}' has unsupported type shape ({}); emitted as Object",
                                        self.text(ident),
                                        cp.kind()
                                    ),
                                    file: self.file.to_path_buf(),
                                    line: cp.start_position().row + 1,
                                    col: cp.start_position().column + 1,
                                });
                                "Object".to_string()
                            }
                        };
                        Some((is_val, self.text(ident).to_string(), ty_java))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Transpiled default expressions for primary-ctor parameters, aligned
    /// by index (`None` where the parameter has no default). Used to fill
    /// enum-constant call sites, since Java has no default arguments.
    fn class_param_defaults(&mut self, decl: tree_sitter::Node) -> Vec<Option<String>> {
        kt::child(decl, "primary_constructor")
            .and_then(|pc| kt::child(pc, "class_parameters"))
            .map(|cps| {
                let mut cursor = cps.walk();
                cps.children(&mut cursor)
                    .filter(|c| c.kind() == "class_parameter")
                    .map(|cp| {
                        // The grammar nests the default as whatever
                        // expression follows the `=` child (no dedicated
                        // wrapper node). Only params that HAVE a `=`
                        // contribute an entry; the last named child is the
                        // default expression.
                        let has_default = cp
                            .children(&mut cp.walk())
                            .any(|c| !c.is_named() && c.kind() == "=");
                        if has_default {
                            cp.children(&mut cp.walk())
                                .filter(|c| c.is_named())
                                .last()
                                .map(|ex| {
                                    let mut e = Expr { unit: self };
                                    e.transpile(ex)
                                })
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `@JvmOverloads` exposes Java overloads for every omitted trailing
    /// primary-constructor default. The Kotlin annotation itself has no Java
    /// counterpart, so emit the overloads that express its ABI instead.
    fn emit_jvm_overloads_primary_constructors(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        params: &[(bool, String, String)],
        out: &mut JavaOut,
    ) {
        let has_jvm_overloads = kt::child(decl, "primary_constructor")
            .and_then(|constructor| kt::child(constructor, "modifiers"))
            .is_some_and(|modifiers| {
                modifiers
                    .children(&mut modifiers.walk())
                    .filter(|node| node.kind() == "annotation")
                    .any(|annotation| self.text(annotation).contains("JvmOverloads"))
            });
        if !has_jvm_overloads || params.is_empty() {
            return;
        }

        // Kotlin primary-constructor default expressions run before an instance
        // exists. Seed the constructor parameters as locals while lowering so
        // `label` stays `label`, rather than becoming `this.getLabel()`.
        let prior_var_types = std::mem::replace(
            &mut self.var_types,
            params
                .iter()
                .map(|(_, param_name, param_type)| (param_name.clone(), param_type.clone()))
                .collect(),
        );
        let defaults = self.class_param_defaults(decl);
        self.var_types = prior_var_types;
        let trailing_defaults = defaults
            .iter()
            .rev()
            .take_while(|value| value.is_some())
            .count();
        if trailing_defaults == 0 {
            return;
        }
        let first_omitted = params.len() - trailing_defaults;
        for omitted in 1..=trailing_defaults {
            let kept = params.len() - omitted;
            let signature = params[..kept]
                .iter()
                .map(|(_, param_name, param_type)| format!("{} {}", param_type, param_name))
                .collect::<Vec<_>>()
                .join(", ");
            let values = params[..kept]
                .iter()
                .map(|(_, param_name, _)| param_name.clone())
                .chain(
                    defaults[kept..]
                        .iter()
                        .map(|value| value.clone().expect("trailing constructor default")),
                )
                .collect::<Vec<_>>()
                .join(", ");
            debug_assert!(kept >= first_omitted);
            out.blank();
            out.open(format!("public {}({})", name, signature));
            out.line(format!("this({});", values));
            out.close();
        }
    }

    /// `enum class` -> native Java enum. Constants become enum constants;
    /// primary-ctor params (`val rgb: Int`) become private final fields +
    /// accessors + a private constructor; body members (functions, properties
    /// incl. `get() =` accessor shapes) become enum members.
    ///
    /// Shapes beyond javac's capability TAINT with N001 instead of emitting
    /// broken Java: generic enums, enum superclass delegation (Java enums
    /// implicitly extend Enum), abstract/bodyless enum methods (they require
    /// per-constant bodies the grammar can't even parse), and class modifiers
    /// like `sealed`. Plain supertypes are fine — Java enums may implement
    /// interfaces.
    #[allow(clippy::too_many_arguments)]
    fn transpile_enum(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        visibility: &str,
        modifiers: &str,
        is_sealed: bool,
        annotations: &[String],
        out: &mut JavaOut,
    ) {
        self.enum_types.insert(name.to_string());
        // Only visibility + (implicitly final) enum is legal Java. Any other
        // class modifier (sealed/abstract/open) on an enum taints.
        let extra = modifiers.trim();
        if !extra.is_empty() || is_sealed {
            self.diag_untranslatable(
                decl,
                format!(
                    "enum '{}' has class modifier(s) '{}' that Java enums cannot express",
                    name, extra
                ),
            );
            return;
        }
        // Java enums cannot be generic.
        if let Some(tp) = kt::child(decl, "type_parameters") {
            self.diag_untranslatable(
                tp,
                format!(
                    "enum '{}' declares type parameters; Java enums cannot be generic",
                    name
                ),
            );
            return;
        }
        // Supertypes: constructor_invocation would mean `extends` — illegal
        // for a Java enum (implicit Enum). Bare/`by` supertypes are
        // interfaces and can be `implements`.
        let mut implements = String::new();
        if let Some(dc) = kt::child(decl, "delegation_specifiers") {
            let mut ifaces: Vec<String> = Vec::new();
            let mut cursor = dc.walk();
            for spec in dc.children(&mut cursor) {
                if spec.kind() != "delegation_specifier" {
                    continue;
                }
                let inner = spec.children(&mut spec.walk()).find(|c| c.is_named());
                match inner.map(|n| n.kind()) {
                    Some("constructor_invocation") => {
                        self.diag_untranslatable(
                            spec,
                            format!(
                                "enum '{}' extends a superclass; Java enums implicitly extend java.lang.Enum and cannot extend another class",
                                name
                            ),
                        );
                        return;
                    }
                    Some("user_type") | Some("nullable_type") | Some("explicit_delegation") => {
                        let t = self
                            .text(inner.unwrap())
                            .replace(" ", "")
                            .trim_start_matches('@')
                            .to_string();
                        if !t.is_empty() {
                            ifaces.push(t);
                        }
                    }
                    other => {
                        self.diag_untranslatable(
                            spec,
                            format!(
                                "enum '{}' supertype form not supported: {}",
                                name,
                                other.unwrap_or("?")
                            ),
                        );
                        return;
                    }
                }
            }
            if !ifaces.is_empty() {
                implements = format!(" implements {}", ifaces.join(", "));
            }
        }

        let params = self.class_params(decl);
        // Register ctor-param names/types so bodies (`rgb.toString(16)`) get
        // primitive-receiver rewrites and type inference like class fields do.
        // `pending_field_types` re-seeds them inside each member's scope
        // (transpile_function clears var_types per declaration).
        self.pending_field_types = params
            .iter()
            .map(|(_, fname, ftype)| (fname.clone(), ftype.clone()))
            .collect();
        // Defaulted ctor params: Java enum constants must pass every
        // trailing argument; a constant that omits a defaulted param gets
        // the default expression transpiled in.
        let param_defaults = self.class_param_defaults(decl);
        let body = kt::child(decl, "enum_class_body").or_else(|| kt::child(decl, "class_body"));

        // Split the body: constants first (Java requires them before any
        // member), then `;`, then members in declaration order.
        let mut entries: Vec<String> = Vec::new();
        let mut members: Vec<tree_sitter::Node> = Vec::new();
        if let Some(body) = body {
            let mut cursor = body.walk();
            for member in body.children(&mut cursor) {
                match member.kind() {
                    "enum_entry" => {
                        let mut e = String::new();
                        let mut has_class_body = false;
                        let mut ec = member.walk();
                        for c in member.children(&mut ec) {
                            match c.kind() {
                                "identifier" => e.push_str(self.text(c)),
                                "class_body" => {
                                    // Per-constant override body: Java enum
                                    // constants accept `NAME { ... }`.
                                    has_class_body = true;
                                    let mut bc = JavaOut::new();
                                    bc.open("");
                                    self.transpile_class_body(c, &mut bc);
                                    bc.close();
                                    e.push_str(&format!(" {}", bc.finish().trim()));
                                }
                                "value_arguments" => {
                                    let mut args: Vec<String> = Vec::new();
                                    let mut ac = c.walk();
                                    for arg in c.children(&mut ac) {
                                        if arg.kind() != "value_argument" {
                                            continue;
                                        }
                                        if let Some(ex) =
                                            arg.children(&mut arg.walk()).find(|x| x.is_named())
                                        {
                                            let mut e2 = Expr { unit: self };
                                            args.push(e2.transpile(ex));
                                        }
                                    }
                                    // Fill omitted defaulted parameters so
                                    // the call site compiles in Java.
                                    while args.len() < param_defaults.len() {
                                        match &param_defaults[args.len()] {
                                            Some(default_expr) => {
                                                args.push(default_expr.clone());
                                            }
                                            None => break,
                                        }
                                    }
                                    e.push_str(&format!("({})", args.join(", ")));
                                }
                                _ => {}
                            }
                        }
                        let _ = has_class_body;
                        entries.push(e);
                    }
                    "line_comment" | "block_comment" | ";" => {}
                    "function_declaration"
                    | "property_declaration"
                    | "class_declaration"
                    | "object_declaration"
                    | "anonymous_initializer"
                    | "secondary_constructor"
                    | "companion_object" => {
                        if member.is_named() {
                            members.push(member);
                        }
                    }
                    _ => {
                        if member.is_named() {
                            self.diag_untranslatable(
                                member,
                                format!("enum member not supported: {}", member.kind()),
                            );
                        }
                    }
                }
            }
        }
        if entries.is_empty() {
            self.diag_untranslatable(
                decl,
                format!(
                    "enum '{}' declares no constants; Java enums require at least one",
                    name
                ),
            );
            return;
        }

        // Bodyless (abstract) enum methods are fine ONLY when every entry
        // carries an override body (checked after entries are built).
        let abstract_fns: Vec<String> = members
            .iter()
            .filter(|m| {
                m.kind() == "function_declaration" && kt::child(**m, "function_body").is_none()
            })
            .map(|m| {
                kt::field(*m, "name")
                    .map(|n| self.text(n).to_string())
                    .unwrap_or_else(|| "?".to_string())
            })
            .collect();
        if !abstract_fns.is_empty() {
            // Every constant must implement each abstract method.
            let bodies_ok = entries.iter().all(|e| e.contains("{"));
            if !bodies_ok {
                self.diag_untranslatable(
                    decl,
                    format!(
                        "enum '{}' declares abstract method(s) {} but not every constant has an override body",
                        name,
                        abstract_fns.join(", ")
                    ),
                );
                return;
            }
        }

        let emit_entries_bridge = self
            .workspace
            .is_some_and(|workspace| workspace.has_enum_entries_consumer(name));
        // Java-native declaration annotations pass through verbatim
        // (collected during the modifiers scan above).
        for annotation in annotations {
            out.line(annotation.clone());
        }
        out.open(format!("{}enum {}{}", visibility, name, implements));
        // constants
        out.line(entries.join(",\n"));
        if !members.is_empty() || !params.is_empty() || emit_entries_bridge {
            out.line(";");
        }
        if emit_entries_bridge {
            out.blank();
            out.open(format!(
                "public static kotlin.enums.EnumEntries<{}> getEntries()",
                name
            ));
            out.line("return kotlin.enums.EnumEntriesKt.enumEntries(values());");
            out.close();
        }
        // ctor params -> fields + accessors + private ctor
        if !params.is_empty() {
            out.blank();
            for (is_val, fname, ftype) in &params {
                let final_kw = if *is_val { "final " } else { "" };
                out.line(format!("private {}{} {};", final_kw, ftype, fname));
            }
            out.blank();
            for (is_val, fname, ftype) in &params {
                let cap = capitalize(fname);
                out.open(format!("public {} get{}()", ftype, cap));
                out.line(format!("return {};", fname));
                out.close();
                if !*is_val {
                    out.blank();
                    out.open(format!("public void set{}({} {})", cap, ftype, fname));
                    out.line(format!("this.{} = {};", fname, fname));
                    out.close();
                }
                out.blank();
            }
            // Kotlin enum constructors are private; Java requires the same.
            out.open(format!(
                "private {}({})",
                name,
                params
                    .iter()
                    .map(|(_, n, t)| format!("{} {}", t, n))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            for (_, fname, _) in &params {
                out.line(format!("this.{} = {};", fname, fname));
            }
            out.close();
            out.blank();
        }
        // body members
        for m in members {
            match m.kind() {
                "function_declaration" => {
                    self.transpile_function(m, true, out);
                    out.blank();
                }
                "property_declaration" => {
                    self.transpile_property(m, out);
                    out.blank();
                }
                "class_declaration" | "object_declaration" => {
                    self.transpile_type_decl(m, out);
                    out.blank();
                }
                _ => {
                    if m.is_named() {
                        self.diag_untranslatable(
                            m,
                            format!("enum member not supported: {}", m.kind()),
                        );
                    }
                }
            }
        }
        self.pending_field_types.clear();
        out.close();
    }

    /// `companion object { ... }` -> static members of the enclosing class.
    ///
    /// Kotlin companion member call sites (`Outer.member`) resolve the same
    /// way as Java statics, so functions become `static` methods, `val`
    /// properties become `static final` fields (with accessors), and `var`
    /// properties become static fields + accessors with an N002 approximation
    /// warning (Kotlin keeps companion state on the Companion singleton, not
    /// as class statics). Named companions and companions holding init logic
    /// or other non-member constructs TAINT with N001 — there is no Java
    /// shape that preserves reachability (`Outer.Factory` vs statics).
    fn transpile_companion(
        &mut self,
        companion: tree_sitter::Node,
        owner: &str,
        out: &mut JavaOut,
    ) {
        // Named companion: `companion object Foo { ... }` — Kotlin reaches
        // members via `Outer.Foo.member`; statics on Outer would change the
        // call path. Taint rather than silently rename.
        let named = companion
            .children(&mut companion.walk())
            .any(|c| c.kind() == "identifier");
        if named {
            self.diag_untranslatable(
                companion,
                "named companion object has no Java counterpart (callers use Outer.CompanionName.member; cannot be redirected to Outer statics)",
            );
            return;
        }
        let Some(body) = kt::child(companion, "class_body") else {
            return;
        };
        let mut mutable_state = false;
        let mut bridge_sigs: Vec<BridgeSig> = Vec::new();
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            match member.kind() {
                "property_declaration" => {
                    if kt::child(member, "var").is_some() {
                        mutable_state = true;
                    }
                    if let Some(vd) = kt::child(member, "variable_declaration")
                        && let Some(n) = kt::child(vd, "identifier")
                    {
                        let pname = self.text(n).to_string();
                        let cap: String = capitalize(&pname);
                        self.companion_members
                            .insert(pname.clone(), format!("get{}()", cap));
                    }
                    self.transpile_property_opts(member, out, true, Some(owner));
                    out.blank();
                }
                "function_declaration" => {
                    // Capture the Java signature from the source AST before
                    // translating, for the nested Companion bridge.
                    if let Some(sig) = self.companion_fn_signature(member) {
                        bridge_sigs.push(sig);
                    }
                    self.transpile_function_opts(
                        member, true, /*make_static=*/ true, false, out,
                    );
                    out.blank();
                }
                // Trivia nodes are named by tree-sitter but are not companion
                // members and must not make an otherwise valid companion fail.
                "line_comment" | "block_comment" | ";" | "{" | "}" => {}
                _ => {
                    if member.is_named() {
                        self.diag_untranslatable(
                            member,
                            format!("companion object member not supported: {}", member.kind()),
                        );
                    }
                }
            }
        }
        if mutable_state {
            self.diag_approx(
                companion,
                "companion object state (var members) approximated as static fields of the enclosing class — Kotlin stores companion state on the Companion singleton instance; API shape preserved, storage location approximated",
            );
        }
        if !bridge_sigs.is_empty() {
            self.diag_approx(
                companion,
                "companion fns also reachable via Owner.Companion.fn(...) in Kotlin call sites; emitted a nested Companion bridge that delegates to the class statics so both call forms compile",
            );
            out.blank();
            out.open(String::from("public static final class Companion"));
            for sig in &bridge_sigs {
                out.open(format!("public {}", sig.signature));
                out.line(format!("return {}.{};", owner, sig.call));
                out.close();
            }
            out.close();
            out.blank();
        }
    }

    /// Java signature + delegate call captured from a companion fn's source
    /// AST (`static Ret fn(Type p, ...)` / `fn(p, ...)`). Params with default
    /// expressions are skipped: the bridge assumes the full argument list.
    fn companion_fn_signature(&mut self, member: tree_sitter::Node<'_>) -> Option<BridgeSig> {
        let name = kt::field(member, "name")?;
        let fn_name = self.text(name).to_string();
        let params_node = kt::child(member, "function_value_parameters")?;
        let mut cursor = params_node.walk();
        let mut args = Vec::new();
        for param in params_node.children(&mut cursor) {
            if param.kind() != "parameter" {
                continue;
            }
            let param_children: Vec<_> = param.children(&mut param.walk()).collect();
            if param_children
                .iter()
                .any(|c| !c.is_named() && c.kind() == "=")
            {
                return None;
            }
            let pname = param_children
                .iter()
                .find(|c| c.kind() == "identifier")
                .map(|c| self.text(*c).to_string())
                .unwrap_or_else(|| format!("p{}", args.len()));
            let ptype_node = param_children.iter().find(|c| {
                matches!(
                    c.kind(),
                    "user_type" | "nullable_type" | "type_reference" | "type"
                )
            })?;
            let ptype = kt::java_type_ann(*ptype_node, self.source, self.annots);
            args.push(format!("{} {}", ptype, pname));
        }
        let ret = kt::child(member, "user_type")
            .or_else(|| kt::child(member, "type_reference"))
            .map(|t| kt::java_type_ann(t, self.source, self.annots))
            .unwrap_or_else(|| String::from("void"));
        let names: Vec<&str> = args
            .iter()
            .filter_map(|a| a.split(' ').next_back())
            .collect();
        Some(BridgeSig {
            signature: format!("static {ret} {fn_name}({})", args.join(", ")),
            call: format!("{fn_name}({})", names.join(", ")),
        })
    }

    fn transpile_object(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        visibility: &str,
        annotations: &[String],
        out: &mut JavaOut,
    ) {
        // Supertypes: `object Idle : State()` — the nested class must extend
        // the supertype or `instanceof Foo.El`/sealed membership fails.
        let mut obj_extends = String::new();
        if let Some(ds) = kt::child(decl, "delegation_specifiers") {
            let mut dcur = ds.walk();
            for spec in ds.children(&mut dcur) {
                if spec.kind() == "delegation_specifier" {
                    let sup = kt::child(spec, "super_type")
                        .or_else(|| spec.children(&mut spec.walk()).find(|c| c.is_named()));
                    if let Some(st) = sup {
                        let mut scur = st.walk();
                        let base = st.children(&mut scur).find(|c| c.is_named()).unwrap_or(st);
                        obj_extends = format!(
                            " extends {}",
                            kt::text(base, self.source).trim().replace(" ", "")
                        );
                    }
                }
            }
        }
        // `static` only legal for a NESTED class (inner inside class_body);
        // a top-level object is a plain public final class.
        let static_kw = if decl.parent().is_some_and(|p| p.kind() == "class_body") {
            "static "
        } else {
            ""
        };
        for annotation in annotations {
            out.line(annotation.clone());
        }
        out.open(format!(
            "{}{}final class {}{}",
            visibility, static_kw, name, obj_extends
        ));
        out.line(format!(
            "public static final {} INSTANCE = new {}();",
            name, name
        ));
        out.line(format!("private {}() {{}}", name));
        out.blank();
        // Self-references inside the body (`val self = Registry`) point at
        // the singleton in Java (`Registry.INSTANCE`).
        self.current_object = Some(name.to_string());
        if let Some(body) = kt::child(decl, "class_body") {
            let mut cursor = body.walk();
            for member in body.children(&mut cursor) {
                match member.kind() {
                    "function_declaration" => {
                        self.transpile_function_opts(member, true, true, false, out);
                        out.blank();
                    }
                    "property_declaration" => {
                        // object properties behave like static fields; record
                        // the accessor so `Cfg.TAG` call sites read via the
                        // generated getter (fields are private).
                        if let Some(vd) = kt::child(member, "variable_declaration")
                            && let Some(n) = kt::child(vd, "identifier")
                        {
                            let pname = self.text(n).to_string();
                            let cap: String = capitalize(&pname);
                            self.companion_members
                                .insert(pname, format!("get{}()", cap));
                        }
                        self.transpile_property_opts(member, out, true, Some(name));
                        out.blank();
                    }
                    "companion_object" => {
                        self.transpile_companion(member, name, out);
                        out.blank();
                    }
                    "secondary_constructor" => {
                        self.diag_untranslatable(
                            member,
                            "secondary constructors not yet supported",
                        );
                    }
                    ";" | "{" | "}" => {}
                    _ => {
                        if member.is_named() {
                            self.diag_untranslatable(
                                member,
                                format!("object member not supported: {}", member.kind()),
                            );
                        }
                    }
                }
            }
        }
        self.current_object = None;
        out.close();
    }

    /// Lower a secondary constructor with a direct `this(...)`/`super(...)`
    /// delegation and, optionally, a simple assignment-only body. Java requires
    /// the delegation call to be the constructor's first statement; bodies
    /// with control flow, calls, defaults, or varargs remain Kotlin until a
    /// wider flow and call-site analysis exists.
    fn transpile_secondary_constructor(
        &mut self,
        member: tree_sitter::Node,
        class_name: &str,
        out: &mut JavaOut,
    ) -> bool {
        let body = kt::child(member, "function_body").or_else(|| kt::child(member, "block"));
        if body.is_some_and(|body| !self.is_safe_secondary_constructor_body(body)) {
            return false;
        }
        let Some(delegation) = kt::child(member, "constructor_delegation_call") else {
            return false;
        };
        let delegation_target = if self.text(delegation).trim_start().starts_with("this") {
            "this"
        } else if self.text(delegation).trim_start().starts_with("super") {
            "super"
        } else {
            return false;
        };
        let Some(params_node) = kt::child(member, "function_value_parameters") else {
            return false;
        };
        let mut params = Vec::new();
        let prior_var_types = std::mem::take(&mut self.var_types);
        for param in params_node.children(&mut params_node.walk()) {
            if param.kind() != "parameter" {
                continue;
            }
            let children: Vec<_> = param.children(&mut param.walk()).collect();
            if children
                .iter()
                .any(|child| (!child.is_named() && child.kind() == "=") || child.kind() == "vararg")
            {
                self.var_types = prior_var_types;
                return false;
            }
            let Some(name_node) = children.iter().find(|child| child.kind() == "identifier") else {
                self.var_types = prior_var_types;
                return false;
            };
            let Some(type_node) = children.iter().find(|child| {
                matches!(
                    child.kind(),
                    "user_type" | "nullable_type" | "type_reference" | "type"
                )
            }) else {
                self.var_types = prior_var_types;
                return false;
            };
            let name = self.text(*name_node).to_string();
            let ty = kt::java_type_ann(*type_node, self.source, self.annots);
            self.var_types.insert(name.clone(), ty.clone());
            params.push((name, ty));
        }
        let Some(args_node) = kt::child(delegation, "value_arguments") else {
            self.var_types = prior_var_types;
            return false;
        };
        let mut args = Vec::new();
        for arg in args_node.children(&mut args_node.walk()) {
            if arg.kind() != "value_argument" || self.text(arg).contains('=') {
                if arg.kind() == "value_argument" {
                    self.var_types = prior_var_types;
                    return false;
                }
                continue;
            }
            let Some(value) = arg.children(&mut arg.walk()).find(|child| child.is_named()) else {
                self.var_types = prior_var_types;
                return false;
            };
            if !self.is_safe_secondary_constructor_argument(value) {
                self.var_types = prior_var_types;
                return false;
            }
            args.push(Expr { unit: self }.transpile(value));
        }
        out.open(format!(
            "public {}({})",
            class_name,
            params
                .iter()
                .map(|(name, ty)| format!("{} {}", ty, name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        out.line(format!("{}({});", delegation_target, args.join(", ")));
        if let Some(body) = body {
            let mut cursor = body.walk();
            for stmt in body.children(&mut cursor) {
                if stmt.is_named() {
                    self.transpile_statement(stmt, out);
                }
            }
        }
        self.var_types = prior_var_types;
        out.close();
        out.blank();
        true
    }

    /// A deliberately narrow constructor-body subset: assignments to fields
    /// on `this`, with a value that can be emitted unchanged in Java. This
    /// preserves Java's required first-statement delegation invariant without
    /// accepting control flow or Kotlin-specific call semantics.
    fn is_safe_secondary_constructor_body(&self, body: tree_sitter::Node) -> bool {
        if body.kind() != "block" {
            return false;
        }
        body.children(&mut body.walk())
            .filter(|stmt| stmt.is_named())
            .all(|stmt| {
                if stmt.kind() != "assignment" {
                    return false;
                }
                let named: Vec<_> = stmt
                    .children(&mut stmt.walk())
                    .filter(|child| child.is_named())
                    .collect();
                let (Some(target), Some(value)) = (named.first(), named.get(1)) else {
                    return false;
                };
                target.kind() == "navigation_expression"
                    && self.text(*target).trim_start().starts_with("this.")
                    && !self.text(*target).contains('(')
                    && self.is_safe_secondary_constructor_argument(*value)
            })
    }

    /// Delegated constructor arguments are deliberately narrower than the
    /// general expression lowerer. This path writes a constructor before the
    /// target build can validate it, so admit only values with a direct,
    /// syntax-preserving Java form.
    fn is_safe_secondary_constructor_argument(&self, node: tree_sitter::Node) -> bool {
        match node.kind() {
            "identifier" | "this_expression" | "number_literal" | "boolean_literal"
            | "hex_literal" | "long_literal" | "real_literal" | "null_literal" => true,
            "string_literal" => !self.text(node).contains('$'),
            // `Owner.VALUE` is valid Java. Call-shaped navigation is not:
            // it may hide a Kotlin extension or collection operation whose
            // lowering has not passed the constructor gate.
            "navigation_expression" => !self.text(node).contains('('),
            _ => false,
        }
    }

    fn transpile_class_body(&mut self, body: tree_sitter::Node, out: &mut JavaOut) {
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            match member.kind() {
                "function_declaration" => {
                    self.transpile_function(member, /*in_class=*/ true, out);
                    out.blank();
                }
                "property_declaration" => {
                    self.transpile_property(member, out);
                    out.blank();
                }
                "companion_object" => {
                    if let Some(cls) = kt::parent_of(body)
                        && let Some(name) = kt::field(cls, "name")
                    {
                        self.transpile_companion(member, self.text(name), out);
                    }
                    out.blank();
                }
                "secondary_constructor" => {
                    let class_name = kt::parent_of(body)
                        .and_then(|class| kt::field(class, "name"))
                        .map(|name| self.text(name).to_string())
                        .unwrap_or_default();
                    if !self.transpile_secondary_constructor(member, &class_name, out) {
                        self.diag_untranslatable(
                            member,
                            "secondary constructor is not a direct `this(...)` or `super(...)` delegation with an empty or simple assignment-only body",
                        );
                    }
                }
                "class_declaration" | "object_declaration" => {
                    self.transpile_type_decl(member, out);
                    out.blank();
                }
                ";" | "{" | "}" => {}
                _ => {
                    if member.is_named() {
                        self.diag_untranslatable(
                            member,
                            format!("class member not supported: {}", member.kind()),
                        );
                    }
                }
            }
        }
    }
}

/// After `@`-prefixing, Kotlin's auto-wrapped array form — unnamed top-level
/// nested annotation invocations (`@JsonSubTypes(T(...), T(...))`) — must
/// become a Java brace array literal (`{...}`) or javac rejects every element
/// with "annotation values must be of the form 'name=value'". Named elements
/// (`name = value`) and single simple values stay untouched.
fn brace_wrap_unnamed_nested_annotations(prefixed: &str) -> String {
    // The argument text arrives wrapped in its enclosing parentheses (from
    // `value_arguments` or the wrapper's parenthesized_expression) — strip
    // them so the top-level element scan sees the bare element list.
    let trimmed = prefixed.trim();
    let trimmed = if trimmed.starts_with('(') && trimmed.ends_with(')') {
        trimmed[1..trimmed.len() - 1].trim()
    } else {
        trimmed
    };
    // Already a brace array literal.
    if trimmed.starts_with('{') {
        return prefixed.to_string();
    }
    // Split the top-level elements at depth-0 commas.
    let mut elements: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for character in trimmed.chars() {
        match character {
            '(' => {
                depth += 1;
                current.push(character);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            '{' => {
                depth += 1;
                current.push(character);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 => {
                elements.push(current.clone());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    elements.push(current);
    // Wrap only when EVERY top-level element is a nested annotation
    // invocation (`@A.B(...)`): that is Kotlin's auto-wrapped array form
    // (a single one included). Any named element (`name = value`) or simple
    // value means the shape is already valid Java.
    let all_nested_annotations = elements
        .iter()
        .all(|element| element.trim_start().starts_with('@'));
    if !all_nested_annotations {
        return prefixed.to_string();
    }
    // Wrap the element list in braces, keeping the enclosing parentheses:
    // Java annotation array values are `@X({elem, elem})`.
    format!("({{{trimmed}}})")
}

/// Java requires the `@` prefix on nested annotation types inside annotation
/// arguments (`@JsonSubTypes.Type(value = X.class)`), while Kotlin omits it
/// (`JsonSubTypes.Type(value = X::class)`). Walk the argument text and prefix
/// `@` on every qualified identifier chain that starts a call — `A.B(` where
/// the chain is preceded by nothing, `(`, `,`, `=`, or whitespace and not
/// already `@`. Only qualified chains (containing `.`) are prefixed: a bare
/// `name(` inside annotation values is not a nested annotation shape.
fn prefix_nested_annotations(argument_text: &str) -> String {
    if !argument_text.contains('(') {
        return argument_text.to_string();
    }
    let mut result = String::with_capacity(argument_text.len() + 8);
    let mut token_start: Option<usize> = None;
    let bytes = argument_text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let character = bytes[index] as char;
        let is_identifier_byte = character.is_alphabetic()
            || character == '_'
            || (character.is_ascii_digit() && token_start.is_some());
        if is_identifier_byte || (character == '.' && token_start.is_some()) {
            if token_start.is_none() {
                token_start = Some(index);
            }
            result.push(character);
            index += 1;
            continue;
        }
        if character == '(' {
            if let Some(start) = token_start {
                let token = &argument_text[start..index];
                let already_annotated =
                    start > 0 && argument_text[..start].trim_end().ends_with('@');
                if token.contains('.') && !already_annotated {
                    let insert_at = result.len() - token.chars().count();
                    result.insert(insert_at, '@');
                }
                token_start = None;
            }
            result.push('(');
            index += 1;
            continue;
        }
        token_start = None;
        result.push(character);
        index += 1;
    }
    result
}
