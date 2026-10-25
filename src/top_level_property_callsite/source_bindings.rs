//! Source-bound static-field import obligations. The expression stays Kotlin.
use super::*;
use crate::facts::protocol::SourcePropertyReference;
use crate::semantics::{SemanticSymbol, symbol_id_for_node};
use serde::{Deserialize, Serialize};

/// The shared field emission proof: the declaration's Java field type exactly
/// as the emitter must write it, and how generated Java binds that name.
///
/// This is the *type* half of a field ABI, so it is persisted on the plan and
/// re-proved at pre-write acceptance instead of being recomputed from a source
/// spelling at emission time. It exists only where the source-tree proof alone
/// is insufficient: a wildcard import makes the declaration's `String` spelling
/// ambiguous for a Java reader, so the type comes from the native declaration
/// descriptor and the emitted name is bound by [`Self::binding_import`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldEmissionProof {
    /// The property declaration this proof describes, in the target file.
    pub start_byte: usize,
    pub end_byte: usize,
    /// blake3 of the exact target source the declaration facts were taken for.
    pub source_hash: [u8; 32],
    /// The simple Java class name of the generated unit that owns the field.
    /// Re-proved at acceptance: a facade class of the same name as the bound
    /// type would shadow the binding from inside its own body.
    pub facade: String,
    /// The JVM descriptor of the declared type, with no call-site substitution.
    pub descriptor: String,
    /// The declared type's nullability.
    pub nullable: bool,
    /// The Java type text the field emitter must write.
    pub emitted_type: String,
    /// The single-type import that makes that text mean the proved type.
    pub binding_import: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyImportProof {
    pub target: SymbolId,
    pub target_hash: [u8; 32],
    pub consumer: SourceLocation,
    pub visible: String,
    pub import_span: Option<(usize, usize)>,
    pub references: Vec<SourcePropertyReference>,
    /// Absent for the shapes the source-tree proof already covers: those keep
    /// their existing emission untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<FieldEmissionProof>,
}

pub(super) struct BindingContext<'a> {
    pub source: &'a str,
    pub file: &'a Path,
    pub root: Node<'a>,
    pub provider: &'a dyn SemanticProvider,
    pub safe_targets: &'a HashMap<SymbolId, bool>,
    pub editable: bool,
}
impl BindingContext<'_> {
    /// A fact proves binding, not emission of the consumer. Keep its whole
    /// ownership boundary Kotlin and retarget only the imported constant name.
    pub(super) fn record(
        &self,
        node: Node<'_>,
        span: (usize, usize),
        visible: &str,
        target: &Target,
        result: &mut RetentionMap,
    ) -> bool {
        if !self.editable
            || self.root.has_error()
            || is_callable_reference(node)
            || !self.safe_targets.get(&target.id).copied().unwrap_or(false)
        {
            return false;
        }
        let hash = *blake3::hash(self.source.as_bytes()).as_bytes();
        let Some(reference) =
            self.provider
                .source_property_reference(self.file, span.0 as u64, &hash)
        else {
            return false;
        };
        if reference.end_byte != span.1 as u64
            || self.source.get(span.0..span.1) != Some(reference.text.as_str())
            || reference.text.trim_matches('`') != visible.trim_matches('`')
            || reference.target_file != target.id.file
            || reference.target_name != target.id.name
            || reference.target_package != target.id.package
        {
            return false;
        }
        let Some(symbol) = self
            .provider
            .symbols_named(&target.id.name)
            .into_iter()
            .find(|symbol| symbol.id == target.id)
        else {
            return false;
        };
        if !self
            .provider
            .source_for_file(&target.id.file)
            .is_some_and(|source| {
                target_span_matches(
                    source,
                    &reference,
                    symbol.location.start_byte,
                    symbol.location.end_byte,
                )
            })
        {
            return false;
        }
        // The declaration's type is part of the field ABI this reference relies
        // on: an unproved type keeps the reference (and the declaration) Kotlin.
        let Some(field) = self.constant_field(symbol, &reference, &target.facade) else {
            return false;
        };
        let Some(import_span) = self.import_binding(target, visible) else {
            return false;
        };
        let Some(owner) = top_level_owner(node) else {
            return false;
        };
        let owner = symbol_id_for_node(self.source, owner, self.file);
        let proof = PropertyImportProof {
            target: target.id.clone(),
            target_hash: symbol.location.snapshot_hash,
            consumer: SourceLocation {
                file: self.file.to_path_buf(),
                snapshot_hash: hash,
                start_byte: 0,
                end_byte: self.source.len(),
            },
            visible: visible.to_owned(),
            import_span,
            references: vec![reference],
            field,
        };
        if let Some(known) = result.binding_imports.iter_mut().find(|known| {
            known.target == proof.target
                && known.consumer == proof.consumer
                && known.visible == proof.visible
                && known.import_span == proof.import_span
        }) {
            if !known.references.contains(&proof.references[0]) {
                known.references.push(proof.references[0].clone());
            }
        } else {
            result.binding_imports.push(proof);
        }
        result
            .binding_consumers
            .entry(target.id.clone())
            .or_default()
            .insert(owner);
        true
    }

    /// Prove the declaration's Java field type for this reference.
    ///
    /// `None` refuses (the reference and its declaration stay Kotlin). The two
    /// `Some` shapes are different contracts:
    /// * `Some(None)` — the source-tree slot proof already covers the type, so
    ///   emission keeps its existing spelling and nothing new is written;
    /// * `Some(Some(_))` — a wildcard import makes that spelling ambiguous, so
    ///   the type is proved from the native declaration facts and the plan
    ///   persists how generated Java must bind the emitted name.
    fn constant_field(
        &self,
        symbol: &SemanticSymbol,
        reference: &SourcePropertyReference,
        facade: &str,
    ) -> Option<Option<FieldEmissionProof>> {
        let source = self.provider.source_for_file(&symbol.id.file)?;
        if *blake3::hash(source.as_bytes()).as_bytes() != symbol.location.snapshot_hash {
            return None;
        }
        let tree = parse(source)?;
        let declaration = tree
            .root_node()
            .named_children(&mut tree.root_node().walk())
            .find(|n| {
                n.start_byte() == symbol.location.start_byte
                    && n.end_byte() == symbol.location.end_byte
            })?;
        let ty = constant_string_declaration(source, declaration)?;
        if crate::semantics::is_canonical_string_slot(source, ty, self.provider) {
            return Some(None);
        }
        // A typealias anywhere the file can see, named `String`, would make the
        // emitted spelling depend on alias lowering rather than on this binding.
        if self
            .provider
            .type_aliases()
            .iter()
            .any(|((_, alias), _)| alias == "String")
        {
            return None;
        }
        // The declaration type is not provable from the source tree. Only the
        // helper's *declaration* descriptor can say which `String` this is, and
        // only a name-binding import can make the emitted spelling mean the same
        // thing to javac.
        let canonical = crate::semantics::canonical_string_field_type(
            source,
            ty,
            facade,
            reference.target_jvm_descriptor.as_deref(),
            reference.target_nullable,
        )?;
        Some(Some(FieldEmissionProof {
            start_byte: declaration.start_byte(),
            end_byte: declaration.end_byte(),
            source_hash: symbol.location.snapshot_hash,
            facade: facade.rsplit('.').next().unwrap_or(facade).to_owned(),
            descriptor: canonical.descriptor.to_owned(),
            nullable: false,
            emitted_type: canonical.emitted.to_owned(),
            binding_import: canonical.binding_import.to_owned(),
        }))
    }

    /// Adding a single-property import must not capture a different binding
    /// previously reachable through wildcard/default imports. Prove every
    /// unqualified use of this spelling in the file, not merely this one read.
    fn all_uses_bind(&self, target: &Target, visible: &str) -> bool {
        let hash = *blake3::hash(self.source.as_bytes()).as_bytes();
        let matches = |start: usize, end: usize| {
            self.provider
                .source_property_reference(self.file, start as u64, &hash)
                .is_some_and(|r| {
                    r.end_byte == end as u64
                        && self.source.get(start..end) == Some(r.text.as_str())
                        && r.target_file == target.id.file
                        && r.target_name == target.id.name
                        && r.target_package == target.id.package
                })
        };
        let mut stack = vec![self.root];
        while let Some(node) = stack.pop() {
            if matches!(node.kind(), "string_literal" | "multiline_string_literal") {
                for (start, end, name) in
                    short_template_names(node.utf8_text(self.source.as_bytes()).unwrap_or(""))
                {
                    if name.trim_matches('`') == visible
                        && !matches(node.start_byte() + start, node.start_byte() + end)
                    {
                        return false;
                    }
                }
            }
            if is_identifier(node)
                && node
                    .utf8_text(self.source.as_bytes())
                    .is_ok_and(|text| text.trim_matches('`') == visible)
                && !in_import(node)
                && !is_declaration_name(node, self.source)
                && qualified_property_reference(node, self.source).0.is_none()
                && (is_callable_reference(node) || !matches(node.start_byte(), node.end_byte()))
            {
                return false;
            }
            stack.extend(node.named_children(&mut node.walk()));
        }
        true
    }

    /// Outer Option: proved retargeting. Inner Option: existing import vs a
    /// name requiring a new explicit import. Resolve header tokens, not lines.
    fn import_binding(&self, target: &Target, visible: &str) -> Option<Option<(usize, usize)>> {
        let name = visible.trim_matches('`');
        if !self.all_uses_bind(target, name) {
            return None;
        }
        let mut matched = Vec::new();
        let mut wildcard_target = false;
        for header in self
            .root
            .named_children(&mut self.root.walk())
            .filter(|n| n.kind() == "import")
        {
            let mut path = None;
            let mut alias = None;
            let mut wildcard = false;
            for child in header.children(&mut header.walk()) {
                match child.kind() {
                    kind if kind.ends_with("comment") => {}
                    "import" | "as" | "." | ";" => {}
                    "*" => wildcard = true,
                    "qualified_identifier" => {
                        let mut segments = Vec::new();
                        for segment in child.named_children(&mut child.walk()) {
                            if segment.kind().ends_with("comment") {
                                continue;
                            }
                            if segment.kind() != "identifier" {
                                return None;
                            }
                            segments.push(
                                segment
                                    .utf8_text(self.source.as_bytes())
                                    .ok()?
                                    .trim_matches('`'),
                            );
                        }
                        if path.replace(segments.join(".")).is_some() {
                            return None;
                        }
                    }
                    "identifier" => {
                        if alias
                            .replace(
                                child
                                    .utf8_text(self.source.as_bytes())
                                    .ok()?
                                    .trim_matches('`'),
                            )
                            .is_some()
                        {
                            return None;
                        }
                    }
                    _ => return None,
                }
            }
            let path = path?;
            if wildcard && path == target.id.package {
                wildcard_target = true;
            }
            if !wildcard && alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or("")) == name {
                if path != format!("{}.{}", target.id.package, target.id.name) {
                    return None;
                }
                matched.push((header.start_byte(), header.end_byte()));
            }
        }
        match matched.as_slice() {
            [span] => Some(Some(*span)),
            [] if (package(self.source, self.root) == target.id.package || wildcard_target)
                && name == target.id.name =>
            {
                Some(None)
            }
            _ => None,
        }
    }
}
/// The declared type of a plain `const val … : T = "<safe literal>"`.
///
/// Both the source-tree slot proof and the native-fact path start here, so an
/// initializer that is not a provably safe Java string literal — or a `var`,
/// an inferred type, a nullable type or a non-const property — can never reach
/// either type proof.
fn constant_string_declaration<'n>(source: &str, declaration: Node<'n>) -> Option<Node<'n>> {
    if declaration.kind() != "property_declaration" {
        return None;
    }
    let is_const = declaration
        .named_children(&mut declaration.walk())
        .filter(|n| n.kind() == "modifiers")
        .flat_map(|n| n.named_children(&mut n.walk()).collect::<Vec<_>>())
        .any(|n| {
            n.children(&mut n.walk())
                .any(|token| token.kind() == "const")
        });
    if !is_const {
        return None;
    }
    let variable = declaration
        .named_children(&mut declaration.walk())
        .find(|n| n.kind() == "variable_declaration")?;
    let ty = variable
        .named_children(&mut variable.walk())
        .find(|n| n.kind() == "user_type")?;
    property_initializer(declaration)
        .filter(|n| {
            n.kind() == "string_literal"
                && n.utf8_text(source.as_bytes())
                    .is_ok_and(crate::semantics::safe_java_string_literal)
        })
        .map(|_| ty)
}

/// Re-prove one persisted field emission proof against the accepted tree.
///
/// Pre-write acceptance re-runs the same two proofs planning used — the
/// declaration shape and the canonical type/name binding — on the exact text
/// about to be emitted for, and refuses a plan whose recorded rendering no
/// longer follows from it. The fact half is bound to that text by
/// `source_hash`, so a plan cannot carry facts about another snapshot.
pub(crate) fn reprove_field_type(source: &str, root: Node<'_>, field: &FieldEmissionProof) -> bool {
    if *blake3::hash(source.as_bytes()).as_bytes() != field.source_hash {
        return false;
    }
    let Some(declaration) = root
        .named_children(&mut root.walk())
        .find(|node| node.start_byte() == field.start_byte && node.end_byte() == field.end_byte)
    else {
        return false;
    };
    let Some(ty) = constant_string_declaration(source, declaration) else {
        return false;
    };
    crate::semantics::canonical_string_field_type(
        source,
        ty,
        &field.facade,
        Some(field.descriptor.as_str()),
        Some(field.nullable),
    )
    .is_some_and(|canonical| {
        canonical.emitted == field.emitted_type
            && canonical.binding_import == field.binding_import
            && canonical.descriptor == field.descriptor
    })
}

/// PSI attaches preceding comments and may exclude a trailing semicolon;
/// tree-sitter owns the declaration tokens only. Prove equivalent ownership
/// structurally instead of widening an offset match by guessed byte counts.
fn target_span_matches(
    source: &str,
    reference: &SourcePropertyReference,
    start: usize,
    end: usize,
) -> bool {
    let Ok(from) = usize::try_from(reference.target_start_byte) else {
        return false;
    };
    let Ok(to) = usize::try_from(reference.target_end_byte) else {
        return false;
    };
    if from > start || to < start || to > source.len() {
        return false;
    }
    let parse_end = if to < end {
        if source
            .get(to..end)
            .is_none_or(|suffix| suffix.trim() != ";")
        {
            return false;
        }
        end
    } else {
        to
    };
    let Some(fragment) = source.get(from..parse_end) else {
        return false;
    };
    let Some(tree) = parse(fragment) else {
        return false;
    };
    if tree.root_node().has_error() {
        return false;
    }
    let declarations = tree
        .root_node()
        .named_children(&mut tree.root_node().walk())
        .filter(|n| !n.kind().ends_with("comment"))
        .collect::<Vec<_>>();
    matches!(declarations.as_slice(), [declaration] if declaration.kind() == "property_declaration"
        && from + declaration.start_byte() == start && from + declaration.end_byte() == end)
}

pub(super) fn top_level_owner(mut node: Node<'_>) -> Option<Node<'_>> {
    while let Some(parent) = node.parent() {
        if parent.kind() == "source_file" {
            return Some(node);
        }
        node = parent;
    }
    None
}

pub(super) fn collect_bound_imports(
    original: &BTreeMap<PathBuf, String>,
    plans: &[TranslationPlan],
    targets: &[Target],
    residual: &BTreeMap<PathBuf, String>,
    edits: &mut BTreeMap<PathBuf, Vec<PendingEdit>>,
) -> Result<HashSet<(PathBuf, usize, usize)>, CallsiteRepairDiagnostic> {
    let mut replaced = HashSet::new();
    let mut inserted = HashSet::new();
    for proof in plans.iter().flat_map(|p| &p.property_imports) {
        let Some(target) = targets.iter().find(|t| t.id == proof.target) else {
            continue;
        };
        let Some(target_source) = original.get(&proof.target.file) else {
            continue;
        };
        let Some(consumer_source) = original.get(&proof.consumer.file) else {
            continue;
        };
        if *blake3::hash(target_source.as_bytes()).as_bytes() != proof.target_hash
            || *blake3::hash(consumer_source.as_bytes()).as_bytes() != proof.consumer.snapshot_hash
            || proof.references.is_empty()
            || proof.references.iter().any(|r| {
                r.target_file != proof.target.file
                    || r.target_name != proof.target.name
                    || r.target_package != proof.target.package
                    || consumer_source.get(r.byte_offset as usize..r.end_byte as usize)
                        != Some(r.text.as_str())
            })
        {
            return Err(diag(
                &proof.consumer.file,
                0,
                "P005",
                "source-bound property import belongs to a stale or contradictory snapshot".into(),
            ));
        }
        if !inserted.insert((
            proof.consumer.file.clone(),
            proof.target.clone(),
            proof.visible.clone(),
        )) {
            continue;
        }
        let Some(current) = residual.get(&proof.consumer.file) else {
            continue;
        };
        let Some(tree) = parse(current) else {
            continue;
        };
        let current_hash = *blake3::hash(current.as_bytes()).as_bytes();
        let fq = format!("{}.{}", target.facade, target.id.name);
        let import = if proof.visible.trim_matches('`') == target.id.name {
            format!("import {fq}")
        } else {
            format!("import {fq} as {}", proof.visible)
        };
        // Re-locate the proved import in the residual tree. Removing an
        // unrelated declaration changes the file hash, not this binding.
        let (start, end, replacement) = if let Some(span) = proof.import_span {
            let original_header = consumer_source.get(span.0..span.1).ok_or_else(|| {
                diag(
                    &proof.consumer.file,
                    0,
                    "P005",
                    "invalid proved property import span".into(),
                )
            })?;
            let headers: Vec<_> = tree
                .root_node()
                .named_children(&mut tree.root_node().walk())
                .filter(|node| node.kind() == "import")
                .filter(|node| current.get(node.byte_range()) == Some(original_header))
                .collect();
            let [header] = headers.as_slice() else {
                return Err(diag(
                    &proof.consumer.file,
                    0,
                    "P005",
                    "proved property import is missing or duplicated in the residual source".into(),
                ));
            };
            replaced.insert((
                proof.consumer.file.clone(),
                header.start_byte(),
                header.end_byte(),
            ));
            (header.start_byte(), header.end_byte(), import)
        } else {
            // File annotations and shebangs must precede imports even when
            // there is no package header.
            let at = tree
                .root_node()
                .named_children(&mut tree.root_node().walk())
                .filter(|n| matches!(n.kind(), "shebang" | "file_annotation" | "package_header"))
                .map(|n| n.end_byte())
                .max()
                .unwrap_or(0);
            (at, at, format!("\n{import}\n"))
        };
        edits
            .entry(proof.consumer.file.clone())
            .or_default()
            .push(PendingEdit {
                edit: edit(&proof.consumer.file, current_hash, start, end, replacement),
                target: proof.target.clone(),
            });
    }
    Ok(replaced)
}
