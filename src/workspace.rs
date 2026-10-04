use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

// 5: `Declaration::constructor_param_count` (primary-constructor arity, not a
// property count — companion members and body properties both inflate that).
// 8: `SourceFile::smart_cast_sites` / `SourceFile::bindings` — the retained-Kotlin
// smart-cast boundary now carries the site shapes and the file's own name->type
// table instead of a bare property-name set.
const CACHE_VERSION: u32 = 11;
const CACHE_DIR: &str = ".notlin";
const CACHE_FILE: &str = "index-v1.bin";
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberConflictClass {
    Exact,
    JavaCovariantReturn,
    SupertypeTypeParameter,
    InvariantGenericConflict,
    UnrelatedReturnTypes,
    UnknownType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberConflict {
    pub member_name: String,
    pub kind: MemberKind,
    pub supertype: String,
    pub inherited_type: String,
    pub implementation_type: String,
    pub parameter_types: Vec<String>,
    pub classification: MemberConflictClass,
    legacy_mismatch: bool,
}

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
    /// `annotation class` — Kotlin annotation type. Annotation USE sites on
    /// declarations referencing an annotation-kind declaration stay Kotlin:
    /// the Java side cannot reference a Kotlin-only annotation element.
    Annotation,
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
    /// `@JvmField` companion/instance val: a real Java static field —
    /// caller sites must emit `Owner.MEMBER`, never the `Companion`
    /// accessor bridge that only exists for plain companion vals.
    pub is_jvm_field: bool,
    pub type_name: Option<String>,
    #[serde(default)]
    pub parameter_types: Vec<String>,
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
    /// Primary-constructor arity. Distinct from the number of `Property`
    /// members: a companion contributes static members and a class body adds
    /// properties that are not constructor parameters (`data class X(val a: T
    /// = ...) { override val b: U get() = ... }` has arity 1, not 2). Call
    /// sites fill omitted trailing defaults from this, so a wrong count
    /// invents a constructor arity that does not exist.
    #[serde(default)]
    pub constructor_param_count: usize,
    /// Primary-constructor parameter names in declaration order. Kotlin named
    /// arguments (`MethodCall(name = "openport", params = p)`) have no Java
    /// form, so call sites lower them to positional arguments — which requires
    /// knowing the declared order to reorder rather than guess.
    #[serde(default)]
    pub constructor_param_names: Vec<String>,
    /// A defaulted constructor parameter that is followed by one WITHOUT a
    /// default. Not a blocker by itself: a caller that omits it needs a shape
    /// Java can express, either by inlining a language-neutral literal into the
    /// call ([`CtorCall`] evidence) or by a delegating overload the emitter
    /// writes for the omission pattern ([`SourceIndex::ctor_omission_evidence`]).
    /// Only a pattern that is actually used by a caller AND cannot be lowered
    /// that way keeps the declaration in Kotlin.
    #[serde(default)]
    pub has_non_trailing_default: bool,
    /// Raw Kotlin text of each primary-constructor parameter's default, in
    /// declaration order (`None` where the parameter has no default).
    ///
    /// A caller that omits a parameter needs that default at the call site: a
    /// language-neutral literal is written straight into the argument list
    /// (`new Example(first, 10, last)`), and anything else needs a delegating
    /// overload the emitter writes. The text stays raw Kotlin — a literal reads
    /// the same in both languages — and a default that names another parameter
    /// is recognisable from it, which is what makes a pattern unlowerable.
    #[serde(default)]
    pub constructor_param_defaults: Vec<Option<String>>,
    /// Parameter types of each SECONDARY constructor, one entry per
    /// `constructor(...)` in declaration order (`constructor(quantity: Number,
    /// unit: ItemUnit)` -> `["Number", "ItemUnit"]`).
    ///
    /// A call written with as many arguments as one of these is a
    /// secondary-constructor call: the primary constructor's parameters are not
    /// involved, so the call is not evidence that any of them can be omitted —
    /// reading it as such retained declarations whose callers were never
    /// omitting anything. Empty for a type with no secondary constructor, which
    /// is exactly when an unmatchable call shape stays unreadable.
    #[serde(default)]
    pub secondary_ctors: Vec<Vec<String>>,
    /// Type-parameter names in declaration order (`ICreateObjectCommand` ->
    /// `["T", "I"]`). Needed to distinguish a generic supertype member typed
    /// by its own parameter (`payload: T`) — Java-erasure compatible with
    /// any implementing type — from a genuinely different concrete type.
    pub type_params: Vec<String>,
}

impl Declaration {
    /// The primary constructor's parameter names as a call site sees them.
    ///
    /// Where the source states them (`constructor_param_names`), those. A Java
    /// record or a Lombok-annotated class has no constructor *declaration* — the
    /// generated one takes the instance state in declaration order — so the
    /// state stands in. Empty when neither is available, which leaves a call
    /// site's named arguments unmapped rather than guessed at.
    pub fn ctor_param_names_or_state(&self) -> Vec<String> {
        if !self.constructor_param_names.is_empty() {
            return self.constructor_param_names.clone();
        }
        if self.language != SourceLanguage::Java {
            return Vec::new();
        }
        self.members
            .iter()
            .filter(|member| {
                !member.is_static && matches!(member.kind, MemberKind::Field | MemberKind::Property)
            })
            .map(|member| member.name.clone())
            .collect()
    }
}

/// A constructor call as one source file writes it: enough to resolve which
/// primary-constructor parameters the caller left out, without re-parsing the
/// file later.
///
/// Only a bare, capitalized callee is recorded: `Outer.Inner(...)` and
/// `Factory(...)` are indistinguishable from a companion `invoke` factory at
/// this granularity, and a false pattern would only cost an unused overload —
/// whereas a missed one could translate a declaration whose caller no longer
/// compiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CtorCall {
    /// Callee simple name as written (`Example`).
    pub callee: String,
    /// Positional arguments, which Kotlin requires to come first.
    pub positional: usize,
    /// Named arguments, in written order.
    pub named: Vec<String>,
    /// An argument whose shape could not be read (a spread, or a positional
    /// argument after a named one). The call's omission set is unknown, so it
    /// cannot certify that a declaration is safe to translate.
    pub unknown: bool,
    /// 1-based line, for the report.
    pub line: usize,
}

/// Which parameters callers leave out of one declaration's primary
/// constructor, and whether any call shape could not be read.
///
/// Kotlin allows any subset of defaulted parameters to be omitted (by name);
/// Java expresses a shape with a delegating overload. This is the evidence that
/// separates the two: an omission pattern nobody uses costs nothing, so a
/// declaration with a middle default is only retained when a caller really
/// omits one in a way the emitter cannot serve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CtorOmissionEvidence {
    /// Omitted parameter index sets, deduplicated and sorted, each one a shape
    /// some caller actually uses.
    pub patterns: Vec<Vec<usize>>,
    /// Some caller names its arguments. Java has no named arguments, so such a
    /// call site has to be lowered to positional order wherever it survives as
    /// Kotlin.
    pub named_callers: bool,
    /// Calls to this name whose argument shape could not be resolved, as
    /// `file:line`. An unreadable caller may omit anything.
    pub unresolvable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: PathBuf,
    pub language: SourceLanguage,
    pub package: Option<String>,
    pub imports: Vec<String>,
    pub declarations: Vec<Declaration>,
    /// Kotlin `typealias` declarations in this file: alias name -> target
    /// type. Aliases are source-only and must be substituted before Java is
    /// emitted; Java has no equivalent declaration form.
    type_aliases: HashMap<String, String>,
    identifier_counts: HashMap<String, usize>,
    enum_entries_qualifiers: HashMap<String, usize>,
    /// Property names a smart cast in this file narrows, whatever the shape.
    smart_cast_properties: HashSet<String>,
    /// Every smart cast in this file, with the shape and whether the rewrite
    /// pass can repair it — the retained-Kotlin smart-cast boundary's evidence.
    smart_cast_sites: Vec<crate::smart_cast::SmartCastSite>,
    /// Name -> simple type name from this file's OWN declarations. Resolves a
    /// smart-cast receiver to the declaration whose property a translation
    /// would move to Java.
    bindings: HashMap<String, String>,
    /// Constructor calls this file makes, as written. A declaration with a
    /// middle default is only retained when a caller really omits one in a
    /// shape the emitter cannot serve, so the shapes have to be recorded where
    /// they are written rather than guessed from a name mention.
    #[serde(default)]
    ctor_calls: Vec<CtorCall>,
}

impl SourceFile {
    /// Constructor calls this file makes, as written.
    pub fn ctor_calls(&self) -> &[CtorCall] {
        &self.ctor_calls
    }

    /// Every smart cast this file performs, with its shape.
    pub fn smart_cast_sites(&self) -> &[crate::smart_cast::SmartCastSite] {
        &self.smart_cast_sites
    }

    /// Property names this file's smart casts narrow.
    pub fn smart_cast_properties(&self) -> &HashSet<String> {
        &self.smart_cast_properties
    }

    /// `name -> simple type` from this file's own declarations.
    pub fn bindings(&self) -> &HashMap<String, String> {
        &self.bindings
    }
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
    /// Simple declaration-name lookup preserving every collision. This avoids
    /// repeated complete-workspace scans for name-based compatibility checks.
    declaration_names: HashMap<String, Vec<(usize, usize)>>,
    /// Constructor calls by callee simple name: `name -> [(file, call)]`.
    /// Derived from the files' recorded calls whenever the index is built (never
    /// cached): a declaration's omission evidence is then one lookup instead of a
    /// workspace scan per declaration.
    ctor_uses: HashMap<String, Vec<(usize, usize)>>,
    /// Exact indexed source paths. `source_file` retains its `paths_match`
    /// fallback for equivalent-but-not-identical caller paths.
    file_paths: HashMap<PathBuf, usize>,
}

#[derive(Debug, Clone)]
pub enum SourceOverlay {
    Replace {
        path: PathBuf,
        language: SourceLanguage,
        source: String,
    },
    Delete {
        path: PathBuf,
    },
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
        let root = fs::canonicalize(root)
            .map_err(|error| format!("{}: {error}", crate::paths::display(root)))?;
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

        let index = Self::from_files(files);

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

    fn from_files(files: Vec<SourceFile>) -> Self {
        let mut index = Self {
            files,
            kotlin_subtypes: HashMap::new(),
            subtype_names: HashMap::new(),
            declaration_names: HashMap::new(),
            ctor_uses: HashMap::new(),
            file_paths: HashMap::new(),
        };
        for (file_index, file) in index.files.iter().enumerate() {
            index
                .file_paths
                .entry(file.path.clone())
                .or_insert(file_index);
            for (call_index, call) in file.ctor_calls().iter().enumerate() {
                index
                    .ctor_uses
                    .entry(call.callee.clone())
                    .or_default()
                    .push((file_index, call_index));
            }
            for (declaration_index, declaration) in file.declarations.iter().enumerate() {
                index
                    .declaration_names
                    .entry(declaration.name.clone())
                    .or_default()
                    .push((file_index, declaration_index));
            }
        }
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
        index
    }

    pub fn with_overlays(&self, overlays: &[SourceOverlay]) -> Result<Self, String> {
        let mut files = self.files.clone();
        for overlay in overlays {
            match overlay {
                SourceOverlay::Delete { path } => {
                    files.retain(|file| !paths_match(&file.path, path))
                }
                SourceOverlay::Replace {
                    path,
                    language,
                    source,
                } => {
                    files.retain(|file| !paths_match(&file.path, path));
                    let package = package_name(source);
                    let imports = import_names(source);
                    let (
                        declarations,
                        type_aliases,
                        identifier_counts,
                        enum_entries_qualifiers,
                        smart_cast_properties,
                        smart_cast_sites,
                        bindings,
                        ctor_calls,
                    ) = parse_declarations(source, *language, package.as_deref())?;
                    files.push(SourceFile {
                        path: path.clone(),
                        language: *language,
                        package,
                        imports,
                        declarations,
                        type_aliases,
                        identifier_counts,
                        enum_entries_qualifiers,
                        smart_cast_properties,
                        smart_cast_sites,
                        bindings,
                        ctor_calls,
                    });
                }
            }
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(Self::from_files(files))
    }

    /// Which primary-constructor parameters callers leave out of `target`'s
    /// constructor, and whether any call shape could not be read.
    ///
    /// Name-keyed: a `Foo(...)` is attributed to every declaration named `Foo`,
    /// because a call site's target is resolved by name at this granularity.
    /// Over-attributing costs at most an unused delegating overload;
    /// under-attributing would translate a declaration whose caller no longer
    /// compiles.
    pub fn ctor_omission_evidence(&self, target: &Declaration) -> CtorOmissionEvidence {
        let mut evidence = CtorOmissionEvidence::default();
        let names = &target.constructor_param_names;
        if names.is_empty() {
            return evidence;
        }
        let defaults = &target.constructor_param_defaults;
        for (file_index, call_index) in self.ctor_uses.get(&target.name).into_iter().flatten() {
            let Some(file) = self.files.get(*file_index) else {
                continue;
            };
            let Some(call) = file.ctor_calls().get(*call_index) else {
                continue;
            };
            let site = format!("{}:{}", file.path.display(), call.line);
            if call.unknown {
                evidence.unresolvable.push(site);
                continue;
            }
            if !call.named.is_empty() {
                evidence.named_callers = true;
            }
            let mut filled: Vec<usize> = (0..call.positional).collect();
            let mut resolved = call.positional <= names.len();
            for name in &call.named {
                match names.iter().position(|param| param == name) {
                    Some(index) => filled.push(index),
                    None => resolved = false,
                }
            }
            if !resolved {
                evidence.unresolvable.push(site);
                continue;
            }
            let omitted: Vec<usize> = (0..names.len())
                .filter(|index| !filled.contains(index))
                .collect();
            // A caller that omits a parameter with no default is not a
            // primary-constructor omission at all: a positional call shorter than
            // the primary constructor is a SECONDARY constructor's — the emitted
            // Java keeps those, and the declaration's own defaults are not
            // involved, so the call certifies nothing either way. The secondary
            // constructor has to match the call's ARITY to explain it: a class
            // that merely declares some other overload does not make an
            // arbitrary shape readable, and treating it as readable would drop
            // evidence for a call site that really can no longer compile.
            // Without a match the shape is genuinely unreadable (a companion
            // `invoke`, a typealias, or a call this resolution missed), and an
            // unreadable caller may omit anything.
            if omitted
                .iter()
                .any(|index| !defaults.get(*index).is_some_and(Option::is_some))
            {
                let written = call.positional + call.named.len();
                if !target
                    .secondary_ctors
                    .iter()
                    .any(|params| params.len() == written)
                {
                    evidence.unresolvable.push(site);
                }
                continue;
            }
            if !omitted.is_empty() && !evidence.patterns.contains(&omitted) {
                evidence.patterns.push(omitted);
            }
        }
        evidence.patterns.sort();
        evidence
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

    /// Top-level declaration simple names of one indexed source file — used
    /// to detect a translated file's own names shadowing its single imports
    /// (legal Kotlin, rejected by javac: "X is already defined in this
    /// compilation unit").
    pub fn decl_names_in_file(&self, path: &Path) -> Vec<String> {
        self.source_file(path)
            .map(|file| {
                file.declarations
                    .iter()
                    .map(|decl| decl.name.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn source_file(&self, path: &Path) -> Option<&SourceFile> {
        if let Some(&file_index) = self.file_paths.get(path) {
            return self.files.get(file_index);
        }
        self.files.iter().find(|file| paths_match(&file.path, path))
    }

    /// Whether adding any simple declaration name to the retention set can
    /// change this Kotlin file's retention coverage on a later fixpoint pass.
    /// Matching deliberately uses simple names, mirroring the conservative
    /// collision behavior of the existing retained-subtype/supertype checks.
    pub fn retained_delta_can_affect(&self, file: &Path, delta: &HashSet<String>) -> bool {
        let source_file = self.source_file(file).or_else(|| {
            fs::canonicalize(file)
                .ok()
                .and_then(|canonical| self.source_file(&canonical))
        });
        let Some(source_file) = source_file else {
            return false;
        };
        source_file.declarations.iter().any(|declaration| {
            delta.contains(&declaration.name)
                || declaration.supertypes.iter().any(|supertype| {
                    let simple = supertype
                        .split('<')
                        .next()
                        .unwrap_or(supertype)
                        .trim()
                        .rsplit('.')
                        .next()
                        .unwrap_or_default();
                    delta.contains(simple)
                })
                || (declaration.kind == DeclarationKind::Interface
                    && self
                        .subtype_names
                        .get(&declaration.name)
                        .is_some_and(|subtypes| {
                            subtypes.iter().any(|subtype| delta.contains(subtype))
                        }))
        })
    }

    pub fn type_aliases_for(&self, file: &Path) -> Vec<(String, String)> {
        let Some(using_file) = self.source_file(file) else {
            return Vec::new();
        };

        let mut aliases = Vec::new();
        for declaring_file in &self.files {
            for (name, target) in &declaring_file.type_aliases {
                let qualified = declaring_file
                    .package
                    .as_ref()
                    .map(|package| format!("{package}.{name}"));
                let visible = paths_match(&declaring_file.path, file)
                    || qualified.as_ref().is_some_and(|qualified| {
                        using_file.imports.iter().any(|import| import == qualified)
                    })
                    || using_file
                        .package
                        .as_ref()
                        .zip(declaring_file.package.as_ref())
                        .is_some_and(|(left, right)| left == right);
                if !visible {
                    continue;
                }
                let resolved = declaring_file
                    .imports
                    .iter()
                    .find(|import| import.rsplit('.').next() == Some(target.as_str()))
                    .cloned()
                    .unwrap_or_else(|| target.clone());
                aliases.push((name.clone(), resolved));
            }
        }
        aliases.sort();
        aliases.dedup_by(|left, right| left.0 == right.0);
        aliases
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

    /// A residual (retained) Kotlin file — not the declaring file —
    /// references `name`. When the referenced declaration is a translated
    /// object singleton, the plain-name expression in the residual source
    /// would break, so callers retain the declaration.
    pub fn has_retained_kotlin_reference(
        &self,
        declaring_file: &Path,
        name: &str,
        retained: &HashSet<String>,
    ) -> bool {
        !self
            .retained_kotlin_referencers(declaring_file, name, retained)
            .is_empty()
    }

    /// Which retained declarations reference `name` from another Kotlin file —
    /// the blame edges behind the retained-reference reason. The declaration
    /// stays Kotlin only while one of these does, so the run-end report
    /// credits their root reasons with it.
    pub fn retained_kotlin_referencers(
        &self,
        declaring_file: &Path,
        name: &str,
        retained: &HashSet<String>,
    ) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for file in self.kotlin_files() {
            if paths_match(&file.path, declaring_file)
                || file.identifier_counts.get(name).copied().unwrap_or(0) == 0
            {
                continue;
            }
            names.extend(
                file.declarations
                    .iter()
                    .filter(|decl| retained.contains(&decl.name))
                    .map(|decl| decl.name.clone()),
            );
        }
        names.sort();
        names.dedup();
        names
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

    /// A Kotlin file that will SURVIVE this run reads `<owner>.entries`: only
    /// such a consumer keeps the enum's Kotlin `entries` ABI alive. Java
    /// consumers are served by the emitted `getEntries()` bridge, and Kotlin
    /// files that translate away lower their own reads in the same run.
    pub fn has_retained_kotlin_enum_entries_consumer(
        &self,
        owner: &str,
        retained: &HashSet<String>,
        translation_roots: &[PathBuf],
    ) -> bool {
        self.kotlin_files().any(|file| {
            if !file.enum_entries_qualifiers.contains_key(owner) {
                return false;
            }
            let declares_owner = file.declarations.iter().any(|decl| decl.name == owner);
            if !self.is_selected(&file.path, translation_roots) {
                // Outside the translation set: the whole file stays Kotlin.
                return true;
            }
            file.declarations.iter().any(|decl| {
                retained.contains(&decl.name) && !(declares_owner && decl.name == owner)
            })
        })
    }

    /// `name` is referenced by Kotlin that survives the run, so the referenced
    /// ABI must survive with it. A file outside the translation set stays
    /// Kotlin whole; inside it, only the declarations the fixpoint retains do.
    /// The declaring file counts only through its OTHER retained declarations
    /// (its references lower together with the declaration).
    pub fn has_surviving_kotlin_reference(
        &self,
        declaring_file: &Path,
        name: &str,
        retained: &HashSet<String>,
        translation_roots: &[PathBuf],
    ) -> bool {
        self.kotlin_files().any(|file| {
            let references = file.identifier_counts.get(name).copied().unwrap_or(0);
            if references == 0 {
                return false;
            }
            let same_file = paths_match(&file.path, declaring_file);
            if same_file && references <= 1 {
                return false;
            }
            if !self.is_selected(&file.path, translation_roots) {
                return true;
            }
            file.declarations
                .iter()
                .any(|decl| retained.contains(&decl.name) && !(same_file && decl.name == name))
        })
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
    pub fn retained_supertype_member_mismatches(
        &self,
        supertypes: &[String],
        class_name: &str,
    ) -> Vec<String> {
        let mut mismatches: Vec<String> = self
            .retained_supertype_member_conflicts(supertypes, class_name)
            .into_iter()
            .filter(|conflict| conflict.legacy_mismatch)
            .map(|conflict| conflict.member_name)
            .collect();
        mismatches.sort();
        mismatches.dedup();
        mismatches
    }

    /// Classify same-name inherited members before projecting them onto the
    /// conservative legacy retention decision. This keeps uncertainty visible
    /// without changing which declarations N5258 retains.
    pub fn retained_supertype_member_conflicts(
        &self,
        supertypes: &[String],
        class_name: &str,
    ) -> Vec<MemberConflict> {
        // A Kotlin supertype whose member has a DIFFERENT type cannot be
        // implemented from Java at all: a parameterized override must match
        // exactly, and erasing the type arguments to force it through would
        // emit raw types (which JPA rejects and generic consumers lose). So a
        // conflict here means the class stays Kotlin — whether or not the
        // supertype itself is translated.
        // The class's own declaration, by name (first hit is this class in
        // its own file because the class-name is canonical in the index).
        let Some(own) = self
            .declarations()
            .find(|d| d.name == class_name && d.language == SourceLanguage::Kotlin)
        else {
            return Vec::new();
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
        let mut conflicts = Vec::new();
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
                if let Some(own_m) = own.members.iter().find(|om| {
                    om.name == m.name
                        && om.kind == m.kind
                        && om.parameter_types == m.parameter_types
                }) && let Some(own_ty) = own_m.type_name.as_deref()
                {
                    conflicts.push(MemberConflict {
                        member_name: m.name.clone(),
                        kind: m.kind,
                        supertype: sup_decl.name.clone(),
                        inherited_type: sup_ty.clone(),
                        implementation_type: own_ty.to_string(),
                        parameter_types: m.parameter_types.clone(),
                        classification: classify_member_conflict(
                            self, own_file, sup_decl, &sup_ty, own_ty,
                        ),
                        legacy_mismatch: own_ty != sup_ty
                            && !sup_decl
                                .type_params
                                .iter()
                                .any(|param| param == sup_ty.trim_end_matches('?').trim())
                            && !is_java_compatible_narrow(self, own_file, &sup_ty, own_ty),
                    });
                }
            }
        }
        conflicts.sort_by(|left, right| {
            (&left.member_name, &left.supertype, &left.inherited_type).cmp(&(
                &right.member_name,
                &right.supertype,
                &right.inherited_type,
            ))
        });
        conflicts.dedup();
        conflicts
    }

    pub fn has_retained_kotlin_supertype(
        &self,
        supertypes: &[String],
        retained: &HashSet<String>,
    ) -> bool {
        !self
            .retained_kotlin_supertype_names(supertypes, retained)
            .is_empty()
    }

    /// Which of a declaration's supertypes are retained Kotlin declarations —
    /// the blame edges behind `one of its supertypes is retained in Kotlin`.
    /// The declaration stays Kotlin while any of them does, so the run-end
    /// report credits their root reasons with it. Name extraction is the same
    /// expression the boolean check has always used: the two must not disagree
    /// about which supertype they are looking at.
    pub fn retained_kotlin_supertype_names(
        &self,
        supertypes: &[String],
        retained: &HashSet<String>,
    ) -> Vec<String> {
        let mut names: Vec<String> = supertypes
            .iter()
            .map(|supertype| {
                supertype
                    .split('<')
                    .next()
                    .unwrap_or(supertype)
                    .trim()
                    .rsplit('.')
                    .next()
                    .unwrap_or_default()
            })
            .filter(|name| {
                retained.contains(*name)
                    && self.declarations().any(|declaration| {
                        declaration.name == *name && declaration.language == SourceLanguage::Kotlin
                    })
            })
            .map(str::to_string)
            .collect();
        names.sort();
        names.dedup();
        names
    }

    pub fn retained_supertype_member_mismatch(
        &self,
        supertypes: &[String],
        class_name: &str,
    ) -> bool {
        !self
            .retained_supertype_member_mismatches(supertypes, class_name)
            .is_empty()
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

    /// Non-static properties inherited by `type_name` in `declaring`'s import
    /// context. Interface default bodies may read those properties through
    /// Kotlin's implicit receiver even when the property is declared only on a
    /// superinterface; Java must call `this.getProperty()`.
    pub fn inherited_property_names_in_file(
        &self,
        declaring: &Path,
        type_name: &str,
    ) -> Vec<String> {
        let _ = declaring;
        let Some(declaration) = self.declarations_named(type_name).next() else {
            return Vec::new();
        };
        let mut names = HashSet::new();
        let mut visited = HashSet::new();
        let mut pending = declaration.supertypes.clone();
        while let Some(supertype) = pending.pop() {
            let simple = supertype
                .split('<')
                .next()
                .unwrap_or(&supertype)
                .trim()
                .rsplit('.')
                .next()
                .unwrap_or_default();
            if !visited.insert(simple.to_string()) {
                continue;
            }
            for parent in self.declarations_named(simple) {
                names.extend(
                    parent
                        .members
                        .iter()
                        .filter(|member| member.kind == MemberKind::Property && !member.is_static)
                        .map(|member| member.name.clone()),
                );
                pending.extend(parent.supertypes.iter().cloned());
            }
        }
        names.into_iter().collect()
    }

    /// The Java type of a property `prop` inherited by `type_name` through
    /// its supertype chain — the override-narrowing rule: a Kotlin
    /// `override val x` WITHOUT a declared type must keep the supertype's
    /// declared type (Java has no bridging getter; emitting the erased
    /// initializer type instead `clashes with getX() in <super>` at javac).
    pub fn inherited_property_type_in_file(
        &self,
        declaring: &Path,
        type_name: &str,
        prop: &str,
    ) -> Option<String> {
        let _ = declaring;
        let declaration = self.declarations_named(type_name).next()?;
        let mut visited = HashSet::new();
        let mut pending = declaration.supertypes.clone();
        while let Some(supertype) = pending.pop() {
            let simple = supertype
                .split('<')
                .next()
                .unwrap_or(&supertype)
                .trim()
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_string();
            if !visited.insert(simple.clone()) {
                continue;
            }
            for parent in self.declarations_named(&simple) {
                if let Some(member) = parent
                    .members
                    .iter()
                    .find(|m| m.kind == MemberKind::Property && m.name == prop)
                    && let Some(t) = &member.type_name
                {
                    return Some(t.clone());
                }
                pending.extend(parent.supertypes.iter().cloned());
            }
        }
        None
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

    /// Static companion property `member` owned by `owner`.
    pub fn find_static_property(&self, owner: &str, member: &str) -> Option<&Declaration> {
        self.declarations().find(|d| {
            d.name == owner
                && d.members
                    .iter()
                    .any(|m| m.name == member && m.kind == MemberKind::Property)
        })
    }

    pub fn declarations(&self) -> impl Iterator<Item = &Declaration> {
        self.files.iter().flat_map(|file| file.declarations.iter())
    }

    /// Every declaration with this simple name, including collisions across
    /// packages. This preserves the conservative semantics of name-based
    /// workspace compatibility checks without re-scanning every declaration.
    pub fn declarations_named(&self, name: &str) -> impl Iterator<Item = &Declaration> {
        self.declaration_names.get(name).into_iter().flatten().map(
            |&(file_index, declaration_index)| {
                &self.files[file_index].declarations[declaration_index]
            },
        )
    }
    /// Whether `name` resolves to a Kotlin `object` declaration that Java
    /// can also see (kind == Object). Java callers must reference the
    /// singleton as `Name.INSTANCE` (a bare `Name` would attempt a
    /// constructor call).
    pub fn object_declaration_named(&self, name: &str) -> Option<&Declaration> {
        self.declarations_named(name)
            .find(|decl| decl.kind == crate::workspace::DeclarationKind::Object)
    }

    /// Java getter name generated for a Kotlin property `id` (`getId`)
    /// inherited from a Kotlin supertype chain member FUNCTION with a
    /// different return type. When such a member exists (e.g.
    /// `fun getId(): LookupEntityId` in a base class), the translated
    /// subtype must NOT synthesize its own getter for the same name: the
    /// base function stands in as the getter and any synthesized getter
    /// clashes on return type.
    pub fn inherited_fun_getter_conflicts(
        &self,
        declaring_file: &Path,
        decl_name: &str,
        getter_name: &str,
    ) -> bool {
        let Some(source) = self.source_file(declaring_file) else {
            return false;
        };
        let Some(target) = source.declarations.iter().find(|d| d.name == decl_name) else {
            return false;
        };
        let mut queue: Vec<&Declaration> = Vec::new();
        for supertype in &target.supertypes {
            if let Some(next) = self.resolve_kotlin_type(source, supertype) {
                queue.push(next);
            }
        }
        let mut visited: HashSet<String> = HashSet::new();
        while let Some(decl) = queue.pop() {
            if !visited.insert(declaration_key(decl)) {
                continue;
            }
            if decl
                .members
                .iter()
                .any(|member| member.kind == MemberKind::Method && member.name == getter_name)
            {
                return true;
            }
            for supertype in &decl.supertypes {
                if let Some(next) = self.resolve_kotlin_type(source, supertype) {
                    queue.push(next);
                }
            }
        }
        false
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

    pub fn all_kotlin_selected(&self, translation_roots: &[PathBuf]) -> bool {
        self.kotlin_files()
            .all(|file| self.is_selected(&file.path, translation_roots))
    }

    pub fn annotation_is_selected(&self, name: &str, translation_roots: &[PathBuf]) -> bool {
        self.files.iter().any(|file| {
            self.is_selected(&file.path, translation_roots)
                && file.declarations.iter().any(|declaration| {
                    declaration.name == name && declaration.kind == DeclarationKind::Annotation
                })
        })
    }

    /// True when anything in the workspace declares `name` as a supertype, in
    /// EITHER language. A class emitted `final` that something extends is a
    /// hard error in both compilers ("cannot inherit from final class"), so a
    /// declaration that is extended must stay open in the Java output even
    /// when Kotlin's own default would have made it final.
    pub fn has_subtype_named(&self, name: &str) -> bool {
        if self
            .subtype_names
            .get(name)
            .is_some_and(|names| !names.is_empty())
        {
            return true;
        }
        // The reverse map is built for the retention fixpoint and may cover
        // Kotlin only; hand-written Java in the workspace can extend a
        // translated class too, and javac would reject the result.
        self.declarations().any(|declaration| {
            declaration.supertypes.iter().any(|supertype| {
                supertype
                    .split('<')
                    .next()
                    .unwrap_or(supertype)
                    .trim()
                    .rsplit('.')
                    .next()
                    .unwrap_or_default()
                    == name
            })
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
        !self
            .retained_kotlin_subtype_names(target, retained)
            .is_empty()
    }

    /// Which simple-name Kotlin subtypes of `target` are retained — the blame
    /// edges behind both subtype-based reasons. `target` stays Kotlin while any
    /// of them does, so the run-end report credits the roots of these names
    /// with `target` too. Shallow (direct subtype edges): the report walks the
    /// graph it builds from these to get the transitive fallout.
    pub fn retained_kotlin_subtype_names(
        &self,
        target: &Declaration,
        retained: &HashSet<String>,
    ) -> Vec<String> {
        let key = declaration_key(target);
        let target_name = key.rsplit_once('.').map(|(_, n)| n).unwrap_or(&key);
        let mut names: Vec<String> = self
            .subtype_names
            .get(target_name)
            .map(|subtypes| {
                subtypes
                    .iter()
                    .filter(|subtype| retained.contains(*subtype))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names.dedup();
        names
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

    /// True when every retained-Kotlin smart cast that depends on one of
    /// `target`'s properties is a site the rewrite pass can repair — so
    /// translating `target` cannot break its residual callers, and the
    /// smart-cast retention reason does not have to fire.
    ///
    /// Conservative by construction. A site only counts once its receiver
    /// RESOLVES: an unresolvable receiver could be `target`, so it keeps the
    /// declaration in Kotlin (`None` below) rather than betting on a rewrite
    /// that would never fire. A site that resolves to `target` counts only when
    /// its shape is supported; sites that resolve to a different declaration are
    /// somebody else's boundary and are ignored.
    pub fn smart_cast_rewrites_cover(&self, declaring_file: &Path, target: &Declaration) -> bool {
        let properties: HashSet<&str> = target
            .members
            .iter()
            .filter(|member| member.kind == MemberKind::Property && !member.is_static)
            .map(|member| member.name.as_str())
            .collect();
        let mut covered = false;
        for file in self.kotlin_files() {
            if paths_match(&file.path, declaring_file)
                || file
                    .identifier_counts
                    .get(&target.name)
                    .is_none_or(|count| *count == 0)
            {
                continue;
            }
            for site in &file.smart_cast_sites {
                // A site nothing depends on cannot break when the owner moves:
                // a single getter call narrows nothing that needs narrowing.
                if !properties.contains(site.property.as_str()) || !site.needs_repair() {
                    continue;
                }
                match file.bindings.get(&site.receiver) {
                    Some(owner) if owner == &target.name => {
                        if !site.repairable {
                            return false;
                        }
                        covered = true;
                    }
                    Some(_) => {}
                    None => return false,
                }
            }
        }
        covered
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
        translation_roots: &[std::path::PathBuf],
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
                // When the property-owning supertype itself translates in
                // this run, its getter lowers into Java together with the
                // implementor; no mixed-ABI collision exists. Retention is
                // only needed when the interface remains Kotlin.
                if self
                    .declaration_source_file(declaration)
                    .is_some_and(|file| self.is_selected(&file.path, translation_roots))
                {
                    continue;
                }
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

        self.has_retained_property_branch_collision(
            source_file,
            target,
            translation_roots,
            &mut HashSet::new(),
        )
    }

    fn has_retained_property_branch_collision(
        &self,
        source_file: &SourceFile,
        declaration: &Declaration,
        translation_roots: &[std::path::PathBuf],
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
            if self
                .declaration_source_file(supertype)
                .is_some_and(|file| self.is_selected(&file.path, translation_roots))
            {
                continue;
            }
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
                self.has_retained_property_branch_collision(
                    file,
                    supertype,
                    translation_roots,
                    visited,
                )
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

    pub(crate) fn declaration_source_file(&self, declaration: &Declaration) -> Option<&SourceFile> {
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
        .map_err(|error| format!("{}: {error}", crate::paths::display(&directory)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("{}: {error}", crate::paths::display(&directory)))?;
    directory_entries.sort_by_key(|entry| entry.file_name());

    let mut entries = BTreeMap::new();
    for entry in directory_entries {
        let name = PathBuf::from(entry.file_name());
        let child_relative = relative.join(&name);
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("{}: {error}", crate::paths::display(&path)))?;
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
                Err(error) => return Err(format!("{}: {error}", crate::paths::display(&path))),
            }
        } else {
            None
        };
        let is_directory =
            followed_type.as_ref().is_some_and(fs::FileType::is_dir) || file_type.is_dir();
        let is_file =
            followed_type.as_ref().is_some_and(fs::FileType::is_file) || file_type.is_file();

        if is_directory {
            let canonical_directory = fs::canonicalize(&path)
                .map_err(|error| format!("{}: {error}", crate::paths::display(&path)))?;
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
    let canonical_path = fs::canonicalize(path)
        .map_err(|error| format!("{}: {error}", crate::paths::display(path)))?;
    let metadata = fs::metadata(&canonical_path)
        .map_err(|error| format!("{}: {error}", crate::paths::display(&canonical_path)))?;
    let size = metadata.len();
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    let bytes = fs::read(&canonical_path)
        .map_err(|error| format!("{}: {error}", crate::paths::display(&canonical_path)))?;
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
        .map_err(|error| format!("{}: {error}", crate::paths::display(&canonical_path)))?;
    let package = package_name(&source);
    let imports = import_names(&source);
    let (
        declarations,
        type_aliases,
        identifier_counts,
        enum_entries_qualifiers,
        smart_cast_properties,
        smart_cast_sites,
        bindings,
        ctor_calls,
    ) = parse_declarations(&source, language, package.as_deref())?;
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
            type_aliases,
            identifier_counts,
            enum_entries_qualifiers,
            smart_cast_properties,
            smart_cast_sites,
            bindings,
            ctor_calls,
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
    HashMap<String, String>,
    HashMap<String, usize>,
    HashMap<String, usize>,
    HashSet<String>,
    Vec<crate::smart_cast::SmartCastSite>,
    HashMap<String, String>,
    Vec<CtorCall>,
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
    let mut type_aliases = HashMap::new();
    // Walk the whole tree so NESTED declarations (a class declared inside
    // another class body) are indexed too: the retention fixpoint must see
    // a nested Kotlin class's implements edge, or its supertype interface
    // gets translated and the retained child can no longer override its
    // properties (a Kotlin val cannot implement a Java-source getter).
    let mut stack: Vec<tree_sitter::Node> = tree
        .root_node()
        .children(&mut tree.root_node().walk())
        .collect();
    stack.reverse();
    while let Some(node) = stack.pop() {
        if language == SourceLanguage::Kotlin && node.kind() == "type_alias" {
            let alias = node.child_by_field_name("type");
            let target = node
                .children(&mut node.walk())
                .find(|child| child.kind() == "user_type");
            if let (Some(alias), Some(target)) = (alias, target)
                && let (Ok(alias), Ok(target)) = (
                    alias.utf8_text(source.as_bytes()),
                    target.utf8_text(source.as_bytes()),
                )
            {
                type_aliases.insert(alias.to_string(), target.to_string());
            }
        }
        if let Some((kind, name_node)) = declaration_shape(node, language) {
            let name = node_text(name_node, source)?;
            if !declarations.iter().any(|d: &Declaration| d.name == name) {
                let ctor_params = constructor_param_names(node, language, source);
                declarations.push(Declaration {
                    name: name.clone(),
                    package: package.map(str::to_string),
                    language,
                    kind,
                    supertypes: supertypes(node, language, source),
                    members: members(node, language, source),
                    has_default_constructor_parameter: has_default_constructor_parameter(
                        node, language,
                    ),
                    constructor_param_count: ctor_params.len(),
                    constructor_param_names: ctor_params,
                    has_non_trailing_default: has_non_trailing_default_constructor_parameter(
                        node, language,
                    ),
                    constructor_param_defaults: constructor_param_defaults(node, language, source),
                    secondary_ctors: secondary_constructor_param_types(node, language, source),
                    type_params: type_param_names(node, source),
                });
            }
            for child in node.children(&mut node.walk()) {
                stack.push(child);
            }
        } else {
            for child in node.children(&mut node.walk()) {
                stack.push(child);
            }
        }
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
    // Smart-cast sites are collected once and the property-name set is derived
    // from them, so the retention decision and the rewrite pass read the same
    // evidence. The index's own tree is reused: a second parse of every Kotlin
    // file would double indexing cost for evidence this pass already has.
    let (smart_cast_sites, bindings) = if language == SourceLanguage::Kotlin {
        (
            crate::smart_cast::sites_in(&tree, source),
            crate::smart_cast::bindings_in(&tree, source),
        )
    } else {
        (Vec::new(), HashMap::new())
    };
    let smart_cast_properties = smart_cast_sites
        .iter()
        .map(|site| site.property.clone())
        .collect();
    let mut ctor_calls = if language == SourceLanguage::Kotlin {
        collect_ctor_calls(&tree, source)
    } else {
        Vec::new()
    };
    ctor_calls.shrink_to_fit();
    Ok((
        declarations,
        type_aliases,
        identifier_counts,
        enum_entries_qualifiers,
        smart_cast_properties,
        smart_cast_sites,
        bindings,
        ctor_calls,
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

/// Parameter type texts of each Kotlin SECONDARY constructor of `node`, in
/// declaration order — one entry per `constructor(...)`; empty for a type that
/// declares none.
///
/// A call written with as many arguments as one of these resolves to that
/// constructor, so it is no evidence about the primary constructor's defaults.
/// Types are recorded, but matching is by count: the index has no subtype
/// relation, so comparing `BigDecimal` against a `Number` parameter by name
/// would reject a real secondary call and read it as unreadable again.
fn secondary_constructor_param_types(
    node: tree_sitter::Node<'_>,
    language: SourceLanguage,
    source: &str,
) -> Vec<Vec<String>> {
    if language != SourceLanguage::Kotlin {
        return Vec::new();
    }
    let Some(body) = node.child_by_field_name("body").or_else(|| {
        node.children(&mut node.walk())
            .find(|child| child.kind() == "class_body")
    }) else {
        return Vec::new();
    };
    body.children(&mut body.walk())
        .filter(|member| member.kind() == "secondary_constructor")
        .map(|constructor| {
            match constructor.child_by_field_name("parameters").or_else(|| {
                constructor
                    .children(&mut constructor.walk())
                    .find(|child| child.kind() == "function_value_parameters")
            }) {
                Some(parameters) => parameters
                    .children(&mut parameters.walk())
                    .filter(|child| child.kind() == "parameter")
                    .map(|parameter| {
                        parameter
                            .child_by_field_name("type")
                            .or_else(|| first_type_node(parameter))
                            .and_then(|node| node_text(node, source).ok())
                            .unwrap_or_default()
                    })
                    .collect(),
                None => Vec::new(),
            }
        })
        .collect()
}

/// Primary-constructor parameter names in declaration order. Used to lower
/// Kotlin named arguments positionally (`MethodCall(name = "x", params = p)`
/// has no Java form) and, via the length, as the constructor arity a call site
/// may fill omitted trailing defaults from. Only a real constructor parameter
/// counts: a companion contributes static members and a class body adds
/// properties that are not parameters (`data class X(val a: T = d) { override
/// val b: U get() = ... }` has arity 1, not 2).
fn constructor_param_names(
    node: tree_sitter::Node<'_>,
    language: SourceLanguage,
    source: &str,
) -> Vec<String> {
    if language != SourceLanguage::Kotlin {
        return java_constructor_param_names(node, source);
    }
    let Some(constructor) = node.child_by_field_name("primary_constructor").or_else(|| {
        node.children(&mut node.walk())
            .find(|child| child.kind() == "primary_constructor")
    }) else {
        return Vec::new();
    };
    let Some(parameters) = constructor
        .children(&mut constructor.walk())
        .find(|child| child.kind() == "class_parameters")
    else {
        return Vec::new();
    };
    parameters
        .named_children(&mut parameters.walk())
        .filter(|parameter| parameter.kind() == "class_parameter")
        .filter_map(|parameter| {
            if let Some(name_node) = parameter
                .named_children(&mut parameter.walk())
                .find(|child| child.kind() == "identifier")
                && let Ok(name) = node_text(name_node, source)
            {
                return Some(name.to_string());
            }
            // Fall back to the raw parameter text before the `:`/`=`.
            let text = node_text(parameter, source).ok()?;
            let name: String = text
                .trim_start_matches("override ")
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

/// A Java class's constructor parameters, from the declaration with the most of
/// them: a class emitted with delegating overloads carries one per shorter
/// arity, and the longest is the canonical constructor the others delegate to.
/// Kotlin named arguments are lowered against it. A record or a
/// Lombok-annotated class has no declaration at all — `ParameterCount` falls
/// back to the class's instance state, which the generated constructor takes in
/// declaration order.
fn java_constructor_param_names(node: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let body = node.child_by_field_name("body").unwrap_or(node);
    let mut best: Vec<String> = Vec::new();
    for member in body.named_children(&mut body.walk()) {
        if member.kind() != "constructor_declaration" {
            continue;
        }
        let Some(parameters) = member.child_by_field_name("parameters") else {
            continue;
        };
        let names: Vec<String> = parameters
            .named_children(&mut parameters.walk())
            .filter(|child| child.kind() == "formal_parameter")
            .filter_map(|parameter| parameter.child_by_field_name("name"))
            .filter_map(|name| node_text(name, source).ok())
            .collect();
        if names.len() > best.len() {
            best = names;
        }
    }
    best
}

/// Raw default text per primary-constructor parameter, in declaration order
/// (`None` where the parameter has no default). Kotlin only: a Java source states
/// no defaults anywhere, so there is nothing for a call site to inline.
///
/// The extraction lives with the repair pass
/// (`crate::ctor_defaults::class_param_default_texts`), which needs the same text
/// to decide what an omitted argument can be replaced with.
fn constructor_param_defaults(
    node: tree_sitter::Node<'_>,
    language: SourceLanguage,
    source: &str,
) -> Vec<Option<String>> {
    if language != SourceLanguage::Kotlin {
        return Vec::new();
    }
    crate::ctor_defaults::class_param_default_texts(node, source)
}

/// Constructor calls this file makes, in the compact form the planner reads.
///
/// The detection rule lives with the repair pass
/// (`crate::ctor_defaults::call_sites`) so the index and the rewrite never
/// disagree about what a call shape is: a shape the index missed would leave a
/// caller with no constructor to land on.
fn collect_ctor_calls(tree: &tree_sitter::Tree, source: &str) -> Vec<CtorCall> {
    crate::ctor_defaults::call_sites_in(tree, source)
        .into_iter()
        .map(|site| CtorCall {
            callee: site.callee,
            positional: site.args.iter().filter(|arg| arg.name.is_none()).count(),
            named: site
                .args
                .iter()
                .filter_map(|arg| arg.name.clone())
                .collect(),
            unknown: site.unknown,
            line: site.line,
        })
        .collect()
}

fn has_non_trailing_default_constructor_parameter(
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
    let defaulted: Vec<bool> = parameters
        .named_children(&mut parameters.walk())
        .filter(|parameter| parameter.kind() == "class_parameter")
        .map(|parameter| {
            parameter
                .children(&mut parameter.walk())
                .any(|child| child.kind() == "=")
        })
        .collect();
    // A default is unexpressible in Java only when a later parameter has none:
    // trailing defaults become delegating overloads, middle ones cannot.
    defaulted
        .iter()
        .enumerate()
        .any(|(index, has_default)| *has_default && defaulted[index + 1..].iter().any(|d| !d))
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
                let is_annotation = node
                    .children(&mut node.walk())
                    .any(|child| child.kind() == "modifiers")
                    && {
                        let mods = node
                            .children(&mut node.walk())
                            .find(|child| child.kind() == "modifiers");
                        mods.map(|m| {
                            m.children(&mut m.walk()).any(|sub| {
                                sub.kind() == "class_modifier" && {
                                    sub.children(&mut sub.walk())
                                        .any(|kw| kw.kind() == "annotation")
                                }
                            })
                        })
                        .unwrap_or(false)
                    };
                (
                    if is_annotation {
                        // `annotation class` — Kotlin annotation TYPE.
                        DeclarationKind::Annotation
                    } else if is_interface {
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
        // A Java `@interface` is an annotation TYPE. Without it the index has
        // no `Annotation` declaration for annotations that were always Java,
        // and rules that key on one (e.g. the `Default` constructor marker
        // MapStruct consumes) silently find nothing.
        (SourceLanguage::Java, "annotation_type_declaration") => node
            .child_by_field_name("name")
            .map(|name| (DeclarationKind::Annotation, name)),
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
    // A Java record carries its state in the header, not the body: each component
    // is a final field whose accessor is named exactly like it. Translating the
    // owner of a Kotlin property turns the read into that accessor call, so the
    // index has to see the components or the owner looks property-less.
    if language == SourceLanguage::Java
        && node.kind() == "record_declaration"
        && let Some(parameters) = node.child_by_field_name("parameters")
    {
        for parameter in parameters
            .named_children(&mut parameters.walk())
            .filter(|parameter| parameter.kind() == "formal_parameter")
        {
            let Some(name_node) = parameter.child_by_field_name("name") else {
                continue;
            };
            let type_node = parameter.child_by_field_name("type");
            result.push(Member {
                name: node_text(name_node, source).unwrap_or_default(),
                kind: MemberKind::Field,
                visibility: None,
                is_static: false,
                is_jvm_field: false,
                type_name: type_node.and_then(|node| node_text(node, source).ok()),
                parameter_types: Vec::new(),
                is_nullable: false,
            });
        }
    }
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
                .child_by_field_name("type")
                .or_else(|| first_type_node(parameter));
            result.push(Member {
                name: node_text(name_node, source).unwrap_or_default(),
                kind: MemberKind::Property,
                visibility: None,
                is_static: false,
                is_jvm_field: false,
                type_name: type_node.and_then(|node| node_text(node, source).ok()),
                parameter_types: Vec::new(),
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

fn member_parameter_types(
    node: tree_sitter::Node<'_>,
    language: SourceLanguage,
    source: &str,
) -> Vec<String> {
    let parameter_container = match language {
        SourceLanguage::Kotlin => node
            .children(&mut node.walk())
            .find(|child| child.kind() == "function_value_parameters"),
        SourceLanguage::Java => node.child_by_field_name("parameters").or_else(|| {
            node.children(&mut node.walk())
                .find(|child| child.kind() == "formal_parameters")
        }),
    };
    let Some(parameter_container) = parameter_container else {
        return Vec::new();
    };
    parameter_container
        .named_children(&mut parameter_container.walk())
        .filter(|parameter| {
            matches!(
                (language, parameter.kind()),
                (SourceLanguage::Kotlin, "parameter")
                    | (SourceLanguage::Java, "formal_parameter")
                    | (SourceLanguage::Java, "spread_parameter")
            )
        })
        .filter_map(|parameter| {
            parameter
                .child_by_field_name("type")
                .or_else(|| first_type_node(parameter))
                .and_then(|ty| node_text(ty, source).ok())
                .map(|ty| ty.trim().to_string())
        })
        .collect()
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
    // A Kotlin `secondary_constructor` has no name node — it takes the class's
    // name. Only the fallback below turns that into `<init>`, but the member
    // itself must still be recorded: "does this type declare another
    // constructor" is what decides whether an unreadable call shape is
    // explainable, and what keeps a delegating overload from duplicating the
    // secondary's signature. Reading a missing name as "not a member" silently
    // dropped every Kotlin secondary constructor from the index.
    let name = name_node
        .map(|node| node_text(node, source))
        .transpose()
        .ok()
        .flatten()
        .unwrap_or_default();
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
    let is_static = modifiers.split_whitespace().any(|word| word == "static");
    let is_jvm_field = modifiers.contains("JvmField");
    Some(Member {
        name: if name.is_empty() {
            "<init>".to_string()
        } else {
            name
        },
        kind,
        visibility,
        is_static,
        is_jvm_field,
        type_name,
        parameter_types: member_parameter_types(node, language, source),
        is_nullable: type_node.is_some_and(|node| node.kind() == "nullable_type"),
    })
}

/// The JVM getter Kotlin generates for a property: `id` -> `getId`. A property
/// whose own name already reads `isX` keeps it (`val isActive: Boolean` has
/// `isActive()`, not `getIsActive()`), which is why this cannot be a plain
/// `format!("get{}", capitalize(name))`.
pub fn property_accessor_name(property: &str) -> String {
    if let Some(rest) = property.strip_prefix("is")
        && rest.chars().next().is_some_and(char::is_uppercase)
    {
        return property.to_string();
    }
    let mut chars = property.chars();
    match chars.next() {
        Some(first) => format!("get{}{}", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

/// First type node of a member declaration, for the layouts where the type
/// carries no `type` field. Annotations are NOT types: `@JsonIgnore val id:
/// String` holds a `user_type` of its own inside the annotation, and indexing
/// that made every annotated member look like it declared the annotation's
/// type — which then read as an ABI conflict against its supertype.
fn first_type_node(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    if matches!(node.kind(), "nullable_type" | "user_type") {
        return Some(node);
    }
    if matches!(node.kind(), "annotation" | "modifiers") {
        return None;
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

fn is_java_compatible_narrow(
    index: &SourceIndex,
    source_file: &SourceFile,
    super_ty: &str,
    own_ty: &str,
) -> bool {
    let super_ty = super_ty.trim().trim_end_matches('?').trim();
    let own_ty = own_ty.trim().trim_end_matches('?').trim();
    if super_ty == own_ty || super_ty == "Object" || super_ty == "Any" {
        return true;
    }
    let (super_base, super_args) = split_generic(super_ty);
    let (own_base, own_args) = split_generic(own_ty);
    if super_base != own_base {
        let Some(own_decl) = index.resolve_kotlin_type_public(source_file, own_ty) else {
            return true;
        };
        return member_supertype_closure_contains(index, source_file, own_decl, super_base);
    }
    match (super_args, own_args) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(super_args), Some(own_args)) => {
            super_args.len() == own_args.len()
                && super_args
                    .iter()
                    .zip(own_args.iter())
                    .all(|(super_arg, own_arg)| super_arg == own_arg)
        }
    }
}

fn classify_member_conflict(
    index: &SourceIndex,
    source_file: &SourceFile,
    super_decl: &Declaration,
    super_ty: &str,
    own_ty: &str,
) -> MemberConflictClass {
    let super_ty = super_ty.trim().trim_end_matches('?').trim();
    let own_ty = own_ty.trim().trim_end_matches('?').trim();
    if super_ty == own_ty {
        return MemberConflictClass::Exact;
    }
    if super_decl.type_params.iter().any(|param| param == super_ty) {
        return MemberConflictClass::SupertypeTypeParameter;
    }

    let (super_base, super_args) = split_generic(super_ty);
    let (own_base, own_args) = split_generic(own_ty);
    if super_base == own_base {
        return match (super_args, own_args) {
            (Some(super_args), Some(own_args))
                if super_args.len() != own_args.len()
                    || super_args
                        .iter()
                        .zip(own_args.iter())
                        .any(|(super_arg, own_arg)| super_arg != own_arg) =>
            {
                MemberConflictClass::InvariantGenericConflict
            }
            (Some(_), None) => MemberConflictClass::InvariantGenericConflict,
            _ => MemberConflictClass::Exact,
        };
    }
    if super_base == "Object" || super_base == "Any" {
        return MemberConflictClass::JavaCovariantReturn;
    }

    let Some(own_decl) = index.resolve_kotlin_type_public(source_file, own_ty) else {
        return MemberConflictClass::UnknownType;
    };
    if member_supertype_closure_contains(index, source_file, own_decl, super_base) {
        MemberConflictClass::JavaCovariantReturn
    } else {
        MemberConflictClass::UnrelatedReturnTypes
    }
}

fn member_supertype_closure_contains(
    index: &SourceIndex,
    source_file: &SourceFile,
    declaration: &Declaration,
    target_base: &str,
) -> bool {
    let mut visited = HashSet::new();
    let mut pending = vec![declaration];
    while let Some(current) = pending.pop() {
        if !visited.insert(declaration_key(current)) {
            continue;
        }
        let current_file = index
            .declaration_source_file(current)
            .unwrap_or(source_file);
        for supertype in &current.supertypes {
            let base = supertype
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
                .unwrap_or("")
                .trim();
            if base == target_base {
                return true;
            }
            if let Some(next) = index.resolve_type(current_file, supertype) {
                pending.push(next);
            }
        }
    }
    false
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
