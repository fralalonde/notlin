//! Class/object/interface/record emission.

use super::Unit;
use super::capitalize;
use crate::diagnostics::DiagnosticKind;
use crate::transpiler::expr::Expr;
use crate::transpiler::java::JavaOut;
use crate::transpiler::kt;

impl<'a> Unit<'a> {
    pub(crate) fn collect_type_relations(&mut self, root: tree_sitter::Node<'a>) {
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
        out.blank();
        if let Some(pkg) = crate::transpiler::types::nullable_import(self.annots) {
            out.line(format!("import {}.*;", pkg));
            out.blank();
        }
        f(self, out);
    }

    pub(crate) fn transpile_type_decl(&mut self, decl: tree_sitter::Node, out: &mut JavaOut) {
        let mut is_data = false;
        let mut is_sealed = false;
        let mut is_enum = false;
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
                                "enum" => is_enum = true,
                                "annotation" | "companion" | "inline" | "value" | "expect"
                                | "actual" | "external" | "inner" | "fun" => {
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
        if is_enum {
            self.transpile_enum(decl, &name, &visibility, &modifiers, is_sealed, out);
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
            if extends.is_empty() {
                out.open(format!(
                    "{}record {}({})",
                    visibility,
                    name,
                    comps.join(", ")
                ));
            } else {
                let inner = if extends.is_empty() {
                    format!("{}final class {}", visibility, name)
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
                        "{}{}final class {} {}",
                        visibility, static_kw, name, extends
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
                out.line("@AllArgsConstructor");
                out.blank();
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
            if !params.is_empty() && !self.lombok {
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
    fn transpile_enum(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        visibility: &str,
        modifiers: &str,
        is_sealed: bool,
        out: &mut JavaOut,
    ) {
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
                        let mut ec = member.walk();
                        for c in member.children(&mut ec) {
                            match c.kind() {
                                "identifier" => e.push_str(self.text(c)),
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
                                    e.push_str(&format!("({})", args.join(", ")));
                                }
                                _ => {}
                            }
                        }
                        entries.push(e);
                    }
                    ";" => {}
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

        // Bodyless (abstract) enum methods need per-constant bodies the
        // grammar cannot parse — javac would reject the emitted enum, so
        // taint instead of emitting broken Java.
        for m in &members {
            if m.kind() == "function_declaration" && kt::child(*m, "function_body").is_none() {
                self.diag_untranslatable(
                    *m,
                    format!(
                        "enum method '{}' is abstract; per-constant bodies are not supported — Java requires every constant to implement it",
                        kt::field(*m, "name")
                            .map(|n| self.text(n).to_string())
                            .unwrap_or_else(|| "?".to_string())
                    ),
                );
                return;
            }
        }

        out.open(format!("{}enum {}{}", visibility, name, implements));
        // constants
        out.line(entries.join(",\n"));
        if !members.is_empty() || !params.is_empty() {
            out.line(";");
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
                            .insert(pname, format!("get{}()", cap));
                    }
                    self.transpile_property_opts(member, out, true, Some(owner));
                    out.blank();
                }
                "function_declaration" => {
                    self.transpile_function_opts(
                        member, true, /*make_static=*/ true, false, out,
                    );
                    out.blank();
                }
                ";" | "{" | "}" => {}
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
    }

    fn transpile_object(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        visibility: &str,
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
                        // object properties behave like static fields
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
}
