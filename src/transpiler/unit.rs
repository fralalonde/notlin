//! Top-level compilation unit: package, imports, declarations.
use crate::diagnostics::{DiagnosticKind, Diagnostics, FileCoverage};
use crate::transpiler::expr::Expr;
use crate::transpiler::java::JavaOut;
use crate::transpiler::kt;
use crate::transpiler::stmt::Stmt;
use crate::transpiler::types::AnnotationSet;
use std::path::Path;

pub struct Unit<'a> {
    pub source: &'a str,
    pub file: &'a Path,
    pub diags: &'a mut Diagnostics,
    pub annots: AnnotationSet,
    pub untranslatable_as_error: bool,
    /// Per-file coverage: which declarations translated, which didn't.
    pub coverage: FileCoverage,
    /// Name of the declaration currently being translated; diagnostics raised
    /// while this is Some are attributed to it for in-place migration policy.
    current_decl: Option<tree_sitter::Node<'a>>,
    /// node-id -> label for open declarations.
    decl_labels: std::collections::HashMap<usize, String>,
    /// identifier name -> Java type (from params and local decls in the
    /// current translation scope). Array params/locals map to `String[]`,
    /// `int[]`, ... so callers can special-case `.size` -> `.length`.
    pub var_types: std::collections::HashMap<String, String>,
    /// Extension functions declared in this file as statics: fn name ->
    /// Java receiver type. Call sites `x.f(...)` are rewritten to the
    /// static form `f(x, ...)` when the callee lands in this map
    /// (same-file approximation; cross-file callers unaware).
    pub extension_fns: std::collections::HashMap<String, String>,
    /// Receiver parameter name while an extension function body is being
    /// translated (None elsewhere). `this` inside the body maps to it.
    pub ext_receiver_name: Option<String>,
    /// superclass simple name -> direct subclasses declared in this file.
    subclass_map: std::collections::HashMap<String, Vec<String>>,
    /// names of sealed class declarations in this file.
    sealed_types: std::collections::HashSet<String>,
}

impl<'a> Unit<'a> {
    pub fn new(
        source: &'a str,
        file: &'a Path,
        diags: &'a mut Diagnostics,
        annots: AnnotationSet,
        untranslatable_as_error: bool,
    ) -> Self {
        Self {
            source,
            file,
            diags,
            annots,
            untranslatable_as_error,
            coverage: FileCoverage::default(),
            current_decl: None,
            decl_labels: std::collections::HashMap::new(),
            var_types: std::collections::HashMap::new(),
            extension_fns: std::collections::HashMap::new(),
            ext_receiver_name: None,
            subclass_map: std::collections::HashMap::new(),
            sealed_types: std::collections::HashSet::new(),
        }
    }

    /// Mark `node` as the declaration under translation: any untranslatable
    /// diagnostic raised inside its subtree taints the whole declaration.
    fn begin_decl(&mut self, node: tree_sitter::Node<'a>, label: String) {
        self.current_decl = Some(node);
        self.coverage.translated.push(label.clone());
        self.decl_labels.insert(node.id(), label);
    }

    fn end_decl(&mut self) {
        // Record the translated span for in-place stripping.
        if let Some(node) = self.current_decl.take() {
            let label = self
                .decl_labels
                .get(&node.id())
                .cloned()
                .unwrap_or_default();
            let tainted = self.coverage.untranslated.contains(&label);
            if !tainted {
                self.coverage
                    .translated_spans
                    .push((node.start_byte(), node.end_byte()));
            } else {
                // stays in the .kt file; not counted as translated
                self.coverage.translated.retain(|l| l != &label);
            }
            self.decl_labels.remove(&node.id());
        }
    }

    /// Mark a declaration label as untranslated (must remain in the .kt file).
    fn taint_decl(&mut self, label: &str) {
        if !self.coverage.untranslated.iter().any(|l| l == label) {
            self.coverage.untranslated.push(label.to_string());
        }
        self.coverage.translated.retain(|l| l != label);
    }

    pub fn text<'t>(&self, node: tree_sitter::Node<'t>) -> &'t str
    where
        'a: 't,
    {
        kt::text(node, self.source)
    }

    pub fn diag_untranslatable(&mut self, node: tree_sitter::Node, msg: impl Into<String>) {
        let sev = if self.untranslatable_as_error {
            crate::diagnostics::Severity::Error
        } else {
            crate::diagnostics::Severity::Warning
        };

        // Taint the enclosing declaration (if any) so --in-place migration
        // knows this declaration must stay in the .kt file.
        {
            // Walk up through the AST: the nearest enclosing declaration node
            // that began via begin_decl (tracked by decl_labels). Handles both
            // top-level decls and members nested in class bodies.
            let mut ancestor = node.parent();
            while let Some(n) = ancestor {
                if self.decl_labels.contains_key(&n.id()) {
                    let label = self.decl_labels.get(&n.id()).cloned().unwrap_or_default();
                    self.taint_decl(&label);
                    break;
                }
                ancestor = n.parent();
            }
        }
        // Record a blocker stub (`// NOTLIN: <code> <message>`), anchored at
        // the diagnostic node's byte offset — migrate.rs emits these comments
        // in --in-place residue when the surrounding declaration is stripped.
        let message: String = msg.into();
        self.coverage.blockers.push((
            node.start_byte(),
            format!(
                "// NOTLIN: {} {}\n",
                DiagnosticKind::Untranslatable.code(),
                message
            ),
        ));

        self.diags.push(crate::diagnostics::Diagnostic {
            severity: sev,
            kind: DiagnosticKind::Untranslatable,
            message,
            file: self.file.to_path_buf(),
            line: node.start_position().row + 1,
            col: node.start_position().column + 1,
        });
    }

    fn diag_approx(&mut self, node: tree_sitter::Node, msg: impl Into<String>) {
        self.diags.push(crate::diagnostics::Diagnostic {
            severity: crate::diagnostics::Severity::Warning,
            kind: DiagnosticKind::Approximated,
            message: msg.into(),
            file: self.file.to_path_buf(),
            line: node.start_position().row + 1,
            col: node.start_position().column + 1,
        });
    }

    pub fn run(&mut self, root: tree_sitter::Node<'a>) -> Vec<(String, String)> {
        // Collect top-level structure
        let mut package = String::new();
        let mut imports: Vec<String> = Vec::new();
        let mut decls: Vec<tree_sitter::Node> = Vec::new();

        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            match child.kind() {
                "package_header" => {
                    if let Some(qi) = kt::child(child, "qualified_identifier") {
                        package = self.text(qi).replace(" ", "");
                    }
                }
                "import" => {
                    if let Some(qi) = kt::child(child, "qualified_identifier") {
                        imports.push(self.text(qi).replace(" ", ""));
                    }
                }
                "shebang_line" | ";" | "line_comment" | "multiline_comment" => {}
                k if k.contains("declaration")
                    || k == "object_declaration"
                    || k == "class_declaration"
                    || k == "function_declaration"
                    || k == "property_declaration" =>
                {
                    decls.push(child);
                }
                _ => {
                    self.diag_untranslatable(
                        child,
                        format!("top-level construct not supported: {}", child.kind()),
                    );
                }
            }
        }

        // Pre-pass: superclass -> subclass relations drive `permits` emission
        // for sealed classes and `final` on their subclasses.
        self.collect_type_relations(root);

        // Partition: named types each get their own file; loose functions and
        // top-level properties go into ONE file named after the .kt source.
        let mut files: Vec<(String, String)> = Vec::new();

        for decl in &decls {
            match decl.kind() {
                "class_declaration" | "object_declaration" => {
                    let type_name = kt::field(*decl, "name")
                        .map(|n| self.text(n).to_string())
                        .unwrap_or_else(|| "Anonymous".to_string());
                    let mut out = JavaOut::new();
                    let imports2 = imports.clone();
                    let package2 = package.clone();
                    self.begin_decl(*decl, type_name.clone());
                    self.transpile_type_decl_set(&mut out, &package2, &imports2, |unit, out| {
                        unit.transpile_type_decl(*decl, out);
                    });
                    self.end_decl();
                    if !self.coverage.untranslated.iter().any(|l| l == &type_name) {
                        files.push((format!("{}.java", type_name), out.finish()));
                    } else {
                        log::info!(
                            "skipping {}.java — declaration has untranslatables",
                            type_name
                        );
                    }
                }
                "function_declaration" | "property_declaration" => {
                    // accumulate for the file-level utility class below
                }
                _ => {}
            }
        }

        let loose: Vec<&tree_sitter::Node> = decls
            .iter()
            .filter(|d| matches!(d.kind(), "function_declaration" | "property_declaration"))
            .collect();

        if !loose.is_empty() {
            let mut file_class_name = self
                .file
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Main".to_string());
            file_class_name = file_class_name
                .chars()
                .map(|c| if c == '-' || c == '.' { '_' } else { c })
                .collect();
            let mut chars = file_class_name.chars();
            if let Some(first) = chars.next() {
                file_class_name = first.to_uppercase().collect::<String>() + chars.as_str();
            }

            let mut out = JavaOut::new();
            self.transpile_type_decl_set(&mut out, &package, &imports, |unit, out| {
                out.open(format!("public final class {}", file_class_name.clone()));
                out.line(format!("private {}() {{}}", file_class_name));
                out.blank();
                for decl in &loose {
                    let label = kt::field(**decl, "name")
                        .map(|n| unit.text(n).to_string())
                        .unwrap_or_else(|| "<anonymous>".to_string());
                    unit.begin_decl(**decl, label.clone());
                    match decl.kind() {
                        "function_declaration" => {
                            let is_main = kt::field(**decl, "name")
                                .map(|n| unit.text(n) == "main")
                                .unwrap_or(false);
                            unit.transpile_function_opts(**decl, true, true, is_main, out);
                            out.blank();
                        }
                        "property_declaration" => {
                            unit.transpile_toplevel_property(**decl, out);
                            out.blank();
                        }
                        _ => {}
                    }
                    unit.end_decl();
                }
                out.close();
            });
            // The file-level utility class is emitted only if at least one
            // loose declaration survived (clean members are in `translated`).
            let clean_count = loose
                .iter()
                .filter(|d| {
                    let label = kt::field(***d, "name")
                        .map(|n| self.text(n).to_string())
                        .unwrap_or_else(|| "<anonymous>".to_string());
                    !self.coverage.untranslated.iter().any(|l| l == &label)
                })
                .count();
            if clean_count > 0 {
                files.push((format!("{}.java", file_class_name), out.finish()));
            } else {
                log::info!("all top-level members untranslatable; skipping utility class");
            }
        }

        files
    }

    /// Pre-pass: walk the whole file for class declarations, recording
    /// superclass -> direct subclasses and the set of sealed type names.
    fn collect_type_relations(&mut self, root: tree_sitter::Node<'a>) {
        let mut stack: Vec<tree_sitter::Node<'a>> = vec![root];
        while let Some(n) = stack.pop() {
            for c in n.children(&mut n.walk()) {
                stack.push(c);
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
            if let Some(sup) = self.superclass_name(n) {
                self.subclass_map.entry(sup).or_default().push(tname);
            }
        }
    }

    /// Simple name of the direct superclass (constructor_invocation supertype).
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

    /// Whether `name` names a class declared somewhere inside `decl`'s subtree
    /// (a nested class — shares `decl`'s emitted file, no permits needed).
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

    fn transpile_type_decl_set(
        &mut self,
        out: &mut JavaOut,
        package: &str,
        imports: &[String],
        f: impl FnOnce(&mut Self, &mut JavaOut),
    ) {
        // Provenance header: every generated .java records which .kt produced
        // it (in-place migration trims the .kt, so the pair must stay matchable).
        out.line(format!(
            "// NOTLIN: generated from {} — do not edit by hand while the source .kt exists",
            self.file.display()
        ));
        if !package.is_empty() {
            out.line(format!("package {};", package));
            out.blank();
        }
        for imp in imports {
            out.line(format!("import {};", imp));
        }
        // The generated body uses ArrayList/HashMap/HashSet/List/Map/Set from
        // stdlib collections; java.util.* covers them all in one line.
        out.line("import java.util.*;");
        out.blank();
        if let Some(pkg) = crate::transpiler::types::nullable_import(self.annots) {
            out.line(format!("import {}.*;", pkg));
            out.blank();
        }
        f(self, out);
    }

    /// class / object (class-like) declarations
    fn transpile_type_decl(&mut self, decl: tree_sitter::Node, out: &mut JavaOut) {
        let mut is_data = false;
        let mut is_sealed = false;
        // Kotlin `interface` parses as class_declaration with an unnamed
        // `interface` keyword child.
        let is_interface = decl
            .children(&mut decl.walk())
            .any(|c| c.kind() == "interface");
        let mut modifiers = String::new();
        if let Some(mods) = kt::child(decl, "modifiers") {
            let mut cursor = mods.walk();
            for m in mods.children(&mut cursor) {
                match m.kind() {
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
                                "annotation" | "companion" | "enum" | "inline" | "value"
                                | "expect" | "actual" | "external" | "inner" | "fun" => {
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
        // visibility: Kotlin default = public; java default = package-private
        // We emit `public ` for Kotlin public (default) and nothing for private etc.
        let visibility = self.visibility_of(decl);

        let name = kt::field(decl, "name")
            .map(|n| self.text(n).to_string())
            .unwrap_or_else(|| "Anonymous".to_string());

        if decl.kind() == "object_declaration" {
            self.transpile_object(decl, &name, &visibility, out);
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
        let params: Vec<(bool, String, String)> = kt::child(decl, "primary_constructor")
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
            .unwrap_or_default();

        // superclass / interfaces
        // The grammar wraps each supertype in `delegation_specifier`
        // (constructor_invocation / explicit_delegation / user_type).
        let mut extends = String::new();
        let mut superclass: Option<String> = None;
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
                    // `: Parent()` — constructor invocation => class superclass
                    "constructor_invocation" => {
                        if let Some(ut) = inner
                            .children(&mut inner.walk())
                            .find(|c| c.kind() == "user_type")
                        {
                            parts.push(format!("class:{}", self.text(ut).replace(" ", "")));
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
                        self.diags.warn_approx(
                            inner,
                            self.file,
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
                    j.push_str(&format!(" implements {}", ifaces.join(", ")));
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
            out.open(format!(
                "{}{}interface {}{}",
                visibility, modifiers, name, extends
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
        if is_data && !params.is_empty() {
            // record: parameters become record components
            let comps: Vec<String> = params
                .iter()
                .map(|(_, n, t)| format!("{} {}", t, n))
                .collect();
            out.open(format!(
                "{}record {}({}){}",
                visibility,
                name,
                comps.join(", "),
                extends
            ));
            // record members: body content after the header (overrides etc.)
            if let Some(body) = kt::child(decl, "class_body") {
                let mut cursor = body.walk();
                for member in body.children(&mut cursor) {
                    if member.kind() == "function_declaration" {
                        out.blank();
                        self.transpile_function(member, false, out);
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
            out.open(format!(
                "{}{}{}{} {}{}{}{}",
                visibility, final_kw, modifiers, kind_word, name, tp, extends, permits
            ));
            // fields
            for (is_val, fname, ftype) in &params {
                let _ = is_val;
                out.line(format!("private {} {};", ftype, fname));
            }
            if !params.is_empty() {
                out.blank();
            }
            // constructor
            if !params.is_empty() {
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
            // accessors
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
            } // body members
            if let Some(body) = kt::child(decl, "class_body") {
                self.transpile_class_body(body, out);
            }
            out.close();
        }
    }

    /// object declaration -> final class with static INSTANCE
    fn transpile_object(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        visibility: &str,
        out: &mut JavaOut,
    ) {
        out.open(format!("{}final class {}", visibility, name));
        out.line(format!(
            "public static final {} INSTANCE = new {}();",
            name, name
        ));
        out.line(format!("private {}() {{}}", name));
        out.blank();
        if let Some(body) = kt::child(decl, "class_body") {
            let mut cursor = body.walk();
            for member in body.children(&mut cursor) {
                match member.kind() {
                    "function_declaration" => {
                        self.transpile_function_opts(member, true, true, false, out);
                        out.blank();
                    }
                    "property_declaration" => {
                        // object properties behave like static fields
                        self.transpile_toplevel_property(member, out);
                        out.blank();
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
        out.close();
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
                "secondary_constructor" => {
                    self.diag_untranslatable(member, "secondary constructors not yet supported");
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

    /// Extract Kotlin visibility; returns the Java modifier string (with trailing space) or "".
    fn visibility_of(&mut self, decl: tree_sitter::Node) -> String {
        if let Some(mods) = kt::child(decl, "modifiers") {
            let mut cursor = mods.walk();
            for m in mods.children(&mut cursor) {
                if m.kind() == "visibility_modifier" {
                    let v = self.text(m).replace(" ", "");
                    return match v.as_str() {
                        "private" => "private ".to_string(),
                        "protected" => "protected ".to_string(),
                        "internal" => {
                            self.diag_approx(
                                m,
                                "Kotlin `internal` approximated as package-private",
                            );
                            "".to_string()
                        }
                        // public is Kotlin's default; explicit public also
                        _ => "public ".to_string(),
                    };
                }
            }
        }
        "public ".to_string()
    }

    /// top-level function -> static method (inside the file utility class)
    fn transpile_function(&mut self, decl: tree_sitter::Node, in_class: bool, out: &mut JavaOut) {
        self.transpile_function_opts(decl, in_class, false, false, out)
    }

    /// `make_static`: for object members and top-level functions.
    /// `is_main`: Kotlin `fun main()` gets a String[] args parameter.
    fn transpile_function_opts(
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

    /// function_body = block | expression (expression-body)
    fn transpile_function_body(&mut self, fb: tree_sitter::Node, out: &mut JavaOut) {
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

    /// class-body property: private field + accessors (or annotated custom accessors)
    fn transpile_property(&mut self, decl: tree_sitter::Node, out: &mut JavaOut) {
        let is_val = kt::child(decl, "val").is_some();
        let vd = kt::child(decl, "variable_declaration");
        let name = vd
            .and_then(|v| kt::child(v, "identifier"))
            .map(|n| self.text(n).to_string())
            .unwrap_or_else(|| "prop".to_string());
        let ty = vd
            .and_then(|v| kt::child(v, "user_type").or_else(|| kt::child(v, "nullable_type")))
            .map(|t| kt::java_type_ann(t, self.source, self.annots));
        let _visibility = self.visibility_of(decl);

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
                            self.diags.warn_approx(
                                delim,
                                self.file,
                                format!(
                                    "`by lazy {{ ... }}` for '{}' emitted as `= null` + warning (lambda body not representable in field initializer); body reads: {}",
                                    name, self.text(body).trim()
                                ),
                            );
                        } else {
                            out.line(format!("private {} {} = {};", tyy, name, java));
                            self.diags.warn_approx(
                                delim,
                                self.file,
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
        let static_kw = if kt::child(decl, "modifiers")
            .map(|m| self.text(m).contains("const"))
            .unwrap_or(false)
        {
            "static final "
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
            out.open(format!("public {} get{}()", ty, cap));
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
                None => out.line(format!("return this.{};", name)),
            }
            out.close();
        }

        if !is_val && !has_setter_method {
            // Kotlin `private set`: the setter exists but is private.
            let set_vis = if setter
                .as_ref()
                .and_then(|s| kt::child(*s, "modifiers"))
                .map(|m| self.text(m).contains("private"))
                .unwrap_or(false)
            {
                "private "
            } else {
                "public "
            };
            out.blank();
            out.open(format!("{}void set{}({} {})", set_vis, cap, ty, name));
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
                None => out.line(format!("this.{} = {};", name, name)),
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

    /// Best-effort type inference from an initializer expression. Known
    /// receiver types (var_types + Map/List/Set shapes) propagate into the
    /// result for member reads and member calls (`m.keys` -> Set<K>,
    /// `xs.first()` -> element, `"abc".uppercase()` -> String). When a
    /// call/navigation initializer's type cannot be determined the local
    /// degrades to Object and N002 is raised (caller-visible approximation).
    pub fn infer_type(&mut self, expr: tree_sitter::Node) -> String {
        match expr.kind() {
            "identifier" => self
                .var_types
                .get(self.text(expr).trim())
                .cloned()
                .unwrap_or_else(|| "Object".to_string()),
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
            "boolean_literal" => "boolean".to_string(),
            "binary_expression" => {
                // Arithmetic ops produce numeric results; comparisons produce boolean.
                let op = kt::field(expr, "operator")
                    .map(|o| self.text(o).to_string())
                    .unwrap_or_default();
                let is_arith = matches!(op.as_str(), "+" | "-" | "*" | "/" | "%");
                if is_arith {
                    "int".to_string()
                } else {
                    "boolean".to_string()
                }
            }
            "call_expression" => {
                // constructor call: Person(...) -> Person
                let callee = kt::child(expr, "identifier")
                    .map(|n| self.text(n).to_string())
                    .unwrap_or_default();
                let targs = kt::child(expr, "type_arguments").map(|t| self.text(t).to_string());
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
                            self.diags.warn_approx(
                                expr,
                                self.file,
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

    /// Infer the type of a bare member read (`m.keys`, `s.length`).
    fn infer_navigation(&mut self, expr: tree_sitter::Node) -> String {
        match self.nav_base_member(expr) {
            Some((base, member)) => self.infer_member_type(base, &member, expr),
            None => "Object".to_string(),
        }
    }

    /// Infer `base.member` from the receiver's known Java type. N002 when
    /// the member is not in the known mapping set (type unknowable).
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
        let unknown = |u: &mut Self| {
            u.diags.warn_approx(
                node,
                u.file,
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
            | "filterIndexed" | "associateBy" | "groupBy" => match recv_ty.as_deref() {
                Some(t) if is_collection_ty(t) => format!("List<{}>", elem_type_of(t)),
                _ => unknown(self),
            },
            "forEach" => match recv_ty.as_deref() {
                Some(t) if is_collection_ty(t) => "void".to_string(),
                _ => unknown(self),
            },
            "joinToString" => "String".to_string(),
            "fold" | "reduce" => match recv_ty.as_deref() {
                Some(t) if is_collection_ty(t) => elem_type_of(t),
                _ => unknown(self),
            },
            _ => unknown(self),
        }
    }

    /// Split a navigation_expression into its receiver (first named child)
    /// and the last member name (`a.b.c` -> (a, "c")). `?.` treated as `.`.
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

    /// True when the receiver is a simple identifier whose known type is a
    /// Java array (`String[]`, `int[]`, ...) — drives `.size` -> `.length`.
    pub fn receiver_is_array(&self, base: tree_sitter::Node) -> bool {
        base.kind() == "identifier"
            && self
                .var_types
                .get(self.text(base).trim())
                .is_some_and(|t| t.ends_with("[]"))
    }

    /// top-level property -> static field + static accessors in the file class
    fn transpile_toplevel_property(&mut self, decl: tree_sitter::Node, out: &mut JavaOut) {
        // same as transpile_property but static
        let mut inner_out = JavaOut::new();
        self.transpile_property(decl, &mut inner_out);
        for line in inner_out.buf.lines() {
            if line.trim_start().starts_with("private ") {
                out.line(line.replacen("private ", "private static ", 1));
            } else {
                out.line(
                    line.replacen("public ", "public static ", 1)
                        .replace("return this.", "return "),
                );
            }
        }
    }

    fn transpile_statement(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) {
        let mut s = Stmt { unit: self };
        s.transpile(stmt, out);
    }
}

pub fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Kotlin primitive-array factories -> Java array type.
fn primitive_array_factory(name: &str) -> Option<&'static str> {
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
