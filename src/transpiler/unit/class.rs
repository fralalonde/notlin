//! Class/object/interface/record emission.

use super::Unit;
use super::capitalize;
use crate::diagnostics::{DiagnosticKind, RetentionKind};
use crate::transpiler::expr::Expr;
use crate::transpiler::java::JavaOut;
use crate::transpiler::kt;

/// Retention closure over the type hierarchy: a retained declaration forces
/// every declaration it shares a hierarchy edge with to stay Kotlin. It exists
/// because the translator refuses mixed-language hierarchies, so it is ON by
/// default and can be switched off (`NOTLIN_RETENTION_HIERARCHY=off`) to probe
/// what the closure is actually buying.
fn hierarchy_closure() -> bool {
    !std::env::var("NOTLIN_RETENTION_HIERARCHY").is_ok_and(|value| value == "off")
}

/// One companion fn captured for the nested `Companion` bridge.
struct BridgeSig {
    signature: String,
    call: String,
}

impl<'src, 'tree> Unit<'src, 'tree> {
    fn declaration_is_enum(&self, decl: tree_sitter::Node<'tree>) -> bool {
        let Some(modifiers) = kt::child(decl, "modifiers") else {
            return false;
        };
        modifiers
            .children(&mut modifiers.walk())
            .filter(|modifier| modifier.kind() == "class_modifier")
            .flat_map(|modifier| modifier.children(&mut modifier.walk()).collect::<Vec<_>>())
            .any(|token| self.text(token) == "enum")
    }

    fn declaration_references_kclass(&self, decl: tree_sitter::Node<'tree>) -> bool {
        let mut pending = vec![decl];
        while let Some(node) = pending.pop() {
            if node.kind() == "user_type"
                && self
                    .text(node)
                    .split(|character: char| !character.is_alphanumeric() && character != '_')
                    .any(|part| part == "KClass")
            {
                return true;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        false
    }

    pub(crate) fn collect_type_relations(&mut self, root: tree_sitter::Node<'tree>) {
        let mut stack: Vec<tree_sitter::Node<'tree>> = vec![root];
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

    fn superclass_name(&self, decl: tree_sitter::Node<'tree>) -> Option<String> {
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
        let mut last: Option<tree_sitter::Node<'tree>> = None;
        for ch in ut.children(&mut ut.walk()) {
            if ch.kind() == "identifier" {
                last = Some(ch);
            }
        }
        last.map(|n| self.text(n).to_string())
    }

    fn decl_contains(&self, decl: tree_sitter::Node<'tree>, name: &str) -> bool {
        let mut stack: Vec<tree_sitter::Node<'tree>> = vec![decl];
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
            normalized_source_path(self.file)
        ));
        if !package.is_empty() {
            out.line(format!("package {};", package));
            out.blank();
        }
        let declaring_file = self.workspace_file.as_deref().unwrap_or(self.file);
        let own_names: Vec<String> = self
            .workspace
            .map(|workspace| workspace.decl_names_in_file(declaring_file))
            .unwrap_or_default();
        let import_anchor = out.buf.len();
        f(self, out);
        // Take the body out before anything rewrites the buffer as a whole.
        // `lower_visible_typealiases` substitutes every occurrence of an alias
        // name, and the generated header carries the source path (`// NOTLIN:
        // generated from …/ktor/src/…`), where an alias spelled like a path
        // fragment (`io`, `ktor`) matches and is replaced by a shorter target.
        // The buffer then ends up shorter than the anchor and slicing panics.
        // The header is a generated comment plus the package name, so nothing
        // there needs lowering anyway.
        let body = out.buf[import_anchor..].to_string();
        let mut lowered = body.clone();
        self.lower_visible_typealiases(&mut lowered);
        let body = lowered;
        // Imports are chosen AFTER the body exists: a source file can be split
        // (some declarations translate, others stay Kotlin), and the retained
        // Kotlin keeps its own `import com.example.KotlinOnlyFn` lines. Carrying
        // such an import into the generated Java is a hard javac error
        // ("cannot find symbol: class KotlinOnlyFn") even though the generated
        // code never mentions it. Emit only imports this Java actually uses.
        // Kotlin resolves a simple name to its import; Java gives the class's
        // OWN name priority inside its own body. So `class Length` with
        // `import javax.measure.quantity.Length` and a member typed
        // `Unit<Length>` (bound: `Unit<Q : Quantity<Q>>`) compiles as Kotlin
        // but javac rejects the type argument after migration — it reads the
        // enclosing `Length`. Qualify the colliding name with its imported
        // FQN inside type-argument lists, where the two resolutions differ.
        let qualified = qualify_shadowing_type_arguments(&body, &own_names, imports);
        // Always rewrite the body region: the alias lowering above already
        // produced a replacement string, so the buffer's copy is stale.
        out.buf.truncate(import_anchor);
        out.buf.push_str(&qualified);
        let body = qualified;
        let mut block = String::new();
        for imp in imports {
            if !imp.is_empty() {
                // Kotlin file-level declarations shadow single imports of the
                // same simple name (`class Length` + `import …quantity.Length`
                // is legal Kotlin, file decl wins) — javac rejects the import
                // with "X is already defined in this compilation unit". Drop
                // the colliding import: the same-file declaration wins every
                // plain-name use site (Kotlin resolution semantics).
                let simple = imp.rsplit('.').next().unwrap_or(imp);
                let collides = imports.iter().any(|other| {
                    other.rsplit('.').next() == Some(simple) && other.rsplit('.').count() == 1
                }) || own_names.iter().any(|n| n == simple);
                if collides || !import_is_referenced(imp, &body) {
                    continue;
                }
                block.push_str(&format!("import {};\n", imp));
            }
        }
        // Under --lombok the emitted @Data/@AllArgsConstructor need their
        // imports; user-declared lombok imports (Lombok-flagged source) may
        // already provide some — add only what's missing, exactly once.
        if self.lombok {
            for want in [
                "lombok.Data",
                "lombok.Value",
                "lombok.NonNull",
                "lombok.AllArgsConstructor",
                "lombok.EqualsAndHashCode",
            ] {
                if !imports.iter().any(|i| i == want) {
                    block.push_str(&format!("import {};\n", want));
                }
            }
        }
        // The generated body uses ArrayList/HashMap/HashSet/List/Map/Set from
        // stdlib collections; java.util.* covers them all in one line.
        block.push_str("import java.util.*;\n");
        block.push_str("import java.util.stream.Stream;\n");
        block.push('\n');
        if let Some(pkg) = crate::transpiler::types::nullable_import(self.annots) {
            block.push_str(&format!("import {}.*;\n", pkg));
            block.push('\n');
        }
        out.buf.insert_str(import_anchor, &block);
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
        // `[` is Kotlin's array literal, and it IS Java-expressible
        // (`{a, b}`) — but lowering it is not free. On the real target the 12
        // declarations retained solely for an array argument turned out to have
        // bodies Java cannot express yet (calls to accessors of retained Kotlin
        // classes, `log` fields, constructor arities): 96 distinct javac
        // errors. The annotation is only preserved once those classes are
        // translatable, so `[` keeps tainting until then.
        if argument_text.contains("${")
            || argument_text.contains('[')
            || argument_text.contains('{')
            || argument_text.contains("-> ")
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
                        ws.declarations_named(simple).any(|d| {
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
        // An unselected Kotlin annotation type retains its declaration. The
        // original reason recorded here — "the Java side cannot reference a
        // Kotlin-only element" — is not sound: a Kotlin `annotation class`
        // compiles to a Java `@interface` on the same module's classpath, so
        // javac can name it.
        //
        // The clause is kept because the conservative behaviour is the measured
        // one: this rule is one of several standing in for "this declaration's
        // surroundings are not translated", and in this corpus removing it
        // changed nothing observable (the 96 javac errors seen while probing
        // were traced to the array-literal case, not to this clause). Loosening
        // it needs its own measurement on a compiling tree, not an argument.
        if let Some(workspace) = self.workspace
            && workspace.declarations_named(&name).any(|decl| {
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

    /// Why this declaration cannot be translated away, or `None` when it can.
    /// The reason is part of the diagnostic so a run log is greppable by
    /// cause, not just by "retained".
    ///
    /// The pair is (blocker KIND, params): the kind is one of the fixed
    /// vocabulary entries in `RetentionKind` — it yields the stable code and
    /// the summary row — and params instantiate that kind's template with the
    /// specific element (the conflicting members, ...). The second half is the
    /// BLOCKERS: the retained declarations that have to translate first for
    /// this one to follow. Empty means the blocker is intrinsic — a fact about
    /// the source no other translation can clear — which is what makes it a
    /// root cause in the run-end report; a non-empty list makes this
    /// declaration fallout, to be credited to the roots behind those names
    /// rather than to a human.
    fn kotlin_retention_reason(
        &self,
        name: &str,
        decl: tree_sitter::Node<'_>,
    ) -> Option<(RetentionKind, Vec<String>, Vec<String>)> {
        // Returns `(kind, params, blockers)`: `params` instantiate the kind's
        // detail template (the specific blocked element a human has to look at),
        // `blockers` name the retained declarations this one waits on.
        let workspace = self.workspace?;
        let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
        let source_file = workspace.source_file(indexed_path)?;
        let target = source_file
            .declarations
            .iter()
            .find(|declaration| declaration.name == name)?;
        if workspace.has_unselected_kotlin_subtype(target, self.translation_roots) {
            return Some((
                RetentionKind::SubtypeOutsideTranslationSet,
                Vec::new(),
                Vec::new(),
            ));
        }
        // Subtype rule, two modes:
        // - No fixpoint hint (single-file mode): retain on ANY Kotlin
        //   subtype - conservative, cannot know what translates later.
        // - With hint (fixpoint pass): retain only when a Kotlin subtype
        //   is ITSELF retained for an intrinsic reason; a retained
        //   Kotlin implementor cannot implement a translated-away
        //   supertype (enum entries ABI, KClass...). Monotone: seeds
        //   (intrinsically tainted decls) never shrink, so iteration
        //   reaches the least fixpoint.
        if hierarchy_closure() && target.kind == crate::workspace::DeclarationKind::Interface {
            let blockers = self.retained_subtypes(workspace, target);
            let retains = match self.retained_hint.as_ref() {
                Some(_) => !blockers.is_empty(),
                None => workspace.has_kotlin_subtype(target),
            };
            if retains {
                return Some((
                    RetentionKind::InterfaceSubtypeRetained,
                    Vec::new(),
                    blockers,
                ));
            }
        }
        // Closed hierarchy, downward half: a retained Kotlin declaration must
        // not inherit from a supertype that was translated away. Only a Kotlin
        // supertype can supply what kotlinc needs from it — the JPA no-arg
        // plugin synthesizes the subclass's `super()` call from the
        // supertype's *default parameter values*, which a Java constructor
        // does not have ("No noarg super constructor"), and InterfaceLowering
        // resolves an inherited member as a real override against a Kotlin
        // declaration, not against a Java default method.
        if hierarchy_closure()
            && self
                .retained_hint
                .as_ref()
                .is_some_and(|retained| workspace.has_retained_kotlin_subtype(target, retained))
        {
            return Some((
                RetentionKind::RetainedInheritor,
                Vec::new(),
                self.retained_subtypes(workspace, target),
            ));
        }
        // Constructor default arguments (N87CB). A default on a middle
        // parameter is not a blocker by itself — what decides is whether a caller
        // omits one in a shape Java can be given. Java expresses an omission by
        // writing a language-neutral literal into the call, or by a delegating
        // overload for the exact pattern a call site uses; only a pattern the
        // emitter cannot write keeps the declaration in Kotlin.
        if target.has_default_constructor_parameter {
            let evidence = workspace.ctor_omission_evidence(target);
            if let Some(reason) = self.ctor_default_plan(decl, target).blocked {
                return Some((
                    RetentionKind::MiddleDefaultParameter,
                    vec![reason],
                    Vec::new(),
                ));
            }
            if let Some(site) = evidence.unresolvable.first() {
                return Some((
                    RetentionKind::MiddleDefaultParameter,
                    vec![format!(
                        "a caller's argument shape could not be read at {site}"
                    )],
                    Vec::new(),
                ));
            }
            if !self.in_place && evidence.named_callers {
                return Some((
                    RetentionKind::MiddleDefaultParameter,
                    vec![
                        "a caller names arguments and this run rewrites no retained Kotlin"
                            .to_string(),
                    ],
                    Vec::new(),
                ));
            }
        }
        // A translated Java object singleton cannot be referenced by
        // plain name from residual Kotlin (no companion object), so an
        // object referenced from a retained Kotlin file has to stay.
        if target.kind == crate::workspace::DeclarationKind::Object
            && self.retained_hint.as_ref().is_some_and(|retained| {
                workspace.has_retained_kotlin_reference(indexed_path, name, retained)
            })
        {
            return Some((
                RetentionKind::ReferencedFromRetainedKotlin,
                Vec::new(),
                self.retained_hint
                    .as_ref()
                    .map(|retained| {
                        workspace.retained_kotlin_referencers(indexed_path, name, retained)
                    })
                    .unwrap_or_default(),
            ));
        }
        if let Some((property, inherited_type, implementation_type)) =
            workspace.nullable_property_narrowing_conflict(source_file, target)
        {
            return Some((
                RetentionKind::NullableNarrowing,
                vec![property, inherited_type, implementation_type],
                Vec::new(),
            ));
        }
        if self.retained_hint.is_some()
            && self.in_place
            && workspace.inherits_retained_kotlin_property_interface(
                source_file,
                target,
                self.translation_roots,
            )
        {
            return Some((
                RetentionKind::RetainedPropertyInterface,
                Vec::new(),
                self.retained_supertypes(workspace, target),
            ));
        }
        // Retained Kotlin that smart-casts one of its properties does not have
        // to keep the owner in Kotlin: the rewrite pass binds the property read
        // into a local first, and a local `val` smart-casts freely. Only a site
        // the rewrite pass cannot repair (an unsupported shape, or a receiver
        // whose owner it cannot prove) keeps the owner here. Not in place, the
        // rewrite pass never runs, so nothing is repaired and the owner stays.
        if self.in_place
            && workspace.property_smart_cast_used_by_kotlin(indexed_path, target)
            && !workspace.smart_cast_rewrites_cover(indexed_path, target)
        {
            return Some((RetentionKind::SmartCast, Vec::new(), Vec::new()));
        }
        if hierarchy_closure()
            && !self.declaration_is_enum(decl)
            && self.retained_hint.as_ref().is_some_and(|retained| {
                workspace.has_retained_kotlin_supertype(&target.supertypes, retained)
            })
        {
            return Some((
                RetentionKind::RetainedSupertype,
                Vec::new(),
                self.retained_supertypes(workspace, target),
            ));
        }
        None
    }

    /// Retained Kotlin subtypes of `target`, by simple name — the blame edges
    /// behind the subtype-based retention reasons. Empty without a fixpoint
    /// hint: the run-end table is built from the hinted final pass, and the
    /// unhinted single-file mode has no retained set to attribute against.
    fn retained_subtypes(
        &self,
        workspace: &crate::workspace::SourceIndex,
        target: &crate::workspace::Declaration,
    ) -> Vec<String> {
        self.retained_hint
            .as_ref()
            .map(|retained| workspace.retained_kotlin_subtype_names(target, retained))
            .unwrap_or_default()
    }

    /// Retained Kotlin supertypes of `target`, by simple name — the blame edges
    /// behind the supertype-based retention reasons.
    fn retained_supertypes(
        &self,
        workspace: &crate::workspace::SourceIndex,
        target: &crate::workspace::Declaration,
    ) -> Vec<String> {
        self.retained_hint
            .as_ref()
            .map(|retained| workspace.retained_kotlin_supertype_names(&target.supertypes, retained))
            .unwrap_or_default()
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
        if self.declaration_references_kclass(decl) {
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
        // Kotlin `open`: the class is subclassable, so the Java form must NOT
        // be final. `open` itself is not Java and must not be emitted.
        let mut is_open = false;
        let mut is_explicit_final = false;
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
                                    match word {
                                        // `sealed`/`abstract` are Java words;
                                        // they are emitted as-is.
                                        "sealed" => is_sealed = true,
                                        // `open` has no Java keyword. Its Java
                                        // form is a class WITHOUT `final`
                                        // (see the final_kw rule below), so the
                                        // flag is recorded and the word is
                                        // never emitted — `open class` is not
                                        // Java.
                                        "open" => {
                                            is_open = true;
                                            continue;
                                        }
                                        _ => {}
                                    }
                                    // Guarded: the same keyword can arrive as
                                    // either node kind depending on the grammar,
                                    // and `abstract abstract class` is not Java.
                                    if !modifiers.contains(word) {
                                        modifiers.push_str(word);
                                        modifiers.push(' ');
                                    }
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
                    "visibility_modifier" => {
                        // handled below via text
                    }
                    "inheritance_modifier" => {
                        // Kotlin's inheritance keywords parse as their OWN node
                        // kind, not as `class_modifier` children:
                        //  - `open` means subclassable, and Java expresses that
                        //    as the ABSENCE of `final` (see the final_kw rule
                        //    below). `open` is not a Java keyword and must never
                        //    be emitted.
                        //  - `abstract`/`sealed` are Java words, emitted as-is
                        //    (`sealed`'s own handling drops it when no subclass
                        //    is known in this file).
                        //  - `final` is Kotlin's default, which the Java form
                        //    states explicitly anyway; recording the keyword
                        //    itself matters because the source is authoritative
                        //    (see the final_kw rule below).
                        for word in self.text(m).split_whitespace() {
                            match word {
                                "open" => is_open = true,
                                "final" => is_explicit_final = true,
                                "sealed" => {
                                    is_sealed = true;
                                    if !modifiers.contains("sealed") {
                                        modifiers.push_str("sealed ");
                                    }
                                }
                                // Guarded: the same keyword can arrive as
                                // either node kind depending on the grammar,
                                // and `abstract abstract class` is not Java.
                                "abstract" if !modifiers.contains("abstract") => {
                                    modifiers.push_str("abstract ");
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if is_fun_interface {
            annotations.push("@FunctionalInterface".to_string());
        }
        // Kotlin nested classes are static (no enclosing instance) unless
        // explicitly `inner`. Java requires the `static` modifier on the
        // declaration, otherwise `new Outer.Inner(...)` (valid Kotlin shape)
        // is rejected by javac ("enclosing instance required").
        if decl.parent().is_some_and(|p| p.kind() == "class_body")
            && (decl.kind() == "class_declaration" || decl.kind() == "object_declaration")
            && !modifiers.contains("static")
            && !self.text(decl).contains("inner class")
        {
            modifiers = format!("static {}", modifiers);
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
        // Register every instance property before emitting any body member.
        // Kotlin permits a default method to read a property declared before
        // or after it; Java must spell that implicit receiver as a getter.
        // Waiting for transpile_property() is order-sensitive and leaves a
        // bare identifier when function bodies are lowered first.
        if let Some(body) = kt::child(decl, "class_body") {
            for member in body.children(&mut body.walk()) {
                if member.kind() != "property_declaration" {
                    continue;
                }
                if let Some(variable) = kt::child(member, "variable_declaration")
                    && let Some(identifier) = kt::child(variable, "identifier")
                {
                    let property = self.text(identifier).to_string();
                    let is_static = kt::parent_of(member)
                        .is_some_and(|parent| parent.kind() == "class_body")
                        && kt::parent_of(member)
                            .and_then(kt::parent_of)
                            .is_some_and(|parent| parent.kind() == "object_declaration");
                    if !is_static {
                        self.self_getters
                            .insert(property.clone(), format!("get{}", capitalize(&property)));
                    }
                }
            }
        }
        if let Some(workspace) = self.workspace
            && let Some(name) = kt::field(decl, "name").map(|node| self.text(node).to_string())
        {
            let declaring = self.workspace_file.as_deref().unwrap_or(self.file);
            for property in workspace.inherited_property_names_in_file(declaring, &name) {
                self.self_getters
                    .entry(property.clone())
                    .or_insert_with(|| format!("get{}", capitalize(&property)));
            }
        }
        let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
        if self.has_companion_operator_invoke(decl)
            && self.workspace.is_some_and(|workspace| {
                // Kotlin class-call ABI `Type(...)`: only Kotlin that stays
                // Kotlin after this run can still consume the operator, so
                // retention follows the surviving-reference rule. Without a
                // fixpoint hint (single-file mode) any residual Kotlin counts,
                // because Java statics can't upgrade to the class-call form.
                match self.retained_hint {
                    Some(_) => self.referenced_by_surviving_kotlin(&name),
                    None => {
                        workspace.kotlin_files().next().is_some()
                            || workspace.has_external_kotlin_reference(indexed_path, &name)
                    }
                }
            })
        {
            self.diag_untranslatable(
                decl,
                "companion operator `invoke` is consumed by residual Kotlin; Java constructors cannot preserve the Kotlin class-call ABI",
            );
            return;
        }
        if let Some((kind, params, blockers)) = self.kotlin_retention_reason(&name, decl) {
            self.retain_decl(decl, &name, kind, &params, &blockers);
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
        // Primary-constructor properties are emitted as Java getters too, but
        // they bypass transpile_property(). Register them before translating
        // any member body so a Kotlin implicit receiver (`parent.key`) does
        // not leak the property name as an unresolved Java local.
        for (is_property, _, member_name, _) in &params {
            if !*is_property {
                continue;
            }
            self.self_getters.insert(
                member_name.clone(),
                format!("get{}", capitalize(member_name)),
            );
        }
        let has_secondary_constructor = kt::child(decl, "class_body").is_some_and(|body| {
            body.children(&mut body.walk())
                .any(|member| member.kind() == "secondary_constructor")
        });

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
                                        if params.iter().any(|(_, _, pname, _)| pname == base)
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
        // Kotlin's `kotlin-jpa` plugin gives @Entity/@Embeddable/
        // @MappedSuperclass classes a zero-argument constructor so the ORM can
        // instantiate them. The Kotlin source never spells that constructor
        // out, so the Java form has to be synthesized here.
        let jpa_no_arg = annotations.iter().any(|a| is_jpa_no_arg_annotation(a));

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
                                    // OVERIDE rule: `override val x` without a
                                    // declared type narrows the SUPERTYPE's
                                    // declared type (Java cannot bridge Object
                                    // against a type-variable getter).
                                    let pty = kt::child(vd, "user_type")
                                        .or_else(|| kt::child(vd, "nullable_type"))
                                        .map(|t| kt::java_type_ann(t, self.source, self.annots))
                                        .or_else(|| {
                                            let is_override = kt::child(member, "modifiers")
                                                .map(|m| self.text(m).contains("override"))
                                                .unwrap_or(false);
                                            if !is_override {
                                                return None;
                                            }
                                            self.workspace.and_then(|ws| {
                                                let declaring = self
                                                    .workspace_file
                                                    .as_deref()
                                                    .unwrap_or(self.file);
                                                ws.inherited_property_type_in_file(
                                                    declaring, &name, &pname,
                                                )
                                            })
                                        })
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
                        "companion_object" => {
                            self.transpile_interface_companion(member, out);
                            out.blank();
                        }
                        "line_comment" | "block_comment" | ";" | "{" | "}" => {}
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
        if is_data
            && !params.is_empty()
            && self.lombok
            // Lombok would generate a second equals/hashCode/toString beside
            // the user-defined one already in the body.
            && !self.class_body_defines_lombok_generated(decl)
        {
            // --lombok: a data class is a VALUE type. All-`val` properties
            // become Lombok's immutable `@Value` (private final fields,
            // getters, equals/hashCode/toString, all-args constructor, and no
            // setters), which is what Kotlin's data class actually is. A
            // writable `var` — or any body instance field, whose presence
            // changes the constructor arity `@Value`'s implied
            // all-args constructor would settle on — keeps mutable `@Data`.
            let lombok_annotation = self.data_class_lombok_annotation(
                decl,
                params.iter().all(|(_, is_mutable, _, _)| !*is_mutable),
            );
            let primary_constructor_annotations = self.primary_constructor_annotations(decl);
            let use_lombok_generated_members = lombok_annotation == "@Value"
                && !self.primary_constructor_has_defaults(decl)
                && primary_constructor_annotations.is_empty()
                && !jpa_no_arg
                && !has_secondary_constructor
                && superclass.is_none();
            out.line(lombok_annotation);
            self.emit_lombok_equals_call_super(decl, out);
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
            let is_non_null_reference = |ftype: &str| {
                !ftype.starts_with('@')
                    && !matches!(
                        ftype,
                        "boolean" | "byte" | "short" | "int" | "long" | "char" | "float" | "double"
                    )
            };
            for (is_property, is_mutable, fname, ftype) in &params {
                if !*is_property {
                    continue;
                }
                // final val fields: @Data omits the setter automatically
                let final_kw = if !*is_mutable { "final " } else { "" };
                // Kotlin writes persistence and metadata annotations on the
                // constructor property; the Java side must carry them on the
                // FIELD, which is the element the ORM reads.
                // This path emits no explicit getter (Lombok generates the
                // accessors), so a `@get:` annotation has nowhere better to go
                // than the field it is written on.
                for annotation in self
                    .param_annotations(decl, fname)
                    .into_iter()
                    .chain(self.param_getter_annotations(decl, fname))
                {
                    out.line(annotation);
                }
                let nullability = if is_non_null_reference(ftype) {
                    "@NonNull "
                } else {
                    ""
                };
                if use_lombok_generated_members {
                    out.line(format!("{}{} {};", nullability, ftype, fname));
                } else {
                    out.line(format!(
                        "{}private {}{} {};",
                        nullability, final_kw, ftype, fname
                    ));
                }
            }
            out.blank();
            // Kotlin metadata makes primary-constructor parameter names available
            // to reflection-based binders (notably Jackson's Kotlin module). The
            // translated Java class has no kotlin.Metadata, so preserve the same
            // constructor-property contract with the JDK annotation rather than
            // requiring every consumer to install a Java parameter-name module.
            for annotation in &primary_constructor_annotations {
                out.line(annotation.clone());
            }
            out.line(format!(
                "@java.beans.ConstructorProperties({{{}}})",
                params
                    .iter()
                    .map(|(_, _, name, _)| format!("\"{}\"", name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            out.open(format!("public {}({})", name, {
                params
                    .iter()
                    .map(|(_, _, n, t)| {
                        let nullability = if is_non_null_reference(t) {
                            "@NonNull "
                        } else {
                            ""
                        };
                        format!("{}{} {}", nullability, t, n)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }));
            for (is_property, _, fname, _) in &params {
                if *is_property {
                    out.line(format!("this.{} = {};", fname, fname));
                }
            }
            out.close();
            if !use_lombok_generated_members {
                // NOTLIN: emit explicit accessors mirroring the original Kotlin ABI.
                // Relying on @Data's synthesized getters breaks cross-language member
                // resolution: kotlinc (reading our generated Java via the kotlin
                // lombok plugin) merges the implemented interface's @Nullable getter
                // into the lookup and reports T? where the original Kotlin member was
                // non-nullable, so call sites fail to typecheck. An explicit getter
                // overrides synthesis and keeps the declared member visible.
                for (is_property, _, fname, ftype) in &params {
                    if !*is_property {
                        continue;
                    }
                    // Kotlin `val authenticated: Boolean` has a Java-style
                    // `getAuthenticated()` accessor. Only a property whose name
                    // itself starts with `is` uses that name as its getter.
                    let getter = if ftype == "boolean"
                        && fname.starts_with("is")
                        && fname.chars().nth(2).is_some_and(|c| c.is_ascii_uppercase())
                    {
                        fname.clone()
                    } else {
                        format!("get{}", capitalize(fname))
                    };
                    // A Kotlin supertype's member function of the same getter
                    // name (e.g. `fun getId(): LookupEntityId`) IS the accessor
                    // the JDK sees for this property: synthesizing one with the
                    // wider property type breaks the override and javac rejects
                    // the return-type clash. Let the inherited accessor stand.
                    if self.workspace.is_some_and(|w| {
                        w.inherited_fun_getter_conflicts(
                            self.workspace_file.as_deref().unwrap_or(self.file),
                            &name,
                            &getter,
                        )
                    }) {
                        continue;
                    }
                    // @NotNull pins the getter's nullability to the (non-null)
                    // field: kotlinc otherwise merges the implemented interface's
                    // nullable property into the member lookup and reports T?
                    // (verified with the jlombok-probe fixture). A source
                    // annotation on the type (e.g. `@Nullable Boolean` from a
                    // `Boolean?` property) already states nullability — keep it
                    // verbatim instead of prepending ours.
                    let nullability = if ftype.starts_with('@') {
                        String::new()
                    } else {
                        "@NotNull ".to_string()
                    };
                    out.line(format!(
                        "{}public {} {}() {{ return {}; }}",
                        nullability, ftype, getter, fname
                    ));
                }

                for (_, is_mutable, fname, ftype) in &params {
                    if *is_mutable && ftype != "boolean" {
                        let nullability = if is_non_null_reference(ftype) {
                            "@NonNull "
                        } else {
                            ""
                        };
                        out.line(format!(
                            "public void set{}({}{} {}) {{ this.{} = {}; }}",
                            capitalize(fname),
                            nullability,
                            ftype,
                            fname,
                            fname,
                            fname
                        ));
                    }
                }
            }
            self.emit_jvm_overloads_primary_constructors(decl, &name, &params, out);
            if jpa_no_arg {
                self.emit_jpa_no_arg_constructor(decl, &name, &params, superclass.is_some(), out);
            }
            out.blank();
            if let Some(body) = kt::child(decl, "class_body") {
                self.transpile_class_body(body, out);
            }
            out.close();
            return;
        }
        if is_data && !params.is_empty() {
            // A `var` PROPERTY in the body is another mutable member Java
            // records reject (instance fields are illegal in records).
            // Redirect to the final-class form so setters survive.
            let body_has_var_property = kt::child(decl, "class_body").is_some_and(|body| {
                body.children(&mut body.walk())
                    .filter(|m| m.kind() == "property_declaration")
                    .any(|m| kt::child(m, "var").is_some())
            });
            if body_has_var_property {
                if let Some(dn) = self.current_decl {
                    let label = self.decl_labels.get(&dn.id()).cloned().unwrap_or_default();
                    self.taint_decl(&label);
                }
                self.diag_untranslatable(
                    decl,
                    "data class body declares a `var` property; Java records cannot hold instance fields — use --lombok for a mutable @Data class",
                );
                return;
            }
            // record: parameters become record components. Records are
            // immutable — a data class with any `var` component loses setter
            // semantics, which is a semantic drop, so without --lombok the
            // declaration is TAINTED (stays in the .kt, warns N001) instead
            // of silently emitting a broken translation.
            if params.iter().any(|(_, is_mutable, _, _)| *is_mutable) {
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
                .map(|(_, _, n, t)| format!("{} {}", t, n))
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
                if jpa_no_arg {
                    self.emit_jpa_no_arg_record_constructor(decl, &name, &params, out);
                }
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
                for (is_property, is_mutable, fname, ftype) in &params {
                    if !*is_property {
                        continue;
                    }
                    let final_kw = if !*is_mutable { "final " } else { "" };
                    // Kotlin writes persistence and metadata annotations on the
                    // constructor property; the Java side must carry them on the
                    // FIELD, which is the element the ORM reads.
                    for annotation in self.param_annotations(decl, fname) {
                        out.line(annotation);
                    }
                    out.line(format!("private {}{} {};", final_kw, ftype, fname));
                }
                out.blank();
                // `data class Layout @Default constructor(...)` — the marker
                // must ride on the generated constructor: the default-argument
                // overloads below give the class several constructors, and
                // MapStruct selects between them by an annotation named
                // `@Default`.
                for annotation in self.primary_constructor_annotations(decl) {
                    out.line(annotation);
                }
                out.open(format!("public {}({})", name, comps.join(", ")));
                for (is_property, _, fname, _) in &params {
                    if *is_property {
                        out.line(format!("this.{} = {};", fname, fname));
                    }
                }
                out.close();
                if jpa_no_arg {
                    self.emit_jpa_no_arg_constructor(
                        decl,
                        &name,
                        &params,
                        superclass.is_some(),
                        out,
                    );
                }
                out.blank();
                for (_, _, fname, ftype) in &params {
                    let cap = capitalize(fname);
                    // `@get:`-targeted annotations belong on the generated
                    // getter, which this path emits.
                    for annotation in self.param_getter_annotations(decl, fname) {
                        out.line(annotation);
                    }
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
                    } else if member.is_named()
                        && !matches!(
                            member.kind(),
                            ";" | "{" | "}" | "line_comment" | "block_comment"
                        )
                    {
                        self.diag_untranslatable(
                            member,
                            format!("record member not supported: {}", member.kind()),
                        );
                    }
                }
            }
            // A record's canonical constructor is its only implicit one,
            // extra constructors may delegate to it with `this(...)`. Kotlin
            // callers omit trailing defaults, so a translated record needs the
            // same delegating overloads a plain class gets — without them the
            // retained Kotlin that called `Configuration()` no longer resolves.
            self.emit_jvm_overloads_primary_constructors(decl, &name, &params, out);
            out.close();
        } else {
            // Java places type params after the class name: `class Name<T>`.
            let tp = type_params.trim_end(); // "<T>" or "" (no space needed before '{')
            // Kotlin classes are final by DEFAULT; Java's default is the
            // opposite. Emitting an ordinary Kotlin class as a plain Java class
            // silently grants an extensibility the source never had, so the
            // faithful Java form is `final`. Deliberate opt-outs, in order:
            //  - a subclass of a FILE-sealed type: Java requires a permitted
            //    subtype to be final, sealed or non-sealed;
            //  - the source says so (`open`/`abstract`/`sealed`);
            //  - something in the workspace extends it: both compilers reject
            //    inheriting a final class, so any subclass not visible here
            //    would turn this into an error at the other end;
            //  - a persistence class, which the ORM subclasses at runtime for
            //    lazy proxies. Such a Kotlin source is `open` only via the
            //    all-open plugin, so there is no `open` token in the AST to
            //    tell us and `final` fails at STARTUP, not at compile time.
            //
            // An EXPLICIT `final` in the source outranks every opt-out below
            // and is never traded away: valid Kotlin cannot extend a `final`
            // class at all, so "something extends it" is a false positive (or
            // a stray Java file that was already broken, which silently
            // un-finaling the class would hide), and a `final` persistence
            // class is one the all-open plugin did NOT open, i.e. not proxied.
            // Dropping the keyword would hand callers an extensibility the
            // Kotlin source explicitly refused them.
            let sealed_parent = superclass
                .as_ref()
                .is_some_and(|parent| self.sealed_types.contains(parent));
            let final_kw = if sealed_parent || is_explicit_final {
                "final "
            } else if is_sealed
                || is_open
                || modifiers.contains("abstract")
                || self
                    .workspace
                    .is_some_and(|workspace| workspace.has_subtype_named(&name))
                || annotations.iter().any(|a| is_orm_proxied_annotation(a))
            {
                ""
            } else {
                "final "
            };
            // --lombok: hand-rolled accessors/equals/hashCode/toString become
            // Lombok annotations placed BEFORE the class declaration.
            // @AllArgsConstructor synthesizes a ctor over EVERY field, so it
            // only stands in for the Kotlin primary constructor when the class
            // has no other instance fields: `class C(val site: SiteId) { var
            // zone: SearchValue? = null }` has a one-parameter Kotlin ctor, but
            // Lombok would demand `(site, zone, …)` and every caller using the
            // Kotlin arity stops compiling. Emit the explicit ctor instead —
            // the body properties keep their inline initializers as fields.
            //
            // The JPA no-arg constructor is a second reason never to lean on
            // Lombok here: Lombok generates NO constructor at all once the
            // class declares one (measured with lombok 1.18.42: @Data +
            // @AllArgsConstructor beside an explicit `public T()` yields only
            // that one), so relying on the synthesized all-args ctor would
            // silently DELETE it and break every call site — the explicit ctor
            // has to be written beside the no-arg one.
            let lombok_supplies_all_args =
                !jpa_no_arg && !self.class_body_declares_instance_fields(decl);
            if self.lombok
                && !params.is_empty()
                // Lombok would generate a second equals/hashCode/toString
                // beside the user-defined one already in the body.
                && !self.class_body_defines_lombok_generated(decl)
            {
                out.line("@Data");
                self.emit_lombok_equals_call_super(decl, out);
                // @AllArgsConstructor's synthesized ctor collides with the
                // explicit super-forwarding ctor emitted below — only
                // annotate when the explicit one is not being written.
                if super_ctor_args.is_none()
                    && !has_secondary_constructor
                    && lombok_supplies_all_args
                {
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
            for (is_property, is_mutable, fname, ftype) in &params {
                if !*is_property {
                    continue;
                }
                let final_kw = if !*is_mutable { "final " } else { "" };
                // Kotlin writes persistence and metadata annotations on the
                // constructor property; the Java side must carry them on the
                // FIELD, which is the element the ORM reads.
                // The getter below is emitted only when this is NOT --lombok
                // (`accessors (skipped under --lombok: @Data generates
                // them)`), so under Lombok a `@get:` annotation has nowhere
                // to land and must stay on the field it is written on:
                // dropping it would lose the very metadata the ORM reads.
                let mut annotations = self.param_annotations(decl, fname);
                let lombok_needs_kotlin_boolean_getter = self.lombok
                    && ftype == "boolean"
                    && !(fname.starts_with("is")
                        && fname.chars().nth(2).is_some_and(|c| c.is_ascii_uppercase()));
                if self.lombok && !lombok_needs_kotlin_boolean_getter {
                    annotations.extend(self.param_getter_annotations(decl, fname));
                }
                for annotation in annotations {
                    out.line(annotation);
                }
                out.line(format!("private {}{} {};", final_kw, ftype, fname));
            }
            if !params.is_empty() {
                out.blank();
            }
            // constructor (redundant under --lombok: AllArgsConstructor)
            let ctor_annotations = self.primary_constructor_annotations(decl);
            if let Some(sargs) = &super_ctor_args {
                // superclass ctor needs arguments: emit an explicit ctor
                // forwarding them (`: Parent("template")` -> super("template")).
                // @AllArgsConstructor's generated ctor cannot express the
                // super-call, so the explicit one is required regardless.
                for annotation in &ctor_annotations {
                    out.line(annotation.clone());
                }
                out.open(format!("public {}({})", name, {
                    params
                        .iter()
                        .map(|(_, _, n, t)| format!("{} {}", t, n))
                        .collect::<Vec<_>>()
                        .join(", ")
                }));
                out.line(format!("super({});", sargs));
                for (is_property, _, fname, _) in &params {
                    if *is_property {
                        out.line(format!("this.{} = {};", fname, fname));
                    }
                }
                out.close();
                out.blank();
            }
            if !params.is_empty()
                && (!self.lombok || has_secondary_constructor || !lombok_supplies_all_args)
                && super_ctor_args.is_none()
            {
                for annotation in &ctor_annotations {
                    out.line(annotation.clone());
                }
                out.open(format!("public {}({})", name, {
                    params
                        .iter()
                        .map(|(_, _, n, t)| format!("{} {}", t, n))
                        .collect::<Vec<_>>()
                        .join(", ")
                }));
                for (is_property, _, fname, _) in &params {
                    if *is_property {
                        out.line(format!("this.{} = {};", fname, fname));
                    }
                }
                out.close();
                out.blank();
            }
            self.emit_jvm_overloads_primary_constructors(decl, &name, &params, out);
            if jpa_no_arg {
                self.emit_jpa_no_arg_constructor(decl, &name, &params, superclass.is_some(), out);
            }
            // accessors (skipped under --lombok: @Data generates them)
            if !self.lombok {
                for (is_property, is_mutable, fname, ftype) in &params {
                    // A plain constructor parameter is not a property: Kotlin
                    // declares no member for it, so an emitted accessor would
                    // read a field that was never written (`return x;` ->
                    // "cannot find symbol x"; for a name that shadows an
                    // inherited property, "id has private access in Base").
                    if !*is_property {
                        continue;
                    }
                    let cap = capitalize(fname);
                    // `@get:`-targeted annotations belong on the generated
                    // getter, which this path emits.
                    for annotation in self.param_getter_annotations(decl, fname) {
                        out.line(annotation);
                    }
                    out.open(format!("public {} get{}()", ftype, cap));
                    out.line(format!("return {};", fname));
                    out.close();
                    if *is_mutable {
                        out.blank();
                        out.open(format!("public void set{}({} {})", cap, ftype, fname));
                        out.line(format!("this.{} = {};", fname, fname));
                        out.close();
                    }
                    out.blank();
                }
            } else {
                // Lombok names a primitive-boolean getter `isEnabled()`, while
                // Kotlin's JVM ABI for `val enabled: Boolean` is
                // `getEnabled()`. Keep the Kotlin getter explicitly; Lombok
                // suppresses only the same method name and may still add its
                // convenience `isEnabled()` without breaking callers.
                for (is_property, _, fname, ftype) in &params {
                    if !*is_property
                        || ftype != "boolean"
                        || (fname.starts_with("is")
                            && fname.chars().nth(2).is_some_and(|c| c.is_ascii_uppercase()))
                    {
                        continue;
                    }
                    let getter = format!("get{}", capitalize(fname));
                    if self.class_declares_method(decl, &getter)
                        || self.workspace.is_some_and(|workspace| {
                            workspace.inherited_fun_getter_conflicts(
                                self.workspace_file.as_deref().unwrap_or(self.file),
                                &name,
                                &getter,
                            )
                        })
                    {
                        continue;
                    }
                    for annotation in self.param_getter_annotations(decl, fname) {
                        out.line(annotation);
                    }
                    out.line(format!(
                        "public boolean {}() {{ return {}; }}",
                        getter, fname
                    ));
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
        for ((_, _, param_name, param_type), default) in params.into_iter().zip(defaults) {
            let suffix = default
                .map(|value| format!(" default {value}"))
                .unwrap_or_default();
            out.line(format!("{param_type} {param_name}(){suffix};"));
        }
        out.close();
    }

    /// Whether the class body explicitly declares `equals`, `hashCode`,
    /// or `toString`. When it does, `@Data` must NOT be emitted: Lombok
    /// would generate a second, conflicting implementation (duplicate
    /// method / unwanted super-call pairing) alongside the user-defined
    /// one the body translation already produced.
    /// Annotations written on the Kotlin primary constructor
    /// (`data class Layout @Default constructor(...)`). They must survive onto
    /// the generated constructor: a Kotlin primary constructor with default
    /// arguments becomes several Java constructors, and MapStruct picks the one
    /// to use by an annotation named `@Default`. Dropping it leaves the mapping
    /// ambiguous, so the generated mapper no longer compiles.
    fn primary_constructor_annotations(&self, decl: tree_sitter::Node) -> Vec<String> {
        // A constructor with no annotations at all has no `modifiers` child;
        // the marker below must still be considered, so this is a plain
        // iterator, not an early return.
        let mut annotations: Vec<String> = kt::child(decl, "primary_constructor")
            .and_then(|constructor| kt::child(constructor, "modifiers"))
            .map(|modifiers| {
                let mut cursor = modifiers.walk();
                modifiers
                    .children(&mut cursor)
                    .filter(|node| matches!(node.kind(), "annotation" | "annotated_expression"))
                    .filter_map(|node| self.transpile_declaration_annotation(node))
                    // `@JvmOverloads` and its family describe the Kotlin ABI;
                    // the overloads themselves are emitted explicitly, so the
                    // annotations have no Java counterpart to carry over.
                    .filter(|annotation| {
                        ![
                            "JvmOverloads",
                            "JvmName",
                            "JvmStatic",
                            "JvmField",
                            "JvmSuppressWildcards",
                        ]
                        .iter()
                        .any(|jvm_only| annotation.contains(jvm_only))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // A constructor with Kotlin default arguments becomes several Java
        // constructors. Kotlin metadata told MapStruct which of them was the
        // primary one; in Java the same consumer disambiguates only by an
        // annotation named `Default`. Keep the marker the source wrote, and
        // supply it (fully qualified — the declaring file rarely imports it)
        // whenever the translation introduces the extra constructors.
        if self.in_place
            && self.primary_constructor_has_defaults(decl)
            && let Some(marker) = self.workspace_constructor_marker()
        {
            let simple = marker.rsplit('.').next().unwrap_or_default();
            if !annotations.iter().any(|written| written.contains(simple)) {
                annotations.push(format!("@{marker}"));
            }
        }
        annotations
    }

    /// Whether the primary constructor declares any default argument, i.e.
    /// whether the migration will add delegating constructor overloads that a
    /// reflection-based consumer then has to disambiguate.
    fn primary_constructor_has_defaults(&self, decl: tree_sitter::Node) -> bool {
        kt::child(decl, "primary_constructor")
            .and_then(|constructor| kt::child(constructor, "class_parameters"))
            .is_some_and(|parameters| {
                parameters
                    .children(&mut parameters.walk())
                    .filter(|parameter| parameter.kind() == "class_parameter")
                    .any(|parameter| {
                        parameter
                            .children(&mut parameter.walk())
                            .any(|child| !child.is_named() && child.kind() == "=")
                    })
            })
    }

    /// The workspace's `Default`-named constructor marker annotation, as a
    /// fully qualified name, when it declares one.
    fn workspace_constructor_marker(&self) -> Option<String> {
        let workspace = self.workspace?;
        let found: Vec<_> = workspace
            .declarations_named("Default")
            .map(|declaration| (declaration.kind, declaration.package.clone()))
            .collect();
        if std::env::var("NOTLIN_DEBUG_MARKER").is_ok() {
            eprintln!("dbg marker: candidates={found:?}");
        }
        workspace
            .declarations_named("Default")
            .find(|declaration| declaration.kind == crate::workspace::DeclarationKind::Annotation)
            .and_then(|declaration| {
                declaration
                    .package
                    .as_deref()
                    .map(|package| format!("{package}.{}", declaration.name))
            })
    }

    /// Body properties (`var zone: SearchValue? = null`) become instance fields
    /// with inline initializers; they are NOT primary-constructor parameters.
    /// Lombok's @AllArgsConstructor covers every field, so it only matches the
    /// Kotlin constructor when the body adds no field of its own.
    /// Lombok's annotation for a translated Kotlin data class: `@Value` when
    /// nothing is writable, `@Data` as soon as something is.
    ///
    /// Kotlin's `data class X(val a: T)` is an immutable value type, and
    /// `@Value` is its faithful Java form. A `var` property makes the Java
    /// object writable, which only `@Data` serves. A body instance field also
    /// forces `@Data`: `@Value` implies `@AllArgsConstructor` over EVERY field,
    /// so the constructor arity Kotlin callers use would change.
    fn data_class_lombok_annotation(
        &self,
        decl: tree_sitter::Node,
        ctor_props_all_final: bool,
    ) -> &'static str {
        if ctor_props_all_final && !self.class_body_declares_instance_fields(decl) {
            "@Value"
        } else {
            "@Data"
        }
    }

    /// Lombok's generated `equals`/`hashCode` cover this class's own fields
    /// only. A value type with a superclass must fold the inherited state in —
    /// and without the call Lombok warns "generating equals/hashCode
    /// implementation but without a call to superclass" on every such class.
    ///
    /// The predicate is `superclass_name`, which only matches a delegation
    /// specifier shaped like `: Base(...)`, i.e. a real class supertype —
    /// interfaces take no parentheses. Keying on the emitted `extends` text
    /// instead would catch interface-only classes and buy the opposite
    /// complaint, Lombok's "supercall to java.lang.Object is pointless".
    fn emit_lombok_equals_call_super(&self, decl: tree_sitter::Node<'tree>, out: &mut JavaOut) {
        if self.superclass_name(decl).is_some() {
            out.line("@EqualsAndHashCode(callSuper = true)");
        }
    }

    /// Annotations written on one declaration, split by what Java can do with
    /// them: a `@get:`-targeted annotation belongs on the generated getter when
    /// the emitter writes one, and everything else on the field. The use-site
    /// prefix itself is dropped either way — Java annotates the element, and
    /// `@JsonProperty` is not Java syntax.
    pub(crate) fn annotation_split(&self, node: tree_sitter::Node) -> (Vec<String>, Vec<String>) {
        let mut field = Vec::new();
        let mut getter = Vec::new();
        // Annotations sit under `modifiers`, but walk the subtree so the
        // grammar's exact nesting cannot matter.
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let mut cursor = current.walk();
            for child in current.children(&mut cursor) {
                if child.kind() == "annotation" {
                    if let Some(text) = self.transpile_declaration_annotation(child) {
                        if use_site_target(self.text(child)) == Some("get") {
                            getter.push(text);
                        } else {
                            field.push(text);
                        }
                    }
                } else {
                    stack.push(child);
                }
            }
        }
        (field, getter)
    }

    /// The `class_parameter` for a primary-constructor property, by name.
    fn param_node(
        &self,
        decl: tree_sitter::Node<'tree>,
        property: &str,
    ) -> Option<tree_sitter::Node<'tree>> {
        let parameters = kt::child(decl, "primary_constructor")
            .and_then(|ctor| kt::child(ctor, "class_parameters"))?;
        let mut cursor = parameters.walk();
        parameters
            .children(&mut cursor)
            .filter(|child| child.kind() == "class_parameter")
            .find(|child| {
                kt::child(*child, "identifier").is_some_and(|name| self.text(name) == property)
            })
    }

    /// Annotations for a constructor property that belong on the FIELD.
    fn param_annotations(&self, decl: tree_sitter::Node, property: &str) -> Vec<String> {
        self.param_node(decl, property)
            .map(|parameter| self.annotation_split(parameter).0)
            .unwrap_or_default()
    }

    /// Annotations for a constructor property that belong on the GETTER —
    /// `@get:`-targeted ones, which say so themselves.
    fn param_getter_annotations(&self, decl: tree_sitter::Node, property: &str) -> Vec<String> {
        self.param_node(decl, property)
            .map(|parameter| self.annotation_split(parameter).1)
            .unwrap_or_default()
    }

    fn class_body_declares_instance_fields(&self, decl: tree_sitter::Node) -> bool {
        let Some(body) = kt::child(decl, "class_body") else {
            return false;
        };
        let mut cursor = body.walk();
        body.children(&mut cursor).any(|member| {
            matches!(
                member.kind(),
                "property_declaration" | "variable_declaration"
            )
        })
    }

    fn class_body_defines_lombok_generated(&self, decl: tree_sitter::Node) -> bool {
        let Some(body) = kt::child(decl, "class_body") else {
            return false;
        };
        body.children(&mut body.walk())
            .filter(|m| m.kind() == "function_declaration")
            .any(|m| {
                kt::field(m, "name")
                    .map(|n| matches!(self.text(n), "equals" | "hashCode" | "toString"))
                    .unwrap_or(false)
            })
    }

    /// Primary constructor parameters -> (is_property, is_mutable, name,
    /// java_type). A parameter WITHOUT `val`/`var` is a plain constructor
    /// argument in Kotlin: it forwards to the superclass and exists only
    /// during initialization — it must NOT become a Java field.
    /// Shared by the class/enum/record paths so ctor-param handling stays in
    /// one place.
    fn class_params(&mut self, decl: tree_sitter::Node) -> Vec<(bool, bool, String, String)> {
        kt::child(decl, "primary_constructor")
            .and_then(|pc| kt::child(pc, "class_parameters"))
            .map(|cps| {
                let mut cursor = cps.walk();
                cps.children(&mut cursor)
                    .filter(|c| c.kind() == "class_parameter")
                    .filter_map(|cp| {
                        let is_property = kt::child(cp, "val").is_some()
                            || kt::child(cp, "var").is_some();
                        let is_mutable = kt::child(cp, "var").is_some();
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
                                    code: None,
                                });
                                "Object".to_string()
                            }
                        };
                        // A field/param cannot be `void`: Kotlin `Unit`
                        // members become boxed `Void` on the Java side.
                        let ty_java = if ty_java == "void" {
                            "Void".to_string()
                        } else {
                            ty_java
                        };
                        // Java reserved words (e.g. Kotlin `default`) cannot
                        // name a field: append '_' for the Java side. Accessor
                        // names derive from the escaped name, so the class is
                        // self-consistent; retained Kotlin callers relying on
                        // the exact bean name surface next compilation pass.
                        let param_name = self.text(ident).to_string();
                        let param_name = if java_reserved(param_name.as_str()) {
                            format!("{param_name}_")
                        } else {
                            param_name
                        };
                        Some((is_property, is_mutable, param_name, ty_java))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn class_param_default_nodes(
        &self,
        decl: tree_sitter::Node<'tree>,
    ) -> Vec<Option<tree_sitter::Node<'tree>>> {
        kt::child(decl, "primary_constructor")
            .and_then(|pc| kt::child(pc, "class_parameters"))
            .map(|cps| {
                let mut cursor = cps.walk();
                cps.children(&mut cursor)
                    .filter(|c| c.kind() == "class_parameter")
                    .map(|cp| {
                        let has_default = cp
                            .children(&mut cp.walk())
                            .any(|c| !c.is_named() && c.kind() == "=");
                        has_default
                            .then(|| cp.children(&mut cp.walk()).filter(|c| c.is_named()).last())
                            .flatten()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Transpiled default expressions for primary-ctor parameters, aligned
    /// by index (`None` where the parameter has no default). Used to fill
    /// enum-constant call sites, since Java has no default arguments.
    fn class_param_defaults(&mut self, decl: tree_sitter::Node) -> Vec<Option<String>> {
        self.class_param_default_nodes(decl)
            .into_iter()
            .map(|default| default.map(|ex| Expr { unit: self }.transpile(ex)))
            .collect()
    }

    /// `@JvmOverloads` on the primary constructor: the annotation that asks
    /// for Java overloads of the omitted trailing defaults.
    fn primary_constructor_has_jvm_overloads(&self, decl: tree_sitter::Node) -> bool {
        kt::child(decl, "primary_constructor")
            .and_then(|constructor| kt::child(constructor, "modifiers"))
            .is_some_and(|modifiers| {
                modifiers
                    .children(&mut modifiers.walk())
                    .filter(|node| node.kind() == "annotation")
                    .any(|annotation| self.text(annotation).contains("JvmOverloads"))
            })
    }

    /// The declaration the index holds for `name` in this file, when there is
    /// one. The retention rule and the emitter both read the INDEX's record of
    /// the constructor — parameter names, default texts — instead of re-reading
    /// the syntax, so the two cannot disagree about what a call site's shape is.
    fn indexed_target(&self, name: &str) -> Option<&crate::workspace::Declaration> {
        let workspace = self.workspace?;
        let indexed_path = self.workspace_file.as_deref().unwrap_or(self.file);
        let source_file = workspace.source_file(indexed_path)?;
        source_file
            .declarations
            .iter()
            .find(|declaration| declaration.name == name)
    }

    /// Whether the delegating overloads for this declaration's default
    /// arguments can be written, and which omission patterns need one.
    ///
    /// Reads the index's omission evidence for the declaration: a pattern no
    /// caller uses costs nothing, so only the shapes call sites actually write
    /// become constructors. `blocked` is why none can be written.
    fn ctor_default_plan(
        &self,
        decl: tree_sitter::Node<'_>,
        target: &crate::workspace::Declaration,
    ) -> crate::ctor_defaults::CtorDefaultPlan {
        let defaults = &target.constructor_param_defaults;
        // The ladder is what the emitter writes for Kotlin's own positional ABI
        // (any in-place run) or for an explicit `@JvmOverloads`; the plan must
        // not write it twice.
        let ladder = if self.in_place || self.primary_constructor_has_jvm_overloads(decl) {
            crate::ctor_defaults::trailing_patterns(defaults)
        } else {
            Vec::new()
        };
        let patterns = self
            .workspace
            .map(|workspace| workspace.ctor_omission_evidence(target).patterns)
            .unwrap_or_default();
        let param_types = crate::ctor_defaults::class_param_types(decl, self.source);
        crate::ctor_defaults::plan_ctor_defaults(&crate::ctor_defaults::CtorShape {
            param_names: &target.constructor_param_names,
            defaults,
            param_types: &param_types,
            patterns: &patterns,
            ladder: &ladder,
            has_secondary_constructor: target
                .members
                .iter()
                .any(|member| member.kind == crate::workspace::MemberKind::Constructor),
        })
    }

    /// `@JvmOverloads` exposes Java overloads for every omitted trailing
    /// primary-constructor default. The Kotlin annotation itself has no Java
    /// counterpart, so emit the overloads that express its ABI instead.
    ///
    /// Two sets of overloads are written, and only these:
    ///
    /// - the trailing ladder, for `@JvmOverloads` or an in-place run: Kotlin's
    ///   own positional constructor ABI, which a retained caller may use;
    /// - one overload per omission pattern a call site actually writes
    ///   ([`Self::ctor_default_plan`]) — a named-argument call omitting a middle
    ///   parameter, or one whose default cannot be written into the call.
    ///
    /// Never every subset: a declaration whose defaults no caller omits gets no
    /// overloads at all.
    fn emit_jvm_overloads_primary_constructors(
        &mut self,
        decl: tree_sitter::Node<'_>,
        name: &str,
        params: &[(bool, bool, String, String)],
        out: &mut JavaOut,
    ) {
        if params.is_empty() {
            return;
        }
        // Only a defaulted parameter can be delegated, and only a call site that
        // omits one needs an overload. Without either there is nothing to write —
        // and nothing to lower, which matters because lowering a default
        // expression can carry diagnostics.
        let ladder_requested = self.in_place || self.primary_constructor_has_jvm_overloads(decl);
        let has_defaults = self
            .indexed_target(name)
            .is_some_and(|target| target.has_default_constructor_parameter);
        if !has_defaults && !ladder_requested {
            return;
        }
        // Kotlin primary-constructor default expressions run before an instance
        // exists. Seed the constructor parameters as locals while lowering so
        // `label` stays `label`, rather than becoming `this.getLabel()`.
        let prior_var_types = std::mem::replace(
            &mut self.var_types,
            params
                .iter()
                .map(|(_, _, param_name, param_type)| (param_name.clone(), param_type.clone()))
                .collect(),
        );
        let defaults = self.class_param_defaults(decl);
        self.var_types = prior_var_types;
        let mut patterns = if self.in_place || self.primary_constructor_has_jvm_overloads(decl) {
            crate::ctor_defaults::trailing_patterns(&defaults)
        } else {
            Vec::new()
        };
        if let Some(target) = self.indexed_target(name) {
            let plan = self.ctor_default_plan(decl, target);
            if plan.blocked.is_none() {
                patterns.extend(plan.overloads);
            }
        }
        for pattern in &patterns {
            // Defensive: a pattern the index derived must line up with the
            // parameters the emitter lowered, and every omitted parameter must
            // have a lowered default to delegate with.
            if pattern.iter().any(|index| *index >= params.len())
                || pattern
                    .iter()
                    .any(|index| defaults.get(*index).is_none_or(Option::is_none))
            {
                continue;
            }
            let signature = (0..params.len())
                .filter(|index| !pattern.contains(index))
                .map(|index| {
                    let (_, _, param_name, param_type) = &params[index];
                    format!("{param_type} {param_name}")
                })
                .collect::<Vec<_>>()
                .join(", ");
            let values = (0..params.len())
                .map(|index| {
                    if pattern.contains(&index) {
                        defaults[index]
                            .clone()
                            .unwrap_or_else(|| "null".to_string())
                    } else {
                        params[index].2.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.blank();
            out.open(format!("public {}({})", name, signature));
            out.line(format!("this({});", values));
            out.close();
        }
    }

    /// Whether the emitter writes a zero-argument constructor for this class
    /// for a reason OTHER than the JPA rule: every primary-constructor
    /// parameter has a default and the delegating overloads are being written
    /// (`@JvmOverloads`, or an in-place run that keeps Kotlin's default
    /// arguments callable). Kotlin has that constructor too — so the JPA rule
    /// must not add a second one, and its body (a `this(...)` delegation) is
    /// the one Kotlin's own bytecode holds.
    fn emits_zero_arg_constructor_overload(
        &self,
        decl: tree_sitter::Node,
        params: &[(bool, bool, String, String)],
        all_params_defaulted: bool,
    ) -> bool {
        !params.is_empty()
            && all_params_defaulted
            && (self.in_place || self.primary_constructor_has_jvm_overloads(decl))
    }

    /// An explicit `constructor()` (zero parameters) already gives the class
    /// the constructor JPA needs. Measured: with one declared, kotlinc's
    /// no-arg plugin emits exactly one zero-argument constructor (the declared
    /// one delegating to the primary), not two.
    fn declares_zero_arg_secondary_constructor(&self, decl: tree_sitter::Node) -> bool {
        let Some(body) = kt::child(decl, "class_body") else {
            return false;
        };
        body.children(&mut body.walk())
            .filter(|member| member.kind() == "secondary_constructor")
            .any(
                |constructor| match kt::child(constructor, "function_value_parameters") {
                    Some(parameters) => !parameters
                        .children(&mut parameters.walk())
                        .any(|child| child.kind() == "parameter"),
                    None => true,
                },
            )
    }

    /// (field name, JVM default literal) for every instance field the class
    /// emitter writes for `decl`: the primary-constructor properties, plus the
    /// class-body properties that emit a backing field AND declare their type.
    ///
    /// A body property whose type is only inferred is skipped: property.rs
    /// resolves those through a cross-file/inherited-type path this pass does
    /// not reproduce, and guessing a primitive wrong would emit `null` into an
    /// `int` field. Skipping costs fidelity (that field keeps its Java
    /// initializer where the plugin's constructor would leave it default);
    /// guessing costs the compile.
    ///
    /// The field predicate is SHARED with property.rs — a property that emits
    /// no field must not be assigned here.
    fn jpa_instance_field_defaults(
        &mut self,
        decl: tree_sitter::Node,
        params: &[(bool, bool, String, String)],
    ) -> Vec<(String, String)> {
        let mut fields: Vec<(String, String)> = params
            .iter()
            .filter(|(is_property, _, _, _)| *is_property)
            .map(|(_, _, name, java_type)| {
                (name.clone(), jvm_default_literal(java_type).to_string())
            })
            .collect();
        let Some(body) = kt::child(decl, "class_body") else {
            return fields;
        };
        let mut cursor = body.walk();
        let members: Vec<tree_sitter::Node> = body.children(&mut cursor).collect();
        for member in members {
            if member.kind() != "property_declaration" || !self.property_emits_backing_field(member)
            {
                continue;
            }
            let Some(variable) = kt::child(member, "variable_declaration") else {
                continue;
            };
            let declared =
                kt::child(variable, "user_type").or_else(|| kt::child(variable, "nullable_type"));
            let (Some(declared), Some(identifier)) = (declared, kt::child(variable, "identifier"))
            else {
                continue;
            };
            let java_type = kt::java_type_ann(declared, self.source, self.annots);
            fields.push((
                self.text(identifier).to_string(),
                jvm_default_literal(&java_type).to_string(),
            ));
        }
        fields
    }

    /// Kotlin's `kotlin-jpa` plugin (= the `no-arg` plugin with the JPA
    /// preset) adds a zero-argument constructor to every @Entity/@Embeddable/
    /// @MappedSuperclass class, so the ORM can instantiate it by reflection.
    /// Nothing in the source spells that constructor out, and the flag that
    /// turns it on lives in the BUILD FILE — not in the AST — so the Java side
    /// has to synthesize it from the annotation alone.
    ///
    /// Measured on the reference module's own bytecode (kotlinc 2.3.21 +
    /// `org.jetbrains.kotlin.plugin.jpa`, `javap -p -c` on the compiled
    /// entities):
    ///
    /// ```text
    /// public AuthenticatedConnectorRefEntity();
    ///   Code: aload_0
    ///         invokespecial .../NameLookupEntity."<init>":()V
    ///         return
    /// ```
    ///
    /// Three properties of that constructor decide the Java emission:
    ///   * it is `public` (NOT synthetic), so Java source may call it;
    ///   * it runs NO property initializer and no `init` block: the fields come
    ///     out at their JVM defaults, even when the source initializes them
    ///     (`WorkView.items` is null, not an empty list; `WorkEntity.status` is
    ///     null, not `CREATED`). That is the plugin's default
    ///     (`invokeInitializers = false`);
    ///   * it forwards to `super()` with NO arguments, so the superclass must
    ///     itself be zero-arg constructible. kotlinc enforces that — `error:
    ///     zero-argument constructor was not found in the superclass` — and a
    ///     JPA superclass gets its constructor from this SAME rule, which is
    ///     why the emitted `super()` resolves for `: NameLookupEntity(id,
    ///     lookupId)` even though those arguments do not exist in a
    ///     zero-argument constructor.
    ///
    /// Java has no synthetic constructor and does not clear a field on its own,
    /// so the faithful form is an explicit constructor: `super()`, then every
    /// instance field assigned its type's default. `final` fields force the
    /// question — an empty constructor body would not compile.
    fn emit_jpa_no_arg_constructor(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        params: &[(bool, bool, String, String)],
        has_class_supertype: bool,
        out: &mut JavaOut,
    ) {
        if params.is_empty() || self.declares_zero_arg_secondary_constructor(decl) {
            return;
        }
        let defaults = self.class_param_defaults(decl);
        let all_defaulted = defaults.len() == params.len()
            && !defaults.is_empty()
            && defaults.iter().all(|default| default.is_some());
        if self.emits_zero_arg_constructor_overload(decl, params, all_defaulted) {
            return;
        }
        if !out.buf.ends_with("\n\n") {
            out.blank();
        }
        out.open(format!("public {}()", name));
        if all_defaulted {
            // Kotlin gives this class a no-argument constructor of its own
            // (every parameter defaulted), and THAT one delegates to the
            // primary constructor — so its initializers do run. Reproduce
            // Kotlin's constructor, not the plugin's field-clearing form.
            let values: Vec<String> = defaults.into_iter().flatten().collect();
            out.line(format!("this({});", values.join(", ")));
        } else {
            if has_class_supertype {
                out.line("super();");
            }
            for (field, literal) in self.jpa_instance_field_defaults(decl, params) {
                out.line(format!("this.{} = {};", field, literal));
            }
        }
        out.close();
    }

    /// The record form of the same constructor. A record's components are
    /// `final`, so a body that assigns them itself does not exist — the
    /// constructor has to delegate to the canonical one (`this(...)`), passing
    /// each component its type's default. That is the same instance Kotlin's
    /// plugin constructor produces: no initializer runs, so a `val name:
    /// String` component is null and an `Int` component is 0.
    fn emit_jpa_no_arg_record_constructor(
        &mut self,
        decl: tree_sitter::Node,
        name: &str,
        params: &[(bool, bool, String, String)],
        out: &mut JavaOut,
    ) {
        if params.is_empty() || self.declares_zero_arg_secondary_constructor(decl) {
            return;
        }
        let defaults = self.class_param_defaults(decl);
        let all_defaulted = defaults.len() == params.len()
            && !defaults.is_empty()
            && defaults.iter().all(|default| default.is_some());
        if self.emits_zero_arg_constructor_overload(decl, params, all_defaulted) {
            return;
        }
        let values: Vec<String> = if all_defaulted {
            defaults.into_iter().flatten().collect()
        } else {
            params
                .iter()
                .map(|(_, _, _, java_type)| jvm_default_literal(java_type).to_string())
                .collect()
        };
        out.blank();
        out.open(format!("public {}()", name));
        out.line(format!("this({});", values.join(", ")));
        out.close();
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
            .map(|(_, _, fname, ftype)| (fname.clone(), ftype.clone()))
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
                                            let mut rendered = e2.transpile(ex);
                                            let expects_object =
                                                params.get(args.len()).is_some_and(
                                                    |(_, _, _, ty)| ty.ends_with("Object"),
                                                );
                                            if expects_object && rendered == "() -> {}" {
                                                rendered = "(kotlin.jvm.functions.Function0<kotlin.Unit>) () -> kotlin.Unit.INSTANCE".to_string();
                                            }
                                            args.push(rendered);
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
            for (is_property, is_mutable, fname, ftype) in &params {
                if !*is_property {
                    continue;
                }
                let final_kw = if !*is_mutable { "final " } else { "" };
                // Kotlin writes persistence and metadata annotations on the
                // constructor property; the Java side must carry them on the
                // FIELD, which is the element the ORM reads.
                for annotation in self.param_annotations(decl, fname) {
                    out.line(annotation);
                }
                out.line(format!("private {}{} {};", final_kw, ftype, fname));
            }
            out.blank();
            for (_is_property, is_mutable, fname, ftype) in &params {
                let cap = capitalize(fname);
                // `@get:`-targeted annotations belong on the generated
                // getter, which this path emits.
                for annotation in self.param_getter_annotations(decl, fname) {
                    out.line(annotation);
                }
                out.open(format!("public {} get{}()", ftype, cap));
                out.line(format!("return {};", fname));
                out.close();
                if *is_mutable {
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
                    .map(|(_, _, n, t)| format!("{} {}", t, n))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            for (is_property, _, fname, _) in &params {
                if *is_property {
                    out.line(format!("this.{} = {};", fname, fname));
                }
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
                "line_comment" | "block_comment" | ";" | "{" | "}" => {}
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
        // a class supertype or implement an interface. Kotlin uses the same
        // syntax for both, so consult the workspace index when available.
        let mut obj_supertype = String::new();
        if let Some(ds) = kt::child(decl, "delegation_specifiers") {
            let mut dcur = ds.walk();
            for spec in ds.children(&mut dcur) {
                if spec.kind() == "delegation_specifier" {
                    let sup = kt::child(spec, "super_type")
                        .or_else(|| spec.children(&mut spec.walk()).find(|c| c.is_named()));
                    if let Some(st) = sup {
                        let mut scur = st.walk();
                        let base = st.children(&mut scur).find(|c| c.is_named()).unwrap_or(st);
                        let ty = kt::text(base, self.source).trim().replace(" ", "");
                        let bare = ty
                            .split('<')
                            .next()
                            .unwrap_or(&ty)
                            .rsplit('.')
                            .next()
                            .unwrap_or(&ty);
                        let is_interface = self.workspace.is_some_and(|workspace| {
                            workspace.declarations_named(bare).any(|declaration| {
                                declaration.name == bare
                                    && declaration.kind
                                        == crate::workspace::DeclarationKind::Interface
                            })
                        });
                        obj_supertype = format!(
                            " {} {}",
                            if is_interface {
                                "implements"
                            } else {
                                "extends"
                            },
                            ty
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
            visibility, static_kw, name, obj_supertype
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
                        // An overridden object property implements an
                        // interface accessor. Java forbids a static method
                        // from overriding that instance method; keep this
                        // member on the singleton instance.
                        let is_override = kt::child(member, "modifiers")
                            .is_some_and(|modifiers| self.text(modifiers).contains("override"));
                        self.transpile_property_opts(member, out, !is_override, Some(name));
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
                    "line_comment" | "block_comment" | ";" | "{" | "}" => {}
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
        let mut written_args = Vec::new();
        for arg in args_node.children(&mut args_node.walk()) {
            if arg.kind() != "value_argument" {
                continue;
            }
            let children: Vec<_> = arg.children(&mut arg.walk()).collect();
            let has_name = children
                .iter()
                .any(|child| !child.is_named() && child.kind() == "=");
            let named: Vec<_> = children
                .iter()
                .copied()
                .filter(|child| child.is_named())
                .collect();
            let Some(value) = named.last().copied() else {
                self.var_types = prior_var_types;
                return false;
            };
            if !self.is_safe_secondary_constructor_argument(value) {
                self.var_types = prior_var_types;
                return false;
            }
            let name = if has_name {
                let Some(name) = named.first().filter(|node| node.kind() == "identifier") else {
                    self.var_types = prior_var_types;
                    return false;
                };
                Some(self.text(*name).to_string())
            } else {
                None
            };
            written_args.push((name, Expr { unit: self }.transpile(value)));
        }
        let args = if delegation_target == "this" {
            let Some(class_decl) = kt::parent_of(member)
                .and_then(kt::parent_of)
                .filter(|node| node.kind() == "class_declaration")
            else {
                self.var_types = prior_var_types;
                return false;
            };
            let primary = self.class_params(class_decl);
            let names: Vec<String> = primary.into_iter().map(|(_, _, name, _)| name).collect();
            let Some(resolved) = crate::ctor_defaults::resolve_args(&names, &written_args) else {
                self.var_types = prior_var_types;
                return false;
            };
            let defaults = self.class_param_default_nodes(class_decl);
            let mut args = Vec::with_capacity(names.len());
            for (index, slot) in resolved.slots.into_iter().enumerate() {
                if let Some(value) = slot {
                    args.push(value);
                    continue;
                }
                let Some(default) = defaults.get(index).copied().flatten() else {
                    self.var_types = prior_var_types;
                    return false;
                };
                if !self.is_safe_secondary_constructor_argument(default)
                    || crate::ctor_defaults::unsupplied_reference(self.text(default), &names, &[])
                        .is_some()
                {
                    self.var_types = prior_var_types;
                    return false;
                }
                args.push(Expr { unit: self }.transpile(default));
            }
            args
        } else {
            if written_args.iter().any(|(name, _)| name.is_some()) {
                self.var_types = prior_var_types;
                return false;
            }
            written_args.into_iter().map(|(_, value)| value).collect()
        };
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

    /// Delegated constructor arguments are narrower than the general expression
    /// lowerer. Admit constructor calls and the Java-compatible library calls
    /// needed by constructor defaults, while leaving Kotlin collection/extension
    /// calls (for example `joinToString`) in Kotlin.
    fn is_safe_secondary_constructor_argument(&self, node: tree_sitter::Node) -> bool {
        match node.kind() {
            "identifier" | "this_expression" | "number_literal" | "boolean_literal"
            | "hex_literal" | "long_literal" | "real_literal" | "null_literal" => true,
            "string_literal" => !self.text(node).contains('$'),
            "navigation_expression" | "parenthesized_expression" | "value_arguments" => node
                .children(&mut node.walk())
                .filter(|child| child.is_named())
                .all(|child| self.is_safe_secondary_constructor_argument(child)),
            "value_argument" => node
                .children(&mut node.walk())
                .filter(|child| child.is_named())
                .last()
                .is_some_and(|value| self.is_safe_secondary_constructor_argument(value)),
            "call_expression" => {
                let named: Vec<_> = node
                    .children(&mut node.walk())
                    .filter(|child| child.is_named())
                    .collect();
                let Some(callee) = named.first().copied() else {
                    return false;
                };
                let callee_text = self.text(callee).trim();
                let method = callee_text
                    .rsplit_once('.')
                    .map_or(callee_text, |(_, method)| method);
                let is_constructor = method
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_uppercase());
                let is_supported_method = matches!(method, "getOrDefault" | "now" | "toInstant")
                    || self.is_declared_zero_arg_instance_call(node, callee_text);
                (is_constructor || is_supported_method)
                    && named
                        .iter()
                        .skip(1)
                        .all(|child| self.is_safe_secondary_constructor_argument(*child))
            }
            _ => false,
        }
    }

    fn is_declared_zero_arg_instance_call(
        &self,
        call: tree_sitter::Node,
        callee_text: &str,
    ) -> bool {
        let Some(arguments) = kt::child(call, "value_arguments") else {
            return false;
        };
        if arguments
            .children(&mut arguments.walk())
            .any(|child| child.kind() == "value_argument")
        {
            return false;
        }
        let Some((receiver, method)) = callee_text.rsplit_once('.') else {
            return false;
        };
        if receiver.contains('.') {
            return false;
        }
        let Some(receiver_type) = self.var_types.get(receiver.trim()) else {
            return false;
        };
        let Some(workspace) = self.workspace else {
            return false;
        };
        let declaring = self.workspace_file.as_deref().unwrap_or(self.file);
        let Some(source_file) = workspace.source_file(declaring) else {
            return false;
        };
        workspace.type_declares_zero_arg_instance_method(source_file, receiver_type, method.trim())
    }

    fn transpile_interface_companion(&mut self, companion: tree_sitter::Node, out: &mut JavaOut) {
        let Some(body) = kt::child(companion, "class_body") else {
            return;
        };
        let mut cursor = body.walk();
        for member in body.children(&mut cursor) {
            match member.kind() {
                "property_declaration" => {
                    self.transpile_property_opts(member, out, true, Some("__interface__"));
                    out.blank();
                }
                "line_comment" | "block_comment" | ";" | "{" | "}" => {}
                _ => {
                    if member.is_named() {
                        self.diag_untranslatable(
                            member,
                            format!(
                                "interface companion member not supported: {}",
                                member.kind()
                            ),
                        );
                    }
                }
            }
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
                    let is_interface = kt::parent_of(body).is_some_and(|class| {
                        class
                            .children(&mut class.walk())
                            .any(|child| child.kind() == "interface")
                    });
                    if is_interface {
                        self.transpile_interface_companion(member, out);
                    } else if let Some(cls) = kt::parent_of(body)
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
                "line_comment" | "block_comment" | ";" | "{" | "}" => {}
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

/// Render a source path for the provenance header: forward slashes only (javac
/// rejects `\uXXXX`-shaped sequences in comments on some paths) and without the
/// Windows verbatim prefix, which otherwise leaks a meaningless `//?/D:/…`.
fn normalized_source_path(path: &std::path::Path) -> String {
    let text = crate::paths::display(path).to_string();
    let text = text
        .strip_prefix("\\\\?\\")
        .or_else(|| text.strip_prefix("//?/"))
        .or_else(|| text.strip_prefix("\\\\.\\"))
        .or_else(|| text.strip_prefix("//./"))
        .unwrap_or(&text);
    text.replace('\\', "/")
}

/// Whether generated Java actually mentions `import`'s simple name. Unused
/// imports are not merely noise — when the name resolves only in Kotlin (a
/// dependency's top-level function, or a declaration that stayed Kotlin in a
/// split file) javac fails the whole compilation unit with "cannot find
/// symbol". Wildcard imports are kept: they are package-wide and there is no
/// name to match.
/// Qualify a simple type name that collides with the enclosing declaration's
/// own name, but ONLY where it sits in a type-argument list (`<Length>`,
/// `<Length,`, `List<Length[]>`): that is where Java's "the class's own name
/// outranks its imports" rule changes the meaning of migrated code. Kotlin
/// resolved such a name to the import, so `Unit<Length>` satisfies
/// `Unit<Q : Quantity<Q>>`; javac instead reads the enclosing `Length` and
/// rejects the type argument. Only the imported top-level FQN can be the
/// intended target, so the import list is the resolver.
fn qualify_shadowing_type_arguments(
    body: &str,
    own_names: &[String],
    imports: &[String],
) -> String {
    // A type declared by this very compilation unit outranks any import in
    // javac's resolution; Kotlin instead resolved the name to the import.
    // Each such collision needs the imported FQN spelled out.
    let mut shadows: Vec<(&str, &str)> = Vec::new();
    for import in imports {
        let path = match import.strip_prefix("static ") {
            Some(path) => path.trim(),
            None => import.trim(),
        };
        if path.contains(' ') || !path.contains('.') {
            continue;
        }
        let Some(last) = path.rsplit('.').next().filter(|last| !last.is_empty()) else {
            continue;
        };
        if own_names
            .iter()
            .any(|own| own.rsplit('.').next() == Some(last))
            && !shadows.iter().any(|(simple, _)| *simple == last)
        {
            shadows.push((last, path));
        }
    }
    if shadows.is_empty() {
        return body.to_string();
    }
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len() + 32);
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            let mut end = index;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            let word = &body[index..end];
            // The token before a type argument is `<` (first) or `,`
            // (later); the token after a type argument is `>`, `,` or `[`.
            // A method-argument list (`f(a, Length)`) never closes that way,
            // and `Length.member` / `new Length(` keep the class's own name.
            let opens = body[..index]
                .trim_end()
                .chars()
                .last()
                .is_some_and(|previous| previous == '<' || previous == ',');
            let closes = body[end..]
                .trim_start()
                .chars()
                .next()
                .is_some_and(|next| next == '>' || next == ',' || next == '[');
            if opens
                && closes
                && let Some((_, fqn)) = shadows.iter().find(|(simple, _)| *simple == word)
            {
                out.push_str(fqn);
            } else {
                out.push_str(word);
            }
            index = end;
            continue;
        }
        // Multi-byte UTF-8 (string literals) must be copied whole.
        if byte.is_ascii() {
            out.push(byte as char);
            index += 1;
        } else {
            let width = body[index..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&body[index..index + width]);
            index += width;
        }
    }
    out
}

fn import_is_referenced(import: &str, body: &str) -> bool {
    let path = import.strip_prefix("static ").unwrap_or(import).trim();
    if path.is_empty() || path.ends_with(".*") {
        return true;
    }
    let simple = path.rsplit('.').next().unwrap_or(path);
    if simple.is_empty() {
        return true;
    }
    let mut rest = body;
    while let Some(index) = rest.find(simple) {
        let before = rest[..index].chars().next_back();
        let after = rest[index + simple.len()..].chars().next();
        let boundary =
            |ch: Option<char>| ch.is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '$'));
        if boundary(before) && boundary(after) {
            return true;
        }
        rest = &rest[index + simple.len()..];
    }
    false
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
    // A `Name(` inside a STRING literal is text, not a nested annotation:
    // `@Anno(name = "a(b)")` must not come out as `"@a(b)"`.
    let mut in_string = false;
    while index < bytes.len() {
        let character = bytes[index] as char;
        if character == '"' {
            in_string = !in_string;
            token_start = None;
            result.push(character);
            index += 1;
            continue;
        }
        if in_string {
            result.push(character);
            index += 1;
            if character == '\\' && index < bytes.len() {
                result.push(bytes[index] as char);
                index += 1;
            }
            continue;
        }
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
                // Any `Name(` in an annotation argument is a nested annotation:
                // Java has no constructor calls or method calls there, only
                // constants and annotations. Kotlin writes the nested form
                // WITHOUT the `@`, so it has to be added — a bare `Index(...)`
                // left as-is is javac's "annotation value must be an
                // annotation".
                if !already_annotated {
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
    // A Kotlin argument list may end with a trailing comma
    // (`@Anno(a = 1,)`); Java annotation arrays reject it — drop a comma
    // immediately followed by the closing paren, across line breaks.
    let mut cleaned = String::with_capacity(result.len());
    let mut characters = result.chars().peekable();
    while let Some(character) = characters.next() {
        if character == ',' {
            let mut lookahead = characters.clone();
            let mut trimmed = String::new();
            while let Some(&next) = lookahead.peek() {
                if next.is_whitespace() {
                    trimmed.push(next);
                    lookahead.next();
                } else {
                    break;
                }
            }
            if matches!(lookahead.peek(), Some(&')') | Some(&'}')) {
                continue; // drop the comma (and surrounding whitespace stays)
            }
        }
        cleaned.push(character);
    }
    cleaned
}

/// Java reserved identifiers that cannot name a field or parameter.
/// A persistence annotation whose classes the ORM must be able to subclass at
/// runtime for lazy proxies. `final` forbids that, and the failure is at
/// STARTUP rather than compile time — Kotlin marks such sources `open` via the
/// all-open plugin, leaving no `open` token in the AST for us to read.
/// The use-site target of an annotation (`@get:Foo` -> `get`), or `None` when
/// it has none. Kotlin lets an annotation say which element it belongs to;
/// dropping the prefix is required (Java has no such syntax) but dropping the
/// TARGET is a decision, not a detail.
fn use_site_target(annotation: &str) -> Option<&str> {
    let rest = annotation.trim_start().strip_prefix('@')?;
    let target = rest.split(':').next()?;
    if rest.contains(':') && !target.is_empty() && target.chars().all(|c| c.is_ascii_alphabetic()) {
        Some(target)
    } else {
        None
    }
}

/// The simple name of an annotation as it reached the emitted Java text:
/// `@jakarta.persistence.Entity` -> `Entity`, `@Type(IEnumerationType::class)`
/// -> `Type`.
fn annotation_simple_name(annotation: &str) -> &str {
    annotation
        .trim_start_matches('@')
        .split('(')
        .next()
        .unwrap_or("")
        .trim()
        .rsplit('.')
        .next()
        .unwrap_or("")
}

fn is_orm_proxied_annotation(annotation: &str) -> bool {
    matches!(
        annotation_simple_name(annotation),
        "Entity" | "MappedSuperclass"
    )
}

/// The no-arg trigger set of Kotlin's `kotlin-jpa` plugin — the `no-arg`
/// plugin with the JPA preset, which annotates exactly these three
/// annotations with "needs a zero-argument constructor".
fn is_jpa_no_arg_annotation(annotation: &str) -> bool {
    matches!(
        annotation_simple_name(annotation),
        "Entity" | "Embeddable" | "MappedSuperclass"
    )
}

/// The value a Java field of this declared type holds when no constructor
/// assigns it: the zero literal for a primitive, `null` for everything else
/// (references, arrays, boxed primitives, type variables). A leading
/// annotation (`@Nullable Boolean`) is not part of the type name, and a
/// generic/array suffix cannot turn a reference type into a primitive.
fn jvm_default_literal(java_type: &str) -> &'static str {
    let trimmed = java_type.trim();
    let bare = match trimmed.strip_prefix('@') {
        Some(rest) => rest.split_once(' ').map(|(_, ty)| ty.trim()).unwrap_or(""),
        None => trimmed,
    };
    let head = bare.split(['<', '[']).next().unwrap_or(bare).trim();
    match head {
        "boolean" => "false",
        "char" => "'\\0'",
        "byte" => "(byte) 0",
        "short" => "(short) 0",
        "int" => "0",
        "long" => "0L",
        "float" => "0.0f",
        "double" => "0.0",
        _ => "null",
    }
}

fn java_reserved(word: &str) -> bool {
    matches!(
        word,
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "var"
            | "void"
            | "volatile"
            | "while"
            | "sealed"
            | "permits"
            | "record"
    )
}
