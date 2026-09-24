//! Top-level compilation unit: package, imports, declarations.
//!
//! Split by concern: `class`, `function`, `property` and `types_infer`
//! submodules hold emission/inference; this file keeps the `Unit` state,
//! the coverage/taint machinery and top-level orchestration.

use crate::diagnostics::{DiagnosticKind, Diagnostics, FileCoverage, warning_code};
use crate::transpiler::java::JavaOut;
use crate::transpiler::kt;
use crate::transpiler::stmt::Stmt;
use crate::transpiler::types::AnnotationSet;
use crate::workspace::SourceIndex;
use std::path::{Path, PathBuf};

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
    /// Assume Apache Commons Lang 3 on the target classpath (--commons-lang).
    pub commons_lang: bool,
    pub in_place: bool,
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
    /// Name of the function whose body is currently being transpiled (used
    /// to detect self-setter field writes that must not re-enter `setX(...)`).
    pub(crate) current_function_name: Option<String>,
    /// Set by navigation_call when the member mapping already consumed the
    /// call args (joinToString) — call.rs must not append its own `(args)`.
    pub(crate) pending_full_call: bool,
    /// Set by binary() when a nested generic-call-with-trailing-lambda
    /// (`spec<String> { ... }` parsed as `spec < String`) returned bare callee
    /// text: the outer `>` half of the merged binary_expression pair is the
    /// type-argument list's closing bracket, NOT a comparison — the outer
    /// binary must skip its compareTo-rewrite and just pass the callee
    /// through so call.rs can attach the lambda.
    pub(crate) pending_generic_call: bool,
    /// function name -> Java return type, filled when each function is
    /// emitted; lets `val p = pair()` infer the fn's return type for
    /// member-call context (Pair.first -> getKey()).
    pub(crate) fn_rets: std::collections::HashMap<String, String>,
    /// Set by navigation_call's assembled stream reducers — call.rs must
    /// not re-run the stream-op path over the already-complete text.
    /// Fully-assembled stream text from navigation_call's curried fold —
    /// the call.rs frame holding the lambda must return it verbatim.
    pub(crate) pending_nav_text: Option<String>,
    /// A trailing `joinToString(sep)` seen on the callee nav — the map/filter
    /// stream arm should collect with joining(sep) (not toList()) and the
    /// member's joinToString tail is stripped.
    pub(crate) pending_join_to_string: Option<String>,
    /// data class name -> record component list `(type, name)` in declaration
    /// order. Filled by a pre-pass so destructuring sites can emit real
    /// `componentN()` extraction instead of `Object x = value; y = null;`.
    pub(crate) data_components: std::collections::HashMap<String, Vec<(String, String)>>,
    /// enum declarations in this file (simple names) — `Enum#name` is
    /// public so `.name` on an enum-typed receiver stays a field read.
    pub(crate) enum_types: std::collections::HashSet<String>,
    /// Instance property accessors across the file: property name -> Java
    /// getter name (`activity` -> `getActivity`). A bare identifier inside
    /// a body that isn't a local/param resolves against this so body text
    /// referencing a before-or-after-declared property member lowers to
    /// the accessor (interfaces especially: `get() = activity`).
    pub(crate) self_getters: std::collections::HashMap<String, String>,
    pub(crate) workspace: Option<&'a SourceIndex>,
    pub(crate) workspace_file: Option<PathBuf>,
    pub(crate) translation_roots: &'a [PathBuf],
}

impl<'a> Unit<'a> {
    pub fn new(
        source: &'a str,
        file: &'a Path,
        diags: &'a mut Diagnostics,
        annots: AnnotationSet,
        untranslatable_as_error: bool,
        lombok: bool,
        commons_lang: bool,
        in_place: bool,
    ) -> Self {
        Self {
            source,
            file,
            diags,
            annots,
            untranslatable_as_error,
            lombok,
            commons_lang,
            in_place,
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
            current_function_name: None,
            pending_full_call: false,
            pending_generic_call: false,
            fn_rets: std::collections::HashMap::new(),
            pending_nav_text: None,
            pending_join_to_string: None,
            pending_field_types: Vec::new(),
            data_components: std::collections::HashMap::new(),
            enum_types: std::collections::HashSet::new(),
            self_getters: std::collections::HashMap::new(),
            workspace: None,
            workspace_file: None,
            translation_roots: &[],
        }
    }

    pub fn with_workspace(
        mut self,
        workspace: Option<&'a SourceIndex>,
        translation_roots: &'a [PathBuf],
    ) -> Self {
        self.workspace = workspace;
        self.workspace_file = std::fs::canonicalize(self.file).ok();
        self.translation_roots = translation_roots;
        self
    }

    fn top_level_name(&self, decl: tree_sitter::Node<'_>) -> Option<String> {
        kt::field(decl, "name")
            .or_else(|| {
                kt::child(decl, "variable_declaration")
                    .and_then(|variable| kt::child(variable, "identifier"))
            })
            .map(|name| self.text(name).to_string())
    }

    fn workspace_requires_top_level_retention(&self, name: &str) -> bool {
        let Some(workspace) = self.workspace else {
            return false;
        };
        let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
        workspace.has_kotlin_reference(indexed_path, name)
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
            format!("// NOTLIN: {} {}\n", warning_code(&message), message),
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
                crate::diagnostics::warning_code(&message),
                message
            ),
        ));
    }

    fn kotlin_import_to_java(&self, node: tree_sitter::Node<'a>) -> String {
        let raw = self.text(node).trim();
        let path = raw
            .strip_prefix("import")
            .map(str::trim)
            .unwrap_or(raw)
            .trim_end_matches(';')
            .trim();
        // kotlin.reflect / kotlin.jvm.* machinery has no Java counterpart:
        // importing it produces "cannot find symbol" noise. Drop them; the
        // using sites are warned separately as approximations.
        if path.starts_with("kotlin.reflect.")
            || path.starts_with("kotlin.jvm.")
            || path.starts_with("kotlin.properties.")
        {
            let _ = path;
            return String::new();
        }
        if path.ends_with(".*") {
            // Member wildcard: `import pkg.Kind.*`. When `Kind` is a known
            // enum this is really an enum-constant import, which Java only
            // accepts as a static wildcard — rewrite to the static form.
            let head = path.trim_end_matches(".*");
            let last = head.rsplit('.').next().unwrap_or(head);
            let is_enum = self.enum_types.contains(last)
                || self.workspace.map(|w| {
                    w.declarations().any(|d| {
                        d.name == last && d.kind == crate::workspace::DeclarationKind::Enum
                    })
                }) == Some(true);
            if is_enum {
                return format!("static {head}.*");
            }
            return path.to_string();
        }
        if path.contains(" as ") {
            return path.to_string();
        }
        let last = path.rsplit('.').next().unwrap_or(path);
        let is_package_name = !last.is_empty()
            && last
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_');
        // Enum-constant member imports (`import pkg.Kind.YES`) don't exist in
        // Java unless spelled `import static`. Mark for the static form; the
        // renderer prepends `import`. Static imports render their full text
        // (sans trailing `;`), so return the whole `static pkg.Kind.YES`.
        if is_package_name {
            format!("{path}.*")
        } else if self.unit_is_enum_constant(path) {
            format!("static {path}")
        } else {
            path.to_string()
        }
    }

    /// An import path's last segment refers to an enum constant when the
    /// parent type is a known enum in the workspace index and the last
    /// segment starts uppercase but isn't itself a known type.
    fn unit_is_enum_constant(&self, path: &str) -> bool {
        let mut parts = path.rsplit('.');
        let last = parts.next().unwrap_or("");
        let Some(parent_ty) = parts.next() else {
            return false;
        };
        // Conventional: enum constants are SCREAMING_CASE; CamelCase segments
        // could be nested classes, lowercase segments could be packages.
        if last.is_empty() || last.chars().any(|c| c.is_ascii_lowercase()) {
            return false;
        }
        // Workspace lookup is authoritative — the enum may live in another
        // file of the same translation root.
        if let Some(workspace) = self.workspace {
            for decl in workspace.declarations() {
                if decl.name == parent_ty && decl.kind == crate::workspace::DeclarationKind::Enum {
                    return true;
                }
            }
        }
        self.enum_types.contains(parent_ty)
    }
    pub fn run(&mut self, root: tree_sitter::Node<'a>) -> Vec<(String, String)> {
        // Collect top-level structure
        let mut package = String::new();
        let mut imports: Vec<String> = Vec::new();
        let mut decls: Vec<tree_sitter::Node> = Vec::new();
        let mut standalone_annotation_targets = std::collections::HashSet::new();
        let mut retain_next_declaration = false;

        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            match child.kind() {
                "package_header" => {
                    if let Some(qi) = kt::child(child, "qualified_identifier") {
                        package = self.text(qi).replace(" ", "");
                    }
                }
                "import" => {
                    let imp = self.kotlin_import_to_java(child);
                    if !imp.is_empty() {
                        imports.push(imp);
                    }
                }
                "shebang_line" | ";" | "line_comment" | "multiline_comment" => {}
                "annotated_expression" => {
                    // An annotation wrapper may contain a declaration whose
                    // annotation semantics are not representable in Java.
                    // Retain the complete Kotlin construct rather than
                    // dropping the declaration while translating siblings.
                    self.diag_untranslatable(
                        child,
                        "annotated top-level declaration is retained in Kotlin",
                    );
                    if self.current_decl.is_none() {
                        self.coverage
                            .untranslated
                            .push(format!("top-level@{}", child.start_byte()));
                    }
                    retain_next_declaration = true;
                }
                k if k.contains("declaration")
                    || k == "object_declaration"
                    || k == "class_declaration"
                    || k == "function_declaration"
                    || k == "property_declaration" =>
                {
                    if retain_next_declaration {
                        standalone_annotation_targets.insert(child.id());
                        retain_next_declaration = false;
                    }
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
                    if standalone_annotation_targets.contains(&decl.id()) {
                        self.diag_untranslatable(
                            *decl,
                            "standalone annotation requires the following declaration to remain Kotlin",
                        );
                        self.end_decl();
                        continue;
                    }
                    // Kotlin enum with a pre-existing Java consumer of the
                    // Kotlin `entries` ABI (`E.getEntries()`): a plain Java
                    // enum drops that static and breaks the caller — retain.
                    let is_enum = kt::child(*decl, "enum_class_body").is_some();
                    if is_enum
                        && self.workspace.is_some_and(|ws| {
                            ws.has_java_get_entries_consumer(&type_name)
                                || ws.has_external_kotlin_reference_by_name(&type_name)
                        })
                    {
                        self.diag_untranslatable(
                            *decl,
                            "Kotlin enum `entries` ABI (`getEntries()`) is consumed by pre-existing Java code; enum remains Kotlin until the consumer migrates",
                        );
                        self.end_decl();
                        continue;
                    }
                    // Mixed-language accessor ABI: an abstract member of a
                    // RETAINED Kotlin supertype whose Java-visible return type
                    // differs from this class's own member of the same name
                    // cannot be satisfied by a Java class (Java has no
                    // covariant cross-language absolvability) — retain.
                    if let Some(ws) = self.workspace {
                        let supers: Vec<String> = kt::child(*decl, "delegation_specifiers")
                            .map(|dc| {
                                let mut c = dc.walk();
                                dc.children(&mut c)
                                    .filter(|s| s.kind() == "delegation_specifier")
                                    .map(|s| {
                                        s.child_by_field_name("user_type")
                                            .map(|u| self.text(u).trim().to_string())
                                            .unwrap_or_else(|| self.text(s).trim().to_string())
                                    })
                                    .filter(|s| !s.is_empty())
                                    .collect()
                            })
                            .unwrap_or_default();
                        if ws.retained_supertype_member_mismatch(&supers, &type_name) {
                            self.diag_untranslatable(
                                *decl,
                                "class implements a retained Kotlin supertype whose abstract member return type is incompatible with the class's own member; Java return types must match exactly",
                            );
                            self.end_decl();
                            continue;
                        }
                    }
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
                    let label = unit
                        .top_level_name(**decl)
                        .unwrap_or_else(|| "<anonymous>".to_string());
                    unit.begin_decl(**decl, label.clone());
                    if unit.workspace_requires_top_level_retention(&label) {
                        unit.diag_untranslatable(
                            **decl,
                            "top-level declaration is referenced by retained Kotlin source",
                        );
                    } else {
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
                    let label = self
                        .top_level_name(***d)
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
