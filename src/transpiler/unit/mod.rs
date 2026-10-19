//! Top-level compilation unit: package, imports, declarations.
//!
//! Split by concern: `class`, `function`, `property` and `types_infer`
//! submodules hold emission/inference; this file keeps the `Unit` state,
//! the coverage/taint machinery and top-level orchestration.

use crate::diagnostics::{DiagnosticKind, Diagnostics, FileCoverage, RetentionKind, warning_code};
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

pub(crate) use types_infer::{instance_property_count, primitive_array_factory};

pub struct Unit<'src, 'tree> {
    pub source: &'src str,
    pub source_hash: [u8; 32],
    pub file: &'src Path,
    pub diags: &'src mut Diagnostics,
    pub annots: AnnotationSet,
    pub untranslatable_as_error: bool,
    /// Assume Lombok on target classpath (--lombok): data classes emit as
    /// @Data classes (mutable), hand-rolled accessors become annotations.
    pub lombok: bool,
    pub in_place: bool,
    pub allow_approximations: bool,
    pub type_facts: Vec<(
        crate::semantics::SourceLocation,
        crate::semantics::FactStatus<crate::semantics::TypeRef>,
    )>,
    pub output_owners: std::collections::HashMap<String, Vec<crate::semantics::SymbolId>>,
    pub(crate) preparation_failure: Option<String>,
    pub semantic_provider: Option<&'src dyn crate::semantics::SemanticProvider>,
    pub(crate) enum_when_lowerings:
        std::collections::HashMap<usize, crate::semantics::EnumWhenLowering>,
    pub(crate) todo_lowerings: std::collections::HashMap<usize, crate::semantics::TodoLowering>,
    pub(crate) reference_string_calls:
        std::collections::HashMap<(usize, usize), crate::semantics::ReferenceStringCall>,
    pub(crate) container_calls:
        std::collections::HashMap<(usize, usize), crate::semantics::ContainerMemberCall>,
    pub(crate) container_lambdas:
        std::collections::HashMap<(usize, usize), crate::semantics::ContainerLambdaBinding>,
    pub(crate) container_property_accesses:
        std::collections::HashMap<(usize, usize), crate::semantics::ContainerPropertyAccess>,
    pub(crate) eager_collection_calls:
        std::collections::HashMap<(usize, usize), crate::semantics::EagerCollectionCall>,
    pub(crate) eager_collection_helpers: Vec<crate::translation_plan::PlannedEagerCollectionHelper>,
    pub(crate) local_expression_aliases: std::collections::HashMap<String, String>,
    pub(crate) local_expression_types: std::collections::HashMap<String, crate::semantics::TypeRef>,
    pub(crate) exact_overload_calls:
        std::collections::HashMap<(usize, usize), crate::semantics::ExactOverloadCall>,
    pub(crate) reified_class_calls:
        std::collections::HashMap<(usize, usize), crate::semantics::ReifiedClassCall>,
    pub(crate) constructor_delegations: std::collections::HashMap<
        (usize, usize),
        crate::translation_plan::PlannedConstructorDelegation,
    >,
    pub(crate) default_getter_bridges: Vec<crate::translation_plan::PlannedDefaultGetterBridge>,
    declaration_diagnostic_start: usize,
    /// Per-file coverage: which declarations translated, which didn't.
    pub coverage: FileCoverage,
    /// O(1) membership for untranslated declarations. Coverage keeps its
    /// ordered Vec for migration output and diagnostics.
    untranslated_names: std::collections::HashSet<String>,
    /// Snapshot-local boundary identities. Names remain a conservative lookup
    /// for unresolved calls, but overload ownership must not be name-based.
    tainted_declarations: std::collections::HashSet<usize>,
    /// Name of the declaration currently being translated; diagnostics raised
    /// while this is Some are attributed to it for in-place migration policy.
    pub(crate) current_decl: Option<tree_sitter::Node<'tree>>,
    /// node-id -> label for open declarations.
    pub(crate) decl_labels: std::collections::HashMap<usize, String>,
    /// identifier name -> Java type (from params and local decls in the
    /// current translation scope). Array params/locals map to `String[]`,
    /// `int[]`, ... so callers can special-case `.size` -> `.length`.
    pub var_types: std::collections::HashMap<String, String>,
    /// Flow-sensitive Kotlin smart casts established by an early-return
    /// negative type guard (`if (x !is T) return`). Cleared per function.
    pub(crate) flow_smart_casts: std::collections::HashMap<String, String>,
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
    /// Whether an explicit `return expr` in the current body may return a
    /// value. Kotlin permits `return unitCall()` in a Unit function; Java
    /// requires the call and `return` to be separate statements.
    pub(crate) current_function_returns_value: bool,
    pub(crate) current_function_return_type: Option<String>,

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
    /// Callee nav text of the shape `.member<ty>` — a generic type argument
    /// written INSIDE the callee navigation (`context.get<T>()`). The call
    /// node itself carries no type_arguments child, so navigation_call stashes
    /// it here and call.rs's reified_type_argument_rewrite consumes it.
    pub(crate) pending_callee_type_arg: Option<String>,
    /// data class name -> record component list `(type, name)` in declaration
    /// order. Filled by a pre-pass so destructuring sites can emit real
    /// `componentN()` extraction instead of `Object x = value; y = null;`.
    pub(crate) data_components: std::collections::HashMap<String, Vec<(String, String)>>,
    /// Data classes that actually emit as Java records.
    pub(crate) record_types: std::collections::HashSet<String>,
    /// Data-class body initializers emitted in primary-constructor order.
    pub(crate) deferred_property_initializers: std::collections::HashSet<usize>,
    /// enum declarations in this file (simple names) — `Enum#name` is
    /// public so `.name` on an enum-typed receiver stays a field read.
    pub(crate) enum_types: std::collections::HashSet<String>,
    /// Instance property accessors across the file: property name -> Java
    /// getter name (`activity` -> `getActivity`). A bare identifier inside
    /// a body that isn't a local/param resolves against this so body text
    /// referencing a before-or-after-declared property member lowers to
    /// the accessor (interfaces especially: `get() = activity`).
    pub(crate) self_getters: std::collections::HashMap<String, String>,
    pub(crate) workspace: Option<&'src SourceIndex>,
    pub(crate) workspace_file: Option<PathBuf>,
    /// Visible Kotlin typealiases, resolved once per source file instead of
    /// once for every Java type emitted from that source.
    pub(crate) visible_typealiases: Vec<(String, String)>,
    /// Provider-proven collection aliases are rewritten only at Java type-use
    /// nodes, so matching source text cannot alter literals or identifiers.
    pub(crate) proven_map_typealiases: Vec<(String, String)>,
    /// Imported alias -> original spelling, collected once from import nodes.
    pub(crate) import_alias_targets: std::collections::HashMap<String, String>,
    pub(crate) translation_roots: &'src [PathBuf],
    /// Retention-fixpoint retained set from the caller (probe passes): names
    /// known to be retained in Kotlin for intrinsic reasons. When Some, the
    /// subtype rule consults it (`has_retained_kotlin_subtype`) instead of
    /// retaining on every Kotlin subtype; None = conservative catch-all.
    pub(crate) retained_hint: Option<&'src std::collections::HashSet<crate::semantics::SymbolId>>,
    /// node-id -> Java-native annotation texts hoisted from a preceding
    /// top-level annotated_expression wrapper (grammar quirk: a leading
    /// `@X("v") object T` parses the annotation outside the declaration).
    /// Transpile_type_decl prepends them to the declaration's own annotations.
    pub(crate) hoisted_annotations: std::collections::HashMap<usize, Vec<String>>,
    /// node-id -> byte spans of the annotated_expression wrappers that
    /// preceded the declaration, so end_decl strips the wrapper text alongside
    /// the translated declaration (an unstripped wrapper dangles in the
    /// residue and re-binds to the NEXT surviving declaration).
    pub(crate) wrapper_spans: std::collections::HashMap<usize, Vec<(usize, usize)>>,
    /// Simple names of TOP-LEVEL function declarations in this file, gathered
    /// in the pre-pass: a translated declaration must not emit a bare call to
    /// one that remained Kotlin.
    pub(crate) top_level_functions: std::collections::HashSet<String>,
    /// Top-level function names that WILL remain Kotlin (referenced by
    /// retained Kotlin source), decided in the pre-pass — the loose-decl pass
    /// runs after the type loop, so callee checks need this decision early.
    pub(crate) retained_file_functions: std::collections::HashSet<String>,
}

fn is_java_type_use(node: tree_sitter::Node<'_>, name: &str, source: &str) -> bool {
    // A file-visible alias does not override a method/class type parameter.
    // Java's type nodes preserve that lexical scope, unlike text replacement.
    let mut ancestor = node.parent();
    while let Some(scope) = ancestor {
        for parameters in scope
            .named_children(&mut scope.walk())
            .filter(|child| child.kind() == "type_parameters")
        {
            if parameters
                .named_children(&mut parameters.walk())
                .filter(|parameter| parameter.kind() == "type_parameter")
                .any(|parameter| {
                    parameter
                        .named_children(&mut parameter.walk())
                        .find(|child| child.kind() == "type_identifier")
                        .and_then(|child| child.utf8_text(source.as_bytes()).ok())
                        == Some(name)
                })
            {
                return false;
            }
        }
        ancestor = scope.parent();
    }
    let mut child = node;
    while let Some(parent) = child.parent() {
        if parent.kind() == "scoped_type_identifier" {
            return false;
        }
        if matches!(
            parent.kind(),
            "generic_type" | "array_type" | "annotated_type" | "type_arguments" | "wildcard"
        ) {
            child = parent;
            continue;
        }
        return parent
            .child_by_field_name("type")
            .is_some_and(|ty| ty.id() == child.id());
    }
    false
}

/// Constructor-mode flags bundled for `Unit::new` (a plain value object
/// keeps the constructor under clippy's argument-count ceiling).
#[derive(Debug, Clone, Copy, Default)]
pub struct UnitOptions {
    pub untranslatable_as_error: bool,
    pub lombok: bool,
    pub in_place: bool,
    pub allow_approximations: bool,
}

impl<'src, 'tree> Unit<'src, 'tree> {
    pub(crate) fn established_standard_string_case_call(&self, node: tree_sitter::Node) -> bool {
        self.semantic_provider.is_some_and(|provider| {
            crate::semantics::established_standard_string_case_call(
                self.source,
                self.workspace_file.as_deref().unwrap_or(self.file),
                node,
                provider,
            )
        })
    }

    pub(crate) fn established_java_callable_member(&self, node: tree_sitter::Node) -> bool {
        let Some(provider) = self.semantic_provider else {
            return false;
        };
        let call = if matches!(node.kind(), "call_expression" | "method_invocation") {
            Some(node)
        } else {
            node.parent()
                .filter(|parent| matches!(parent.kind(), "call_expression" | "method_invocation"))
        };
        call.is_some_and(|call| {
            crate::semantics::established_java_callable_member(
                self.source,
                self.workspace_file.as_deref().unwrap_or(self.file),
                call,
                provider,
            )
        })
    }

    pub(crate) fn established_platform_call_return_type(
        &self,
        node: tree_sitter::Node,
    ) -> Option<&'static str> {
        let call = if matches!(node.kind(), "call_expression" | "method_invocation") {
            node
        } else {
            node.parent()
                .filter(|parent| matches!(parent.kind(), "call_expression" | "method_invocation"))?
        };
        crate::semantics::established_platform_call_return_type(
            self.source,
            self.workspace_file.as_deref().unwrap_or(self.file),
            call,
            self.semantic_provider?,
        )
    }

    pub(crate) fn is_retained_type(&self, name: &str) -> bool {
        let Some(retained) = self.retained_hint else {
            return false;
        };
        if let Some(workspace) = self.workspace
            && let Some(file) =
                workspace.source_file(self.workspace_file.as_deref().unwrap_or(self.file))
            && let Some(declaration) = workspace.resolve_type(file, name)
        {
            return workspace.declaration_retained(declaration, retained);
        }
        // An unresolved reference is conservatively blocked by any matching identity.
        crate::transpiler::retention_queries::contains_name(retained, name)
    }

    pub(crate) fn known_declared_method(&self, receiver: &str, member: &str) -> bool {
        let receiver = receiver.trim();
        let ty = self
            .var_types
            .get(receiver)
            .map(String::as_str)
            .unwrap_or(receiver);
        let ty = ty.trim().strip_prefix("new ").unwrap_or(ty);
        let ty = ty.split(['(', '<']).next().unwrap_or(ty).trim();
        let ty = ty
            .strip_suffix(".Companion")
            .or_else(|| ty.strip_suffix(".super"))
            .unwrap_or(ty);
        if let Some(workspace) = self.workspace
            && let Some(file) =
                workspace.source_file(self.workspace_file.as_deref().unwrap_or(self.file))
            && let Some(declaration) = workspace.resolve_type(file, ty)
        {
            return declaration
                .members
                .iter()
                .any(|m| m.name == member && m.kind == crate::workspace::MemberKind::Method);
        }
        use crate::semantics::SemanticProvider;
        if let Some(provider) = self.semantic_provider {
            let file = self.workspace_file.as_deref().unwrap_or(self.file);
            return provider
                .symbols_named(member)
                .into_iter()
                .filter(|symbol| {
                    (symbol.id.file == file || symbol.id.file == self.file)
                        && symbol.id.kind == "function"
                        && symbol.id.owner_path.last().is_some_and(|owner| owner == ty)
                })
                .count()
                == 1;
        }
        let provider = crate::semantics::SyntaxSemanticProvider::new([(
            self.file.to_path_buf(),
            self.source.to_string(),
        )]);
        provider
            .symbols_named(member)
            .into_iter()
            .filter(|s| {
                s.id.name == member
                    && s.id.kind == "function"
                    && s.id.owner_path.last().is_some_and(|owner| owner == ty)
            })
            .count()
            == 1
    }
    pub fn new(
        source: &'src str,
        file: &'src Path,
        diags: &'src mut Diagnostics,
        annots: AnnotationSet,
        options: UnitOptions,
    ) -> Self {
        Self {
            source,
            source_hash: *blake3::hash(source.as_bytes()).as_bytes(),
            file,
            diags,
            annots,
            untranslatable_as_error: options.untranslatable_as_error,
            lombok: options.lombok,
            in_place: options.in_place,
            allow_approximations: options.allow_approximations,
            declaration_diagnostic_start: 0,
            coverage: FileCoverage::default(),
            untranslated_names: std::collections::HashSet::new(),
            type_facts: Vec::new(),
            output_owners: std::collections::HashMap::new(),
            preparation_failure: None,
            semantic_provider: None,
            enum_when_lowerings: std::collections::HashMap::new(),
            todo_lowerings: std::collections::HashMap::new(),
            reference_string_calls: std::collections::HashMap::new(),
            container_calls: std::collections::HashMap::new(),
            container_lambdas: std::collections::HashMap::new(),
            container_property_accesses: std::collections::HashMap::new(),
            eager_collection_calls: std::collections::HashMap::new(),
            eager_collection_helpers: Vec::new(),
            local_expression_aliases: std::collections::HashMap::new(),
            local_expression_types: std::collections::HashMap::new(),
            exact_overload_calls: std::collections::HashMap::new(),
            reified_class_calls: std::collections::HashMap::new(),
            constructor_delegations: std::collections::HashMap::new(),
            default_getter_bridges: Vec::new(),
            tainted_declarations: std::collections::HashSet::new(),
            current_decl: None,
            decl_labels: std::collections::HashMap::new(),
            var_types: std::collections::HashMap::new(),
            flow_smart_casts: std::collections::HashMap::new(),
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
            current_function_returns_value: false,
            current_function_return_type: None,

            pending_full_call: false,
            pending_callee_type_arg: None,
            pending_generic_call: false,
            fn_rets: std::collections::HashMap::new(),
            pending_nav_text: None,
            pending_join_to_string: None,
            pending_field_types: Vec::new(),
            data_components: std::collections::HashMap::new(),
            record_types: std::collections::HashSet::new(),
            deferred_property_initializers: std::collections::HashSet::new(),
            enum_types: std::collections::HashSet::new(),
            self_getters: std::collections::HashMap::new(),
            workspace: None,
            workspace_file: None,
            visible_typealiases: Vec::new(),
            proven_map_typealiases: Vec::new(),
            import_alias_targets: std::collections::HashMap::new(),
            translation_roots: &[],
            retained_hint: None,
            hoisted_annotations: std::collections::HashMap::new(),
            wrapper_spans: std::collections::HashMap::new(),
            top_level_functions: std::collections::HashSet::new(),
            retained_file_functions: std::collections::HashSet::new(),
        }
    }

    pub fn with_workspace(
        self,
        workspace: Option<&'src SourceIndex>,
        translation_roots: &'src [PathBuf],
    ) -> Self {
        self.with_workspace_selection(workspace, translation_roots, None)
    }

    pub(crate) fn with_workspace_selection(
        mut self,
        workspace: Option<&'src SourceIndex>,
        translation_roots: &'src [PathBuf],
        workspace_file: Option<&Path>,
    ) -> Self {
        self.workspace = workspace;
        self.workspace_file = workspace_file
            .map(Path::to_path_buf)
            .or_else(|| std::fs::canonicalize(self.file).ok());
        if let (Some(workspace), Some(file)) = (workspace, self.workspace_file.as_deref()) {
            self.visible_typealiases = workspace.type_aliases_for(file);
        }
        self.translation_roots = translation_roots;
        self
    }

    pub(crate) fn lower_visible_typealiases(&self, java: &mut String) {
        for (alias, target) in &self.visible_typealiases {
            let mut lowered = String::with_capacity(java.len());
            let mut rest = java.as_str();
            while let Some(start) = rest.find(alias.as_str()) {
                let end = start + alias.len();
                let before = rest[..start].chars().last();
                let after = rest[end..].chars().next();
                let is_boundary =
                    |ch: Option<char>| !ch.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
                if is_boundary(before) && is_boundary(after) {
                    lowered.push_str(&rest[..start]);
                    lowered.push_str(target);
                    rest = &rest[end..];
                } else {
                    lowered.push_str(&rest[..end]);
                    rest = &rest[end..];
                }
            }
            lowered.push_str(rest);
            *java = lowered;
        }
    }

    pub(crate) fn add_proven_map_aliases(&mut self, root: tree_sitter::Node<'_>) {
        let Some(provider) = self.semantic_provider else {
            return;
        };
        let package = root
            .named_children(&mut root.walk())
            .find(|node| node.kind() == "package_header")
            .and_then(|header| {
                header
                    .named_children(&mut header.walk())
                    .find(|node| node.kind() == "qualified_identifier")
            })
            .and_then(|node| node.utf8_text(self.source.as_bytes()).ok())
            .unwrap_or_default()
            .replace(' ', "");
        let mut visible = std::collections::BTreeSet::new();
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if node.kind() == "user_type"
                && let Ok(raw) = node.utf8_text(self.source.as_bytes())
            {
                let raw = raw.trim();
                if !raw.contains('.') && !raw.contains('<') {
                    visible.insert(raw.trim_end_matches('?').to_owned());
                }
            }
            pending.extend(node.named_children(&mut node.walk()));
        }
        let mut candidate_aliases = visible
            .iter()
            .filter(|name| provider.type_alias_definition(&package, name).is_some())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        candidate_aliases.extend(
            self.visible_typealiases
                .iter()
                .map(|(name, _)| name)
                .filter(|name| provider.type_alias_definition(&package, name).is_some())
                .cloned(),
        );
        for child in root.named_children(&mut root.walk()) {
            if child.kind() != "import" {
                continue;
            }
            let Some(declaration) = self.text(child).trim().strip_prefix("import ") else {
                continue;
            };
            let (path, alias) = declaration
                .split_once(" as ")
                .map_or((declaration, None), |(path, alias)| {
                    (path.trim(), Some(alias.trim()))
                });
            let Some((target_package, target_name)) = path.rsplit_once('.') else {
                continue;
            };
            if target_package.ends_with('*')
                || provider
                    .type_alias_definition(target_package, target_name)
                    .is_none()
            {
                continue;
            }
            let visible_name = alias.unwrap_or(target_name);
            if visible.contains(visible_name) {
                candidate_aliases.insert(visible_name.to_owned());
            }
        }
        for alias in candidate_aliases {
            let crate::semantics::FactStatus::Established(arguments) =
                provider.map_alias_type_arguments(self.file, &package, &alias)
            else {
                continue;
            };
            self.visible_typealiases
                .retain(|(existing, _)| existing != &alias);
            self.proven_map_typealiases.push((
                alias,
                format!("java.util.Map<{}, {}>", arguments.key, arguments.value),
            ));
        }
    }

    pub(crate) fn lower_proven_map_typealiases(&self, java: &mut String) -> Result<(), String> {
        if self.proven_map_typealiases.is_empty() {
            return Ok(());
        }
        let prefix = "class __NotlinAliasScope {\n";
        let wrapped = format!("{prefix}{java}\n}}");
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .map_err(|error| format!("cannot initialize Java alias lowering: {error}"))?;
        let tree = parser
            .parse(&wrapped, None)
            .ok_or("Java alias lowering did not produce a syntax tree")?;
        if tree.root_node().has_error() {
            return Err("cannot lower proven map aliases in invalid Java candidate syntax".into());
        }
        let aliases = self
            .proven_map_typealiases
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut replacements = Vec::new();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "type_identifier"
                && let Ok(name) = node.utf8_text(wrapped.as_bytes())
                && let Some(target) = aliases.get(name)
                && is_java_type_use(node, name, &wrapped)
            {
                replacements.push((
                    node.start_byte() - prefix.len(),
                    node.end_byte() - prefix.len(),
                    target.clone(),
                ));
            }
            stack.extend(node.named_children(&mut node.walk()));
        }
        replacements.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
        for (start, end, target) in replacements {
            java.replace_range(start..end, &target);
        }
        Ok(())
    }

    fn annotated_interface_parsed_as_expression(&self, node: tree_sitter::Node) -> Option<String> {
        let mut pending = vec![node];
        while let Some(wrapper) = pending.pop() {
            for part in wrapper.children(&mut wrapper.walk()) {
                if part.kind() == "annotated_expression" {
                    pending.push(part);
                } else if part.kind() == "infix_expression"
                    && kt::child(part, "identifier")
                        .is_some_and(|name| self.text(name) == "interface")
                {
                    let name = self
                        .text(part)
                        .trim_start()
                        .strip_prefix("interface")?
                        .trim_start()
                        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                        .next()
                        .filter(|name| !name.is_empty())?;
                    return Some(name.to_string());
                }
            }
        }
        None
    }

    fn annotated_annotation_class_parsed_as_expression(
        &self,
        node: tree_sitter::Node,
    ) -> Option<String> {
        crate::semantics::recovered_annotation_class_name(node, self.source)
    }

    /// Kotlin's parser can represent a stacked class annotation block as a
    /// chain of `annotated_expression` wrappers instead of putting the
    /// annotations in the declaration's `modifiers`. Preserve every wrapper
    /// separately so each one keeps its own argument list (notably for ORM
    /// annotations with string or nested annotation arguments).
    fn transpile_annotation_wrapper_chain(
        &self,
        wrapper: tree_sitter::Node<'tree>,
    ) -> Option<Vec<String>> {
        fn collect(
            unit: &Unit<'_, '_>,
            wrapper: tree_sitter::Node<'_>,
            annotations: &mut Vec<String>,
        ) -> Option<()> {
            if wrapper.kind() != "annotated_expression" {
                return None;
            }
            let mut cursor = wrapper.walk();
            let children = wrapper.children(&mut cursor).collect::<Vec<_>>();
            if children
                .iter()
                .filter(|child| child.kind() == "annotation")
                .count()
                != 1
                || children.iter().any(|child| {
                    child.is_named()
                        && !matches!(
                            child.kind(),
                            "annotation" | "annotated_expression" | "parenthesized_expression"
                        )
                })
            {
                return None;
            }
            annotations.push(unit.transpile_declaration_annotation(wrapper)?);
            for child in children {
                if child.kind() == "annotated_expression" {
                    collect(unit, child, annotations)?;
                }
            }
            Some(())
        }

        let mut annotations = Vec::new();
        collect(self, wrapper, &mut annotations)?;
        Some(annotations)
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
        self.referenced_by_surviving_kotlin(name)
    }

    /// Only surviving Kotlin callers constrain the unchanged function and
    /// companion-call ABI. Property field references are planned separately.
    fn referenced_by_surviving_kotlin(&self, name: &str) -> bool {
        let Some(workspace) = self.workspace else {
            return false;
        };
        let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
        match self.retained_hint {
            None => workspace.has_kotlin_reference(indexed_path, name),
            Some(retained) => workspace.has_surviving_kotlin_reference(
                indexed_path,
                name,
                retained,
                self.translation_roots,
            ),
        }
    }

    fn begin_decl(&mut self, node: tree_sitter::Node<'tree>, label: String) {
        self.declaration_diagnostic_start = self.diags.items.len();
        self.current_decl = Some(node);
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
            if !self.allow_approximations {
                let unsupported: Vec<_> = self.diags.items[self.declaration_diagnostic_start..]
                    .iter()
                    .filter(|diagnostic| {
                        diagnostic.kind == DiagnosticKind::Approximated
                            && crate::diagnostics::classify_approximation(&diagnostic.message)
                                != crate::diagnostics::ApproximationClass::Informational
                    })
                    .map(|diagnostic| diagnostic.message.clone())
                    .collect();
                if !unsupported.is_empty() {
                    self.tainted_declarations.insert(node.id());
                    if self.untranslated_names.insert(label.clone()) {
                        self.coverage.untranslated.push(label.clone());
                    }
                    for message in unsupported {
                        self.coverage.blockers.push((
                            node.start_byte(),
                            format!(
                                "// NOTLIN: {} retained: {message}\n",
                                warning_code(&message)
                            ),
                        ));
                    }
                }
            }
            let tainted = self.tainted_declarations.contains(&node.id());
            if !tainted {
                self.coverage.translated.push(label.clone());
                self.coverage
                    .translated_spans
                    .push((node.start_byte(), node.end_byte()));
                // A preceding annotated_expression wrapper (grammar quirk:
                // `@X(...) decl` parses the annotation as a separate top-level
                // node) belongs to this translated declaration — strip it too
                // or it dangles in the residue and re-binds to the NEXT
                // surviving declaration (kotlinc: "annotation is not
                // repeatable" when the next decl carries the same annotation).
                // Strip every annotated_expression wrapper that preceded this
                // translated declaration (grammar quirk: `@X(...) decl` parses
                // the annotation as a separate top-level node). An unstripped
                // wrapper dangles in the residue and re-binds to the NEXT
                // surviving declaration (kotlinc: "annotation is not
                // repeatable" when that decl carries the same annotation).
                if let Some(ranges) = self.wrapper_spans.remove(&node.id()) {
                    self.coverage.translated_spans.extend(ranges);
                }
            }
            self.decl_labels.remove(&node.id());
        }
    }

    pub(crate) fn taint_decl(&mut self, label: &str) {
        if let Some(declaration) = self.current_decl {
            self.tainted_declarations.insert(declaration.id());
        }
        if self.untranslated_names.insert(label.to_string()) {
            self.coverage.untranslated.push(label.to_string());
        }
    }

    fn is_untranslated(&self, label: &str) -> bool {
        self.untranslated_names.contains(label)
    }

    /// True when `name` is a top-level function declared in THIS file whose
    /// declaration remained Kotlin (labeled untranslated). Bare calls to it
    /// from translated declarations cannot resolve in Java.
    pub(crate) fn is_untranslated_file_function(&self, name: &str) -> bool {
        if self.retained_file_functions.contains(name) {
            return true;
        }
        self.top_level_functions.contains(name) && self.is_untranslated(name)
    }

    pub fn text(&self, node: tree_sitter::Node<'_>) -> &'src str {
        kt::text(node, self.source)
    }

    fn retention_context(&self, node: tree_sitter::Node<'_>) -> Option<String> {
        let mut method = None;
        let mut ancestor = Some(node);
        while let Some(current) = ancestor {
            if method.is_none() && current.kind() == "function_declaration" {
                method = kt::field(current, "name").map(|name| self.text(name).to_string());
            }
            if let Some(name) = self.decl_labels.get(&current.id()) {
                let declaration = match current.kind() {
                    "function_declaration" => format!("function {name}()"),
                    "property_declaration" => format!("property {name}"),
                    _ => self.retained_declaration_label(name),
                };
                return Some(match method {
                    Some(method) if current.kind() != "function_declaration" => {
                        format!("affected declaration: {declaration}; source method: {method}()")
                    }
                    _ => format!("affected declaration: {declaration}"),
                });
            }
            ancestor = current.parent();
        }
        None
    }

    pub fn diag_untranslatable(&mut self, node: tree_sitter::Node, msg: impl Into<String>) {
        self.diag_untranslatable_coded(node, msg, None);
    }

    /// As [`Self::diag_untranslatable`], but with an explicit code instead of
    /// one hashed from the message. A retention warning names the specific
    /// element it blocked on — so its message varies per declaration — while
    /// its code has to identify the blocker TYPE: that is what keeps the code
    /// a human greps for in the log identical to the run-end table row, and the
    /// table a worklist instead of a per-declaration dump.
    pub fn diag_untranslatable_coded(
        &mut self,
        node: tree_sitter::Node,
        msg: impl Into<String>,
        code: Option<String>,
    ) {
        let sev = if self.untranslatable_as_error {
            crate::diagnostics::Severity::Error
        } else {
            crate::diagnostics::Severity::Warning
        };

        // Taint the enclosing declaration (if any) so --in-place migration
        // knows this declaration must stay in the .kt file.
        let mut marker_anchor = node.start_byte();
        {
            // The diagnostic node itself, then walk up through the AST: the
            // nearest enclosing declaration node that began via begin_decl
            // (tracked by decl_labels). Handles both top-level decls (diag
            // anchored on the decl or a member of it) and members nested in
            // class bodies.
            let mut ancestor = Some(node);
            let mut in_modifiers = node.kind() == "annotation";
            while let Some(n) = ancestor {
                in_modifiers |= n.kind() == "modifiers";
                if self.decl_labels.contains_key(&n.id()) {
                    let label = self.decl_labels.get(&n.id()).cloned().unwrap_or_default();
                    self.taint_decl(&label);
                    if in_modifiers {
                        marker_anchor = n.start_byte();
                    }
                    break;
                }
                ancestor = n.parent();
            }
        }
        // Record a blocker stub (`// NOTLIN: <code> <message>`), anchored at
        // the diagnostic node's byte offset — migrate.rs emits these comments
        // in --in-place residue when the surrounding declaration is stripped.
        let message: String = msg.into();
        let blocker_code = code.clone().unwrap_or_else(|| warning_code(&message));
        let residue_message = if message.contains("retained") && !message.starts_with("retained ") {
            self.retention_context(node)
                .map(|context| format!("{message}; {context}"))
                .unwrap_or_else(|| message.clone())
        } else {
            message.clone()
        };
        self.coverage.blockers.push((
            marker_anchor,
            format!("// NOTLIN: {blocker_code} {residue_message}\n"),
        ));

        self.diags.push(crate::diagnostics::Diagnostic {
            severity: sev,
            kind: DiagnosticKind::Untranslatable,
            message,
            code,
            file: self.file.to_path_buf(),
            line: node.start_position().row + 1,
            col: node.start_position().column + 1,
        });
    }

    fn retained_declaration_label(&self, name: &str) -> String {
        let current = self
            .workspace
            .and_then(|workspace| {
                let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
                workspace.source_file(indexed_path)
            })
            .and_then(|source_file| {
                source_file
                    .declarations
                    .iter()
                    .find(|declaration| declaration.name == name)
            });
        match current.map(|declaration| declaration.kind) {
            Some(crate::workspace::DeclarationKind::Class) => format!("class {name}"),
            Some(crate::workspace::DeclarationKind::Interface) => format!("interface {name}"),
            Some(crate::workspace::DeclarationKind::Enum) => format!("enum {name}"),
            Some(crate::workspace::DeclarationKind::Object) => format!("object {name}"),
            Some(crate::workspace::DeclarationKind::Record) => format!("record {name}"),
            Some(crate::workspace::DeclarationKind::Annotation) => {
                format!("annotation class {name}")
            }
            None => format!("declaration {name}"),
        }
    }

    fn retained_blocker_label(&self, name: &str) -> String {
        let Some(workspace) = self.workspace else {
            return format!("declaration {name}");
        };
        let mut labels = workspace
            .declarations_named(name)
            .map(|declaration| match declaration.kind {
                crate::workspace::DeclarationKind::Class => "class",
                crate::workspace::DeclarationKind::Interface => "interface",
                crate::workspace::DeclarationKind::Enum => "enum",
                crate::workspace::DeclarationKind::Object => "object",
                crate::workspace::DeclarationKind::Record => "record",
                crate::workspace::DeclarationKind::Annotation => "annotation class",
            })
            .collect::<Vec<_>>();
        labels.sort_unstable();
        labels.dedup();
        if labels.len() == 1 {
            format!("{} {name}", labels[0])
        } else {
            format!("declaration {name}")
        }
    }

    /// A declaration that has to stay Kotlin because residual Kotlin still
    /// needs it: record it for the run-end table AND print the warning, both
    /// from the same kind — so the code a human reads in the table row is the
    /// code they can grep for in the log. `params` instantiate the kind's
    /// detail template (the specific blocked element); `blockers` names the
    /// retained declarations this one waits on, so the table can credit a root
    /// kind with the whole cascade it holds back.
    pub(crate) fn retain_decl(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        kind: RetentionKind,
        params: &[String],
        blockers: &[String],
    ) {
        let declaration = self.retained_declaration_label(name);
        let blocker_labels = blockers
            .iter()
            .map(|blocker| self.retained_blocker_label(blocker))
            .collect::<Vec<_>>();
        let blocker_details = blockers
            .iter()
            .cloned()
            .zip(blocker_labels.iter().cloned())
            .collect::<Vec<_>>();
        crate::diagnostics::record_retention(
            kind,
            params,
            self.file,
            decl.start_position().row + 1,
            &declaration,
            name,
            &blocker_details,
        );
        self.diag_untranslatable_coded(
            decl,
            crate::diagnostics::retention_site_message(&declaration, kind, params, &blocker_labels),
            Some(kind.code()),
        );
    }

    pub(crate) fn diag_approx(&mut self, node: tree_sitter::Node, msg: impl Into<String>) {
        // Console diagnostic (compiler-style listing)…
        let message = msg.into();
        if !self.allow_approximations
            && crate::diagnostics::classify_approximation(&message)
                != crate::diagnostics::ApproximationClass::Informational
        {
            self.diag_untranslatable(node, format!("retained to avoid approximation: {message}"));
            return;
        }
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

    fn kotlin_import_to_java(&self, node: tree_sitter::Node<'tree>) -> String {
        let raw = self.text(node).trim();
        let path = raw
            .strip_prefix("import")
            .map(str::trim)
            .unwrap_or(raw)
            .trim_end_matches(';')
            .trim();
        let imported_path = path
            .split_once(" as ")
            .map_or(path, |(path, _)| path.trim());
        // Kotlin imports can name top-level properties and functions. Their
        // callers use a qualified facade after translation, while a use that
        // still needs its Kotlin owner keeps this unit out of Java emission.
        // Omit the import instead of rendering `import pkg.value` as an invalid
        // Java type import (or an accidental static member import).
        if self.is_indexed_top_level_property_or_function_import(imported_path) {
            return String::new();
        }
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
                    w.declarations_named(last)
                        .any(|d| d.kind == crate::workspace::DeclarationKind::Enum)
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

    fn is_indexed_top_level_property_or_function_import(&self, path: &str) -> bool {
        let Some((package, name)) = path.rsplit_once('.') else {
            return false;
        };
        let Some(provider) = self.semantic_provider else {
            return false;
        };
        let symbols = provider.symbols_named(name);
        // A Kotlin factory and a classifier may share the imported spelling.
        // Keep the Java type import when it also names an actual classifier.
        if symbols.iter().any(|symbol| {
            symbol.id.owner_path.is_empty()
                && symbol.id.package == package
                && matches!(
                    symbol.id.kind.as_str(),
                    "class" | "interface" | "object" | "enum" | "annotation"
                )
        }) {
            return false;
        }
        symbols.into_iter().any(|symbol| {
            matches!(symbol.id.kind.as_str(), "property" | "function")
                && symbol.id.owner_path.is_empty()
                && symbol.id.package == package
                && symbol.id.file != self.file
                && (symbol.id.kind != "function"
                    || symbol
                        .id
                        .file
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("kt")))
        })
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
        if let Some(workspace) = self.workspace
            && workspace
                .declarations_named(parent_ty)
                .any(|decl| decl.kind == crate::workspace::DeclarationKind::Enum)
        {
            return true;
        }
        self.enum_types.contains(parent_ty)
    }
    pub fn run(&mut self, root: tree_sitter::Node<'tree>) -> Vec<(String, String)> {
        let plan = crate::translation_plan::analyze(self.source, root, self.file);
        self.run_planned(root, &plan)
    }

    pub(crate) fn run_planned(
        &mut self,
        root: tree_sitter::Node<'tree>,
        plan: &crate::translation_plan::TranslationPlan,
    ) -> Vec<(String, String)> {
        // Collect top-level structure
        assert_eq!(
            plan.source_hash, self.source_hash,
            "Java preparation requires the planned snapshot"
        );
        self.enum_when_lowerings = plan
            .enum_when_lowerings
            .iter()
            .map(|lowering| (lowering.location.start_byte, lowering.clone()))
            .collect();
        self.default_getter_bridges = plan.default_getter_bridges.clone();
        self.todo_lowerings = plan
            .todo_lowerings
            .iter()
            .map(|lowering| (lowering.location.start_byte, lowering.clone()))
            .collect();
        self.reference_string_calls = plan
            .reference_string_calls
            .iter()
            .map(|fact| {
                (
                    (fact.location.start_byte, fact.location.end_byte),
                    fact.clone(),
                )
            })
            .collect();
        self.container_calls = plan
            .container_calls
            .iter()
            .map(|fact| {
                (
                    (fact.location.start_byte, fact.location.end_byte),
                    fact.clone(),
                )
            })
            .collect();
        self.exact_overload_calls = plan
            .exact_overload_calls
            .iter()
            .map(|fact| {
                (
                    (fact.location.start_byte, fact.location.end_byte),
                    fact.clone(),
                )
            })
            .collect();
        self.container_lambdas = plan
            .container_calls
            .iter()
            .filter_map(|call| call.lambda_binding.as_ref())
            .map(|binding| {
                (
                    (binding.location.start_byte, binding.location.end_byte),
                    binding.clone(),
                )
            })
            .collect();
        self.container_property_accesses = self
            .container_lambdas
            .values()
            .flat_map(|binding| binding.property_accesses.iter())
            .map(|access| {
                (
                    (access.location.start_byte, access.location.end_byte),
                    access.clone(),
                )
            })
            .collect();
        self.eager_collection_calls = plan
            .eager_collection_calls
            .iter()
            .map(|fact| {
                (
                    (fact.location.start_byte, fact.location.end_byte),
                    fact.clone(),
                )
            })
            .collect();
        self.eager_collection_helpers = plan.eager_collection_helpers.clone();
        self.reified_class_calls = plan
            .reified_class_calls
            .iter()
            .map(|fact| {
                (
                    (fact.location.start_byte, fact.location.end_byte),
                    fact.clone(),
                )
            })
            .collect();
        self.constructor_delegations = plan
            .constructor_delegations
            .iter()
            .map(|fact| {
                (
                    (fact.location.start_byte, fact.location.end_byte),
                    fact.clone(),
                )
            })
            .collect();
        self.import_alias_targets.clear();
        let mut package = String::new();
        let mut imports: Vec<String> = Vec::new();
        let mut decls: Vec<tree_sitter::Node> = Vec::new();
        let mut standalone_annotation_targets = std::collections::HashMap::new();
        let mut file_jvm_name: Option<String> = None;
        let mut retain_next_declaration = false;
        let mut retained_annotations: Vec<String> = Vec::new();
        // Java-native annotations hoisted from top-level annotated_expression
        // wrappers (grammar quirk) waiting for the next declaration.
        let mut hoisted_annotations: Vec<String> = Vec::new();
        // Byte ranges of wrappers seen since the last declaration; donated to
        // the NEXT declaration so end_decl can strip them with it.
        let mut wrapper_spans_for_next: Vec<(usize, usize)> = Vec::new();

        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            match child.kind() {
                "package_header" => {
                    if let Some(qi) = kt::child(child, "qualified_identifier") {
                        package = self.text(qi).replace(" ", "");
                    }
                }
                "import" => {
                    let alias = self
                        .text(child)
                        .trim()
                        .strip_prefix("import ")
                        .and_then(|import| import.split_once(" as "))
                        .and_then(|(path, alias)| {
                            Some((
                                alias.trim().to_owned(),
                                path.trim().rsplit('.').next()?.to_owned(),
                            ))
                        });
                    if let Some((alias, target)) = alias {
                        self.import_alias_targets.insert(alias, target);
                    }
                    let imp = self.kotlin_import_to_java(child);
                    if !imp.is_empty() {
                        imports.push(imp);
                    }
                }
                "shebang_line" | ";" | "line_comment" | "multiline_comment" | "block_comment" => {}
                "file_annotation" | "annotated_expression" => {
                    // Queue parser-recovered interfaces for the normal
                    // declaration-dispatch pass below. That pass runs after
                    // class-body facts and plans have been prepared.
                    if child.kind() == "annotated_expression"
                        && (self
                            .annotated_interface_parsed_as_expression(child)
                            .is_some()
                            || self
                                .annotated_annotation_class_parsed_as_expression(child)
                                .is_some())
                    {
                        decls.push(child);
                        wrapper_spans_for_next.clear();
                        hoisted_annotations.clear();
                        retain_next_declaration = false;
                        continue;
                    }
                    let annotation = self.text(child).trim();
                    if let Some(name) = annotation
                        .strip_prefix("@file:JvmName(\"")
                        .and_then(|tail| tail.split_once("\")").map(|(name, _)| name))
                        .filter(|name| !name.is_empty())
                    {
                        file_jvm_name = Some(name.to_string());
                        continue;
                    }
                    // Grammar quirk: a leading top-level `@X("v") object T`
                    // parses the annotation into a SEPARATE
                    // annotated_expression (the object lands outside it).
                    // Lower the annotation via the passthrough helper: when
                    // it is Java-native, buffer the text and prepend it to
                    // the NEXT declaration's annotations; when it carries
                    // Kotlin-only syntax, retain as before. Either way the
                    // wrapper byte span is remembered so end_decl can strip
                    // it with the following declaration when THAT translates
                    // (an unstripped wrapper dangles in the residue and
                    // re-binds to the NEXT surviving declaration).
                    wrapper_spans_for_next.push((child.start_byte(), child.end_byte()));
                    match self.transpile_annotation_wrapper_chain(child) {
                        Some(annotations) => hoisted_annotations.extend(annotations),
                        None => {
                            retained_annotations.push(annotation.to_string());
                            retain_next_declaration = true;
                        }
                    }
                }
                k if k.contains("declaration")
                    || k == "object_declaration"
                    || k == "class_declaration"
                    || k == "function_declaration"
                    || k == "property_declaration" =>
                {
                    if retain_next_declaration {
                        standalone_annotation_targets
                            .insert(child.id(), std::mem::take(&mut retained_annotations));
                        retain_next_declaration = false;
                    }
                    if !wrapper_spans_for_next.is_empty() {
                        // Every wrapper preceding this declaration maps to it,
                        // so end_decl can strip the whole group (Some hoists
                        // AND None taints alike).
                        let donated = std::mem::take(&mut wrapper_spans_for_next);
                        self.wrapper_spans
                            .entry(child.id())
                            .or_default()
                            .extend(donated);
                    }
                    if !hoisted_annotations.is_empty() {
                        let drained = std::mem::take(&mut hoisted_annotations);
                        self.hoisted_annotations
                            .entry(child.id())
                            .or_default()
                            .extend(drained);
                    }
                    decls.push(child);
                }
                _ => {
                    self.diag_untranslatable(
                        child,
                        format!("top-level construct not supported: {}", child.kind()),
                    );
                    self.coverage
                        .untranslated
                        .push(format!("top-level@{}", child.start_byte()));
                }
            }
        }

        // Pre-pass: superclass -> subclass relations drive `permits` emission
        // for sealed classes and `final` on their subclasses.
        self.collect_type_relations(root);
        // Preflight blockers must be visible even to declarations emitted
        // before their callees. Name lookup remains conservative until calls
        // carry resolved symbol identities.
        for decl in &decls {
            if decl.kind() == "function_declaration"
                && plan
                    .retain_reasons_at(decl.start_byte())
                    .is_some_and(|reasons| !reasons.is_empty())
                && let Some(name) = self.top_level_name(*decl)
            {
                self.retained_file_functions.insert(name);
            }
        }

        // Partition: named types each get their own file; loose functions and
        // top-level properties go into ONE file named after the .kt source.
        let mut files: Vec<(String, String)> = Vec::new();

        for decl in &decls {
            match decl.kind() {
                "class_declaration" | "object_declaration" | "annotated_expression" => {
                    let original_decl = *decl;
                    let recovery_name = if original_decl.kind() == "annotated_expression" {
                        self.annotated_interface_parsed_as_expression(original_decl)
                            .or_else(|| {
                                self.annotated_annotation_class_parsed_as_expression(original_decl)
                            })
                    } else {
                        None
                    };
                    let recovered_tree = recovery_name.as_ref().and_then(|_| {
                        crate::semantics::recover_annotated_interface_tree(
                            self.source,
                            original_decl,
                        )
                    });
                    let recovered_decl = recovery_name.as_ref().and_then(|_| {
                        recovered_tree.as_ref().and_then(|tree| {
                            crate::semantics::recovered_annotated_interface_decl(
                                self.source,
                                original_decl,
                                tree.root_node(),
                            )
                        })
                    });
                    if let Some(name) = recovery_name.as_ref().filter(|_| recovered_decl.is_none())
                    {
                        self.diag_untranslatable_coded(
                            original_decl,
                            format!(
                                "annotated top-level expression contains an unparsed declaration; retained in Kotlin; affected declaration: {name}"
                            ),
                            Some("N47BC".to_string()),
                        );
                        self.coverage
                            .untranslated
                            .push(format!("top-level@{}", original_decl.start_byte()));
                        continue;
                    }
                    let declaration = recovered_decl.unwrap_or(original_decl);
                    let type_name = kt::field(declaration, "name")
                        .map(|n| self.text(n).to_string())
                        .unwrap_or_else(|| "Anonymous".to_string());
                    let mut out = JavaOut::new();
                    let imports2 = imports.clone();
                    let package2 = package.clone();
                    self.begin_decl(original_decl, type_name.clone());
                    if let Some(recovered) = recovered_decl {
                        // Diagnostics from the recovered tree follow its own
                        // parent chain; map that root to this transaction.
                        self.decl_labels.insert(recovered.id(), type_name.clone());
                    }
                    if self.apply_preflight_retention(original_decl, plan) {
                        self.end_decl();
                        if let Some(recovered) = recovered_decl {
                            self.decl_labels.remove(&recovered.id());
                        }
                        continue;
                    }
                    if let Some(retained) = standalone_annotation_targets.get(&original_decl.id()) {
                        for annotation in retained {
                            self.diag_untranslatable_coded(
                                original_decl,
                                format!(
                                    "declaration annotation is retained in Kotlin; annotation: `{annotation}`; its type or argument expression cannot yet be lowered in this run"
                                ),
                                Some(crate::diagnostics::warning_code(
                                    "declaration annotation is retained in Kotlin",
                                )),
                            );
                        }
                        self.end_decl();
                        if let Some(recovered) = recovered_decl {
                            self.decl_labels.remove(&recovered.id());
                        }
                        continue;
                    }
                    // Mixed-language accessor ABI: an abstract member of a
                    // RETAINED Kotlin supertype whose Java-visible return type
                    // differs from this class's own member of the same name
                    // cannot be satisfied by a Java class (Java has no
                    // covariant cross-language absolvability) — retain.
                    if let Some(ws) = self.workspace {
                        let supers: Vec<String> = kt::child(declaration, "delegation_specifiers")
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
                        let declaring = self.workspace_file.as_deref().unwrap_or(self.file);
                        let retained = self.retained_hint.cloned().unwrap_or_default();
                        let mut conflicts = ws
                            .retained_supertype_member_conflicts_in_file(
                                declaring, &supers, &type_name,
                            )
                            .into_iter()
                            .filter(|conflict| conflict.blocks_java_override())
                            .filter(|conflict| {
                                !ws.selected_covariant_property_conflict_is_safe(
                                    declaring,
                                    conflict,
                                    self.translation_roots,
                                    &retained,
                                )
                            })
                            .map(|conflict| conflict.detail())
                            .collect::<Vec<_>>();
                        conflicts.sort();
                        conflicts.dedup();
                        if !conflicts.is_empty() {
                            // Erasing the member to a raw type would make the
                            // override compile, but a raw `List`/`Set` loses
                            // its element type: JPA rejects it outright
                            // ("declared with a raw type and has an explicit
                            // targetEntity") and every generic consumer loses
                            // the check. So the class stays Kotlin instead and
                            // the conflict is reported for a human. The
                            // conflicting members travel as PARAMS: one kind,
                            // one code, however many member lists turn up.
                            self.retain_decl(
                                declaration,
                                &type_name,
                                RetentionKind::SupertypeMemberType,
                                &conflicts,
                                &[],
                            );
                            self.end_decl();
                            if let Some(recovered) = recovered_decl {
                                self.decl_labels.remove(&recovered.id());
                            }
                            continue;
                        }
                    }
                    self.transpile_type_decl_set(&mut out, &package2, &imports2, |unit, out| {
                        unit.transpile_type_decl(declaration, out);
                    });
                    self.end_decl();
                    if let Some(recovered) = recovered_decl {
                        self.decl_labels.remove(&recovered.id());
                    }
                    if !self.tainted_declarations.contains(&original_decl.id()) {
                        let path = format!("{}.java", type_name);
                        self.output_owners.insert(
                            path.clone(),
                            plan.declarations
                                .iter()
                                .filter(|d| d.id.start_byte == original_decl.start_byte())
                                .map(|d| d.symbol_id.clone())
                                .collect(),
                        );
                        files.push((path, out.finish()));
                    } else {
                        log::debug!(
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
            let mut file_class_name = file_jvm_name.clone().unwrap_or_else(|| {
                self.file
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Main".to_string())
            });
            file_class_name = file_class_name
                .chars()
                .map(|c| if c == '-' || c == '.' { '_' } else { c })
                .collect();
            let mut chars = file_class_name.chars();
            if let Some(first) = chars.next() {
                file_class_name = first.to_uppercase().collect::<String>() + chars.as_str();
            }
            file_class_name = crate::function_callsite::non_conflicting_facade_name(
                self.source,
                &file_class_name,
            );

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
                    // Each member lowers into its own transaction. A late
                    // blocker cannot leak a partial method into a clean facade.
                    let mut member = JavaOut::new();
                    member.indent = out.indent;
                    if unit.apply_preflight_retention(**decl, plan) {
                        // The plan already owns this declaration in Kotlin.
                    } else {
                        match decl.kind() {
                            "function_declaration" => {
                                let is_main = kt::field(**decl, "name")
                                    .map(|n| unit.text(n) == "main")
                                    .unwrap_or(false);
                                unit.transpile_function_opts(
                                    **decl,
                                    true,
                                    true,
                                    is_main,
                                    &mut member,
                                );
                                member.blank();
                            }
                            "property_declaration" => {
                                unit.transpile_toplevel_property(
                                    **decl,
                                    &mut member,
                                    &file_class_name,
                                );
                                member.blank();
                            }
                            _ => {}
                        }
                    }
                    unit.end_decl();
                    if !unit.tainted_declarations.contains(&decl.id()) {
                        out.buf.push_str(&member.finish());
                    }
                }
                out.close();
            });
            // The file-level utility class is emitted only if at least one
            // loose declaration survived (clean members are in `translated`).
            let clean_count = loose
                .iter()
                .filter(|d| !self.tainted_declarations.contains(&d.id()))
                .count();
            if clean_count > 0 {
                let path = format!("{}.java", file_class_name);
                self.output_owners.insert(
                    path.clone(),
                    loose
                        .iter()
                        .filter(|decl| !self.tainted_declarations.contains(&decl.id()))
                        .filter_map(|decl| {
                            plan.declarations
                                .iter()
                                .find(|d| d.id.start_byte == decl.start_byte())
                                .map(|d| d.symbol_id.clone())
                        })
                        .collect(),
                );
                files.push((path, out.finish()));
            } else {
                log::debug!("all top-level members untranslatable; skipping utility class");
            }
        }

        files
    }

    fn apply_preflight_retention(
        &mut self,
        declaration: tree_sitter::Node,
        plan: &crate::translation_plan::TranslationPlan,
    ) -> bool {
        let reasons = plan
            .retain_reasons_at(declaration.start_byte())
            .unwrap_or(&[]);
        if reasons.is_empty() {
            return false;
        }
        for reason in reasons {
            self.diag_untranslatable_coded(
                declaration,
                reason.message(),
                Some(reason.code().to_string()),
            );
        }
        true
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
        // Comments are trivia, not code: a `//` line between two statements
        // must never taint the enclosing declaration as untranslatable.
        if matches!(stmt.kind(), "line_comment" | "block_comment") {
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proven_map_alias_lowering_preserves_names_literals_and_generic_scopes() {
        let mut diags = Diagnostics::new();
        let mut unit = Unit::new(
            "",
            Path::new("Aliases.kt"),
            &mut diags,
            AnnotationSet::None,
            UnitOptions::default(),
        );
        unit.proven_map_typealiases.push((
            "ResultMap".into(),
            "java.util.Map<java.lang.String, java.lang.Integer>".into(),
        ));
        let mut candidate = r#"class Values {
    ResultMap field;
    String ResultMap = "ResultMap";
    <ResultMap> ResultMap echo(ResultMap value) { return value; }
    class Inner<ResultMap> { ResultMap item; }
    java.util.List<ResultMap> all() { return null; }
}"#
        .to_owned();
        unit.lower_proven_map_typealiases(&mut candidate).unwrap();
        assert!(candidate.contains("java.util.Map<java.lang.String, java.lang.Integer> field;"));
        assert!(candidate.contains("String ResultMap = \"ResultMap\";"));
        assert!(candidate.contains("<ResultMap> ResultMap echo(ResultMap value)"));
        assert!(candidate.contains("class Inner<ResultMap> { ResultMap item; }"));
        assert!(
            candidate.contains(
                "java.util.List<java.util.Map<java.lang.String, java.lang.Integer>> all()"
            )
        );
    }

    #[test]
    fn proven_map_alias_lowering_rejects_invalid_candidate_syntax() {
        let mut diags = Diagnostics::new();
        let mut unit = Unit::new(
            "",
            Path::new("Aliases.kt"),
            &mut diags,
            AnnotationSet::None,
            UnitOptions::default(),
        );
        unit.proven_map_typealiases.push((
            "ResultMap".into(),
            "java.util.Map<java.lang.String, java.lang.Integer>".into(),
        ));
        let mut candidate = "public class Broken { ResultMap value( }".to_owned();
        let original = candidate.clone();
        assert!(unit.lower_proven_map_typealiases(&mut candidate).is_err());
        assert_eq!(candidate, original, "failed lowering must be transactional");
    }

    #[test]
    fn untranslated_membership_deduplicates_while_preserving_coverage_order() {
        let source = "";
        let file = Path::new("coverage.kt");
        let mut diags = Diagnostics::new();
        let mut unit = Unit::new(
            source,
            file,
            &mut diags,
            AnnotationSet::None,
            UnitOptions::default(),
        );

        unit.taint_decl("First");
        unit.taint_decl("Second");
        unit.taint_decl("First");

        assert!(unit.is_untranslated("First"));
        assert!(unit.is_untranslated("Second"));
        assert!(!unit.is_untranslated("Third"));
        assert_eq!(unit.coverage.untranslated, ["First", "Second"]);
    }
}
