use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const CACHE_VERSION: u32 = 2;
const CACHE_DIR: &str = ".notlin";
const CACHE_FILE: &str = "index-v1.bin";
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceLanguage {
    Kotlin,
    Java,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeclarationKind {
    Class,
    Interface,
    Enum,
    Object,
    Record,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemberKind {
    Property,
    Method,
    Field,
    Constructor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub kind: MemberKind,
    pub visibility: Option<String>,
    pub is_static: bool,
    pub type_name: Option<String>,
    pub is_nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    pub name: String,
    pub package: Option<String>,
    pub language: SourceLanguage,
    pub kind: DeclarationKind,
    pub supertypes: Vec<String>,
    pub members: Vec<Member>,
    pub has_default_constructor_parameter: bool,
    /// Type-parameter names in declaration order (`ICreateObjectCommand` ->
    /// `["T", "I"]`). Needed to distinguish a generic supertype member typed
    /// by its own parameter (`payload: T`) — Java-erasure compatible with
    /// any implementing type — from a genuinely different concrete type.
    pub type_params: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: PathBuf,
    pub language: SourceLanguage,
    pub package: Option<String>,
    pub imports: Vec<String>,
    pub declarations: Vec<Declaration>,
    identifier_counts: HashMap<String, usize>,
    enum_entries_qualifiers: HashMap<String, usize>,
    smart_cast_properties: HashSet<String>,
}

#[derive(Debug, Default)]
pub struct SourceIndex {
    pub files: Vec<SourceFile>,
    kotlin_subtypes: HashMap<String, Vec<PathBuf>>,
    /// Reverse subtype edges by SIMPLE name: `name -> [subtype simple names]`.
    /// Shallow but unambiguous enough for retention fixpointing: the
    /// fixpoint pre-pass taints a declaration when any of its simple-name
    /// subtypes is retained, and simple-name collisions over-taint
    /// conservatively (fewer translations, never a wrong Java ABI).
    subtype_names: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub parsed_files: usize,
    pub reused_files: usize,
    pub cache_written: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct IndexCache {
    version: u32,
    producer: u128,
    root_digest: [u8; 32],
    sources: Vec<CachedPathSource>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CachedPathSource {
    path: PathBuf,
    source: CachedSource,
}

#[derive(Debug, PartialEq, Eq)]
struct CachedDirectory {
    digest: [u8; 32],
    entries: BTreeMap<PathBuf, CachedEntry>,
}

#[derive(Debug, PartialEq, Eq)]
enum CachedEntry {
    Directory(Box<CachedDirectory>),
    // Boxed: a `SourceFile` carries its declarations map, so the source
    // variant dwarfs the directory pointer and this keeps the enum compact.
    Source(Box<CachedSource>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CachedSource {
    size: u64,
    modified_nanos: u128,
    digest: [u8; 32],
    source_file: SourceFile,
}

impl CachedEntry {
    fn digest(&self) -> &[u8; 32] {
        match self {
            Self::Directory(directory) => &directory.digest,
            Self::Source(source) => &source.digest,
        }
    }
}

fn flatten_sources(root: &CachedDirectory) -> Vec<CachedPathSource> {
    fn visit(directory: &CachedDirectory, prefix: &Path, output: &mut Vec<CachedPathSource>) {
        for (name, entry) in &directory.entries {
            let path = prefix.join(name);
            match entry {
                CachedEntry::Directory(child) => visit(child, &path, output),
                CachedEntry::Source(source) => output.push(CachedPathSource {
                    path,
                    source: (**source).clone(),
                }),
            }
        }
    }

    let mut sources = Vec::new();
    visit(root, Path::new(""), &mut sources);
    sources
}

impl SourceIndex {
    pub fn discover(root: &Path) -> Result<Self, String> {
        Self::discover_with_stats(root).map(|(index, _)| index)
    }

    pub fn discover_with_stats(root: &Path) -> Result<(Self, IndexStats), String> {
        let root =
            fs::canonicalize(root).map_err(|error| format!("{}: {error}", root.display()))?;
        let old_cache = load_cache(&root);
        let old_sources = old_cache
            .as_ref()
            .map(|cache| {
                cache
                    .sources
                    .iter()
                    .map(|entry| (entry.path.clone(), &entry.source))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let mut files = Vec::new();
        let mut stats = IndexStats::default();
        let mut visited_directories = HashSet::from([root.clone()]);
        let new_root = scan_directory(
            &root,
            Path::new(""),
            &old_sources,
            &mut visited_directories,
            &mut files,
            &mut stats,
        )?;
        files.sort_by(|left, right| left.path.cmp(&right.path));

        let mut index = Self {
            files,
            kotlin_subtypes: HashMap::new(),
            subtype_names: HashMap::new(),
        };
        let mut subtype_edges = Vec::new();
        let mut subtype_name_edges = Vec::new();
        for file in index.kotlin_files() {
            for declaration in &file.declarations {
                for supertype in &declaration.supertypes {
                    if let Some(target) = index.resolve_type(file, supertype) {
                        subtype_edges.push((declaration_key(target), file.path.clone()));
                        subtype_name_edges
                            .push((declaration_key(target), declaration.name.clone()));
                    }
                }
            }
        }
        for (target, path) in subtype_edges {
            index.kotlin_subtypes.entry(target).or_default().push(path);
        }
        // Reverse subtype edges by simple name for the retention fixpoint
        // (`subtype_names[hub] = [subtype names...]`). Simple names only:
        // the fixpoint over-taints on name collisions, never under-taints.
        for (target, subtype_name) in subtype_name_edges {
            let target_name = target.rsplit_once('.').map(|(_, n)| n).unwrap_or(&target);
            index
                .subtype_names
                .entry(target_name.to_string())
                .or_default()
                .push(subtype_name);
        }

        let new_cache = IndexCache {
            version: CACHE_VERSION,
            producer: cache_producer(),
            root_digest: new_root.digest,
            sources: flatten_sources(&new_root),
        };
        if old_cache.as_ref() != Some(&new_cache) {
            stats.cache_written = save_cache(&root, &new_cache);
        }
        Ok((index, stats))
    }

    pub fn kotlin_files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files
            .iter()
            .filter(|file| file.language == SourceLanguage::Kotlin)
    }

    pub fn java_files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files
            .iter()
            .filter(|file| file.language == SourceLanguage::Java)
    }

    pub fn source_file(&self, path: &Path) -> Option<&SourceFile> {
        self.files.iter().find(|file| paths_match(&file.path, path))
    }

    pub fn has_kotlin_reference(&self, declaring_file: &Path, name: &str) -> bool {
        self.kotlin_files().any(|file| {
            let count = file.identifier_counts.get(name).copied().unwrap_or(0);
            if paths_match(&file.path, declaring_file) {
                count > 1
            } else {
                count > 0
            }
        })
    }

    /// Residual-Kotlin consumption from OTHER files only: the declaring
    /// file is translated as a unit, so its internal references lower
    /// together with the declaration. Cross-file KClass-bound callers are
    /// the ones a `Class` ABI change would break.
    pub fn has_external_kotlin_reference(&self, declaring_file: &Path, name: &str) -> bool {
        self.kotlin_files().any(|file| {
            !paths_match(&file.path, declaring_file) && file.identifier_counts.contains_key(name)
        })
    }

    /// A pre-existing Java source file references `<Owner>.getEntries()` —
    /// the Kotlin enum `entries` ABI. Translating the owning enum to a plain
    /// Java enum would drop that static and break the Java caller.
    pub fn has_java_get_entries_consumer(&self, owner: &str) -> bool {
        self.java_files().any(|file| {
            file.enum_entries_qualifiers.contains_key(owner)
                || std::env::var("NOTLIN_DIAG_BROAD_ABI").is_ok_and(|v| v == "1")
                    && file.identifier_counts.contains_key("getEntries")
                    && file.imports.iter().any(|imp| imp.ends_with(".*"))
        })
    }

    /// A residual Kotlin source reads `<Owner>.entries`. The enum's own file
    /// is excluded because same-unit references lower together with it.
    pub fn has_kotlin_enum_entries_consumer(&self, owner: &str) -> bool {
        self.kotlin_files().any(|file| {
            !file.declarations.iter().any(|decl| decl.name == owner)
                && file.enum_entries_qualifiers.contains_key(owner)
        })
    }

    pub fn has_enum_entries_consumer(&self, owner: &str) -> bool {
        self.has_java_get_entries_consumer(owner) || self.has_kotlin_enum_entries_consumer(owner)
    }

    /// The recorded type of a property named exactly `prop` (lower-case
    /// field/property references, e.g. `contexts`). First hit wins;地产 the
    /// index is keyed by name only — caller-level overloads are not tracked.
    /// True when this class (declared in `file` by the given tree-sitter
    /// node-independent fields we can parse cheaply from the member list we
    /// already indexed) implements a supertype that REMAINS Kotlin and
    /// declares an abstract member whose Java-visible type conflicts with
    /// the class's own same-name member. Kotlin resolves such conflicts via
    /// fake overrides; Java cannot — the class must stay Kotlin.
    pub fn retained_supertype_member_mismatch(
        &self,
        supertypes: &[String],
        class_name: &str,
    ) -> bool {
        // The class's own declaration, by name (first hit is this class in
        // its own file because the class-name is canonical in the index).
        let Some(own) = self
            .declarations()
            .find(|d| d.name == class_name && d.language == SourceLanguage::Kotlin)
        else {
            return false;
        };
        // The own declaration's file resolves type names written in the
        // implementing class (imports/same-package rules apply there).
        let own_file = self.declaration_source_file(own).unwrap();
        let mut pending: Vec<&String> = supertypes.iter().collect();
        let mut visited: Vec<String> = supertypes
            .iter()
            .map(|s| {
                s.split('<')
                    .next()
                    .unwrap_or(s)
                    .trim()
                    .trim_start_matches('*')
                    .to_string()
            })
            .collect();
        while let Some(sup) = pending.pop() {
            let sup_base = sup
                .split('<')
                .next()
                .unwrap_or(sup)
                .trim()
                .trim_start_matches('*')
                .to_string();
            let Some(sup_decl) = self.declarations().find(|d| {
                d.name == sup_base
                    && matches!(d.kind, DeclarationKind::Interface | DeclarationKind::Class)
            }) else {
                continue;
            };
            // Only a supertype that remains Kotlin forces the ABI match, but
            // its OWN supertypes still widen the conflict transitively
            // (ImportContainer extends IImportContainer, IBarcodeAware).
            for sup_sup in &sup_decl.supertypes {
                let base = sup_sup
                    .split('<')
                    .next()
                    .unwrap_or(sup_sup)
                    .trim()
                    .to_string();
                if !visited.contains(&base) {
                    visited.push(base.clone());
                    pending.push(sup_sup);
                }
            }
            if sup_decl.language != SourceLanguage::Kotlin {
                continue;
            }
            for m in &sup_decl.members {
                let Some(sup_ty) = m.type_name.clone() else {
                    continue;
                };
                if let Some(own_m) = own
                    .members
                    .iter()
                    .find(|om| om.name == m.name && om.kind == m.kind)
                    && own_m.type_name.as_deref() != Some(sup_ty.as_str())
                {
                    // A supertype member typed by one of the SUPERTYPE's own
                    // type parameters (`interface IObjectCommand<T, I> {
                    // val payload: T }`) erases in Java to the parameter's
                    // bound — ANY implementing type satisfies it, so a text
                    // mismatch against the concrete class member is a false
                    // positive, not a fake-override ABI conflict.
                    if sup_decl.type_params.contains(&sup_ty)
                        || is_java_compatible_narrow(
                            self,
                            own_file,
                            &sup_ty,
                            own_m.type_name.as_deref().unwrap_or(""),
                        )
                    {
                        continue;
                    }
                    // Type strings are index-qualified names; conflicting
                    // here means Kotlin fake-override semantics are being
                    // relied on — unsupported in plain Java.
                    return true;
                }
            }
        }
        false
    }

    /// The recorded return type of a METHOD named exactly `name`, searching
    /// the declaring file first, then any declaration. Used for receiver-type
    /// checks on method calls (`this.getBaseUnit()` returning Optional).
    pub fn method_return_type_in_file(&self, declaring: &Path, method: &str) -> Option<String> {
        let m = method.trim_end_matches("()").trim_start_matches("this.");
        let getter_prop = m.strip_prefix("get").map(|rest| {
            format!(
                "{}{}",
                rest.chars()
                    .next()
                    .map(|c| c.to_ascii_lowercase().to_string())
                    .unwrap_or_default(),
                rest.chars().skip(1).collect::<String>()
            )
        });
        let same = self.source_file(declaring).and_then(|file| {
            file.declarations.iter().find_map(|d| {
                d.members.iter().find_map(|mm| {
                    if mm.kind == MemberKind::Method
                        && (mm.name == m || Some(&mm.name) == getter_prop.as_ref())
                    {
                        mm.type_name.clone()
                    } else {
                        None
                    }
                })
            })
        });
        let wide = self.declarations().find_map(|d| {
            d.members.iter().find_map(|mm| {
                if mm.kind == MemberKind::Method
                    && (mm.name == m || Some(&mm.name) == getter_prop.as_ref())
                {
                    mm.type_name.clone()
                } else {
                    None
                }
            })
        });
        same.or(wide)
    }

    pub fn bare_property_type(&self, prop: &str) -> Option<String> {
        self.declarations().find_map(|d| {
            d.members.iter().find_map(|m| {
                if m.kind == MemberKind::Property && m.name == prop {
                    m.type_name.clone()
                } else {
                    None
                }
            })
        })
    }

    /// Property type resolved within a specific declaring file first
    /// (same-file member beats cross-file name collisions for bare
    /// references like `contexts`), then any indexed declaration.
    pub fn property_type_in_file(&self, declaring: &Path, prop: &str) -> Option<String> {
        let same = self.source_file(declaring).and_then(|file| {
            let candidates: Vec<String> = file
                .declarations
                .iter()
                .flat_map(|d| d.members.iter())
                .filter(|m| m.kind == MemberKind::Property && m.name == prop)
                .filter_map(|m| m.type_name.clone())
                .collect();
            enum_typed_candidate(self, &candidates).or_else(|| candidates.first().cloned())
        });
        if same.is_some() {
            return same;
        }
        // Cross-file fallthrough: the same property name can exist on many
        // types (one may hold the enum, another a plain class). Prefer the
        // candidate whose declared type is an indexed ENUM — java enums
        // don't have `getX()` accessors, so an arbitrary first-hit type
        // silently changes the member's Java form. Everything else keeps
        // the deterministic first declaration order.
        let candidates: Vec<Option<String>> = self
            .declarations()
            .filter_map(|d| {
                d.members
                    .iter()
                    .find(|m| m.kind == MemberKind::Property && m.name == prop)
                    .map(|m| m.type_name.clone())
            })
            .collect();
        let enum_typed = candidates.iter().flatten().find_map(|t| {
            let bare = t.split('<').next().unwrap_or("").trim().to_string();
            if bare.is_empty() {
                return None;
            }
            self.declarations()
                .any(|d| d.name == bare && d.kind == crate::workspace::DeclarationKind::Enum)
                .then(|| t.clone())
        });
        match enum_typed {
            Some(t) => Some(t),
            None => candidates.into_iter().flatten().next(),
        }
    }

    /// A RETAINED Kotlin file (other than the enum's own declaring file)
    /// references the enum by name (member/local type use). Match by file
    /// stem so harness call-sites without the indexed path still exclude the
    /// declaring file.
    pub fn has_external_kotlin_reference_by_name(&self, name: &str) -> bool {
        self.kotlin_files().any(|file| {
            let self_file = file.declarations.iter().any(|decl| decl.name == name)
                || file
                    .path
                    .file_stem()
                    .map(|s| s.to_string_lossy() == name)
                    .unwrap_or(false);
            let references = file.identifier_counts.get(name).copied().unwrap_or(0);
            let entries_only = file.enum_entries_qualifiers.get(name).copied().unwrap_or(0);
            !self_file && references > entries_only
        })
    }

    /// The recorded type of the property backing a Java getter name
    /// (`getContexts` -> `contexts`). Powers expression-shape decisions
    /// (`m + n` collection algebra) where the operand is a member access.
    pub fn property_type_of_getter(&self, getter: &str) -> Option<String> {
        let prop = getter.strip_prefix("get")?;
        let prop = if prop.is_empty() {
            return None;
        } else {
            format!(
                "{}{}",
                prop.chars().next()?.to_ascii_lowercase(),
                &prop[1..]
            )
        };
        self.declarations().find_map(|d| {
            d.members.iter().find_map(|m| {
                if m.kind == MemberKind::Property && m.name == prop {
                    m.type_name.clone()
                } else {
                    None
                }
            })
        })
    }

    /// A workspace declaration owning a non-static PROPERTY member `name`.
    /// A bare identifier resolving to it reads through the property's Java
    /// accessor on the implicit `this`, regardless of whether the owner is
    /// translated (Java getter shed) or retained (Kotlin property ABI has
    /// the same getter shape).
    pub fn find_property_owner(&self, name: &str) -> Option<&Declaration> {
        self.declarations().find(|d| {
            d.members
                .iter()
                .any(|m| m.kind == MemberKind::Property && m.name == name && !m.is_static)
        })
    }

    /// A workspace declaration owning a STATIC member `name` (companion
    /// object function/property owned by the declaration itself). The
    /// declaration's language tells the caller whether the callee is Java
    /// (callable) or retained Kotlin (reified inline generics have no
    /// Java-callable ABI).
    pub fn find_static_member_owner(&self, name: &str) -> Option<&Declaration> {
        self.declarations().find(|d| {
            d.members
                .iter()
                .any(|m| m.name == name && m.is_static && m.kind == MemberKind::Method)
        })
    }

    /// Static member `member` owned by the declaration named `owner` — the
    /// precise companion lookup for `Owner.member(...)` call sites. Java
    /// declarations win over Kotlin ones (a translated owner with the same
    /// simple name already exposes the Java static; its leftover Kotlin
    /// source from pre-migration state must not route callers through
    /// `Companion`).
    pub fn find_static_member(&self, owner: &str, member: &str) -> Option<&Declaration> {
        let has_member = |d: &Declaration| {
            d.members
                .iter()
                .any(|m| m.name == member && m.is_static && m.kind == MemberKind::Method)
        };
        self.declarations()
            .filter(|d| d.name == owner && d.language == SourceLanguage::Java && has_member(d))
            .chain(self.declarations().filter(|d| {
                d.name == owner && d.language == SourceLanguage::Kotlin && has_member(d)
            }))
            .next()
    }

    pub fn declarations(&self) -> impl Iterator<Item = &Declaration> {
        self.files.iter().flat_map(|file| file.declarations.iter())
    }
    pub fn is_selected(&self, path: &Path, translation_roots: &[PathBuf]) -> bool {
        translation_roots.iter().any(|root| {
            if path.starts_with(root) {
                return true;
            }
            if root.to_string_lossy().starts_with(r"\\?\") {
                return false;
            }
            fs::canonicalize(root)
                .ok()
                .is_some_and(|canonical| path.starts_with(canonical))
        })
    }

    pub fn has_kotlin_subtype(&self, target: &Declaration) -> bool {
        self.kotlin_subtypes.contains_key(&declaration_key(target))
    }

    /// Retention fixpoint seed: true when any simple-name Kotlin subtype of
    /// `target` is retained for an INTRINSIC reason (its own file taints
    /// under the current fixpoint pass, independent of the subtype rule).
    /// Shallow by simple name — collisions over-taint, never under-taint.
    pub fn has_retained_kotlin_subtype(
        &self,
        target: &Declaration,
        retained: &HashSet<String>,
    ) -> bool {
        let key = declaration_key(target);
        let target_name = key.rsplit_once('.').map(|(_, n)| n).unwrap_or(&key);
        self.subtype_names
            .get(target_name)
            .is_some_and(|subtypes| subtypes.iter().any(|s| retained.contains(s)))
    }
    pub fn has_unselected_kotlin_subtype(
        &self,
        target: &Declaration,
        translation_roots: &[PathBuf],
    ) -> bool {
        self.kotlin_subtypes
            .get(&declaration_key(target))
            .is_some_and(|paths| {
                paths
                    .iter()
                    .any(|path| !self.is_selected(path, translation_roots))
            })
    }

    pub fn property_smart_cast_used_by_kotlin(
        &self,
        declaring_file: &Path,
        target: &Declaration,
    ) -> bool {
        self.kotlin_files().any(|file| {
            !paths_match(&file.path, declaring_file)
                && file
                    .identifier_counts
                    .get(&target.name)
                    .is_some_and(|count| *count > 0)
                && target.members.iter().any(|member| {
                    member.kind == MemberKind::Property
                        && file.smart_cast_properties.contains(&member.name)
                })
        })
    }

    pub fn narrows_nullable_kotlin_property(
        &self,
        source_file: &SourceFile,
        target: &Declaration,
    ) -> bool {
        target.supertypes.iter().any(|supertype| {
            let Some(contract) = self.resolve_kotlin_type(source_file, supertype) else {
                return false;
            };
            contract.members.iter().any(|contract_member| {
                contract_member.kind == MemberKind::Property
                    && contract_member.is_nullable
                    && target.members.iter().any(|member| {
                        member.kind == MemberKind::Property
                            && member.name == contract_member.name
                            && !member.is_nullable
                    })
            })
        })
    }

    /// True when translating `target` would split a retained Kotlin interface
    /// property from a fake override that still depends on it.
    pub fn inherits_retained_kotlin_property_interface(
        &self,
        source_file: &SourceFile,
        target: &Declaration,
    ) -> bool {
        let own_properties: HashSet<&str> = target
            .members
            .iter()
            .filter(|member| member.kind == MemberKind::Property && !member.is_static)
            .map(|member| member.name.as_str())
            .collect();
        let mut inherited_properties = HashSet::new();
        for supertype in &target.supertypes {
            if let Some(declaration) = self.resolve_kotlin_type(source_file, supertype) {
                self.collect_retained_interface_property_names(
                    declaration,
                    &mut HashSet::new(),
                    &mut inherited_properties,
                );
            }
        }
        if inherited_properties
            .iter()
            .any(|property| own_properties.contains(property.as_str()))
        {
            return true;
        }

        self.has_retained_property_branch_collision(source_file, target, &mut HashSet::new())
    }

    fn has_retained_property_branch_collision(
        &self,
        source_file: &SourceFile,
        declaration: &Declaration,
        visited: &mut HashSet<String>,
    ) -> bool {
        if !visited.insert(declaration_key(declaration)) {
            return false;
        }

        let mut seen = HashSet::new();
        let mut direct_supertypes = Vec::new();
        for supertype in &declaration.supertypes {
            let Some(supertype) = self.resolve_kotlin_type(source_file, supertype) else {
                continue;
            };
            let mut branch_properties = HashSet::new();
            self.collect_retained_interface_property_names(
                supertype,
                &mut HashSet::new(),
                &mut branch_properties,
            );
            if branch_properties
                .iter()
                .any(|property| !seen.insert(property.clone()))
            {
                return true;
            }
            direct_supertypes.push(supertype);
        }

        direct_supertypes.into_iter().any(|supertype| {
            self.declaration_source_file(supertype).is_some_and(|file| {
                self.has_retained_property_branch_collision(file, supertype, visited)
            })
        })
    }

    fn collect_retained_interface_property_names(
        &self,
        declaration: &Declaration,
        visited: &mut HashSet<String>,
        properties: &mut HashSet<String>,
    ) {
        if !visited.insert(declaration_key(declaration)) {
            return;
        }
        if declaration.kind == DeclarationKind::Interface && self.has_kotlin_subtype(declaration) {
            properties.extend(
                declaration
                    .members
                    .iter()
                    .filter(|member| {
                        member.kind == MemberKind::Property
                            && !member.is_static
                            && member.visibility.as_deref() != Some("private")
                    })
                    .map(|member| member.name.clone()),
            );
        }
        let Some(source_file) = self.declaration_source_file(declaration) else {
            return;
        };
        for supertype in &declaration.supertypes {
            if let Some(supertype) = self.resolve_kotlin_type(source_file, supertype) {
                self.collect_retained_interface_property_names(supertype, visited, properties);
            }
        }
    }

    fn declaration_source_file(&self, declaration: &Declaration) -> Option<&SourceFile> {
        self.files.iter().find(|file| {
            file.declarations
                .iter()
                .any(|candidate| std::ptr::eq(candidate, declaration))
        })
    }

    fn resolve_kotlin_type<'a>(
        &'a self,
        source_file: &SourceFile,
        type_name: &str,
    ) -> Option<&'a Declaration> {
        let simple = type_name
            .trim()
            .trim_end_matches('?')
            .split('<')
            .next()
            .unwrap_or("")
            .split('(')
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("");
        let find = |qualified: &str| {
            let (package, name) = qualified.rsplit_once('.')?;
            self.declarations().find(|declaration| {
                declaration.language == SourceLanguage::Kotlin
                    && declaration.name == name
                    && declaration.package.as_deref() == Some(package)
            })
        };
        if type_name.contains('.') {
            return find(type_name.trim().trim_end_matches('?'));
        }
        for import in &source_file.imports {
            if import.ends_with(&format!(".{simple}"))
                && let Some(found) = find(import)
            {
                return Some(found);
            }
            if let Some(package) = import.strip_suffix(".*")
                && let Some(found) = find(&format!("{package}.{simple}"))
            {
                return Some(found);
            }
        }
        if let Some(package) = &source_file.package
            && let Some(found) = find(&format!("{package}.{simple}"))
        {
            return Some(found);
        }
        let mut matches = self
            .declarations()
            .filter(|declaration| declaration.language == SourceLanguage::Kotlin)
            .filter(|declaration| declaration.name == simple);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    pub fn resolve_type<'a>(
        &'a self,
        source_file: &SourceFile,
        type_name: &str,
    ) -> Option<&'a Declaration> {
        let simple = type_name
            .trim()
            .trim_end_matches('?')
            .split('<')
            .next()
            .unwrap_or("")
            .split('(')
            .next()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("");
        if type_name.contains('.') {
            return self.find_qualified(type_name.trim().trim_end_matches('?'));
        }
        for import in &source_file.imports {
            if import.ends_with(&format!(".{simple}"))
                && let Some(found) = self.find_qualified(import)
            {
                return Some(found);
            }
            if let Some(package) = import.strip_suffix(".*")
                && let Some(found) = self.find_qualified(&format!("{package}.{simple}"))
            {
                return Some(found);
            }
        }
        if let Some(package) = &source_file.package
            && let Some(found) = self.find_qualified(&format!("{package}.{simple}"))
        {
            return Some(found);
        }
        let mut matches = self
            .declarations()
            .filter(|declaration| declaration.name == simple);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    /// Public wrapper for narrowing checks: resolve a type name exactly like
    /// the internal Kotlin-type resolver (imports + same-package + unique
    /// Kotlin simple name).
    pub fn resolve_kotlin_type_public<'a>(
        &'a self,
        source_file: &SourceFile,
        type_name: &str,
    ) -> Option<&'a Declaration> {
        self.resolve_kotlin_type(source_file, type_name)
    }

    /// True when `decl`'s transitive supertype closure (Kotlin and Java
    /// edges alike) contains the type named `target_base`.
    pub fn supertype_closure_contains(
        &self,
        source_file: &SourceFile,
        decl: &Declaration,
        target_base: &str,
    ) -> bool {
        let mut visited = std::collections::HashSet::new();
        let mut pending: Vec<&Declaration> = vec![decl];
        while let Some(current) = pending.pop() {
            if !visited.insert(declaration_key(current)) {
                continue;
            }
            let current_file = self.declaration_source_file(current).unwrap_or(source_file);
            for supertype in &current.supertypes {
                let base = supertype
                    .trim()
                    .trim_end_matches('?')
                    .split('<')
                    .next()
                    .unwrap_or("")
                    .rsplit('.')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if base == target_base {
                    return true;
                }
                if let Some(next) = self.resolve_type(current_file, supertype) {
                    pending.push(next);
                }
            }
        }
        false
    }

    fn find_qualified(&self, qualified: &str) -> Option<&Declaration> {
        let (package, name) = qualified.rsplit_once('.')?;
        self.declarations().find(|declaration| {
            declaration.name == name && declaration.package.as_deref() == Some(package)
        })
    }
}

fn declaration_key(declaration: &Declaration) -> String {
    match &declaration.package {
        Some(package) => format!("{package}.{}", declaration.name),
        None => declaration.name.clone(),
    }
}

fn paths_match(left: &Path, right: &Path) -> bool {
    fn normalized(path: &Path) -> String {
        let value = path.to_string_lossy().replace('\\', "/");
        let value = value.strip_prefix("//?/").unwrap_or(&value);
        if cfg!(windows) {
            value.to_ascii_lowercase()
        } else {
            value.to_string()
        }
    }
    normalized(left) == normalized(right)
}

fn cache_producer() -> u128 {
    std::env::current_exe()
        .ok()
        .and_then(|path| fs::metadata(path).ok())
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn open_cache_directory(root: &Path, create: bool) -> std::io::Result<Dir> {
    let root_directory = Dir::open_ambient_dir(root, ambient_authority())?;
    match root_directory.symlink_metadata(CACHE_DIR) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "cache directory must not be a symlink",
                ));
            }
        }
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
            root_directory.create_dir(CACHE_DIR)?;
        }
        Err(error) => return Err(error),
    }
    let metadata = root_directory.symlink_metadata(CACHE_DIR)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cache directory must not be a symlink",
        ));
    }
    root_directory.open_dir(CACHE_DIR)
}

fn load_cache(root: &Path) -> Option<IndexCache> {
    let cache_dir = open_cache_directory(root, false).ok()?;
    let metadata = cache_dir.symlink_metadata(CACHE_FILE).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        log::debug!("ignoring unsafe index cache {CACHE_FILE}");
        return None;
    }
    let file = cache_dir.open(CACHE_FILE).ok()?;
    let mut reader = BufReader::new(file).take(MAX_CACHE_BYTES + 1);
    let decoded = serde_json::from_reader::<_, IndexCache>(&mut reader);
    if reader.limit() == 0 {
        log::debug!("ignoring oversized index cache {CACHE_FILE}");
        return None;
    }
    match decoded {
        Ok(cache) if cache.version == CACHE_VERSION && cache.producer == cache_producer() => {
            Some(cache)
        }
        Ok(_) => None,
        Err(error) => {
            log::debug!("ignoring index cache {CACHE_FILE}: {error}");
            None
        }
    }
}

fn save_cache(root: &Path, cache: &IndexCache) -> bool {
    let cache_dir = match open_cache_directory(root, true) {
        Ok(directory) => directory,
        Err(error) => {
            log::debug!("could not open workspace cache directory: {error}");
            return false;
        }
    };
    let temporary = format!("{CACHE_FILE}.tmp-{}", std::process::id());
    let backup = format!("{CACHE_FILE}.bak-{}", std::process::id());
    let result = (|| -> Result<(), String> {
        if let Ok(metadata) = cache_dir.symlink_metadata(CACHE_FILE)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(format!("unsafe index cache {CACHE_FILE}"));
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let file = cache_dir
            .open_with(&temporary, &options)
            .map_err(|error| format!("{temporary}: {error}"))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, cache)
            .map_err(|error| format!("{temporary}: {error}"))?;
        writer
            .flush()
            .map_err(|error| format!("{temporary}: {error}"))?;
        let cache_exists = cache_dir.symlink_metadata(CACHE_FILE).is_ok();
        if cache_exists {
            let _ = cache_dir.remove_file(&backup);
            cache_dir
                .rename(CACHE_FILE, &cache_dir, &backup)
                .map_err(|error| format!("{CACHE_FILE} -> {backup}: {error}"))?;
        }
        if let Err(error) = cache_dir.rename(&temporary, &cache_dir, CACHE_FILE) {
            if cache_dir.symlink_metadata(&backup).is_ok() {
                let _ = cache_dir.rename(&backup, &cache_dir, CACHE_FILE);
            }
            return Err(format!("{temporary} -> {CACHE_FILE}: {error}"));
        }
        let _ = cache_dir.remove_file(&backup);
        Ok(())
    })();
    match result {
        Ok(()) => true,
        Err(error) => {
            let _ = cache_dir.remove_file(&temporary);
            log::debug!("could not persist workspace index: {error}");
            false
        }
    }
}

fn scan_directory(
    root: &Path,
    relative: &Path,
    old_sources: &HashMap<PathBuf, &CachedSource>,
    visited_directories: &mut HashSet<PathBuf>,
    files: &mut Vec<SourceFile>,
    stats: &mut IndexStats,
) -> Result<CachedDirectory, String> {
    let directory = root.join(relative);
    let mut directory_entries = fs::read_dir(&directory)
        .map_err(|error| format!("{}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", directory.display()))?;
    directory_entries.sort_by_key(|entry| entry.file_name());

    let mut entries = BTreeMap::new();
    for entry in directory_entries {
        let name = PathBuf::from(entry.file_name());
        let child_relative = relative.join(&name);
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if should_skip_directory(&name) && (file_type.is_dir() || file_type.is_symlink()) {
            continue;
        }
        let source_language = || match path.extension().and_then(|extension| extension.to_str()) {
            Some("kt") => Some(SourceLanguage::Kotlin),
            Some("java") => Some(SourceLanguage::Java),
            _ => None,
        };
        let followed_type = if file_type.is_symlink() {
            match fs::metadata(&path) {
                Ok(metadata) => Some(metadata.file_type()),
                Err(_) if source_language().is_none() => continue,
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        } else {
            None
        };
        let is_directory =
            followed_type.as_ref().is_some_and(fs::FileType::is_dir) || file_type.is_dir();
        let is_file =
            followed_type.as_ref().is_some_and(fs::FileType::is_file) || file_type.is_file();

        if is_directory {
            let canonical_directory =
                fs::canonicalize(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            if !visited_directories.insert(canonical_directory) {
                continue;
            }
            let child = scan_directory(
                root,
                &child_relative,
                old_sources,
                visited_directories,
                files,
                stats,
            )?;
            entries.insert(name, CachedEntry::Directory(Box::new(child)));
            continue;
        }
        if !is_file {
            continue;
        }
        let Some(language) = source_language() else {
            continue;
        };
        let previous = old_sources.get(&child_relative).copied();
        let source = scan_source(&path, language, previous, stats)?;
        files.push(source.source_file.clone());
        entries.insert(name, CachedEntry::Source(Box::new(source)));
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"notlin-directory-v1\0");
    for (name, entry) in &entries {
        hasher.update(name.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(match entry {
            CachedEntry::Directory(_) => b"d",
            CachedEntry::Source(_) => b"f",
        });
        hasher.update(entry.digest());
    }
    Ok(CachedDirectory {
        digest: *hasher.finalize().as_bytes(),
        entries,
    })
}

fn scan_source(
    path: &Path,
    language: SourceLanguage,
    previous: Option<&CachedSource>,
    stats: &mut IndexStats,
) -> Result<CachedSource, String> {
    let canonical_path =
        fs::canonicalize(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let metadata = fs::metadata(&canonical_path)
        .map_err(|error| format!("{}: {error}", canonical_path.display()))?;
    let size = metadata.len();
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    let bytes = fs::read(&canonical_path)
        .map_err(|error| format!("{}: {error}", canonical_path.display()))?;
    let digest = *blake3::hash(&bytes).as_bytes();
    if let Some(previous) = previous
        && previous.digest == digest
    {
        let mut cached = previous.source_file.clone();
        cached.path = canonical_path.clone();
        stats.reused_files += 1;
        return Ok(CachedSource {
            size,
            modified_nanos,
            digest,
            source_file: cached,
        });
    }

    let source = String::from_utf8(bytes)
        .map_err(|error| format!("{}: {error}", canonical_path.display()))?;
    let package = package_name(&source);
    let imports = import_names(&source);
    let (declarations, identifier_counts, enum_entries_qualifiers, smart_cast_properties) =
        parse_declarations(&source, language, package.as_deref())?;
    stats.parsed_files += 1;
    Ok(CachedSource {
        size,
        modified_nanos,
        digest,
        source_file: SourceFile {
            path: canonical_path,
            language,
            package,
            imports,
            declarations,
            identifier_counts,
            enum_entries_qualifiers,
            smart_cast_properties,
        },
    })
}

fn should_skip_directory(name: &Path) -> bool {
    // `.notlin` cache dir, plus gradle/cargo build-output mirrors: indexing
    // them shadows migrated sources (`build/k2j/...` twins) and breaks
    // companion-owner resolution.
    name.to_str() == Some(CACHE_DIR) || name.to_str() == Some("build")
}

fn package_name(source: &str) -> Option<String> {
    source.lines().map(str::trim).find_map(|line| {
        let package = line.strip_prefix("package ")?.trim();
        (!package.is_empty()).then(|| package.trim_end_matches(';').to_string())
    })
}

fn import_names(source: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("import "))
        .map(|import| import.trim_end_matches(';').trim().to_string())
        .filter(|import| !import.is_empty())
        .collect()
}
/// Everything `parse_declarations` extracts from one source file's
/// declaration tree (factored out of a tuple so the signature stays legible).
type ParseDeclarations = (
    Vec<Declaration>,
    HashMap<String, usize>,
    HashMap<String, usize>,
    HashSet<String>,
);

fn parse_declarations(
    source: &str,
    language: SourceLanguage,
    package: Option<&str>,
) -> Result<ParseDeclarations, String> {
    let mut parser = tree_sitter::Parser::new();
    let grammar = match language {
        SourceLanguage::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
        SourceLanguage::Java => tree_sitter_java::LANGUAGE.into(),
    };
    parser
        .set_language(&grammar)
        .map_err(|error| format!("failed to load source grammar: {error}"))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "source parse failed".to_string())?;
    let mut declarations = Vec::new();
    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().children(&mut cursor) {
        let Some((kind, name_node)) = declaration_shape(node, language) else {
            continue;
        };
        let name = node_text(name_node, source)?;
        declarations.push(Declaration {
            name,
            package: package.map(str::to_string),
            language,
            kind,
            supertypes: supertypes(node, language, source),
            members: members(node, language, source),
            has_default_constructor_parameter: has_default_constructor_parameter(node, language),
            type_params: type_param_names(node, source),
        });
    }
    let mut identifier_counts = HashMap::new();
    collect_identifier_counts(tree.root_node(), source, &mut identifier_counts);
    let mut enum_entries_qualifiers = HashMap::new();
    collect_enum_entries_qualifiers(
        tree.root_node(),
        source,
        language,
        &mut enum_entries_qualifiers,
    );
    let mut smart_cast_properties = HashSet::new();
    if language == SourceLanguage::Kotlin {
        collect_smart_cast_properties(tree.root_node(), source, &mut smart_cast_properties);
    }
    Ok((
        declarations,
        identifier_counts,
        enum_entries_qualifiers,
        smart_cast_properties,
    ))
}

fn collect_enum_entries_qualifiers(
    node: tree_sitter::Node<'_>,
    source: &str,
    language: SourceLanguage,
    qualifiers: &mut HashMap<String, usize>,
) {
    let owner = match language {
        SourceLanguage::Kotlin if node.kind() == "navigation_expression" => {
            let identifiers: Vec<_> = node
                .named_children(&mut node.walk())
                .filter(|child| child.kind() == "identifier")
                .collect();
            if identifiers.len() == 2
                && identifiers[1].utf8_text(source.as_bytes()).ok() == Some("entries")
            {
                identifiers[0].utf8_text(source.as_bytes()).ok()
            } else {
                None
            }
        }
        SourceLanguage::Java if node.kind() == "method_invocation" => {
            let object = node.child_by_field_name("object");
            let name = node.child_by_field_name("name");
            if object.is_some_and(|object| object.kind() == "identifier")
                && name.and_then(|name| name.utf8_text(source.as_bytes()).ok())
                    == Some("getEntries")
            {
                object.and_then(|object| object.utf8_text(source.as_bytes()).ok())
            } else {
                None
            }
        }
        _ => None,
    };
    if let Some(owner) = owner {
        *qualifiers.entry(owner.to_string()).or_default() += 1;
    }
    for child in node.named_children(&mut node.walk()) {
        collect_enum_entries_qualifiers(child, source, language, qualifiers);
    }
}

fn collect_smart_cast_properties(
    node: tree_sitter::Node<'_>,
    source: &str,
    properties: &mut HashSet<String>,
) {
    if node.kind() == "is_expression"
        && let Some(left) = node.child_by_field_name("left")
        && left.kind() == "navigation_expression"
        && let Some(property) = left
            .named_children(&mut left.walk())
            .filter(|child| child.kind() == "identifier")
            .last()
        && let Ok(name) = property.utf8_text(source.as_bytes())
    {
        properties.insert(name.to_string());
    }
    for child in node.named_children(&mut node.walk()) {
        collect_smart_cast_properties(child, source, properties);
    }
}

fn collect_identifier_counts(
    node: tree_sitter::Node<'_>,
    source: &str,
    counts: &mut HashMap<String, usize>,
) {
    if node.kind() == "identifier"
        && let Ok(name) = node.utf8_text(source.as_bytes())
    {
        *counts.entry(name.to_string()).or_default() += 1;
    }
    for child in node.named_children(&mut node.walk()) {
        collect_identifier_counts(child, source, counts);
    }
}

fn has_default_constructor_parameter(
    node: tree_sitter::Node<'_>,
    language: SourceLanguage,
) -> bool {
    if language != SourceLanguage::Kotlin {
        return false;
    }
    let Some(constructor) = node.child_by_field_name("primary_constructor").or_else(|| {
        node.children(&mut node.walk())
            .find(|child| child.kind() == "primary_constructor")
    }) else {
        return false;
    };
    let Some(parameters) = constructor
        .children(&mut constructor.walk())
        .find(|child| child.kind() == "class_parameters")
    else {
        return false;
    };
    parameters
        .named_children(&mut parameters.walk())
        .filter(|parameter| parameter.kind() == "class_parameter")
        .any(|parameter| {
            parameter
                .children(&mut parameter.walk())
                .any(|child| child.kind() == "=")
        })
}

fn declaration_shape(
    node: tree_sitter::Node<'_>,
    language: SourceLanguage,
) -> Option<(DeclarationKind, tree_sitter::Node<'_>)> {
    match (language, node.kind()) {
        (SourceLanguage::Kotlin, "class_declaration") => {
            node.child_by_field_name("name").map(|name| {
                let is_interface = node
                    .children(&mut node.walk())
                    .any(|child| child.kind() == "interface");
                // `enum` rides inside the `modifiers` node, not at the
                // declaration's top level.
                let is_enum = node
                    .children(&mut node.walk())
                    .any(|child| child.kind() == "modifiers")
                    && {
                        let mods = node
                            .children(&mut node.walk())
                            .find(|child| child.kind() == "modifiers");
                        mods.map(|m| {
                            m.children(&mut m.walk()).any(|sub| {
                                sub.kind() == "class_modifier" && {
                                    sub.children(&mut sub.walk()).any(|kw| kw.kind() == "enum")
                                }
                            })
                        })
                        .unwrap_or(false)
                    };
                (
                    if is_interface {
                        DeclarationKind::Interface
                    } else if is_enum {
                        DeclarationKind::Enum
                    } else {
                        DeclarationKind::Class
                    },
                    name,
                )
            })
        }
        (SourceLanguage::Kotlin, "object_declaration") => node
            .child_by_field_name("name")
            .map(|name| (DeclarationKind::Object, name)),
        (SourceLanguage::Java, "class_declaration") => node
            .child_by_field_name("name")
            .map(|name| (DeclarationKind::Class, name)),
        (SourceLanguage::Java, "interface_declaration") => node
            .child_by_field_name("name")
            .map(|name| (DeclarationKind::Interface, name)),
        (SourceLanguage::Java, "enum_declaration") => node
            .child_by_field_name("name")
            .map(|name| (DeclarationKind::Enum, name)),
        (SourceLanguage::Java, "record_declaration") => node
            .child_by_field_name("name")
            .map(|name| (DeclarationKind::Record, name)),
        _ => None,
    }
}

fn supertypes(node: tree_sitter::Node<'_>, language: SourceLanguage, source: &str) -> Vec<String> {
    let fields: &[&str] = match language {
        SourceLanguage::Kotlin => &["delegation_specifiers"],
        SourceLanguage::Java => &["superclass", "interfaces", "super_interfaces"],
    };
    let mut nodes: Vec<tree_sitter::Node<'_>> = fields
        .iter()
        .filter_map(|field| node.child_by_field_name(field))
        .collect();
    if nodes.is_empty() {
        let kinds: &[&str] = match language {
            SourceLanguage::Kotlin => &["delegation_specifiers"],
            SourceLanguage::Java => &["superclass", "super_interfaces", "interfaces"],
        };
        nodes = node
            .children(&mut node.walk())
            .filter(|child| kinds.contains(&child.kind()))
            .collect();
    }
    nodes
        .into_iter()
        .flat_map(|child| {
            if language == SourceLanguage::Kotlin && child.kind() == "delegation_specifiers" {
                child
                    .named_children(&mut child.walk())
                    .filter(|specifier| specifier.kind() == "delegation_specifier")
                    .collect::<Vec<_>>()
            } else {
                vec![child]
            }
        })
        .map(|child| {
            child
                .utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .filter(|text| !text.is_empty())
        .collect()
}

/// Type-parameter names of a declaration (`class Foo<T, I : Bar>` ->
/// `["T", "I"]`), from the `type_parameters` AST child. Java only.
fn type_param_names(node: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let Some(params) = node
        .children(&mut node.walk())
        .find(|child| child.kind() == "type_parameters")
    else {
        return Vec::new();
    };
    params
        .children(&mut params.walk())
        .filter(|child| child.kind() == "type_parameter")
        .filter_map(|child| {
            child
                .children(&mut child.walk())
                .find(|inner| inner.kind() == "identifier")
                .and_then(|id| node_text(id, source).ok())
        })
        .collect()
}

fn members(node: tree_sitter::Node<'_>, language: SourceLanguage, source: &str) -> Vec<Member> {
    let mut result = Vec::new();
    if language == SourceLanguage::Kotlin
        && let Some(parameters) = node
            .children(&mut node.walk())
            .find(|child| child.kind() == "primary_constructor")
            .and_then(|constructor| {
                constructor
                    .children(&mut constructor.walk())
                    .find(|child| child.kind() == "class_parameters")
            })
    {
        for parameter in parameters
            .named_children(&mut parameters.walk())
            .filter(|parameter| parameter.kind() == "class_parameter")
        {
            let is_property = parameter
                .children(&mut parameter.walk())
                .any(|child| matches!(child.kind(), "val" | "var"));
            if !is_property {
                continue;
            }
            let Some(name_node) = parameter
                .named_children(&mut parameter.walk())
                .find(|child| child.kind() == "identifier")
            else {
                continue;
            };
            let type_node = parameter
                .named_children(&mut parameter.walk())
                .find(|child| matches!(child.kind(), "user_type" | "nullable_type"));
            result.push(Member {
                name: node_text(name_node, source).unwrap_or_default(),
                kind: MemberKind::Property,
                visibility: None,
                is_static: false,
                type_name: type_node.and_then(|node| node_text(node, source).ok()),
                is_nullable: type_node.is_some_and(|node| node.kind() == "nullable_type"),
            });
        }
    }
    if let Some(body) = node.child_by_field_name("body").or_else(|| {
        node.children(&mut node.walk())
            .find(|child| child.kind() == "class_body")
    }) {
        result.extend(
            body.children(&mut body.walk())
                .filter_map(|child| member_from_node(child, language, source)),
        );
        // Companion-object members are static members of the owner: Kotlin
        // `Owner.make` on the companion resolves through the owner name in
        // Java. Index them (marked static) so call sites can be resolved —
        // and reified companion fns flagged as un-Java-callable.
        for companion in body
            .children(&mut body.walk())
            .filter(|child| child.kind() == "companion_object")
        {
            if let Some(cb) = companion.child_by_field_name("body").or_else(|| {
                companion
                    .children(&mut companion.walk())
                    .find(|c| c.kind() == "class_body")
            }) {
                for member in cb.children(&mut cb.walk()) {
                    let mut member = member_from_node(member, language, source);
                    if let Some(m) = member.as_mut() {
                        m.is_static = true;
                    }
                    result.extend(member);
                }
            }
        }
    }
    result
}

fn member_from_node(
    node: tree_sitter::Node<'_>,
    language: SourceLanguage,
    source: &str,
) -> Option<Member> {
    let (kind, name_node) = match (language, node.kind()) {
        (SourceLanguage::Kotlin, "property_declaration") => (
            MemberKind::Property,
            node.child_by_field_name("name").or_else(|| {
                node.named_children(&mut node.walk())
                    .find(|child| child.kind() == "variable_declaration")
                    .and_then(|variable| {
                        variable
                            .named_children(&mut variable.walk())
                            .find(|child| child.kind() == "identifier")
                    })
            }),
        ),
        (SourceLanguage::Kotlin, "function_declaration") => {
            (MemberKind::Method, node.child_by_field_name("name"))
        }
        (SourceLanguage::Kotlin, "secondary_constructor") => (MemberKind::Constructor, None),
        (SourceLanguage::Java, "field_declaration") => (MemberKind::Field, first_identifier(node)),
        (SourceLanguage::Java, "method_declaration") => {
            (MemberKind::Method, node.child_by_field_name("name"))
        }
        (SourceLanguage::Java, "constructor_declaration") => {
            (MemberKind::Constructor, node.child_by_field_name("name"))
        }
        _ => return None,
    };
    let name = name_node
        .map(|node| node_text(node, source))
        .transpose()
        .ok()??;
    let modifiers = node
        .child_by_field_name("modifiers")
        .or_else(|| {
            node.children(&mut node.walk())
                .find(|child| child.kind() == "modifiers")
        })
        .map(|node| node.utf8_text(source.as_bytes()).unwrap_or(""))
        .unwrap_or("");
    let visibility = ["public", "protected", "internal", "private"]
        .iter()
        .find(|visibility| {
            modifiers
                .split_whitespace()
                .any(|word| word == **visibility)
        })
        .map(|visibility| (*visibility).to_string());
    let type_node = node
        .child_by_field_name("type")
        .or_else(|| first_type_node(node));
    let type_name = type_node.map(|node| {
        node.utf8_text(source.as_bytes())
            .unwrap_or("")
            .trim()
            .to_string()
    });
    Some(Member {
        name: if name.is_empty() {
            "<init>".to_string()
        } else {
            name
        },
        kind,
        visibility,
        is_static: modifiers.split_whitespace().any(|word| word == "static"),
        type_name,
        is_nullable: type_node.is_some_and(|node| node.kind() == "nullable_type"),
    })
}

fn first_type_node(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    if matches!(node.kind(), "nullable_type" | "user_type") {
        return Some(node);
    }
    node.named_children(&mut node.walk())
        .find_map(first_type_node)
}

fn first_identifier(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "identifier" {
            return Some(current);
        }
        stack.extend(current.children(&mut current.walk()));
    }
    None
}

fn node_text(node: tree_sitter::Node<'_>, source: &str) -> Result<String, String> {
    node.utf8_text(source.as_bytes())
        .map(str::to_string)
        .map_err(|error| format!("invalid source text: {error}"))
}

/// A Java member may NARROW a supertype member's return type when the own
/// type is a Java SUBTYPE of the supertype's type: covariant return types
/// are legal in Java for both classes and interfaces (JLS 8.4.5). Equal
/// names, `Object`/`Any`, and type-parameter members always qualify.
/// Everything else (unrelated types, invariant generics like
/// `List<A>` vs `List<B>`) is a mismatch.
fn is_java_compatible_narrow(
    index: &SourceIndex,
    source_file: &SourceFile,
    super_ty: &str,
    own_ty: &str,
) -> bool {
    if super_ty == own_ty || super_ty == "Object" || super_ty == "Any" {
        return true;
    }
    // Invariant generics: `List<A>` only satisfies `List<A>` (wildcards are
    // not emitted by the transpiler today).
    let (sup_base, sup_args) = split_generic(super_ty);
    let (own_base, own_args) = split_generic(own_ty);
    if sup_base != own_base {
        // Covariant narrowing requires own to be a SUBTYPE of sup: walk own's
        // supertype closure through the index.
        return index
            .resolve_kotlin_type_public(source_file, own_ty)
            .is_some_and(|own_decl| {
                index.supertype_closure_contains(source_file, own_decl, sup_base)
            });
    }
    match (sup_args, own_args) {
        (None, _) => true, // raw sup type accepts any instantiation
        (Some(_), None) => false,
        (Some(sup), Some(own)) => {
            // Generics are INVARIANT in Java: a nested argument must match
            // exactly (`List<ItemImpl>` does not satisfy `List<Item>`, even
            // though ItemImpl is an Item). Only the TOP-LEVEL narrowing is
            // covariant.
            sup.len() == own.len() && sup.iter().zip(own.iter()).all(|(s, o)| s == o)
        }
    }
}

/// `Map<String, List<Foo>>` -> (`Map`, Some(["String", "List<Foo>"])).
fn split_generic(ty: &str) -> (&str, Option<Vec<&str>>) {
    let open = ty.find('<');
    let Some(open) = open else {
        return (ty.trim(), None);
    };
    if !ty.ends_with('>') {
        return (ty.trim(), None);
    }
    let base = ty[..open].trim();
    let body = &ty[open + 1..ty.len() - 1];
    let mut args = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in body.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                args.push(body[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    args.push(body[start..].trim());
    (base, Some(args))
}

/// Prefer a candidate type whose bare name is an indexed enum declaration.
/// Java enums replace `name` with the JDK accessor, so when the same member
/// name exists on both an interface and an enum-typed holder, the enum-typed
/// candidate decides the member's Java form (`name()` vs `getName()`).
fn enum_typed_candidate(ws: &SourceIndex, candidates: &[String]) -> Option<String> {
    for t in candidates {
        let bare = t.split('<').next().unwrap_or("").trim();
        if bare.is_empty() {
            continue;
        }
        if ws
            .declarations()
            .any(|d| d.name == bare && d.kind == crate::workspace::DeclarationKind::Enum)
        {
            return Some(t.clone());
        }
    }
    None
}
