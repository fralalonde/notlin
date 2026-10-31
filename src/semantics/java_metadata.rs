//! Bound library annotations whose semantics preserve Java member declarations.
//! This proves neither serialization equivalence nor a nullness default. The
//! existing constructor/metadata and nullability contracts remain independent.

use super::{FactStatus, SemanticProvider, SymbolId};
use std::{collections::BTreeSet, path::Path};
use tree_sitter::Node;

#[derive(Debug, Clone)]
pub(super) struct Binding {
    pub written: String,
    pub canonical: String,
    anchor: String,
    canonical_anchor: String,
    qualified: bool,
    start: usize,
    end: usize,
    lexical_owners: Vec<usize>,
    foreign_stars: Vec<String>,
}

impl Binding {
    pub fn neutral_for_nullness(&self) -> bool {
        !matches!(
            self.canonical.as_str(),
            "org.jetbrains.annotations.NotNull" | "org.jetbrains.annotations.Nullable"
        )
    }

    pub fn at(&self, annotation: Node<'_>) -> bool {
        self.start == annotation.start_byte() && self.end == annotation.end_byte()
    }

    pub fn valid(
        &self,
        file: &Path,
        package: &str,
        provider: &super::SyntaxSemanticProvider,
    ) -> bool {
        let head = self.written.split('.').next().unwrap_or("");
        if self.foreign_stars.iter().any(|package| {
            !provider.has_source_package(package)
                || provider
                    .symbols_named(head)
                    .iter()
                    .any(|symbol| symbol.id.package == *package && symbol.id.owner_path.is_empty())
        }) {
            return false;
        }
        for start in &self.lexical_owners {
            let Some(owner) = provider
                .type_symbols_by_location
                .get(&(file.to_path_buf(), *start))
                .and_then(|index| provider.symbols.get(*index))
            else {
                return false;
            };
            if !provider.has_complete_inheritance(&owner.id) {
                return false;
            }
            let mut pending = vec![owner.id.clone()];
            let mut seen = BTreeSet::new();
            while let Some(parent) = pending.pop() {
                if !seen.insert(parent.clone()) {
                    continue;
                }
                let mut path = parent.owner_path.clone();
                path.push(parent.name.clone());
                if provider.symbols_named(head).iter().any(|symbol| {
                    super::is_type_symbol(&symbol.id)
                        && symbol.id.file == parent.file
                        && symbol.id.owner_path == path
                }) {
                    return false;
                }
                pending.extend(provider.parents.get(&parent).into_iter().flatten().cloned());
            }
        }
        if self.qualified {
            let prefix = self.written.split('.').next().unwrap_or("");
            if !matches!(
                provider.resolve_type_in_file(file, package, prefix),
                FactStatus::Unknown
            ) {
                return false;
            }
        }
        match provider.resolve_type_in_file(file, package, &self.anchor) {
            FactStatus::Unknown => !provider.has_type_alias_in_package(package, &self.anchor),
            FactStatus::Established(id) => {
                super::qualified_source_type_name(&id) == self.canonical_anchor
            }
            _ => false,
        }
    }
}

// These declarations supply runtime serialization/diagnostic metadata, not AST
// member generators or ambient method nullness defaults. Nullness markers are
// separate effects and must never be ignored while classifying method returns.
// https://github.com/FasterXML/jackson-annotations
// https://github.com/FasterXML/jackson-databind/wiki/Databind-annotations
fn preserves_members(name: &str) -> bool {
    match name {
        "java.lang.Deprecated"
        | "java.lang.Override"
        | "java.lang.SuppressWarnings"
        | "java.lang.SafeVarargs"
        | "java.lang.FunctionalInterface"
        | "javax.annotation.processing.Generated"
        | "org.jetbrains.annotations.NotNull"
        | "org.jetbrains.annotations.Nullable" => true,
        name if name.starts_with("com.fasterxml.jackson.annotation.") => matches!(
            name.trim_start_matches("com.fasterxml.jackson.annotation."),
            "JacksonAnnotation"
                | "JacksonAnnotationsInside"
                | "JacksonInject"
                | "JsonAlias"
                | "JsonAnyGetter"
                | "JsonAnySetter"
                | "JsonAutoDetect"
                | "JsonBackReference"
                | "JsonClassDescription"
                | "JsonCreator"
                | "JsonEnumDefaultValue"
                | "JsonFilter"
                | "JsonFormat"
                | "JsonGetter"
                | "JsonIdentityInfo"
                | "JsonIdentityReference"
                | "JsonIgnore"
                | "JsonIgnoreProperties"
                | "JsonIgnoreType"
                | "JsonInclude"
                | "JsonIncludeProperties"
                | "JsonKey"
                | "JsonManagedReference"
                | "JsonMerge"
                | "JsonProperty"
                | "JsonPropertyDescription"
                | "JsonPropertyOrder"
                | "JsonRawValue"
                | "JsonRootName"
                | "JsonSetter"
                | "JsonSubTypes"
                | "JsonSubTypes.Type"
                | "JsonTypeId"
                | "JsonTypeInfo"
                | "JsonTypeName"
                | "JsonUnwrapped"
                | "JsonValue"
                | "JsonView"
        ),
        name if name.starts_with("com.fasterxml.jackson.databind.annotation.") => matches!(
            name.trim_start_matches("com.fasterxml.jackson.databind.annotation."),
            "JsonDeserialize"
                | "JsonSerialize"
                | "JsonNaming"
                | "JsonPOJOBuilder"
                | "JsonAppend"
                | "JsonAppend.Attr"
                | "JsonAppend.Prop"
                | "JsonTypeIdResolver"
                | "JsonTypeResolver"
                | "JsonValueInstantiator"
        ),
        _ => false,
    }
}

/// Import and lexical classifier evidence collected once from the file parse.
/// No syntax node survives into a provider fact.
pub(super) struct Scope<'src> {
    source: &'src str,
    exact: std::collections::BTreeMap<String, BTreeSet<(String, bool)>>,
    stars: BTreeSet<String>,
    names: BTreeSet<String>,
}

impl<'src> Scope<'src> {
    pub fn new(source: &'src str, root: Node<'_>) -> Self {
        let mut exact = std::collections::BTreeMap::<String, BTreeSet<(String, bool)>>::new();
        let mut stars = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == "import_declaration"
                && let Some(path) = node
                    .named_children(&mut node.walk())
                    .find(|part| matches!(part.kind(), "identifier" | "scoped_identifier"))
                    .and_then(|path| path.utf8_text(source.as_bytes()).ok())
            {
                let static_import = node
                    .children(&mut node.walk())
                    .any(|part| part.kind() == "static");
                let star = node
                    .children(&mut node.walk())
                    .any(|part| matches!(part.kind(), "asterisk" | "*"));
                if star {
                    stars.insert(path.to_owned());
                } else if let Some(name) = path.rsplit('.').next() {
                    exact
                        .entry(name.to_owned())
                        .or_default()
                        .insert((path.to_owned(), static_import));
                }
            }
            if matches!(
                node.kind(),
                "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                    | "annotation_type_declaration"
            ) && let Some(name) = super::node_name(node, source)
            {
                names.insert(name);
            }
            stack.extend(node.named_children(&mut node.walk()));
        }
        Self {
            source,
            exact,
            stars,
            names,
        }
    }

    pub fn binding(&self, annotation: Node<'_>) -> Option<Binding> {
        let written = annotation
            .child_by_field_name("name")?
            .utf8_text(self.source.as_bytes())
            .ok()?
            .trim();
        let head = written.split('.').next()?;
        if self.names.contains(head) {
            return None;
        }
        let exact = self.exact.get(head);
        let qualified = preserves_members(written);
        let mut foreign_stars = Vec::new();
        let (anchor, canonical_anchor, canonical) = if qualified {
            if exact.is_some() {
                return None;
            }
            (written.to_owned(), written.to_owned(), written.to_owned())
        } else if let Some(exact) = exact {
            let imports = exact.iter().collect::<Vec<_>>();
            let [(import, false)] = imports.as_slice() else {
                return None;
            };
            let suffix = written.strip_prefix(head)?;
            let canonical = format!("{import}{suffix}");
            if !preserves_members(&canonical) {
                return None;
            }
            (head.to_owned(), import.clone(), canonical)
        } else {
            let package = super::package_name(self.source);
            let local = format!("{package}.{written}");
            let implicit = format!("java.lang.{written}");
            let canonical = if preserves_members(&local) {
                local
            } else if preserves_members(&implicit) {
                implicit
            } else {
                let candidates = self
                    .stars
                    .iter()
                    .map(|package| format!("{package}.{written}"))
                    .filter(|name| preserves_members(name))
                    .collect::<Vec<_>>();
                let [canonical] = candidates.as_slice() else {
                    return None;
                };
                let package = canonical.strip_suffix(written)?.trim_end_matches('.');
                foreign_stars = self
                    .stars
                    .iter()
                    .filter(|star| {
                        star.as_str() != package
                            && !matches!(star.as_str(), "java.lang" | "java.util")
                    })
                    .cloned()
                    .collect();
                canonical.clone()
            };
            let suffix = written.strip_prefix(head)?;
            let canonical_anchor = canonical
                .strip_suffix(suffix)
                .unwrap_or(&canonical)
                .to_owned();
            (head.to_owned(), canonical_anchor, canonical)
        };
        let mut lexical_owners = Vec::new();
        let mut ancestor = annotation.parent();
        while let Some(node) = ancestor {
            if matches!(
                node.kind(),
                "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                    | "annotation_type_declaration"
            ) {
                lexical_owners.push(node.start_byte());
            }
            ancestor = node.parent();
        }
        Some(Binding {
            written: written.to_owned(),
            canonical,
            anchor,
            canonical_anchor,
            qualified,
            start: annotation.start_byte(),
            end: annotation.end_byte(),
            lexical_owners,
            foreign_stars,
        })
    }

    pub fn owner_bindings(&self, owner: Node<'_>) -> Option<Vec<Binding>> {
        if owner.has_error()
            || !matches!(
                owner.kind(),
                "class_declaration" | "interface_declaration" | "enum_declaration"
            )
        {
            return None;
        }
        let mut stack = vec![owner];
        let mut ancestor = owner.parent();
        while let Some(node) = ancestor {
            stack.extend(
                node.named_children(&mut node.walk())
                    .filter(|part| part.kind() == "modifiers"),
            );
            ancestor = node.parent();
        }
        let mut result = Vec::new();
        while let Some(node) = stack.pop() {
            if matches!(node.kind(), "annotation" | "marker_annotation") {
                result.push(self.binding(node)?);
            }
            stack.extend(node.named_children(&mut node.walk()));
        }
        Some(result)
    }
}

impl super::SyntaxSemanticProvider {
    pub(super) fn java_metadata_preserves_members(&self, owner: &SymbolId) -> bool {
        let candidates = self
            .symbols_named(&owner.name)
            .into_iter()
            .filter(|symbol| symbol.id == *owner)
            .collect::<Vec<_>>();
        let [symbol] = candidates.as_slice() else {
            return false;
        };
        self.source_for_file(&owner.file).is_some_and(|source| {
            *blake3::hash(source.as_bytes()).as_bytes() == symbol.location.snapshot_hash
        }) && self
            .java_member_metadata
            .get(&(owner.file.clone(), symbol.location.start_byte))
            .is_some_and(|bindings| {
                bindings
                    .iter()
                    .all(|binding| binding.valid(&owner.file, &owner.package, self))
            })
    }
}
