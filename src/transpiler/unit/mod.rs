//! Top-level compilation unit: package, imports, declarations.
//!
//! Split by concern: `class`, `function`, `property` and `types_infer`
//! submodules hold emission/inference; this file keeps the `Unit` state,
//! the coverage/taint machinery and top-level orchestration.

use crate::diagnostics::{DiagnosticKind, Diagnostics, FileCoverage};
use crate::transpiler::java::JavaOut;
use crate::transpiler::kt;
use crate::transpiler::stmt::Stmt;
use crate::transpiler::types::AnnotationSet;
use std::path::Path;

mod class;
mod function;
mod property;
mod types_infer;

pub(crate) use types_infer::primitive_array_factory;

pub struct Unit<'a> {
    pub source: &'a str,
    pub file: &'a Path,
    pub diags: &'a mut Diagnostics,
    pub annots: AnnotationSet,
    pub untranslatable_as_error: bool,
    /// Assume Lombok on target classpath (--lombok): data classes emit as
    /// @Data classes (mutable), hand-rolled accessors become annotations.
    pub lombok: bool,
    /// Per-file coverage: which declarations translated, which didn't.
    pub coverage: FileCoverage,
    /// Name of the declaration currently being translated; diagnostics raised
    /// while this is Some are attributed to it for in-place migration policy.
    pub(crate) current_decl: Option<tree_sitter::Node<'a>>,
    /// node-id -> label for open declarations.
    pub(crate) decl_labels: std::collections::HashMap<usize, String>,
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
    pub(crate) subclass_map: std::collections::HashMap<String, Vec<String>>,
    /// names of sealed class declarations in this file.
    pub(crate) sealed_types: std::collections::HashSet<String>,
    /// companion-object members across the file: member name -> Java access
    /// expression on the outer class (getter call or function ref). Static
    /// call-site rewrite `Outer.MAX` -> `Use.getMAX()`.
    pub(crate) companion_members: std::collections::HashMap<String, String>,
    /// Field types to re-seed into every enclosing declaration scope: enum
    /// ctor params are instance fields visible to all enum body methods.
    pub(crate) pending_field_types: Vec<(String, String)>,
    /// Class property registry: property name -> Java setter method name
    /// (`late` -> `setLate`). Assignment targets read through this instead of
    /// emitting an illegal `obj.getProp() = value`. Empty setter name = val
    /// (no setter; assignment to it is a compile error in Kotlin too).
    pub(crate) class_props: std::collections::HashMap<String, String>,
    /// Name of the object currently being translated: bare self-references
    /// inside an object body (`val self = Registry`) must map to
    /// `Registry.INSTANCE` — a plain `Registry` is an unresolved symbol.
    pub(crate) current_object: Option<String>,
    /// object/companion member return types: member name -> Java type, so
    /// `Registry.instance` infers instead of degrading to Object.
    pub(crate) static_member_types: std::collections::HashMap<String, String>,
    /// Set by transpile_target when the LHS was rewritten to a setter call —
    /// the assignment emitter then closes the call instead of emitting `=`.
    pub(crate) pending_setter: bool,
    /// Set by navigation_call when the member mapping already consumed the
    /// call args (joinToString) — call.rs must not append its own `(args)`.
    pub(crate) pending_full_call: bool,
    /// function name -> Java return type, filled when each function is
    /// emitted; lets `val p = pair()` infer the fn's return type for
    /// member-call context (Pair.first -> getKey()).
    pub(crate) fn_rets: std::collections::HashMap<String, String>,
    /// Set by navigation_call's assembled stream reducers — call.rs must
    /// not re-run the stream-op path over the already-complete text.
    /// Fully-assembled stream text from navigation_call's curried fold —
    /// the call.rs frame holding the lambda must return it verbatim.
    pub(crate) pending_nav_text: Option<String>,
    /// data class name -> record component list `(type, name)` in declaration
    /// order. Filled by a pre-pass so destructuring sites can emit real
    /// `componentN()` extraction instead of `Object x = value; y = null;`.
    pub(crate) data_components: std::collections::HashMap<String, Vec<(String, String)>>,
    /// enum declarations in this file (simple names) — `Enum#name` is
    /// public so `.name` on an enum-typed receiver stays a field read.
    pub(crate) enum_types: std::collections::HashSet<String>,
}

impl<'a> Unit<'a> {
    pub fn new(
        source: &'a str,
        file: &'a Path,
        diags: &'a mut Diagnostics,
        annots: AnnotationSet,
        untranslatable_as_error: bool,
        lombok: bool,
    ) -> Self {
        Self {
            source,
            file,
            diags,
            annots,
            untranslatable_as_error,
            lombok,
            coverage: FileCoverage::default(),
            current_decl: None,
            decl_labels: std::collections::HashMap::new(),
            var_types: std::collections::HashMap::new(),
            extension_fns: std::collections::HashMap::new(),
            ext_receiver_name: None,
            subclass_map: std::collections::HashMap::new(),
            sealed_types: std::collections::HashSet::new(),
            companion_members: std::collections::HashMap::new(),
            class_props: std::collections::HashMap::new(),
            current_object: None,
            static_member_types: std::collections::HashMap::new(),
            pending_setter: false,
            pending_full_call: false,
            fn_rets: std::collections::HashMap::new(),
            pending_nav_text: None,
            pending_field_types: Vec::new(),
            data_components: std::collections::HashMap::new(),
            enum_types: std::collections::HashSet::new(),
        }
    }

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

    pub(crate) fn taint_decl(&mut self, label: &str) {
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
            // The diagnostic node itself, then walk up through the AST: the
            // nearest enclosing declaration node that began via begin_decl
            // (tracked by decl_labels). Handles both top-level decls (diag
            // anchored on the decl or a member of it) and members nested in
            // class bodies.
            let mut ancestor = Some(node);
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

    pub(crate) fn diag_approx(&mut self, node: tree_sitter::Node, msg: impl Into<String>) {
        // Console diagnostic (compiler-style listing)…
        let message = msg.into();
        self.coverage.diags_approx.push((
            node.start_byte(),
            message.clone(),
            node.start_position().row + 1,
            node.start_position().column + 1,
        ));
        // …and an in-place residue `// NOTLIN: N002 …` comment anchored
        // directly above the element (same mechanism as N001 blockers) —
        // every translated-but-lossy construct is marked at its site, not
        // just hard blockers.
        self.coverage.blockers.push((
            node.start_byte(),
            format!(
                "// NOTLIN: {} {}\n",
                DiagnosticKind::Approximated.code(),
                message
            ),
        ));
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
                            unit.transpile_toplevel_property(**decl, out, &file_class_name);
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

    pub(crate) fn visibility_of(&mut self, decl: tree_sitter::Node) -> String {
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

    pub(crate) fn transpile_statement(&mut self, stmt: tree_sitter::Node, out: &mut JavaOut) {
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
