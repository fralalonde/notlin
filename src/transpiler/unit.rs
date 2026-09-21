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
    /// identifier name -> primitive marker, from inferred local decls and
    /// primitive-typed function parameters in the current translation scope.
    pub var_types: std::collections::HashMap<String, String>,
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
        if let Some(decl_node) = self.current_decl {
            // Simple containment: diagnostic node's byte range inside the
            // declaration's byte range.
            let inside = node.start_byte() >= decl_node.start_byte()
                && node.end_byte() <= decl_node.end_byte();
            if inside {
                let label = self
                    .decl_labels
                    .get(&decl_node.id())
                    .cloned()
                    .unwrap_or_default();
                self.taint_decl(&label);
            }
        }

        self.diags.push(crate::diagnostics::Diagnostic {
            severity: sev,
            kind: DiagnosticKind::Untranslatable,
            message: msg.into(),
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

    /// Emit package + imports + annotation import into a new output file.
    fn transpile_type_decl_set(
        &mut self,
        out: &mut JavaOut,
        package: &str,
        imports: &[String],
        f: impl FnOnce(&mut Self, &mut JavaOut),
    ) {
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
                                    modifiers.push_str(self.text(cm));
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
                            .or_else(|| kt::child(cp, "nullable_type"))?;
                        Some((
                            is_val,
                            self.text(ident).to_string(),
                            kt::java_type_ann(ty, self.source, self.annots),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // superclass / interfaces
        let mut extends = String::new();
        if let Some(dc) = kt::child(decl, "delegation_specifiers") {
            let mut parts: Vec<String> = Vec::new();
            let mut cursor = dc.walk();
            for sp in dc.children(&mut cursor) {
                if sp.kind() == "super_type"
                    || sp.kind() == "user_type"
                    || sp.kind() == "annotation"
                {
                    let t = self.text(sp).replace(" ", "");
                    if !t.starts_with("@") {
                        parts.push(t);
                    }
                }
            }
            if !parts.is_empty() {
                extends = format!(" extends {}", parts.join(", "));
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
                            // interface property: accessor signatures only
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
                                    out.line(format!("{} get{}();", pty, cap));
                                    if kt::child(member, "val").is_none() {
                                        out.line(format!("void set{}({} value);", cap, pty));
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
            out.close();
        } else {
            out.open(format!(
                "{}{}{} {}{}",
                visibility, modifiers, kind_word, name, extends
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
                if let (Some(id), Some(bound)) = (
                    kt::child(t, "identifier"),
                    kt::child(t, "user_type").or_else(|| kt::child(t, "nullable_type")),
                ) {
                    let id_text = self.text(id).to_string();
                    let bound = kt::java_type_ann(bound, self.source, self.annots);
                    if bound == "Object" || bound == "Any" {
                        parts.push(id_text);
                    } else {
                        parts.push(format!("{} extends {}", id_text, bound));
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
            let mut cursor = fvp.walk();
            for p in fvp.children(&mut cursor) {
                if p.kind() == "parameter" {
                    let pname = kt::child(p, "identifier")
                        .map(|n| self.text(n).to_string())
                        .unwrap_or_else(|| "arg".to_string());
                    let pty = kt::child(p, "user_type")
                        .or_else(|| kt::child(p, "nullable_type"))
                        .map(|t| kt::java_type_ann(t, self.source, self.annots))
                        .unwrap_or_else(|| "Object".to_string());
                    params.push(format!("{} {}", pty, pname));
                    // Track param types for == and ordered-comparison logic
                    self.var_types.insert(pname, pty.clone());
                }
            }
        }
        if is_main && params.is_empty() {
            params.push("String[] args".to_string());
        }

        let is_static = if make_static { "static " } else { "" };
        let has_body = kt::child(decl, "function_body").is_some();
        // Inside an interface: bodyless stays implicit, with body -> default
        let in_interface = kt::parent_of(decl)
            .and_then(|p| kt::parent_of(p))
            .map(|gp| gp.children(&mut gp.walk()).any(|c| c.kind() == "interface"))
            .unwrap_or(false);
        let abstract_kw = if has_body {
            if in_interface { "default " } else { "" }
        } else {
            "abstract "
        };
        if !has_body {
            // Bodyless: signature-only (abstract / interface method)
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
                if member.kind() == "function_declaration" {
                    if let Some(mname) = kt::field(member, "name") {
                        conflicts.push(self.text(mname).to_string());
                    }
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
            out.blank();
            out.open(format!("public void set{}({} {})", cap, ty, name));
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

    /// Best-effort type inference from an initializer expression.
    pub fn infer_type(&self, expr: tree_sitter::Node) -> String {
        match expr.kind() {
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
                        // Uppercase callee with no dot: constructor call
                        if !callee.contains('.')
                            && callee
                                .chars()
                                .next()
                                .is_some_and(|c| c.is_ascii_uppercase())
                        {
                            callee
                        } else {
                            "Object".to_string()
                        }
                    }
                }
            }
            _ => "Object".to_string(),
        }
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
