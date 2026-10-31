//! Snapshot-scoped contracts for Java synthetic property reads.
//!
//! JVM primitives and reference nullability are independent facts: a boxed
//! `Double` must never acquire the primitive contract of `double`.

use super::{
    FactStatus, JavaPropertyRead, KotlinType, ReferenceReturnNullability, SemanticProvider,
    SymbolId, SyntaxSemanticProvider, TypeRef, declaration_type_node, has_java_modifier,
    own_type_parameters, source_calls,
};
use rayon::prelude::*;
use std::collections::BTreeSet;
use tree_sitter::Node;

#[derive(Debug, Clone)]
pub(super) struct GetterShape {
    pub result_type: TypeRef,
    pub nullability: ReferenceReturnNullability,
    pub jvm_primitive: bool,
    boxed_simple_name: Option<String>,
    annotation_bindings: Vec<(String, String)>,
    metadata_bindings: Vec<super::java_metadata::Binding>,
}

/// An annotation may introduce an accessor that has no source method symbol.
/// Keep this negative capability even without an active Lombok profile: the
/// positive generated-getter cache cannot prove that a missing getter is absent.
pub(super) fn may_have_generated_getters(owner: Node<'_>) -> bool {
    let annotated = |node: Node<'_>| {
        node.named_children(&mut node.walk())
            .filter(|child| child.kind() == "modifiers")
            .any(|modifiers| {
                modifiers
                    .named_children(&mut modifiers.walk())
                    .any(|child| matches!(child.kind(), "annotation" | "marker_annotation"))
            })
    };
    let mut enclosing = Some(owner);
    while let Some(node) = enclosing {
        if annotated(node) {
            return true;
        }
        enclosing = node.parent();
    }
    owner.child_by_field_name("body").is_some_and(|body| {
        body.named_children(&mut body.walk())
            .filter(|child| matches!(child.kind(), "field_declaration" | "constant_declaration"))
            .any(annotated)
    })
}

pub(super) fn collect(
    source: &str,
    node: Node<'_>,
    metadata: &super::java_metadata::Scope<'_>,
) -> Option<GetterShape> {
    let name = node
        .child_by_field_name("name")?
        .utf8_text(source.as_bytes())
        .ok()?;
    if !name.strip_prefix("get").is_some_and(|suffix| {
        suffix
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
    }) {
        return None;
    }
    if node.kind() != "method_declaration"
        || own_type_parameters(node).is_some()
        || source_calls::java_method_declares_throws(source, node)
        || has_java_modifier(node, "static")
        || has_java_modifier(node, "private")
        || has_java_modifier(node, "protected")
        || !(has_java_modifier(node, "public")
            || node
                .parent()
                .is_some_and(|parent| parent.kind() == "interface_body"))
        || node
            .child_by_field_name("parameters")?
            .named_children(&mut node.walk())
            .any(|child| !child.is_extra())
    {
        return None;
    }
    let mut ancestor = node.parent();
    while let Some(parent) = ancestor {
        if own_type_parameters(parent).is_some() {
            return None;
        }
        ancestor = parent.parent();
    }
    let spelling = declaration_type_node(node)?
        .utf8_text(source.as_bytes())
        .ok()?
        .trim();
    if spelling.contains(['@', '<', '[']) || spelling == "void" {
        // Nested platform nullability and generic substitution need their own
        // contract; the outer Java type alone does not prove element types.
        return None;
    }
    let jvm_primitive = matches!(
        spelling,
        "boolean" | "byte" | "short" | "int" | "long" | "char" | "float" | "double"
    );
    // Validate annotations even on primitives; an unknown annotation may be a
    // processor contract rather than a harmless nullness marker.
    let EnclosingAnnotations {
        legacy: mut annotation_bindings,
        metadata: mut metadata_bindings,
    } = enclosing_annotation_bindings(source, node, metadata)?;
    if let Some(modifiers) = node
        .named_children(&mut node.walk())
        .find(|child| child.kind() == "modifiers")
    {
        for annotation in modifiers
            .named_children(&mut modifiers.walk())
            .filter(|child| matches!(child.kind(), "annotation" | "marker_annotation"))
        {
            if let Some(binding) = metadata.binding(annotation)
                && binding.neutral_for_nullness()
                && binding.canonical != "java.lang.Override"
            {
                metadata_bindings.push(binding);
                continue;
            }
            let name = annotation
                .child_by_field_name("name")?
                .utf8_text(source.as_bytes())
                .ok()?;
            let canonical = match name {
                "Nullable" => "org.jetbrains.annotations.Nullable",
                "NotNull" => "org.jetbrains.annotations.NotNull",
                "Override" => "java.lang.Override",
                _ => continue,
            };
            if !canonical_annotation_import(source, node, canonical, name == "Override") {
                return None;
            }
            annotation_bindings.push((name.to_owned(), canonical.to_owned()));
        }
    }
    let reference_nullability = source_calls::java_method_return_nullability_with_metadata(
        source,
        node,
        &metadata_bindings,
    )?;
    let nullability = if jvm_primitive {
        ReferenceReturnNullability::NonNull
    } else {
        reference_nullability
    };
    let mut result_type = TypeRef::from_java_name(spelling);
    if spelling == "java.lang.String" {
        // Keep qualification: a declaring-package String must not rebind a
        // fully qualified Java return after Kotlin primitive normalization.
        result_type = TypeRef::parse(spelling);
    }
    // Java's simple boxed names need the same Kotlin type as qualified ones.
    // Keep their reference status separately, including @NotNull boxed values.
    let mut boxed_simple_name = None;
    if !jvm_primitive {
        let boxed = match spelling {
            "Boolean" => Some("Boolean"),
            "Byte" => Some("Byte"),
            "Short" => Some("Short"),
            "Integer" => Some("Int"),
            "Long" => Some("Long"),
            "Character" => Some("Char"),
            "Float" => Some("Float"),
            "Double" => Some("Double"),
            _ => None,
        };
        if let Some(name) = boxed {
            if !plain_java_lang_binding(source, node, spelling) {
                return None;
            }
            result_type.ty = KotlinType::Primitive(name.into());
            boxed_simple_name = Some(spelling.to_owned());
        }
        result_type.nullable = nullability != ReferenceReturnNullability::NonNull;
    }
    Some(GetterShape {
        result_type,
        nullability,
        jvm_primitive,
        boxed_simple_name,
        annotation_bindings,
        metadata_bindings,
    })
}

fn plain_java_lang_binding(source: &str, mut node: Node<'_>, simple: &str) -> bool {
    while let Some(parent) = node.parent() {
        node = parent;
    }
    node.named_children(&mut node.walk())
        .filter(|child| child.kind() == "import_declaration")
        .all(|import| {
            let Some(path) = import
                .named_children(&mut import.walk())
                .find(|child| matches!(child.kind(), "identifier" | "scoped_identifier"))
                .and_then(|path| path.utf8_text(source.as_bytes()).ok())
            else {
                return false;
            };
            let wildcard = import
                .children(&mut import.walk())
                .any(|child| child.kind() == "asterisk");
            if wildcard {
                return path == "java.lang";
            }
            path.rsplit('.').next() != Some(simple)
                || (path == format!("java.lang.{simple}")
                    && !import
                        .children(&mut import.walk())
                        .any(|child| child.kind() == "static"))
        })
}

fn canonical_annotation_import(
    source: &str,
    mut node: Node<'_>,
    canonical: &str,
    implicit: bool,
) -> bool {
    while let Some(parent) = node.parent() {
        node = parent;
    }
    let simple = canonical.rsplit('.').next().unwrap_or(canonical);
    let mut exact = implicit;
    for import in node
        .named_children(&mut node.walk())
        .filter(|child| child.kind() == "import_declaration")
    {
        let Some(path) = import
            .named_children(&mut import.walk())
            .find(|child| matches!(child.kind(), "identifier" | "scoped_identifier"))
            .and_then(|path| path.utf8_text(source.as_bytes()).ok())
        else {
            return false;
        };
        if path.rsplit('.').next() == Some(simple) {
            if path != canonical
                || import
                    .children(&mut import.walk())
                    .any(|child| child.kind() == "static")
            {
                return false;
            }
            exact = true;
        }
    }
    exact
}

struct EnclosingAnnotations {
    legacy: Vec<(String, String)>,
    metadata: Vec<super::java_metadata::Binding>,
}

fn enclosing_annotation_bindings(
    source: &str,
    node: Node<'_>,
    metadata: &super::java_metadata::Scope<'_>,
) -> Option<EnclosingAnnotations> {
    let mut bindings = Vec::new();
    let mut metadata_bindings = Vec::new();
    let mut ancestor = node.parent();
    while let Some(owner) = ancestor {
        ancestor = owner.parent();
        if !matches!(
            owner.kind(),
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        ) {
            continue;
        }
        let Some(modifiers) = owner
            .named_children(&mut owner.walk())
            .find(|child| child.kind() == "modifiers")
        else {
            continue;
        };
        for annotation in modifiers
            .named_children(&mut modifiers.walk())
            .filter(|child| matches!(child.kind(), "annotation" | "marker_annotation"))
        {
            if let Some(binding) = metadata.binding(annotation)
                && binding.neutral_for_nullness()
            {
                metadata_bindings.push(binding);
                continue;
            }
            let name = annotation
                .child_by_field_name("name")?
                .utf8_text(source.as_bytes())
                .ok()?;
            let simple = name.rsplit('.').next()?;
            if !matches!(
                simple,
                "Data"
                    | "Value"
                    | "Getter"
                    | "Setter"
                    | "AllArgsConstructor"
                    | "NoArgsConstructor"
                    | "RequiredArgsConstructor"
                    | "EqualsAndHashCode"
                    | "ToString"
            ) {
                return None;
            }
            let canonical = format!("lombok.{simple}");
            if name == canonical {
                continue;
            }
            if name != simple || !canonical_annotation_import(source, node, &canonical, false) {
                return None;
            }
            bindings.push((name.to_owned(), canonical));
        }
    }
    Some(EnclosingAnnotations {
        legacy: bindings,
        metadata: metadata_bindings,
    })
}

impl SyntaxSemanticProvider {
    pub(super) fn enrich_generated_property_reads(
        &mut self,
        index: &crate::workspace::SourceIndex,
    ) {
        // Accepted output keeps its original Kotlin lineage. Source-derived
        // facts fill gaps without giving the same getter a second identity.
        let accepted_keys = index
            .generated_getters()
            .iter()
            .map(generated_property_key)
            .collect::<BTreeSet<_>>();
        let java_sources = index
            .java_files()
            .filter(|file| {
                index.has_default_lombok_accessors(&file.path)
                    && file.source_text().contains("lombok")
            })
            .collect::<Vec<_>>();
        let source_contracts = java_sources
            .par_iter()
            .map(|file| {
                crate::lombok_getter_contracts::collect_java_source(
                    &file.path,
                    file.source_text(),
                    self,
                )
            })
            .collect::<Vec<_>>();
        for getter in index.generated_getters().iter().chain(
            source_contracts
                .iter()
                .flatten()
                .filter(|getter| !accepted_keys.contains(&generated_property_key(getter))),
        ) {
            let owners = self
                .symbols_named(&getter.owner.name)
                .into_iter()
                .filter(|symbol| {
                    symbol.id.file == getter.output_path
                        && symbol.id.module == getter.owner.module
                        && symbol.id.package == getter.owner.package
                        && symbol.id.owner_path == getter.owner.owner_path
                        && symbol.id.kind == "class"
                        && symbol.location.snapshot_hash == getter.output_hash
                })
                .collect::<Vec<_>>();
            let [owner] = owners.as_slice() else {
                continue;
            };
            if getter.jvm_signature.name != getter.method_name
                || !getter.jvm_signature.descriptor.starts_with("()")
            {
                continue;
            }
            let mut dependencies = getter.dependencies.clone();
            dependencies.push(owner.id.clone());
            dependencies.sort();
            dependencies.dedup();
            let key = (owner.id.clone(), getter.property_name.clone());
            let read = JavaPropertyRead {
                target: getter.target.clone(),
                method_name: getter.method_name.clone(),
                location: owner.location.clone(),
                result_type: getter.result_type.clone(),
                nullability: getter.nullability,
                jvm_primitive: getter.jvm_primitive,
                dependencies,
            };
            let reads = self.generated_java_property_reads.entry(key).or_default();
            if !reads.contains(&read) {
                reads.push(read);
            }
        }
    }

    pub(super) fn direct_java_property_getter_name(
        &self,
        owner: &SymbolId,
        property: &str,
    ) -> Option<String> {
        self.java_property_getter_name(owner, property, false)
    }

    fn java_property_getter_name(
        &self,
        owner: &SymbolId,
        property: &str,
        generated_private_field: bool,
    ) -> Option<String> {
        if !self.has_complete_inheritance(owner) {
            return None;
        }
        // A visible field can change Kotlin's synthetic-property binding.
        let mut pending = vec![owner.clone()];
        let mut seen = BTreeSet::new();
        while let Some(current) = pending.pop() {
            if !seen.insert(current.clone()) {
                continue;
            }
            let mut path = current.owner_path.clone();
            path.push(current.name.clone());
            if (!generated_private_field || current != *owner)
                && self.java_visible_fields.contains(&(
                    current.file.clone(),
                    path,
                    property.to_owned(),
                ))
            {
                return None;
            }
            pending.extend(self.parents.get(&current).into_iter().flatten().cloned());
        }
        let mut chars = property.chars();
        let first = chars.next().filter(char::is_ascii_lowercase)?;
        if chars
            .clone()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
        {
            return None;
        }
        Some(format!(
            "get{}{}",
            first.to_ascii_uppercase(),
            chars.as_str()
        ))
    }

    pub(super) fn java_property_read_contract(
        &self,
        owner: &SymbolId,
        property: &str,
    ) -> Option<JavaPropertyRead> {
        if owner
            .file
            .extension()
            .is_none_or(|extension| extension != "java")
            || !self.has_complete_inheritance(owner)
            || self.annotated_java_packages.contains(&owner.package)
            || self.annotated_java_packages.contains("*")
        {
            return None;
        }
        let generated = self
            .generated_java_property_reads
            .get(&(owner.clone(), property.to_owned()));
        let name = self.java_property_getter_name(owner, property, generated.is_some())?;
        let hierarchy = self.java_property_owner_hierarchy(owner)?;
        let mut declarations = Vec::new();
        let mut dependencies = hierarchy.clone();
        for declaring_owner in &hierarchy {
            if let Some(read) = self.java_local_property_read(declaring_owner, property, &name)? {
                dependencies.extend(read.dependencies.iter().cloned());
                declarations.push((declaring_owner.clone(), read));
            }
        }
        if declarations.is_empty() {
            return None;
        }
        let mut most_specific = Vec::new();
        for (index, (candidate_owner, _)) in declarations.iter().enumerate() {
            let shadowed =
                declarations
                    .iter()
                    .enumerate()
                    .any(|(other_index, (other_owner, _))| {
                        index != other_index
                            && candidate_owner != other_owner
                            && super::reified_calls::subtype_path(
                                other_owner,
                                candidate_owner,
                                self,
                            )
                            .is_some()
                    });
            if !shadowed {
                most_specific.push(index);
            }
        }
        let [selected_index] = most_specific.as_slice() else {
            // Two unrelated interface declarations (including defaults) are
            // ambiguous even when they happen to spell the same return type.
            return None;
        };
        let (selected_owner, selected) = &declarations[*selected_index];
        for (contract_owner, contract) in &declarations {
            if contract.target == selected.target {
                continue;
            }
            if !self.java_override_return_compatible(
                selected_owner,
                selected,
                contract_owner,
                contract,
                &mut dependencies,
            ) {
                return None;
            }
        }
        dependencies.extend(selected.dependencies.iter().cloned());
        dependencies.sort();
        dependencies.dedup();
        let mut result = selected.clone();
        result.dependencies = dependencies;
        Some(result)
    }

    fn java_property_owner_hierarchy(&self, owner: &SymbolId) -> Option<Vec<SymbolId>> {
        fn visit(
            provider: &SyntaxSemanticProvider,
            current: &SymbolId,
            active: &mut BTreeSet<SymbolId>,
            seen: &mut BTreeSet<SymbolId>,
            source_hashes: &mut std::collections::BTreeMap<std::path::PathBuf, [u8; 32]>,
            result: &mut Vec<SymbolId>,
        ) -> Option<()> {
            if active.contains(current) {
                return None;
            }
            if seen.contains(current) {
                return Some(());
            }
            let owners = provider
                .symbols_named(&current.name)
                .into_iter()
                .filter(|symbol| symbol.id == *current)
                .collect::<Vec<_>>();
            let [symbol] = owners.as_slice() else {
                return None;
            };
            let source_hash = if let Some(hash) = source_hashes.get(&current.file) {
                *hash
            } else {
                let source = provider.source_for_file(&current.file)?;
                let hash = *blake3::hash(source.as_bytes()).as_bytes();
                source_hashes.insert(current.file.clone(), hash);
                hash
            };
            if source_hash != symbol.location.snapshot_hash {
                return None;
            }
            active.insert(current.clone());
            let mut parents = provider.parents.get(current).cloned().unwrap_or_default();
            parents.sort();
            parents.dedup();
            for parent in &parents {
                visit(provider, parent, active, seen, source_hashes, result)?;
            }
            active.remove(current);
            seen.insert(current.clone());
            result.push(current.clone());
            Some(())
        }
        let mut result = Vec::new();
        visit(
            self,
            owner,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
            &mut std::collections::BTreeMap::new(),
            &mut result,
        )?;
        Some(result)
    }

    // Some(None) means this owner contributes no getter. None means it has an
    // uncertain or conflicting declaration and blocks the complete lookup.
    fn java_local_property_read(
        &self,
        owner: &SymbolId,
        property: &str,
        name: &str,
    ) -> Option<Option<JavaPropertyRead>> {
        let owners = self
            .symbols_named(&owner.name)
            .into_iter()
            .filter(|symbol| symbol.id == *owner)
            .collect::<Vec<_>>();
        let [owner_symbol] = owners.as_slice() else {
            return None;
        };
        if owner
            .file
            .extension()
            .is_some_and(|extension| extension != "java")
        {
            // Kotlin properties synthesize JVM getters without a method symbol
            // in this provider. They can override/hide a Java getter in a mixed
            // hierarchy, so do not silently treat the inherited method as the
            // selected declaration.
            let mut owner_path = owner.owner_path.clone();
            owner_path.push(owner.name.clone());
            if self.symbols_named(property).into_iter().any(|symbol| {
                symbol.id.file == owner.file
                    && symbol.id.owner_path == owner_path
                    && symbol.id.kind == "property"
            }) {
                return None;
            }
        }
        let generated = self
            .generated_java_property_reads
            .get(&(owner.clone(), property.to_owned()));
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        let candidates = self
            .symbols_named(name)
            .into_iter()
            .filter(|symbol| {
                symbol.id.file == owner.file
                    && symbol.id.owner_path == path
                    && symbol.id.name == name
                    && matches!(symbol.id.kind.as_str(), "method" | "function")
                    && symbol.id.parameters.is_empty()
                    && symbol.id.receiver.is_none()
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() && generated.is_none() {
            if self.java_uncertain_getter_owners.contains(owner)
                && !self.java_metadata_preserves_members(owner)
            {
                return None;
            }
            return Some(None);
        }
        if self.annotated_java_packages.contains(&owner.package)
            || self.annotated_java_packages.contains("*")
        {
            return None;
        }
        if candidates.is_empty() {
            let Some(reads) = generated else {
                return Some(None);
            };
            let [read] = reads.as_slice() else {
                return None;
            };
            return (read.location.snapshot_hash == owner_symbol.location.snapshot_hash)
                .then(|| Some(read.clone()));
        }
        if generated.is_some() {
            return None;
        }
        let [symbol] = candidates.as_slice() else {
            return None;
        };
        if symbol.location.snapshot_hash != owner_symbol.location.snapshot_hash {
            return None;
        }
        let shape = self
            .java_property_read_shapes
            .get(&(owner.file.clone(), symbol.location.start_byte))?;
        if !shape.metadata_bindings.is_empty()
            && (!self.java_metadata_preserves_members(owner)
                || shape
                    .metadata_bindings
                    .iter()
                    .any(|binding| !binding.valid(&owner.file, &owner.package, self)))
        {
            return None;
        }
        for (annotation, canonical) in &shape.annotation_bindings {
            match self.resolve_type_in_file(&owner.file, &owner.package, annotation) {
                FactStatus::Unknown => {}
                FactStatus::Established(id)
                    if super::qualified_source_type_name(&id) == *canonical => {}
                _ => return None,
            }
        }
        if let Some(java_name) = &shape.boxed_simple_name
            && (!matches!(
                self.resolve_type_in_file(&owner.file, &owner.package, java_name),
                FactStatus::Unknown
            ) || self.has_type_alias_in_package(&owner.package, java_name))
        {
            return None;
        }
        let mut dependencies = vec![owner.clone()];
        let result_type = if matches!(shape.result_type.ty, KotlinType::Primitive(_)) {
            shape.result_type.clone()
        } else {
            super::container_types::qualify_type(
                &shape.result_type,
                &owner.file,
                self,
                &mut dependencies,
            )?
        };
        Some(Some(JavaPropertyRead {
            target: symbol.id.clone(),
            method_name: symbol.id.name.clone(),
            location: symbol.location.clone(),
            result_type,
            nullability: shape.nullability,
            jvm_primitive: shape.jvm_primitive,
            dependencies,
        }))
    }

    fn java_override_return_compatible(
        &self,
        selected_owner: &SymbolId,
        selected: &JavaPropertyRead,
        parent_owner: &SymbolId,
        parent: &JavaPropertyRead,
        dependencies: &mut Vec<SymbolId>,
    ) -> bool {
        if selected.jvm_primitive != parent.jvm_primitive {
            return false;
        }
        if parent.nullability == ReferenceReturnNullability::NonNull
            && selected.nullability != ReferenceReturnNullability::NonNull
        {
            return false;
        }
        if parent.nullability == ReferenceReturnNullability::Nullable
            && selected.nullability == ReferenceReturnNullability::Platform
        {
            return false;
        }
        if selected.result_type.ty == parent.result_type.ty {
            return true;
        }
        if selected.jvm_primitive
            || matches!(selected.result_type.ty, KotlinType::Primitive(_))
            || matches!(parent.result_type.ty, KotlinType::Primitive(_))
        {
            return false;
        }
        let (
            KotlinType::Named {
                name: actual,
                arguments: actual_args,
            },
            KotlinType::Named {
                name: expected,
                arguments: expected_args,
            },
        ) = (&selected.result_type.ty, &parent.result_type.ty)
        else {
            return false;
        };
        if !actual_args.is_empty() || !expected_args.is_empty() {
            return false;
        }
        if matches!(expected.as_str(), "java.lang.Object" | "kotlin.Any") {
            return true;
        }
        let actual = match self.resolve_type_in_file(
            &selected_owner.file,
            &selected_owner.package,
            actual,
        ) {
            FactStatus::Established(id)
                if matches!(
                    self.is_ordinary_jvm_reference(&id),
                    FactStatus::Established(true)
                ) =>
            {
                id
            }
            _ => return false,
        };
        let expected =
            match self.resolve_type_in_file(&parent_owner.file, &parent_owner.package, expected) {
                FactStatus::Established(id)
                    if matches!(
                        self.is_ordinary_jvm_reference(&id),
                        FactStatus::Established(true)
                    ) =>
                {
                    id
                }
                _ => return false,
            };
        let Some(path) = super::reified_calls::subtype_path(&actual, &expected, self) else {
            return false;
        };
        dependencies.extend(path);
        true
    }
}

fn generated_property_key(
    getter: &crate::lombok_getter_contracts::GeneratedGetterContract,
) -> (SymbolId, String) {
    let mut owner = getter.owner.clone();
    owner.file = getter.output_path.clone();
    (owner, getter.property_name.clone())
}
