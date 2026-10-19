use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

mod default_getters;
pub(crate) mod header_facts;
pub(crate) mod profile;

// 5: `Declaration::constructor_param_count` (primary-constructor arity, not a
// property count — companion members and body properties both inflate that).
// 8: `SourceFile::smart_cast_sites` / `SourceFile::bindings` — the retained-Kotlin
// smart-cast boundary now carries the site shapes and the file's own name->type
// table instead of a bare property-name set.
// 22: Java generic declaration type parameters recognize tree-sitter-java's
// `type_identifier` nodes, so cached declarations must refresh `type_params`.
// 23: cached top-level Kotlin ownership-boundary counts are now part of each
// source snapshot.
// 25: callable members carry a source-level arity contract used to rule out
// same-named overloads that cannot collide with a property bridge.
// 26: constructor-property visibility is indexed for inherited setter proofs.
// 28: value-class identity excludes unboxed getters from reference covariance.
// 29: method-generic and inline-bound header facts reuse the indexing parse.
const CACHE_VERSION: u32 = 30;
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

/// A source edit that makes one retained Kotlin read-only `List` contract
/// covariant in its Java ABI. Requirements for every conflicting parent are
/// returned together so callers can apply or discard the group atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JvmWildcardContractRequirement {
    pub contract: crate::semantics::SymbolId,
    pub implementation: crate::semantics::SymbolId,
    pub member_name: String,
    pub location: crate::semantics::SourceLocation,
    pub replacement: String,
    /// Existing exact fully-qualified annotation means the source edit is
    /// already satisfied, but the requirement remains useful for ABI proof.
    pub already_present: bool,
}

impl MemberConflict {
    pub fn blocks_java_override(&self) -> bool {
        self.legacy_mismatch
    }

    pub fn detail(&self) -> String {
        format!(
            "`{}`: inherited `{}` from `{}`, implementation `{}` ({:?})",
            self.member_name,
            self.inherited_type,
            self.supertype,
            self.implementation_type,
            self.classification
        )
    }
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

/// The source-call arities accepted by a method declaration.
///
/// `fixed` is the number of non-vararg parameters. `required` counts those
/// parameters without defaults; Kotlin callers may omit the rest, and a
/// trailing vararg may consume any additional arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallableArity {
    pub required: usize,
    pub fixed: usize,
    pub vararg: bool,
}

impl CallableArity {
    pub fn accepts_arity(self, arity: usize) -> bool {
        arity >= self.required && (self.vararg || arity <= self.fixed)
    }
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
    /// Kotlin `var` property. Java interface setters are part of the ABI and
    /// must be bridged separately when retained Kotlin implements that ABI.
    #[serde(default)]
    pub is_mutable: bool,
    /// Property annotations outside the getter/setter annotation targets that
    /// the retained-property ABI repair can preserve.
    #[serde(default)]
    pub has_unsupported_property_annotations: bool,
    /// Kotlin property forms incompatible with the field bridge (`open`,
    /// `lateinit`, delegated, or abstract properties).
    #[serde(default)]
    pub has_unsupported_property_shape: bool,
    /// Kotlin primary-constructor `val`/`var` property. Its constructor
    /// initializer is not a custom accessor and remains field-bridgeable.
    #[serde(default)]
    pub is_constructor_property: bool,
    /// A Kotlin property with an explicit `get` or `set` accessor. Keep this
    /// separate from `has_body`: an initializer is stored state and remains
    /// bridgeable in all-open/JPA classes, while a custom accessor is virtual.
    #[serde(default)]
    pub has_custom_accessor: bool,
    /// An explicitly typed, annotation-free readonly getter with no initializer,
    /// delegate or backing-field access. It can retain its JVM method body
    /// without introducing a storage property when a Java contract owns it.
    #[serde(default)]
    pub has_plain_computed_getter: bool,
    /// The property's getter begins on the declaration line. The property ABI
    /// rewriter only handles accessor blocks on following lines, and this bit
    /// must come from the indexed (possibly speculative) source rather than a
    /// stale file on disk.
    #[serde(default)]
    pub has_inline_getter: bool,
    pub type_name: Option<String>,
    #[serde(default)]
    pub parameter_types: Vec<String>,
    /// Source-level argument counts accepted by a parsed method signature.
    /// `None` means the signature shape is unsupported or could not be read.
    #[serde(default)]
    pub callable_arity: Option<CallableArity>,
    pub is_nullable: bool,
    /// A method body or property getter implementation, rather than an
    /// abstract declaration. Used to detect Java default-method obligations.
    pub has_body: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    pub name: String,
    pub package: Option<String>,
    pub language: SourceLanguage,
    pub kind: DeclarationKind,
    #[serde(default)]
    pub is_abstract: bool,
    #[serde(default)]
    pub is_jvm_value_class: bool,
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

#[derive(Debug, Clone)]
pub struct DefaultGetterConflict {
    pub provider: crate::semantics::SymbolId,
    pub implementation: crate::semantics::SymbolId,
    pub competing_owner: crate::semantics::SymbolId,
    pub property: String,
    pub provider_type: Option<String>,
    pub competing_type: Option<String>,
}

impl DefaultGetterConflict {
    pub fn message(&self) -> String {
        let shape = match (&self.provider_type, &self.competing_type) {
            (Some(narrow), Some(wide)) => format!("`{narrow}` and `{wide}`"),
            _ => "an unresolved return type".to_string(),
        };
        format!(
            "default property `{}` has competing inherited getter types {shape} on a retained Kotlin descendant `{}` (contract `{}`)",
            self.property, self.implementation.name, self.competing_owner.name,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: PathBuf,
    pub language: SourceLanguage,
    /// Exact indexed text, including speculative overlays. Analyses that
    /// participate in migration convergence must never fall back to stale
    /// filesystem contents after an in-memory rewrite.
    #[serde(default)]
    source_text: String,
    #[serde(default)]
    header_facts: header_facts::HeaderFacts,
    /// Top-level Kotlin ownership boundaries counted from the parser tree.
    /// Java files always report zero. Stored in the workspace cache so display
    /// statistics do not need to parse Kotlin source again.
    #[serde(default)]
    kotlin_declaration_count: usize,
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
    /// Locals whose source type is inferred through a method-return expression.
    /// Resolved only after the complete workspace index exists.
    #[serde(default)]
    inferred_binding_calls: Vec<crate::smart_cast::InferredBindingCall>,
    /// Constructor calls this file makes, as written. A declaration with a
    /// middle default is only retained when a caller really omits one in a
    /// shape the emitter cannot serve, so the shapes have to be recorded where
    /// they are written rather than guessed from a name mention.
    #[serde(default)]
    ctor_calls: Vec<CtorCall>,
}

impl SourceFile {
    pub fn source_text(&self) -> &str {
        &self.source_text
    }

    pub(crate) fn inline_type_parameter_upper_bound(
        &self,
        owner: &Declaration,
        parameter: &str,
    ) -> Option<String> {
        self.header_facts
            .inline_bound(&owner.name, owner.kind, parameter)
            .map(str::to_owned)
    }

    /// Number of top-level Kotlin declarations that translation plans own.
    /// Class members and nested declarations belong to their containing owner.
    pub fn kotlin_declaration_count(&self) -> usize {
        if self.language == SourceLanguage::Kotlin {
            self.kotlin_declaration_count
        } else {
            0
        }
    }

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
    /// Accepted generated-member facts, never inferred from arbitrary Java text.
    generated_getters: Vec<crate::lombok_getter_contracts::GeneratedGetterContract>,
    /// Java paths with an explicitly enabled, snapshot-checked default Lombok profile.
    default_lombok_accessors: HashSet<PathBuf>,
    /// Ownership-independent getter bridge facts, scoped to this immutable index snapshot.
    getter_bridge_cache: default_getters::BridgeCache,
    /// Symbols are populated together for a file on its first retention query.
    /// Per-file cells avoid serializing independent Rayon planning workers.
    declaration_symbols: Vec<std::sync::OnceLock<HashMap<usize, crate::semantics::SymbolId>>>,
    kotlin_subtypes: HashMap<String, Vec<PathBuf>>,
    /// Reverse subtype edges by SIMPLE name: `name -> [subtype simple names]`.
    /// Shallow but unambiguous enough for retention fixpointing: the
    /// fixpoint pre-pass taints a declaration when any of its simple-name
    /// subtypes is retained, and simple-name collisions over-taint
    /// conservatively (fewer translations, never a wrong Java ABI).
    subtype_names: HashMap<String, Vec<String>>,
    /// Reverse subtype edges for both Kotlin and Java. ABI bridge planning
    /// needs exact descendants without adding Java names to the Kotlin
    /// retention fixpoint graph.
    all_subtype_names: HashMap<String, Vec<String>>,
    unresolved_supertype_names: HashSet<String>,
    /// Simple declaration-name lookup preserving every collision. This avoids
    /// repeated complete-workspace scans for name-based compatibility checks.
    declaration_names: HashMap<String, Vec<(usize, usize)>>,
    /// Alias candidates by short name, for scoped identity checks without
    /// rescanning every source file at each inheritance edge.
    alias_names: HashMap<String, Vec<usize>>,
    /// Fully-qualified declaration lookup. Type resolution is on the hot path
    /// while rebuilding speculative indexes, so avoid scanning every
    /// declaration for each supertype edge.
    qualified_declarations: HashMap<String, Vec<(usize, usize)>>,
    /// Member-name lookup preserving workspace declaration/member order.
    /// Expression lowering often asks for one member's type; this avoids a
    /// full declaration/member scan for each such query.
    member_names: HashMap<String, Vec<(usize, usize, usize)>>,
    /// Declaration address to its owning file. Declarations live in the boxed
    /// buffers owned by `files`, so these addresses remain stable for the
    /// lifetime of an index.
    declaration_files: HashMap<usize, usize>,
    /// Exact reverse subtype edges keyed by the indexed target declaration's
    /// address. Descendants are ordered by simple name, then file and
    /// declaration order to match `direct_subtypes`' former name-based walk.
    direct_subtype_declarations: HashMap<usize, Vec<(usize, usize)>>,
    /// Constructor calls by callee simple name: `name -> [(file, call)]`.
    /// Derived from the files' recorded calls whenever the index is built (never
    /// cached): a declaration's omission evidence is then one lookup instead of a
    /// workspace scan per declaration.
    ctor_uses: HashMap<String, Vec<(usize, usize)>>,
    /// Exact indexed source paths. `source_file` retains its `paths_match`
    /// fallback for equivalent-but-not-identical caller paths.
    file_paths: HashMap<PathBuf, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumDefaultBridge {
    pub method_name: String,
    pub return_type: String,
    pub provider: String,
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

fn changed_bridge_dependencies(
    old_files: &[SourceFile],
    new_files: &[SourceFile],
) -> (HashSet<String>, HashSet<String>) {
    let old_by_path = old_files
        .iter()
        .map(|file| (normalized_path(&file.path), file))
        .collect::<HashMap<_, _>>();
    let new_by_path = new_files
        .iter()
        .map(|file| (normalized_path(&file.path), file))
        .collect::<HashMap<_, _>>();
    let paths = old_by_path
        .keys()
        .chain(new_by_path.keys())
        .cloned()
        .collect::<HashSet<_>>();
    let mut changed_files = HashSet::new();
    let mut changed_names = HashSet::new();
    for path in paths {
        let old = old_by_path.get(&path).copied();
        let new = new_by_path.get(&path).copied();
        let changed = old.is_some_and(|left| {
            new.is_none_or(|right| {
                left.language != right.language || left.source_text != right.source_text
            })
        }) || (old.is_none() && new.is_some());
        if changed {
            changed_files.insert(path);
            let old_names = old.map_or_else(HashMap::new, namespace_name_counts);
            let new_names = new.map_or_else(HashMap::new, namespace_name_counts);
            for name in old_names.keys().chain(new_names.keys()) {
                if old_names.get(name) != new_names.get(name) {
                    changed_names.insert(name.1.clone());
                }
            }
        }
    }
    (changed_files, changed_names)
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum BridgeNamespaceKind {
    DeclarationSimple,
    DeclarationQualified,
    AliasSimple,
    AliasQualified,
    MemberSimple,
}

fn namespace_name_counts(file: &SourceFile) -> HashMap<(BridgeNamespaceKind, String), usize> {
    fn add(
        counts: &mut HashMap<(BridgeNamespaceKind, String), usize>,
        kind: BridgeNamespaceKind,
        name: &str,
    ) {
        *counts.entry((kind, name.to_owned())).or_default() += 1;
    }

    let mut counts = HashMap::new();
    for declaration in &file.declarations {
        add(
            &mut counts,
            BridgeNamespaceKind::DeclarationSimple,
            &declaration.name,
        );
        let qualified = declaration_key(declaration);
        if qualified != declaration.name {
            add(
                &mut counts,
                BridgeNamespaceKind::DeclarationQualified,
                &qualified,
            );
        }
        for member in &declaration.members {
            add(&mut counts, BridgeNamespaceKind::MemberSimple, &member.name);
        }
    }
    for alias in file.type_aliases.keys() {
        add(&mut counts, BridgeNamespaceKind::AliasSimple, alias);
        if let Some(package) = &file.package {
            add(
                &mut counts,
                BridgeNamespaceKind::AliasQualified,
                &format!("{package}.{alias}"),
            );
        }
    }
    counts
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
    pub(crate) fn set_default_lombok_accessors(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
    ) {
        let enabled = paths
            .into_iter()
            .map(|path| normalized_path(&path))
            .collect::<HashSet<_>>();
        self.default_lombok_accessors = self
            .java_files()
            .filter(|file| enabled.contains(&normalized_path(&file.path)))
            .map(|file| file.path.clone())
            .collect();
    }

    pub(crate) fn has_default_lombok_accessors(&self, path: &Path) -> bool {
        self.default_lombok_accessors.contains(path)
    }

    pub(crate) fn generated_getters(
        &self,
    ) -> &[crate::lombok_getter_contracts::GeneratedGetterContract] {
        &self.generated_getters
    }

    pub(crate) fn set_generated_getters(
        &mut self,
        getters: impl IntoIterator<Item = crate::lombok_getter_contracts::GeneratedGetterContract>,
    ) {
        self.generated_getters.clear();
        let getters = getters.into_iter().collect::<Vec<_>>();
        if getters.is_empty() {
            return;
        }
        let files = self
            .files
            .iter()
            .filter(|file| file.language == SourceLanguage::Java)
            .map(|file| (normalized_path(&file.path), file))
            .collect::<HashMap<_, _>>();
        let mut hashes = HashMap::new();
        let mut contracts_by_target = HashMap::<_, Vec<usize>>::new();
        for mut getter in getters {
            let Some(file) = files.get(&normalized_path(&getter.output_path)) else {
                continue;
            };
            let hash = hashes
                .entry(&file.path)
                .or_insert_with(|| *blake3::hash(file.source_text().as_bytes()).as_bytes());
            if *hash != getter.output_hash {
                continue;
            }
            getter.output_path = file.path.clone();
            let matching = contracts_by_target
                .entry((getter.output_path.clone(), getter.target.clone()))
                .or_default();
            if !matching
                .iter()
                .any(|&index| self.generated_getters[index] == getter)
            {
                matching.push(self.generated_getters.len());
                self.generated_getters.push(getter);
            }
        }
    }

    pub(crate) fn cached_declaration_symbol(
        &self,
        declaration: &Declaration,
    ) -> Option<crate::semantics::SymbolId> {
        let address = declaration as *const Declaration as usize;
        let file_index = self.declaration_files.get(&address).copied();
        file_index
            .and_then(|file_index| {
                self.declaration_symbols
                    .get(file_index)
                    .map(|cell| (file_index, cell))
            })
            .and_then(|(file_index, cell)| {
                cell.get_or_init(|| {
                    let file = &self.files[file_index];
                    let ids = crate::semantics::workspace_symbols_for_file(file);
                    file.declarations
                        .iter()
                        .zip(ids)
                        .map(|(declaration, symbol)| {
                            (declaration as *const Declaration as usize, symbol)
                        })
                        .collect()
                })
                .get(&address)
                .cloned()
            })
    }

    pub fn declaration_retained(
        &self,
        declaration: &Declaration,
        retained: &HashSet<crate::semantics::SymbolId>,
    ) -> bool {
        let symbol = crate::semantics::workspace_symbol(self, declaration);
        self.symbol_retained(&symbol, retained)
    }

    /// Nested types share their enclosing top-level declaration's ownership.
    /// Record that boundary read as well as the exact symbol read so cached
    /// preparation is revisited when either ownership decision changes.
    pub(crate) fn symbol_retained(
        &self,
        symbol: &crate::semantics::SymbolId,
        retained: &HashSet<crate::semantics::SymbolId>,
    ) -> bool {
        if crate::transpiler::retention_queries::contains(retained, symbol)
            || symbol.kind.starts_with("ambiguous:")
        {
            return true;
        }
        let Some(outer_name) = symbol.owner_path.first() else {
            return false;
        };
        let Some(file) = self.source_file(&symbol.file) else {
            return true;
        };
        let mut boundaries = file.declarations.iter().filter_map(|declaration| {
            let boundary = crate::semantics::workspace_symbol(self, declaration);
            (boundary.owner_path.is_empty() && &boundary.name == outer_name).then_some(boundary)
        });
        let Some(boundary) = boundaries.next() else {
            return true;
        };
        boundaries.next().is_some()
            || crate::transpiler::retention_queries::contains(retained, &boundary)
    }
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
        let profile_started = profile::enabled().then(std::time::Instant::now);
        let profiled_files = profile_started
            .as_ref()
            .map(|_| files.len())
            .unwrap_or_default();
        let profiled_declarations = profile_started
            .as_ref()
            .map(|_| {
                files
                    .iter()
                    .map(|file| file.declarations.len())
                    .sum::<usize>()
            })
            .unwrap_or_default();
        let mut profiled_supertype_edges = 0usize;
        let mut profiled_resolved_edges = 0usize;
        let mut profiled_unresolved_edges = 0usize;
        let declaration_symbols = std::iter::repeat_with(std::sync::OnceLock::new)
            .take(files.len())
            .collect();
        let mut index = Self {
            files,
            generated_getters: Vec::new(),
            default_lombok_accessors: HashSet::new(),
            getter_bridge_cache: default_getters::BridgeCache::default(),
            declaration_symbols,
            kotlin_subtypes: HashMap::new(),
            subtype_names: HashMap::new(),
            all_subtype_names: HashMap::new(),
            unresolved_supertype_names: HashSet::new(),
            declaration_names: HashMap::new(),
            alias_names: HashMap::new(),
            qualified_declarations: HashMap::new(),
            member_names: HashMap::new(),
            declaration_files: HashMap::new(),
            direct_subtype_declarations: HashMap::new(),
            ctor_uses: HashMap::new(),
            file_paths: HashMap::new(),
        };
        for (file_index, file) in index.files.iter().enumerate() {
            for name in file.type_aliases.keys() {
                index
                    .alias_names
                    .entry(name.clone())
                    .or_default()
                    .push(file_index);
            }
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
                    .declaration_files
                    .insert(declaration as *const Declaration as usize, file_index);
                index
                    .declaration_names
                    .entry(declaration.name.clone())
                    .or_default()
                    .push((file_index, declaration_index));
                index
                    .qualified_declarations
                    .entry(declaration_key(declaration))
                    .or_default()
                    .push((file_index, declaration_index));
                for (member_index, member) in declaration.members.iter().enumerate() {
                    index
                        .member_names
                        .entry(member.name.clone())
                        .or_default()
                        .push((file_index, declaration_index, member_index));
                }
            }
        }
        let lookup_index_elapsed = profile_started.as_ref().map(std::time::Instant::elapsed);
        let hierarchy_started = profile::enabled().then(std::time::Instant::now);
        let mut subtype_edges = Vec::new();
        let mut subtype_name_edges = Vec::new();
        let mut all_subtype_name_edges = Vec::new();
        let mut direct_subtype_edges = Vec::new();
        let mut unresolved_supertype_names = Vec::new();
        for (file_index, file) in index.files.iter().enumerate() {
            for (declaration_index, declaration) in file.declarations.iter().enumerate() {
                let mut resolved_supers = HashSet::new();
                for supertype in &declaration.supertypes {
                    if profile_started.is_some() {
                        profiled_supertype_edges += 1;
                    }
                    if let Some(target) = index.resolve_type(file, supertype) {
                        if profile_started.is_some() {
                            profiled_resolved_edges += 1;
                        }
                        let target_address = target as *const Declaration as usize;
                        if resolved_supers.insert(target_address) {
                            direct_subtype_edges.push((
                                target_address,
                                file_index,
                                declaration_index,
                            ));
                        }
                        if file.language == SourceLanguage::Kotlin {
                            subtype_edges.push((declaration_key(target), file.path.clone()));
                            subtype_name_edges
                                .push((declaration_key(target), declaration.name.clone()));
                        }
                        all_subtype_name_edges
                            .push((declaration_key(target), declaration.name.clone()));
                    } else {
                        if profile_started.is_some() {
                            profiled_unresolved_edges += 1;
                        }
                        let raw = supertype
                            .split(['<', '('])
                            .next()
                            .unwrap_or(supertype)
                            .trim();
                        if !raw.is_empty() {
                            unresolved_supertype_names.push(raw.to_string());
                        }
                    }
                }
            }
        }
        for (target, file_index, declaration_index) in direct_subtype_edges {
            index
                .direct_subtype_declarations
                .entry(target)
                .or_default()
                .push((file_index, declaration_index));
        }
        for descendants in index.direct_subtype_declarations.values_mut() {
            descendants.sort_by(|(left_file, left_decl), (right_file, right_decl)| {
                index.files[*left_file].declarations[*left_decl]
                    .name
                    .cmp(&index.files[*right_file].declarations[*right_decl].name)
                    .then(left_file.cmp(right_file))
                    .then(left_decl.cmp(right_decl))
            });
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
        for (target, subtype_name) in all_subtype_name_edges {
            let target_name = target.rsplit_once('.').map(|(_, n)| n).unwrap_or(&target);
            index
                .all_subtype_names
                .entry(target_name.to_string())
                .or_default()
                .push(subtype_name);
        }
        index
            .unresolved_supertype_names
            .extend(unresolved_supertype_names);
        if let (Some(total_started), Some(hierarchy_started), Some(lookup_index_elapsed)) =
            (profile_started, hierarchy_started, lookup_index_elapsed)
        {
            eprintln!(
                "NOTLIN_PROFILE source index: files={profiled_files} declarations={profiled_declarations} members={} lookup={:.3}s hierarchy={:.3}s edges={profiled_supertype_edges} resolved={profiled_resolved_edges} unresolved={profiled_unresolved_edges} total={:.3}s",
                index.member_names.values().map(Vec::len).sum::<usize>(),
                lookup_index_elapsed.as_secs_f64(),
                hierarchy_started.elapsed().as_secs_f64(),
                total_started.elapsed().as_secs_f64(),
            );
        }
        index
    }

    pub fn with_overlays(&self, overlays: &[SourceOverlay]) -> Result<Self, String> {
        self.with_overlays_using_cache_from(overlays, self)
    }

    /// Build this snapshot from `self` while carrying only getter facts that
    /// remain valid relative to `cache_source`. The source index is always
    /// assembled from `self`; `cache_source` contributes cache entries only.
    pub(crate) fn with_overlays_using_cache_from(
        &self,
        overlays: &[SourceOverlay],
        cache_source: &SourceIndex,
    ) -> Result<Self, String> {
        let overlay_started = profile::enabled().then(std::time::Instant::now);
        let mut by_path = HashMap::with_capacity(overlays.len());
        for overlay in overlays {
            let path = match overlay {
                SourceOverlay::Delete { path } | SourceOverlay::Replace { path, .. } => path,
            };
            by_path.insert(normalized_path(&absolute_source_path(path)), overlay);
        }
        let mut files = Vec::with_capacity(self.files.len() + overlays.len());
        let (reused_files, replacement_count, deletion_count) = if overlay_started.is_some() {
            let reused = self
                .files
                .iter()
                .filter(|file| !by_path.contains_key(&normalized_path(&file.path)))
                .count();
            let replacements = by_path
                .values()
                .filter(|overlay| matches!(overlay, SourceOverlay::Replace { .. }))
                .count();
            (
                reused,
                replacements,
                by_path.len().saturating_sub(replacements),
            )
        } else {
            (0, 0, 0)
        };
        files.extend(
            self.files
                .iter()
                .filter(|file| !by_path.contains_key(&normalized_path(&file.path)))
                .cloned(),
        );
        for overlay in by_path.into_values() {
            match overlay {
                SourceOverlay::Delete { .. } => {}
                SourceOverlay::Replace {
                    path,
                    language,
                    source,
                } => {
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
                        inferred_binding_calls,
                        ctor_calls,
                        kotlin_declaration_count,
                        header_facts,
                    ) = parse_declarations(source, *language, package.as_deref())?;
                    files.push(SourceFile {
                        path: absolute_source_path(path),
                        language: *language,
                        source_text: source.clone(),
                        header_facts,
                        kotlin_declaration_count,
                        package,
                        imports,
                        declarations,
                        type_aliases,
                        identifier_counts,
                        enum_entries_qualifiers,
                        smart_cast_properties,
                        smart_cast_sites,
                        bindings,
                        inferred_binding_calls,
                        ctor_calls,
                    });
                }
            }
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        if let Some(started) = overlay_started {
            eprintln!(
                "NOTLIN_PROFILE source overlay: replacements={replacement_count} deletions={deletion_count} reused={reused_files} assembly={:.3}s",
                started.elapsed().as_secs_f64(),
            );
        }
        let mut index = Self::from_files(files);
        index.set_generated_getters(cache_source.generated_getters.iter().cloned());
        index.set_default_lombok_accessors(cache_source.default_lombok_accessors.iter().cloned());
        let (changed_files, changed_names) =
            changed_bridge_dependencies(&cache_source.files, &index.files);
        let (reused, invalidated) = index.getter_bridge_cache.carry_compatible_entries(
            &cache_source.getter_bridge_cache,
            &index,
            &changed_files,
            &changed_names,
        );
        profile::getter_cache_overlay(reused, invalidated);
        Ok(index)
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
        default_getters::record_source_path(path);
        if let Some(&file_index) = self.file_paths.get(path) {
            let file = self.files.get(file_index);
            if let Some(file) = file {
                default_getters::record_source_path(&file.path);
            }
            return file;
        }
        if let Ok(canonical) = fs::canonicalize(path) {
            if let Some(&file_index) = self.file_paths.get(&canonical) {
                let file = self.files.get(file_index);
                if let Some(file) = file {
                    default_getters::record_source_path(&file.path);
                }
                return file;
            }
            if let Some(file) = self
                .files
                .iter()
                .find(|file| paths_match(&file.path, &canonical))
            {
                default_getters::record_source_path(&file.path);
                return Some(file);
            }
        }
        let file = self.files.iter().find(|file| paths_match(&file.path, path));
        if let Some(file) = file {
            default_getters::record_source_path(&file.path);
        }
        file
    }

    /// Whether a resolved Java annotation may be written on `ElementType.FIELD`
    /// or `ElementType.PARAMETER`. `None` means the annotation is unresolved,
    /// Kotlin-defined, or its Java declaration could not be inspected.
    pub fn java_annotation_allows(
        &self,
        context: &SourceFile,
        annotation: &str,
        element_type: &str,
    ) -> Option<bool> {
        let declaration = self.resolve_type(context, annotation)?;
        if declaration.kind != DeclarationKind::Annotation
            || declaration.language != SourceLanguage::Java
        {
            return None;
        }
        let file = self.declaration_source_file(declaration)?;
        java_annotation_target(file.source_text(), &declaration.name, element_type)
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
        retained: &HashSet<crate::semantics::SymbolId>,
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
        retained: &HashSet<crate::semantics::SymbolId>,
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
                    .filter(|decl| self.declaration_retained(decl, retained))
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

    /// `name` is referenced by Kotlin that survives the run, so the referenced
    /// ABI must survive with it. A file outside the translation set stays
    /// Kotlin whole; inside it, only the declarations the fixpoint retains do.
    /// The declaring file counts only through its OTHER retained declarations
    /// (its references lower together with the declaration).
    pub fn has_surviving_kotlin_reference(
        &self,
        declaring_file: &Path,
        name: &str,
        retained: &HashSet<crate::semantics::SymbolId>,
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
            file.declarations.iter().any(|decl| {
                self.declaration_retained(decl, retained) && !(same_file && decl.name == name)
            })
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

    /// Classify inherited members for callers whose declaration name is unique.
    /// Translation uses the file-scoped variant below to avoid package collisions.
    pub fn retained_supertype_member_conflicts(
        &self,
        supertypes: &[String],
        class_name: &str,
    ) -> Vec<MemberConflict> {
        let Some(own) = self
            .declarations()
            .find(|d| d.name == class_name && d.language == SourceLanguage::Kotlin)
        else {
            return Vec::new();
        };
        self.member_conflicts_for(supertypes, own)
    }

    pub fn retained_supertype_member_conflicts_in_file(
        &self,
        declaring: &Path,
        supertypes: &[String],
        class_name: &str,
    ) -> Vec<MemberConflict> {
        let Some(own) = self
            .source_file(declaring)
            .and_then(|file| file.declarations.iter().find(|d| d.name == class_name))
        else {
            return Vec::new();
        };
        self.member_conflicts_for(supertypes, own)
    }

    pub fn retained_supertype_member_mismatches_in_file(
        &self,
        declaring: &Path,
        supertypes: &[String],
        class_name: &str,
    ) -> Vec<String> {
        let mut names: Vec<_> = self
            .retained_supertype_member_conflicts_in_file(declaring, supertypes, class_name)
            .into_iter()
            .filter(|conflict| conflict.legacy_mismatch)
            .map(|conflict| conflict.member_name)
            .collect();
        names.sort();
        names.dedup();
        names
    }

    pub fn retained_supertype_member_mismatch_details_in_file(
        &self,
        declaring: &Path,
        supertypes: &[String],
        class_name: &str,
    ) -> Vec<String> {
        let mut details = self
            .retained_supertype_member_conflicts_in_file(declaring, supertypes, class_name)
            .into_iter()
            .filter(|conflict| conflict.legacy_mismatch)
            .map(|conflict| conflict.detail())
            .collect::<Vec<_>>();
        details.sort();
        details.dedup();
        details
    }

    /// Whether a retained implementation's property return can implement the
    /// original Kotlin property contract after it becomes a Java getter.
    /// Exact source types and proven Java-covariant reference returns are safe;
    /// invariant generic mismatches, unknown types, and nullable mismatches
    /// remain blockers.
    pub fn property_getter_return_compatible(
        &self,
        implementation_file: &Path,
        implementation_type: &str,
        contract_owner: &Declaration,
        contract_type: &str,
    ) -> bool {
        if implementation_type.trim().ends_with('?') != contract_type.trim().ends_with('?') {
            return false;
        }
        let Some(own_file) = self.source_file(implementation_file) else {
            return false;
        };
        if self.declaration_source_file(contract_owner).is_none() {
            return false;
        }
        matches!(
            classify_member_conflict(
                self,
                own_file,
                contract_owner,
                contract_type,
                implementation_type,
            ),
            MemberConflictClass::Exact | MemberConflictClass::JavaCovariantReturn
        ) || declaration_covariant_return_is_subtype(
            self,
            own_file,
            implementation_type,
            self.declaration_source_file(contract_owner)
                .unwrap_or(own_file),
            contract_type,
        )
    }

    /// Whether one property getter return type is exact or covariant using the
    /// source context that actually spells each type. This is needed for
    /// inherited generic contracts, where substituted arguments originate in
    /// the file declaring the inheritance edge rather than the contract owner.
    pub fn property_getter_return_compatible_in_files(
        &self,
        implementation_file: &Path,
        implementation_type: &str,
        contract_file: &Path,
        contract_type: &str,
    ) -> bool {
        if implementation_type.trim().ends_with('?') != contract_type.trim().ends_with('?') {
            return false;
        }
        let (Some(own_file), Some(super_file)) = (
            self.source_file(implementation_file),
            self.source_file(contract_file),
        ) else {
            return false;
        };
        if !type_reference_is_visible(self, own_file, implementation_type)
            || !type_reference_is_visible(self, super_file, contract_type)
        {
            return false;
        }
        if source_types_have_same_identity(
            self,
            own_file,
            implementation_type.trim(),
            super_file,
            contract_type.trim(),
        ) {
            return true;
        }
        // Equal simple spellings are not sufficient across two source files:
        // each may resolve the name to a different package type or type
        // parameter. Exact generic trees are checked by the caller with both
        // source contexts; this helper proves only ordinary reference
        // covariance after exact identity has been established separately.
        if implementation_type.trim() == contract_type.trim() {
            return false;
        }
        declaration_covariant_return_is_subtype(
            self,
            own_file,
            implementation_type,
            super_file,
            contract_type,
        ) || proven_reference_subtype(
            self,
            own_file,
            implementation_type.trim(),
            super_file,
            contract_type.trim(),
        )
    }

    /// Return a descriptor only for a proven nonnullable builtin getter type.
    /// Reuse source-scoped identity resolution: aliases, source collisions and
    /// explicit imports take precedence over the provider's builtin facts.
    pub(crate) fn builtin_getter_descriptor_in_file(
        &self,
        declaring: &Path,
        type_name: &str,
    ) -> Option<&'static str> {
        let type_name = type_name.trim();
        if !matches!(
            type_name.rsplit('.').next()?,
            "String" | "Boolean" | "Byte" | "Short" | "Int" | "Long" | "Float" | "Double" | "Char"
        ) {
            return None;
        }
        let file = self.source_file(declaring)?;
        let identity = canonical_getter_named_identity(self, file, type_name)?;
        match identity.as_str() {
            "kotlin.String" => Some("()Ljava/lang/String;"),
            "kotlin.Boolean" => Some("()Z"),
            "kotlin.Byte" => Some("()B"),
            "kotlin.Short" => Some("()S"),
            "kotlin.Int" => Some("()I"),
            "kotlin.Long" => Some("()J"),
            "kotlin.Float" => Some("()F"),
            "kotlin.Double" => Some("()D"),
            "kotlin.Char" => Some("()C"),
            _ => None,
        }
    }

    /// A Kotlin read-only collection property is declaration-site covariant.
    /// When both sides move to Java together, Notlin can preserve that contract
    /// by emitting a wildcard return on the interface (`List<? extends Base>`)
    /// and the exact return on the implementation (`List<Derived>`). This is
    /// unsafe while the Kotlin supertype remains, because its JVM signature has
    /// the invariant generic return Kotlin emitted.
    pub fn selected_covariant_property_conflict_is_safe(
        &self,
        declaring: &Path,
        conflict: &MemberConflict,
        translation_roots: &[PathBuf],
        retained: &HashSet<crate::semantics::SymbolId>,
    ) -> bool {
        let property_conflict = conflict.kind == MemberKind::Property;
        let getter_method_conflict = conflict.kind == MemberKind::Method
            && conflict.parameter_types.is_empty()
            && conflict
                .member_name
                .strip_prefix("get")
                .is_some_and(|suffix| suffix.chars().next().is_some_and(char::is_uppercase));
        if !conflict.legacy_mismatch || !(property_conflict || getter_method_conflict) {
            return false;
        }
        let Some(super_decl) = self
            .declarations()
            .find(|decl| declaration_key(decl) == conflict.supertype)
        else {
            return false;
        };
        let Some(super_file) = self.declaration_source_file(super_decl) else {
            return false;
        };
        if self.declaration_retained(super_decl, retained)
            && !readonly_list_jvm_wildcard_abi_proven(
                self,
                super_file,
                super_decl,
                &conflict.member_name,
                &conflict.inherited_type,
            )
        {
            return false;
        }
        if getter_method_conflict && super_decl.kind != DeclarationKind::Interface {
            return false;
        }
        if getter_method_conflict
            && !self.readonly_interface_getter_is_covariant_source(
                super_file,
                super_decl,
                &conflict.member_name,
            )
        {
            return false;
        }
        if getter_method_conflict {
            let suffix = &conflict.member_name[3..];
            let mut chars = suffix.chars();
            let property = chars
                .next()
                .map(|ch| {
                    format!(
                        "{}{}",
                        ch.to_lowercase().collect::<String>(),
                        chars.as_str()
                    )
                })
                .unwrap_or_default();
            if super_decl.members.iter().any(|member| {
                (member.kind == MemberKind::Method && member.name == format!("set{suffix}"))
                    || (member.kind == MemberKind::Property
                        && member.name == property
                        && member.is_mutable)
            }) {
                return false;
            }
        }
        if !self.is_selected(&super_file.path, translation_roots) {
            return false;
        }
        let Some(own_file) = self.source_file(declaring) else {
            return false;
        };
        // Covariant Java returns may narrow Kotlin nullability, but they may
        // not widen a non-null supertype contract. The collection subtype
        // proof below strips `?` while comparing generic arguments, so enforce
        // the return-level nullability direction before relying on it.
        if conflict.implementation_type.trim().ends_with('?')
            && !conflict.inherited_type.trim().ends_with('?')
        {
            return false;
        }
        declaration_covariant_return_is_subtype(
            self,
            own_file,
            &conflict.implementation_type,
            super_file,
            &conflict.inherited_type,
        )
    }

    /// Find a complete, atomic set of exact source edits for a selected
    /// implementation whose retained Kotlin `val` overrides one or more
    /// retained Kotlin `List` contracts with narrower element types.
    ///
    /// `conflicts` must be the complete conflict set for this member, as
    /// returned by the workspace conflict planner. Exact/nonblocking parent
    /// contracts need no edit and are skipped; this deliberately rejects
    /// methods, mutable properties, nullable widening, wrappers, projections,
    /// aliases, and any contract that cannot be located uniquely in its
    /// indexed source snapshot.
    pub fn readonly_list_jvm_wildcard_requirements(
        &self,
        declaring: &Declaration,
        member_name: &str,
        conflicts: &[MemberConflict],
        translation_roots: &[PathBuf],
        retained: &HashSet<crate::semantics::SymbolId>,
    ) -> Option<Vec<JvmWildcardContractRequirement>> {
        if declaring.language != SourceLanguage::Kotlin
            || !self.is_selected(
                self.declaration_source_file(declaring)?.path.as_path(),
                translation_roots,
            )
        {
            return None;
        }
        let own_file = self.declaration_source_file(declaring)?;
        let own_member = declaring
            .members
            .iter()
            .find(|member| member.kind == MemberKind::Property && member.name == member_name)?;
        if own_member.is_mutable
            || own_member.has_unsupported_property_shape
            || own_member.has_unsupported_property_annotations
        {
            return None;
        }
        let own_ty = own_member.type_name.as_deref()?;
        let (own_base, Some(own_args)) = split_generic(own_ty.trim().trim_end_matches('?')) else {
            return None;
        };
        if own_base != "List"
            || own_args.len() != 1
            || own_args[0].trim().ends_with('?')
            || !readonly_kotlin_list_base_is_proven(self, own_file, declaring)
        {
            return None;
        }

        let all_matching = conflicts
            .iter()
            .filter(|conflict| conflict.member_name == member_name)
            .collect::<Vec<_>>();
        let matching = all_matching
            .iter()
            .copied()
            .filter(|conflict| conflict.legacy_mismatch)
            .collect::<Vec<_>>();
        if matching.is_empty()
            || conflicts.iter().any(|conflict| {
                conflict.member_name == member_name
                    && conflict.legacy_mismatch
                    && conflict.kind != MemberKind::Property
            })
        {
            return None;
        }
        let mut requirements = Vec::with_capacity(matching.len());
        let mut seen_contracts = HashSet::new();
        for conflict in matching {
            if conflict.kind != MemberKind::Property {
                return None;
            }
            let super_decl = self
                .declarations()
                .find(|decl| declaration_key(decl) == conflict.supertype)?;
            let super_file = self.declaration_source_file(super_decl)?;
            if super_decl.language != SourceLanguage::Kotlin
                || super_decl.kind != DeclarationKind::Interface
                || !self.is_selected(&super_file.path, translation_roots)
                || !self.declaration_retained(super_decl, retained)
            {
                return None;
            }
            let contract_member = super_decl
                .members
                .iter()
                .find(|member| member.kind == MemberKind::Property && member.name == member_name)?;
            if contract_member.is_mutable
                || contract_member.has_unsupported_property_shape
                || (contract_member.has_unsupported_property_annotations
                    && !readonly_list_wildcard_property_annotations_supported(
                        super_file,
                        super_decl,
                        member_name,
                    ))
            {
                return None;
            }
            let contract_ty = contract_member.type_name.as_deref()?;
            if contract_ty.trim().ends_with('?') != conflict.inherited_type.trim().ends_with('?')
                || conflict.implementation_type.trim() != own_ty.trim()
                || conflict.inherited_type.trim() != contract_ty.trim()
                || (own_ty.trim().ends_with('?') && !contract_ty.trim().ends_with('?'))
            {
                return None;
            }
            let (contract_base, Some(contract_args)) =
                split_generic(contract_ty.trim().trim_end_matches('?'))
            else {
                return None;
            };
            if contract_base != "List"
                || contract_args.len() != 1
                || !readonly_kotlin_list_base_is_proven(self, super_file, super_decl)
                || !declaration_covariant_return_is_subtype(
                    self,
                    own_file,
                    own_ty,
                    super_file,
                    contract_ty,
                )
            {
                return None;
            }
            let contract_symbol = crate::semantics::workspace_symbol(self, super_decl);
            if !seen_contracts.insert(contract_symbol.clone()) {
                return None;
            }
            let (location, replacement, already_present) =
                readonly_list_type_argument_edit(super_file, super_decl, member_name)?;
            requirements.push(JvmWildcardContractRequirement {
                contract: contract_symbol,
                implementation: crate::semantics::workspace_symbol(self, declaring),
                member_name: member_name.to_owned(),
                location,
                replacement,
                already_present,
            });
        }
        Some(requirements)
    }

    /// Whether an interface getter has Kotlin's read-only collection contract,
    /// so Java can express its declaration-site variance with a wildcard.
    /// This source-only proof is shared by interface emission and override
    /// compatibility to keep their decisions aligned.
    pub fn readonly_interface_getter_is_covariant_source(
        &self,
        source_file: &SourceFile,
        owner: &Declaration,
        getter: &str,
    ) -> bool {
        if owner.language != SourceLanguage::Kotlin
            || owner.kind != DeclarationKind::Interface
            || getter
                .strip_prefix("get")
                .is_none_or(|suffix| !suffix.chars().next().is_some_and(char::is_uppercase))
        {
            return false;
        }
        let mut getter_members = owner.members.iter().filter(|member| {
            member.kind == MemberKind::Method
                && member.name == getter
                && !member.is_static
                && member.parameter_types.is_empty()
        });
        let Some(member) = getter_members.next() else {
            return false;
        };
        if getter_members.next().is_some() {
            return false;
        }
        let Some(return_type) = member.type_name.as_deref() else {
            return false;
        };
        let base = return_type
            .trim()
            .trim_end_matches('?')
            .split('<')
            .next()
            .unwrap_or_default()
            .trim();
        if !matches!(
            base,
            "List" | "Set" | "Collection" | "Iterable" | "Iterator" | "Sequence" | "Map"
        ) || !collection_base_is_unshadowed(self, source_file, base)
        {
            return false;
        }
        let suffix = &getter[3..];
        let mut chars = suffix.chars();
        let property = chars
            .next()
            .map(|ch| {
                format!(
                    "{}{}",
                    ch.to_lowercase().collect::<String>(),
                    chars.as_str()
                )
            })
            .unwrap_or_default();
        let has_mutator = |decl: &Declaration| {
            decl.members.iter().any(|member| {
                (member.kind == MemberKind::Method && member.name == format!("set{suffix}"))
                    || (member.kind == MemberKind::Property
                        && member.name == property
                        && member.is_mutable)
            })
        };
        if has_mutator(owner) {
            return false;
        }
        let mut pending: Vec<_> = owner
            .supertypes
            .iter()
            .map(|s| (source_file, s.as_str()))
            .collect();
        let mut seen = HashSet::new();
        while let Some((context, supertype)) = pending.pop() {
            let Some(parent) = self.resolve_type(context, supertype) else {
                // An unresolved edge might hide an invariant Java getter.
                return false;
            };
            let key = declaration_key(parent);
            if !seen.insert(key) {
                continue;
            }
            if parent.language == SourceLanguage::Java
                && parent.members.iter().any(|member| {
                    member.kind == MemberKind::Method
                        && member.name == getter
                        && !member.is_static
                        && member.parameter_types.is_empty()
                })
            {
                return false;
            }
            if has_mutator(parent) {
                return false;
            }
            if let Some(parent_file) = self.declaration_source_file(parent) {
                pending.extend(parent.supertypes.iter().map(|s| (parent_file, s.as_str())));
            }
        }
        true
    }

    fn member_conflicts_for(
        &self,
        supertypes: &[String],
        own: &Declaration,
    ) -> Vec<MemberConflict> {
        // Resolve every inheritance edge in the file that declares that edge.
        // A return may narrow through Java covariance or an explicit wildcard;
        // incompatible invariant generics remain retained rather than erased.
        let own_file = self.declaration_source_file(own).unwrap();
        let mut pending: Vec<_> = supertypes
            .iter()
            .map(|supertype| {
                (
                    own_file,
                    supertype.clone(),
                    HashMap::<String, String>::new(),
                )
            })
            .collect();
        let mut visited = HashSet::new();
        let mut conflicts = Vec::new();
        while let Some((context, sup, inherited_bindings)) = pending.pop() {
            let resolved_sup = substitute_type_parameters(&sup, &inherited_bindings);
            let Some(sup_decl) = self.resolve_type(context, &resolved_sup) else {
                continue;
            };
            let (_, supplied_args) = split_generic(resolved_sup.trim().trim_end_matches("()"));
            let bindings: HashMap<String, String> = sup_decl
                .type_params
                .iter()
                .zip(supplied_args.unwrap_or_default())
                .map(|(parameter, argument)| (parameter.clone(), argument.to_string()))
                .collect();
            let visit_key = format!(
                "{}<{}>",
                declaration_key(sup_decl),
                sup_decl
                    .type_params
                    .iter()
                    .filter_map(|parameter| bindings.get(parameter))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            );
            if !visited.insert(visit_key) {
                continue;
            }
            let sup_file = self.declaration_source_file(sup_decl).unwrap_or(context);
            // Only a supertype that remains Kotlin forces the ABI match, but
            // its OWN supertypes still widen the conflict transitively
            // (ImportContainer extends IImportContainer, IBarcodeAware).
            for sup_sup in &sup_decl.supertypes {
                pending.push((sup_file, sup_sup.clone(), bindings.clone()));
            }
            if sup_decl.language != SourceLanguage::Kotlin {
                continue;
            }
            for m in &sup_decl.members {
                if m.is_static {
                    continue;
                }
                let Some(raw_sup_ty) = m.type_name.as_deref() else {
                    continue;
                };
                let sup_ty = substitute_type_parameters(raw_sup_ty, &bindings);
                let parameter_types: Vec<String> = m
                    .parameter_types
                    .iter()
                    .map(|parameter| substitute_type_parameters(parameter, &bindings))
                    .collect();
                if let Some(own_m) = own.members.iter().find(|om| {
                    om.name == m.name
                        && !om.is_static
                        && om.kind == m.kind
                        && om.parameter_types == parameter_types
                }) && let Some(own_ty) = own_m.type_name.as_deref()
                {
                    conflicts.push(MemberConflict {
                        member_name: m.name.clone(),
                        kind: m.kind,
                        supertype: declaration_key(sup_decl),
                        inherited_type: sup_ty.clone(),
                        implementation_type: own_ty.to_string(),
                        parameter_types,
                        classification: classify_member_conflict(
                            self, own_file, sup_decl, &sup_ty, own_ty,
                        ),
                        legacy_mismatch: own_ty != sup_ty
                            && !sup_decl
                                .type_params
                                .iter()
                                .any(|param| param == sup_ty.trim_end_matches('?').trim())
                            && !is_java_compatible_narrow(self, own_file, &sup_ty, own_ty)
                            && !projected_return_is_subtype(
                                self, own_file, own_ty, sup_file, &sup_ty,
                            ),
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

    /// Kotlin synthesizes some inherited implementations that Java does not:
    /// unrelated defaults, or an abstract narrowing layered over a default.
    /// Until an explicit bridge can be emitted, an enum relying on one stays
    /// Kotlin. Ordinary single-default inheritance remains representable.
    pub fn enum_default_obligations(&self, declaring: &Path, name: &str) -> Vec<String> {
        let Some(file) = self.source_file(declaring) else {
            return Vec::new();
        };
        let Some(owner) = file.declarations.iter().find(|d| d.name == name) else {
            return Vec::new();
        };
        let signature = |m: &Member| {
            (
                if m.kind == MemberKind::Property {
                    property_accessor_name(&m.name)
                } else {
                    m.name.clone()
                },
                m.parameter_types.clone(),
            )
        };
        let own: HashSet<_> = owner
            .members
            .iter()
            .filter(|m| !m.is_static)
            .map(&signature)
            .collect();
        let mut groups: BTreeMap<_, Vec<(&Declaration, &Member)>> = BTreeMap::new();
        let mut pending: Vec<_> = owner.supertypes.iter().map(|s| (file, s)).collect();
        let mut visited = HashSet::new();
        while let Some((context, ty)) = pending.pop() {
            let Some(decl) = self.resolve_type(context, ty) else {
                continue;
            };
            if !visited.insert(declaration_key(decl)) {
                continue;
            }
            let context = self.declaration_source_file(decl).unwrap_or(context);
            pending.extend(decl.supertypes.iter().map(|s| (context, s)));
            if decl.kind != DeclarationKind::Interface {
                continue;
            }
            for member in &decl.members {
                if member.is_static
                    || member.visibility.as_deref() == Some("private")
                    || !matches!(member.kind, MemberKind::Property | MemberKind::Method)
                {
                    continue;
                }
                let key = signature(member);
                if !own.contains(&key) {
                    groups.entry(key).or_default().push((decl, member));
                }
            }
        }
        groups
            .into_iter()
            .filter_map(|((name, _), members)| {
                if !members.iter().any(|(_, m)| m.has_body) {
                    return None;
                }
                let most_specific: Vec<_> = members
                    .iter()
                    .filter(|(decl, _)| {
                        !members.iter().any(|(other, _)| {
                            declaration_key(other) != declaration_key(decl)
                                && proven_reference_subtype(
                                    self,
                                    self.declaration_source_file(other).unwrap_or(file),
                                    &declaration_key(other),
                                    self.declaration_source_file(decl).unwrap_or(file),
                                    &declaration_key(decl),
                                )
                        })
                    })
                    .collect();
                (most_specific.len() > 1 || most_specific.iter().any(|(_, m)| !m.has_body))
                    .then_some(name)
            })
            .collect()
    }

    /// Explicit zero-argument bridges that make Kotlin's inherited interface
    /// resolution legal in Java. Kotlin accepts a concrete property getter
    /// from one direct interface beside an unrelated abstract declaration;
    /// javac requires the implementing enum to choose the concrete provider.
    pub fn enum_default_property_bridges(
        &self,
        declaring: &Path,
        name: &str,
    ) -> Vec<EnumDefaultBridge> {
        let obligations = self.enum_default_obligations(declaring, name);
        if obligations.is_empty() {
            return Vec::new();
        }
        let Some(file) = self.source_file(declaring) else {
            return Vec::new();
        };
        let Some(owner) = file
            .declarations
            .iter()
            .find(|declaration| declaration.name == name)
        else {
            return Vec::new();
        };
        obligations
            .into_iter()
            .filter_map(|method_name| {
                owner.supertypes.iter().find_map(|supertype| {
                    let provider = self.resolve_type(file, supertype)?;
                    if provider.kind != DeclarationKind::Interface {
                        return None;
                    }
                    // Kotlin resolves the inherited implementation through the
                    // first direct superinterface branch.  The implementation
                    // may be declared by an ancestor of that direct interface;
                    // Java's bridge must still qualify the direct interface so
                    // that the same branch is selected.  Looking only at
                    // members declared directly on `provider` accidentally
                    // skipped such branches and selected a later interface.
                    let (member_owner, member) =
                        self.effective_default_getter(provider, file, &method_name)?;
                    let raw_return_type = member.type_name.clone()?;
                    // This traversal has not carried generic substitutions.
                    // Do not leak an unbound provider parameter into an enum
                    // bridge, or guess the identity of an unknown return type.
                    canonical_getter_type(self, member_owner, &raw_return_type, &HashMap::new())?;
                    let provider_file = self.declaration_source_file(member_owner).unwrap_or(file);
                    let return_type = self
                        .resolve_type(provider_file, raw_return_type.trim_end_matches('?'))
                        .map(declaration_key)
                        .unwrap_or(raw_return_type);
                    Some(EnumDefaultBridge {
                        method_name: method_name.clone(),
                        return_type,
                        provider: supertype
                            .split('<')
                            .next()
                            .unwrap_or(supertype)
                            .trim()
                            .to_string(),
                    })
                })
            })
            .collect()
    }

    /// Find the most-specific property implementation visible through one
    /// direct interface branch. An abstract redeclaration deliberately returns
    /// no provider because it shadows a concrete ancestor.
    fn effective_default_getter<'a>(
        &'a self,
        provider: &'a Declaration,
        fallback_file: &'a SourceFile,
        method_name: &str,
    ) -> Option<(&'a Declaration, &'a Member)> {
        let mut members = Vec::new();
        let mut pending = vec![(fallback_file, provider)];
        let mut visited = HashSet::new();
        while let Some((context, declaration)) = pending.pop() {
            if !visited.insert(declaration_key(declaration)) {
                continue;
            }
            let declaration_file = self.declaration_source_file(declaration).unwrap_or(context);
            pending.extend(declaration.supertypes.iter().filter_map(|supertype| {
                self.resolve_type(declaration_file, supertype)
                    .map(|parent| (declaration_file, parent))
            }));
            if declaration.kind != DeclarationKind::Interface {
                continue;
            }
            members.extend(declaration.members.iter().filter_map(|member| {
                (!member.is_static
                    && member.visibility.as_deref() != Some("private")
                    && (member.kind == MemberKind::Property
                        && property_accessor_name(&member.name) == method_name
                        || is_physical_zero_arg_getter(member, method_name)))
                .then_some((declaration, member))
            }));
        }

        let mut most_specific = members.iter().copied().filter(|(declaration, _)| {
            !members.iter().any(|(other, _)| {
                declaration_key(other) != declaration_key(declaration)
                    && proven_reference_subtype(
                        self,
                        self.declaration_source_file(other).unwrap_or(fallback_file),
                        &declaration_key(other),
                        self.declaration_source_file(declaration)
                            .unwrap_or(fallback_file),
                        &declaration_key(declaration),
                    )
            })
        });
        let result = most_specific.next()?;
        if most_specific.next().is_some() || !result.1.has_body {
            return None;
        }
        Some(result)
    }

    pub(crate) fn direct_supertype_for_member(
        &self,
        declaring: &Path,
        owner_name: &str,
        member_name: &str,
    ) -> Option<String> {
        let file = self.source_file(declaring)?;
        let owner = file
            .declarations
            .iter()
            .find(|declaration| declaration.name == owner_name)?;
        owner.supertypes.iter().find_map(|supertype| {
            let target = self.resolve_type(file, supertype)?;
            target
                .members
                .iter()
                .any(|member| {
                    member.name == member_name
                        || (member.kind == MemberKind::Property
                            && property_accessor_name(&member.name) == member_name)
                })
                .then(|| {
                    supertype
                        .split('<')
                        .next()
                        .unwrap_or(supertype)
                        .trim()
                        .to_string()
                })
        })
    }

    pub fn has_retained_kotlin_supertype(
        &self,
        supertypes: &[String],
        retained: &HashSet<crate::semantics::SymbolId>,
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
        retained: &HashSet<crate::semantics::SymbolId>,
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
                self.declarations_named(name)
                    .any(|declaration| self.declaration_retained(declaration, retained))
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

    pub fn retained_supertypes_of(
        &self,
        target: &Declaration,
        retained: &HashSet<crate::semantics::SymbolId>,
    ) -> Vec<String> {
        let Some(file) = self.declaration_source_file(target) else {
            return Vec::new();
        };
        let mut names = target
            .supertypes
            .iter()
            .filter_map(|name| self.resolve_type(file, name))
            .filter(|declaration| {
                declaration.language == SourceLanguage::Kotlin
                    && self.declaration_retained(declaration, retained)
            })
            .map(|declaration| declaration.name.clone())
            .collect::<Vec<_>>();
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
        let mut names = vec![m];
        if let Some(getter_prop) = getter_prop.as_deref() {
            names.push(getter_prop);
        }
        let wide = self
            .members_named_any(&names)
            .into_iter()
            .find_map(|(_, member)| {
                (member.kind == MemberKind::Method)
                    .then(|| member.type_name.clone())
                    .flatten()
            });
        same.or(wide)
    }

    pub fn bare_property_type(&self, prop: &str) -> Option<String> {
        self.members_named(prop).find_map(|(_, member)| {
            (member.kind == MemberKind::Property)
                .then(|| member.type_name.clone())
                .flatten()
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
        let mut candidates: Vec<Option<String>> = Vec::new();
        let mut previous_declaration = None;
        for (declaration, member) in self.members_named(prop) {
            if member.kind != MemberKind::Property {
                continue;
            }
            let declaration_address = declaration as *const Declaration as usize;
            if previous_declaration == Some(declaration_address) {
                continue;
            }
            previous_declaration = Some(declaration_address);
            candidates.push(member.type_name.clone());
        }
        let enum_typed = candidates.iter().flatten().find_map(|t| {
            let bare = t.split('<').next().unwrap_or("").trim().to_string();
            if bare.is_empty() {
                return None;
            }
            self.declarations_named(&bare)
                .any(|d| d.kind == crate::workspace::DeclarationKind::Enum)
                .then(|| t.clone())
        });
        match enum_typed {
            Some(t) => Some(t),
            None => candidates.into_iter().flatten().next(),
        }
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
        self.members_named(&prop).find_map(|(_, member)| {
            (member.kind == MemberKind::Property)
                .then(|| member.type_name.clone())
                .flatten()
        })
    }

    /// A workspace declaration owning a non-static PROPERTY member `name`.
    /// A bare identifier resolving to it reads through the property's Java
    /// accessor on the implicit `this`, regardless of whether the owner is
    /// translated (Java getter shed) or retained (Kotlin property ABI has
    /// the same getter shape).
    pub fn find_property_owner(&self, name: &str) -> Option<&Declaration> {
        self.members_named(name)
            .find(|(_, member)| member.kind == MemberKind::Property && !member.is_static)
            .map(|(declaration, _)| declaration)
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

    /// The ordinary JavaBean setter inherited by `type_name` for a mutable
    /// Kotlin property, if each indexed supertype edge proves the conventional
    /// setter ABI. Every edge is resolved in its declaring file's context.
    pub fn inherited_mutable_property_setter_in_file(
        &self,
        declaring: &Path,
        type_name: &str,
        property_name: &str,
    ) -> Option<String> {
        // Keep calls independent of Java overload selection. Source defaults
        // and varargs can make an otherwise different signature applicable.
        let suffix = property_name
            .strip_prefix("is")
            .filter(|rest| rest.chars().next().is_some_and(char::is_uppercase))
            .unwrap_or(property_name);
        if suffix != property_name {
            return None;
        }
        let mut chars = suffix.chars();
        let setter = format!("set{}{}", chars.next()?.to_uppercase(), chars.as_str());
        let overlaps = |declaration: &Declaration| {
            declaration.members.iter().any(|member| {
                member.kind == MemberKind::Method
                    && member.name == setter
                    && member
                        .callable_arity
                        .is_none_or(|arity| arity.accepts_arity(1))
            })
        };
        let own_file = self.source_file(declaring)?;
        let mut owners = own_file
            .declarations
            .iter()
            .filter(|declaration| declaration.name == type_name);
        let owner = owners.next()?;
        if owners.next().is_some() || overlaps(owner) {
            return None;
        }
        let mut pending = owner
            .supertypes
            .iter()
            .cloned()
            .map(|supertype| (supertype, own_file.path.clone()))
            .collect::<Vec<_>>();
        let mut visited = HashSet::new();
        let mut found = false;

        while let Some((supertype, context_path)) = pending.pop() {
            let context = self.source_file(&context_path)?;
            let declaration = self.resolve_type(context, &supertype)?;
            if overlaps(declaration) {
                return None;
            }
            let key = (
                declaration.package.clone(),
                declaration.name.clone(),
                declaration.language as u8,
            );
            if !visited.insert(key) {
                continue;
            }
            let parent_file = self.declaration_source_file(declaration)?;
            for property in declaration.members.iter().filter(|member| {
                member.kind == MemberKind::Property
                    && member.name == property_name
                    && !member.is_static
            }) {
                found = true;
                if !property.is_mutable
                    || property.is_jvm_field
                    || property.has_unsupported_property_annotations
                    || property.has_unsupported_property_shape
                    || property.has_custom_accessor
                    || !matches!(
                        property.visibility.as_deref(),
                        None | Some("public" | "protected")
                    )
                {
                    return None;
                }
            }
            pending.extend(
                declaration
                    .supertypes
                    .iter()
                    .cloned()
                    .map(|supertype| (supertype, parent_file.path.clone())),
            );
        }

        if !found {
            return None;
        }
        Some(setter)
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
        default_getters::record_alias_lookup(self, name);
        self.declaration_names.get(name).into_iter().flatten().map(
            |&(file_index, declaration_index)| {
                &self.files[file_index].declarations[declaration_index]
            },
        )
    }
    pub(crate) fn members_named(
        &self,
        name: &str,
    ) -> impl Iterator<Item = (&Declaration, &Member)> {
        default_getters::record_alias_lookup(self, name);
        self.member_names.get(name).into_iter().flatten().map(
            |&(file_index, declaration_index, member_index)| {
                let declaration = &self.files[file_index].declarations[declaration_index];
                (declaration, &declaration.members[member_index])
            },
        )
    }

    /// Multiple exact names in original workspace order. Some lookups accept
    /// both a method name and its property-getter form; sorting their indexed
    /// hits together preserves the old declaration/member traversal order.
    fn members_named_any(&self, names: &[&str]) -> Vec<(&Declaration, &Member)> {
        for name in names {
            default_getters::record_alias_lookup(self, name);
        }
        let mut locations = names
            .iter()
            .filter_map(|name| self.member_names.get(*name))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        locations.sort_unstable();
        locations.dedup();
        locations
            .into_iter()
            .map(|(file_index, declaration_index, member_index)| {
                let declaration = &self.files[file_index].declarations[declaration_index];
                (declaration, &declaration.members[member_index])
            })
            .collect()
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
            if decl.members.iter().any(|member| {
                member.kind == MemberKind::Method && member.name == getter_name && member.has_body
            }) {
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

    /// Retain a Kotlin interface that supplies a default property getter when
    /// an actually retained Kotlin concrete descendant would otherwise lose
    /// its physical covariant getter bridge. Java's type system still resolves
    /// the narrow default method correctly, but annotation processors may pick
    /// a broad accessor inherited along another interface path instead.
    ///
    /// This is deliberately based on exact declaration identities for retained
    /// descendants and exact resolved type declarations. Unselected Kotlin
    /// descendants count as retained. Any differing or unresolved competing
    /// property contract is treated conservatively as a collision.
    pub fn default_property_provider_needs_physical_getter(
        &self,
        provider: &Declaration,
        retained: &HashSet<crate::semantics::SymbolId>,
        translation_roots: &[PathBuf],
    ) -> Option<String> {
        self.default_property_getter_conflict(provider, retained, translation_roots)
            .map(|conflict| conflict.message())
    }

    pub(crate) fn default_property_provider_retention_reason(
        &self,
        provider: &Declaration,
        retained: &HashSet<crate::semantics::SymbolId>,
        translation_roots: &[PathBuf],
        allow_repairs: bool,
    ) -> Option<String> {
        self.default_property_getter_conflict_with_repairs(
            provider,
            retained,
            translation_roots,
            allow_repairs,
        )
        .map(|conflict| conflict.message())
    }

    /// Exact declarations and return shapes behind the annotation-processor
    /// getter safeguard. Missing types still block translation, but must not
    /// become source-change recommendations.
    pub fn default_property_getter_conflict(
        &self,
        provider: &Declaration,
        retained: &HashSet<crate::semantics::SymbolId>,
        translation_roots: &[PathBuf],
    ) -> Option<DefaultGetterConflict> {
        self.default_property_getter_conflict_with_repairs(
            provider,
            retained,
            translation_roots,
            true,
        )
    }

    fn default_property_getter_conflict_with_repairs(
        &self,
        provider: &Declaration,
        retained: &HashSet<crate::semantics::SymbolId>,
        translation_roots: &[PathBuf],
        allow_repairs: bool,
    ) -> Option<DefaultGetterConflict> {
        if provider.language != SourceLanguage::Kotlin
            || provider.kind != DeclarationKind::Interface
        {
            return None;
        }
        self.declaration_source_file(provider)?;
        for (property, property_name) in provider.members.iter().filter_map(|member| {
            let name = member_property_contract_name(member)?;
            (member.has_body
                && !member.is_static
                && member.visibility.as_deref() != Some("private"))
            .then_some((member, name))
        }) {
            let inferred_property_type = property
                .type_name
                .is_none()
                .then(|| self.inferred_simple_property_type(provider, property))
                .flatten();
            let property_type = property
                .type_name
                .as_deref()
                .or_else(|| inferred_property_type.as_ref().map(|(ty, _)| ty.as_str()));
            let mut pending = vec![provider];
            let mut visited = HashSet::new();
            while let Some(current) = pending.pop() {
                if !visited.insert(crate::semantics::workspace_symbol(self, current).stable_key()) {
                    continue;
                }
                for descendant in self.direct_subtypes(current) {
                    pending.push(descendant);
                    if descendant.language != SourceLanguage::Kotlin
                        || descendant.is_abstract
                        || !matches!(
                            descendant.kind,
                            DeclarationKind::Class
                                | DeclarationKind::Enum
                                | DeclarationKind::Object
                        )
                    {
                        continue;
                    }
                    let Some(descendant_file) = self.declaration_source_file(descendant) else {
                        continue;
                    };
                    let retained_descendant = self.declaration_retained(descendant, retained)
                        || !self.is_selected(&descendant_file.path, translation_roots);
                    if !retained_descendant
                        || declaration_declares_property_getter(descendant, &property_name)
                    {
                        continue;
                    }

                    // Inspect every inherited branch from this concrete owner,
                    // including sibling paths outside the provider's own
                    // ancestry. A same-spelled contract with a different or
                    // unknown return type is enough to make inherited Java
                    // accessor selection unsafe.
                    // Keep each inheritance path's type-parameter bindings.
                    // Comparing raw names here incorrectly treats `I` declared
                    // by different interfaces as either unrelated (when owner
                    // keys differ) or equal (if only the spelling is checked).
                    // Seed the concrete descendant's parameters with scoped
                    // symbols, then substitute each edge as it is traversed.
                    let descendant_identity =
                        crate::semantics::workspace_symbol(self, descendant).stable_key();
                    let mut ancestors = vec![(
                        descendant,
                        scoped_type_parameter_bindings(self, descendant),
                        HashSet::from([descendant_identity]),
                    )];
                    let mut ancestor_seen = HashSet::new();
                    let mut inherited_getters = Vec::new();
                    let mut getter_ancestry = HashMap::<
                        crate::semantics::SymbolId,
                        HashSet<crate::semantics::SymbolId>,
                    >::new();
                    let mut explored_ancestors = 0usize;
                    while let Some((owner, owner_bindings, owner_path)) = ancestors.pop() {
                        explored_ancestors += 1;
                        if explored_ancestors > 256 {
                            return Some(DefaultGetterConflict {
                                provider: crate::semantics::workspace_symbol(self, provider),
                                implementation: crate::semantics::workspace_symbol(
                                    self, descendant,
                                ),
                                competing_owner: crate::semantics::workspace_symbol(self, owner),
                                property: property_name.clone(),
                                provider_type: property_type.map(str::to_owned),
                                competing_type: None,
                            });
                        }
                        let binding_key = owner
                            .type_params
                            .iter()
                            .filter_map(|parameter| {
                                owner_bindings
                                    .get(parameter)
                                    .map(|value| format!("{parameter}={value:?}"))
                            })
                            .collect::<Vec<_>>()
                            .join(",");
                        if !ancestor_seen.insert((
                            crate::semantics::workspace_symbol(self, owner).stable_key(),
                            binding_key,
                        )) {
                            continue;
                        }
                        let Some(owner_file) = self.declaration_source_file(owner) else {
                            return Some(DefaultGetterConflict {
                                provider: crate::semantics::workspace_symbol(self, provider),
                                implementation: crate::semantics::workspace_symbol(
                                    self, descendant,
                                ),
                                competing_owner: crate::semantics::workspace_symbol(self, owner),
                                property: property_name.clone(),
                                provider_type: property_type.map(str::to_owned),
                                competing_type: None,
                            });
                        };
                        for supertype in &owner.supertypes {
                            let Some(parent) =
                                resolve_unique_getter_type(self, owner_file, supertype)
                            else {
                                // Eligibility and bridge planning must agree
                                // about external types that declare no getters.
                                if default_getters::known_external_without_getters(
                                    self, owner_file, supertype,
                                ) {
                                    continue;
                                }
                                return Some(DefaultGetterConflict {
                                    provider: crate::semantics::workspace_symbol(self, provider),
                                    implementation: crate::semantics::workspace_symbol(
                                        self, descendant,
                                    ),
                                    competing_owner: crate::semantics::workspace_symbol(
                                        self, owner,
                                    ),
                                    property: property_name.clone(),
                                    provider_type: property_type.map(str::to_owned),
                                    competing_type: None,
                                });
                            };
                            let parent_identity =
                                crate::semantics::workspace_symbol(self, parent).stable_key();
                            getter_ancestry
                                .entry(crate::semantics::workspace_symbol(self, owner))
                                .or_default()
                                .insert(crate::semantics::workspace_symbol(self, parent));
                            if owner_path.contains(&parent_identity) {
                                return Some(DefaultGetterConflict {
                                    provider: crate::semantics::workspace_symbol(self, provider),
                                    implementation: crate::semantics::workspace_symbol(
                                        self, descendant,
                                    ),
                                    competing_owner: crate::semantics::workspace_symbol(
                                        self, parent,
                                    ),
                                    property: property_name.clone(),
                                    provider_type: property_type.map(str::to_owned),
                                    competing_type: None,
                                });
                            }
                            let parent_bindings = inherited_type_parameter_bindings(
                                self,
                                owner,
                                parent,
                                supertype,
                                &owner_bindings,
                            );
                            for inherited in parent.members.iter().filter(|member| {
                                member_is_property_getter(member, &property_name)
                                    && !member.is_static
                            }) {
                                let inherited_type = inherited.type_name.clone().or_else(|| {
                                    self.inferred_simple_property_type(parent, inherited)
                                        .map(|(ty, _)| ty)
                                });
                                let effective_type = inherited_type.as_deref().and_then(|ty| {
                                    canonical_getter_type(self, parent, ty, &parent_bindings)
                                });
                                inherited_getters.push((
                                    parent,
                                    inherited_type,
                                    effective_type,
                                    inherited,
                                ));
                            }
                            let mut parent_path = owner_path.clone();
                            parent_path.insert(parent_identity);
                            ancestors.push((parent, parent_bindings, parent_path));
                        }
                    }

                    // A single provider declaration without any competing
                    // accessor needs no cross-owner return-type proof. Its
                    // own emission and semantic checks still apply normally.
                    if matches!(inherited_getters.as_slice(), [(owner, _, _, _)] if crate::semantics::workspace_symbol(self, owner) == crate::semantics::workspace_symbol(self, provider))
                    {
                        continue;
                    }
                    // A more-specific default replaces this provider on the
                    // concrete descendant. Check that implementation against
                    // every inherited contract, rather than demanding that an
                    // overridden wider default return the narrower type.
                    // A unique interface implementation must dominate every
                    // getter branch; sibling defaults remain uncertain.
                    let getter_owners = inherited_getters
                        .iter()
                        .map(|(owner, _, _, _)| crate::semantics::workspace_symbol(self, owner))
                        .collect::<HashSet<_>>();
                    let mut seen_defaults = HashSet::new();
                    let effective_defaults = inherited_getters
                        .iter()
                        .filter(|(owner, _, _, member)| {
                            if owner.kind != DeclarationKind::Interface
                                || !member.has_body
                                || !getter_is_public(member, owner)
                            {
                                return false;
                            }
                            let identity = crate::semantics::workspace_symbol(self, owner);
                            seen_defaults.insert((identity.clone(), member.name.clone()))
                                && getter_owner_ancestors(&getter_ancestry, &identity)
                                    .is_some_and(|ancestors| ancestors.is_superset(&getter_owners))
                        })
                        .collect::<Vec<_>>();
                    let (effective_provider, effective_property) =
                        match effective_defaults.as_slice() {
                            [(owner, _, _, member)] => (*owner, *member),
                            _ => (provider, property),
                        };
                    let provider_candidates = inherited_getters
                        .iter()
                        .filter(|(owner, _, _, _)| {
                            crate::semantics::workspace_symbol(self, owner).stable_key()
                                == crate::semantics::workspace_symbol(self, effective_provider)
                                    .stable_key()
                        })
                        .collect::<Vec<_>>();
                    let provider_types = provider_candidates
                        .iter()
                        .filter_map(|(_, raw_type, effective_type, _)| {
                            Some((raw_type.as_deref()?, effective_type.as_ref()?))
                        })
                        .collect::<Vec<_>>();
                    if provider_types.len() != provider_candidates.len()
                        || provider_types.is_empty()
                        || provider_types
                            .iter()
                            .any(|(_, effective)| *effective != provider_types[0].1)
                    {
                        return Some(DefaultGetterConflict {
                            provider: crate::semantics::workspace_symbol(self, provider),
                            implementation: crate::semantics::workspace_symbol(self, descendant),
                            competing_owner: crate::semantics::workspace_symbol(self, provider),
                            property: property_name.clone(),
                            provider_type: property_type.map(str::to_owned),
                            competing_type: None,
                        });
                    }
                    for (parent, inherited_type, effective_type, inherited_member) in
                        &inherited_getters
                    {
                        if crate::semantics::workspace_symbol(self, parent).stable_key()
                            == crate::semantics::workspace_symbol(self, effective_provider)
                                .stable_key()
                        {
                            continue;
                        }
                        let Some(inherited_type) = inherited_type.as_deref() else {
                            // An unknown inherited getter type cannot prove
                            // compatibility with the provider's ABI.
                            return Some(DefaultGetterConflict {
                                provider: crate::semantics::workspace_symbol(self, provider),
                                implementation: crate::semantics::workspace_symbol(
                                    self, descendant,
                                ),
                                competing_owner: crate::semantics::workspace_symbol(self, parent),
                                property: property_name.clone(),
                                provider_type: property_type.map(str::to_owned),
                                competing_type: None,
                            });
                        };
                        let Some(effective_type) = effective_type.as_ref() else {
                            return Some(DefaultGetterConflict {
                                provider: crate::semantics::workspace_symbol(self, provider),
                                implementation: crate::semantics::workspace_symbol(
                                    self, descendant,
                                ),
                                competing_owner: crate::semantics::workspace_symbol(self, parent),
                                property: property_name.clone(),
                                provider_type: property_type.map(str::to_owned),
                                competing_type: Some(inherited_type.to_owned()),
                            });
                        };
                        for (provider_raw_type, provider_effective_type) in &provider_types {
                            if effective_type == *provider_effective_type
                                || self.default_method_erases_to_java_getter(
                                    effective_provider,
                                    effective_property,
                                    provider_raw_type,
                                    parent,
                                    inherited_type,
                                )
                                || (self.default_getter_has_proven_covariant_reference_return(
                                    effective_provider,
                                    effective_property,
                                    provider_raw_type,
                                    parent,
                                    inherited_member,
                                    inherited_type,
                                ) && allow_repairs
                                    && crate::default_getter_repair::can_repair_contract(
                                        self,
                                        descendant,
                                        provider,
                                        &property_accessor_name(&property_name),
                                        retained,
                                        translation_roots,
                                    ))
                            {
                                continue;
                            }
                            return Some(DefaultGetterConflict {
                                provider: crate::semantics::workspace_symbol(self, provider),
                                implementation: crate::semantics::workspace_symbol(
                                    self, descendant,
                                ),
                                competing_owner: crate::semantics::workspace_symbol(self, parent),
                                property: property_name.clone(),
                                provider_type: Some((*provider_raw_type).to_owned()),
                                competing_type: Some(inherited_type.to_owned()),
                            });
                        }
                    }
                }
            }
        }
        None
    }

    /// Prove that a public, non-null Kotlin default getter returns a source
    /// reference subtype of one inherited getter contract. This is deliberately
    /// narrower than general override compatibility: a method's owner type
    /// parameter may use one explicit nonnullable reference bound, but generic
    /// bounds, aliases and method type parameters remain unknown. This proves
    /// logical covariance only; the caller still requires a concrete bridge
    /// plan when JVM erasures differ.
    fn default_getter_has_proven_covariant_reference_return(
        &self,
        provider: &Declaration,
        provider_member: &Member,
        provider_type: &str,
        parent: &Declaration,
        inherited_member: &Member,
        inherited_type: &str,
    ) -> bool {
        if provider_member.is_static
            || provider_member.is_nullable
            || !provider_member.parameter_types.is_empty()
            || !getter_is_public(provider_member, provider)
            || inherited_member.is_static
            || !getter_is_public(inherited_member, parent)
            || !matches!(
                provider_member.kind,
                MemberKind::Property | MemberKind::Method
            )
            || !matches!(
                inherited_member.kind,
                MemberKind::Property | MemberKind::Method
            )
        {
            return false;
        }
        let Some(provider_file) = self.declaration_source_file(provider) else {
            return false;
        };
        let Some(parent_file) = self.declaration_source_file(parent) else {
            return false;
        };
        if declaration_type_parameter_shadows(parent, inherited_type) {
            return false;
        }
        if (provider_member.kind == MemberKind::Method
            && !getter_has_no_method_type_parameters(
                provider_file,
                provider,
                &provider_member.name,
            ))
            || (inherited_member.kind == MemberKind::Method
                && !getter_has_no_method_type_parameters(
                    parent_file,
                    parent,
                    &inherited_member.name,
                ))
        {
            return false;
        }

        let provider_bound = if declaration_type_parameter_shadows(provider, provider_type) {
            // A Kotlin method returning T is a subtype of its declared bound.
            // Do not replace T with that bound in the emitted signature or
            // assume equal JVM descriptors: bridge planning checks those
            // separately for each concrete descendant.
            if provider.language != SourceLanguage::Kotlin
                || provider_member.kind != MemberKind::Method
                || !provider
                    .type_params
                    .iter()
                    .any(|parameter| parameter == provider_type)
            {
                return false;
            }
            let Some(bound) = crate::property_abi::inline_type_parameter_upper_bound(
                self,
                provider,
                provider_type,
            ) else {
                return false;
            };
            if bound.contains('?') || declaration_type_parameter_shadows(provider, &bound) {
                return false;
            }
            Some(bound)
        } else {
            None
        };
        let provider_type = provider_bound.as_deref().unwrap_or(provider_type);

        let Some(provider_decl) = exact_source_reference_type(self, provider_file, provider_type)
        else {
            return false;
        };
        if provider_decl.is_jvm_value_class || !provider_decl.type_params.is_empty() {
            return false;
        }
        // Ordinary source reference types have java.lang.Object as a JVM
        // supertype. Keep the name proof scoped: an imported/custom Any,
        // an alias, or an unknown wildcard cannot establish this contract.
        let inherited_base = inherited_type.trim().trim_end_matches('?');
        if canonical_getter_named_identity(self, parent_file, inherited_base).as_deref()
            == Some("kotlin.Any")
            && (inherited_base.contains('.')
                || parent_file
                    .imports
                    .iter()
                    .any(|import| matches!(import.as_str(), "kotlin.Any" | "java.lang.Object"))
                || !parent_file
                    .imports
                    .iter()
                    .any(|import| import.ends_with(".*")))
        {
            return true;
        }
        let Some(parent_decl) = exact_source_reference_type(self, parent_file, inherited_type)
        else {
            return false;
        };
        if !matches!(
            provider_decl.kind,
            DeclarationKind::Class
                | DeclarationKind::Interface
                | DeclarationKind::Enum
                | DeclarationKind::Object
        ) || !matches!(
            parent_decl.kind,
            DeclarationKind::Class
                | DeclarationKind::Interface
                | DeclarationKind::Enum
                | DeclarationKind::Object
        ) || !provider_decl.type_params.is_empty()
            || !parent_decl.type_params.is_empty()
        {
            return false;
        }

        let target = crate::semantics::workspace_symbol(self, parent_decl).stable_key();
        let mut pending = vec![provider_decl];
        let mut visited = HashSet::new();
        let mut explored = 0usize;
        while let Some(current) = pending.pop() {
            explored += 1;
            if explored > 256 {
                return false;
            }
            let current_key = crate::semantics::workspace_symbol(self, current).stable_key();
            if current_key == target {
                return true;
            }
            if !visited.insert(current_key) {
                continue;
            }
            let Some(current_file) = self.declaration_source_file(current) else {
                continue;
            };
            for supertype in &current.supertypes {
                // This proof does not perform generic substitution. Other
                // non-generic source edges can still prove the path.
                if supertype.contains(['<', '>', '?', '&', '(', ')', '[', ']']) {
                    continue;
                }
                if let Some(next) = exact_source_reference_type(self, current_file, supertype) {
                    pending.push(next);
                }
            }
        }
        false
    }

    /// An actual Kotlin default method returning its owner's type parameter
    /// can already have exactly the descriptor declared by a Java ancestor.
    /// This proves equality of the explicit first bound, not general return
    /// covariance, and deliberately does not waive Kotlin property safeguards.
    fn default_method_erases_to_java_getter(
        &self,
        provider: &Declaration,
        method: &Member,
        return_type: &str,
        parent: &Declaration,
        inherited_type: &str,
    ) -> bool {
        if method.kind != MemberKind::Method
            || method.is_nullable
            || !method.parameter_types.is_empty()
            || parent.language != SourceLanguage::Java
            || !provider
                .type_params
                .iter()
                .any(|parameter| parameter == return_type)
            || parent
                .type_params
                .iter()
                .any(|parameter| parameter == inherited_type)
            || inherited_type.contains(['<', '>', '?', '&', '(', ')', '[', ']'])
        {
            return false;
        }
        let Some(bound) =
            crate::property_abi::inline_type_parameter_upper_bound(self, provider, return_type)
        else {
            return false;
        };
        if bound.contains(['<', '>', '?', '&', '(', ')', '[', ']', ' '])
            || provider
                .type_params
                .iter()
                .any(|parameter| parameter == &bound)
            || matches!(
                bound.as_str(),
                "Any"
                    | "Nothing"
                    | "Unit"
                    | "Boolean"
                    | "Byte"
                    | "Short"
                    | "Int"
                    | "Long"
                    | "Float"
                    | "Double"
                    | "Char"
            )
        {
            return false;
        }
        let Some(provider_file) = self.declaration_source_file(provider) else {
            return false;
        };
        let Some(parent_file) = self.declaration_source_file(parent) else {
            return false;
        };
        if !java_getter_has_no_method_type_parameters(parent_file, parent, &method.name) {
            return false;
        }
        let Some(bound_declaration) = resolve_unique_getter_type(self, provider_file, &bound)
        else {
            return false;
        };
        let Some(inherited_declaration) =
            resolve_unique_getter_type(self, parent_file, inherited_type)
        else {
            return false;
        };
        // A generic bound needs structured erasure analysis of its own.
        bound_declaration.type_params.is_empty()
            && inherited_declaration.type_params.is_empty()
            && crate::semantics::workspace_symbol(self, bound_declaration)
                == crate::semantics::workspace_symbol(self, inherited_declaration)
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

    /// True when any indexed Kotlin or Java declaration names `name` as a
    /// supertype. Used by ABI repairs that introduce fields: the repair is
    /// safe only when no descendant can observe an overridden property through
    /// a parent-typed reference.
    pub fn has_any_subtype(&self, name: &str) -> bool {
        self.has_subtype_named(name)
    }

    /// Direct Kotlin or Java subtype names recorded for this resolved declaration.
    /// Callers that require collision-free identity must still resolve each
    /// candidate in its declaring-file context.
    pub fn direct_subtype_names(&self, target: &Declaration) -> Vec<String> {
        let key = declaration_key(target);
        let target_name = key.rsplit_once('.').map(|(_, name)| name).unwrap_or(&key);
        let mut names = self
            .all_subtype_names
            .get(target_name)
            .cloned()
            .unwrap_or_default();
        names.sort();
        names.dedup();
        names
    }

    pub fn direct_subtypes<'a>(&'a self, target: &Declaration) -> Vec<&'a Declaration> {
        let target_address = target as *const Declaration as usize;
        if self.declaration_files.contains_key(&target_address) {
            return self
                .direct_subtype_declarations
                .get(&target_address)
                .into_iter()
                .flatten()
                .filter_map(|(file_index, declaration_index)| {
                    self.files
                        .get(*file_index)?
                        .declarations
                        .get(*declaration_index)
                })
                .collect();
        }
        self.direct_subtypes_fallback(target)
    }

    /// Preserve behavior for borrowed declarations that are not owned by this
    /// index (for example a caller-provided clone). Indexed declarations take
    /// the exact cached path above.
    fn direct_subtypes_fallback<'a>(&'a self, target: &Declaration) -> Vec<&'a Declaration> {
        let target_path = self.declaration_source_file(target).map(|file| &file.path);
        self.direct_subtype_names(target)
            .into_iter()
            .filter_map(|name| self.declaration_names.get(&name))
            .flatten()
            .filter_map(|(file_index, declaration_index)| {
                let file = self.files.get(*file_index)?;
                let declaration = file.declarations.get(*declaration_index)?;
                declaration
                    .supertypes
                    .iter()
                    .any(|supertype| {
                        self.resolve_type(file, supertype).is_some_and(|resolved| {
                            resolved.name == target.name
                                && self.declaration_source_file(resolved).is_some_and(
                                    |resolved_file| target_path == Some(&resolved_file.path),
                                )
                        })
                    })
                    .then_some(declaration)
            })
            .collect()
    }

    pub fn has_unresolved_supertype(&self, target: &Declaration) -> bool {
        let qualified = target
            .package
            .as_deref()
            .map(|package| format!("{package}.{}", target.name));
        self.unresolved_supertype_names.iter().any(|raw| {
            if raw.contains('.') {
                qualified.as_deref() == Some(raw.as_str())
            } else {
                raw == &target.name
            }
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
        retained: &HashSet<crate::semantics::SymbolId>,
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
        retained: &HashSet<crate::semantics::SymbolId>,
    ) -> Vec<String> {
        let mut names: Vec<String> = self
            .direct_subtypes(target)
            .into_iter()
            .filter(|declaration| {
                declaration.language == SourceLanguage::Kotlin
                    && self.declaration_retained(declaration, retained)
            })
            .map(|declaration| declaration.name.clone())
            .collect();
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

    /// Binding types used by N6C94 planning and the residual-Kotlin rewrite.
    /// Inferred locals are resolved per smart-cast site and may override a
    /// same-named explicit binding from another function in the file. A single
    /// flat table is returned only when every relevant site for that name has
    /// the same resolved inferred type; otherwise that name is removed so the
    /// planner fails closed.
    pub fn smart_cast_bindings_for_path(&self, path: &Path) -> HashMap<String, String> {
        let Some(file) = self.source_file(path) else {
            return HashMap::new();
        };
        let mut bindings = file.bindings.clone();
        let inferred_names: HashSet<&str> = file
            .inferred_binding_calls
            .iter()
            .map(|call| call.name.as_str())
            .collect();
        for name in inferred_names {
            let sites: Vec<_> = file
                .smart_cast_sites
                .iter()
                .filter(|site| site.needs_repair() && site.receiver == name)
                .collect();
            if sites.is_empty() {
                continue;
            }
            let mut common_type = None;
            let mut consistent = true;
            for site in sites {
                let resolved: HashSet<String> = file
                    .inferred_binding_calls
                    .iter()
                    .filter(|call| {
                        call.name == name
                            && call.scope_start <= site.insert_at
                            && site.insert_at < call.scope_end
                    })
                    .filter_map(|call| self.inferred_binding_type(file, call))
                    .collect();
                if resolved.len() != 1 {
                    consistent = false;
                    break;
                }
                let inferred = resolved.into_iter().next().expect("one resolved type");
                match &common_type {
                    Some(existing) if existing != &inferred => {
                        consistent = false;
                        break;
                    }
                    Some(_) => {}
                    None => common_type = Some(inferred),
                }
            }
            if consistent && let Some(inferred) = common_type {
                bindings.insert(name.to_string(), inferred);
            } else {
                bindings.remove(name);
            }
        }
        bindings
    }

    fn inferred_binding_type(
        &self,
        file: &SourceFile,
        call: &crate::smart_cast::InferredBindingCall,
    ) -> Option<String> {
        let enclosing = file
            .declarations
            .iter()
            .find(|declaration| declaration.name == call.enclosing_type)?;
        let return_type = self.method_return_type_on_declaration(
            file,
            enclosing,
            &call.callee,
            &mut HashSet::new(),
        )?;
        optional_element_type(&return_type)
    }

    fn method_return_type_on_declaration(
        &self,
        source_file: &SourceFile,
        declaration: &Declaration,
        method: &str,
        visited: &mut HashSet<String>,
    ) -> Option<String> {
        if !visited.insert(declaration_key(declaration)) {
            return None;
        }
        if let Some(return_type) = declaration.members.iter().find_map(|member| {
            (member.kind == MemberKind::Method && member.name == method)
                .then(|| member.type_name.clone())
                .flatten()
        }) {
            return Some(return_type);
        }
        let declaring_file = self
            .declaration_source_file(declaration)
            .unwrap_or(source_file);
        declaration.supertypes.iter().find_map(|supertype| {
            let parent = self.resolve_type(declaring_file, supertype)?;
            self.method_return_type_on_declaration(declaring_file, parent, method, visited)
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
        for file in self.kotlin_files() {
            if paths_match(&file.path, declaring_file)
                || file
                    .identifier_counts
                    .get(&target.name)
                    .is_none_or(|count| *count == 0)
            {
                continue;
            }
            let bindings = self.smart_cast_bindings_for_path(&file.path);
            for site in &file.smart_cast_sites {
                // A site nothing depends on cannot break when the owner moves:
                // a single getter call narrows nothing that needs narrowing.
                if !properties.contains(site.property.as_str()) || !site.needs_repair() {
                    continue;
                }
                match bindings.get(&site.receiver) {
                    Some(owner) if owner == &target.name => {
                        if !site.repairable {
                            return false;
                        }
                    }
                    Some(_) => {}
                    // A file-level mention of `target` does not establish that
                    // an unrelated or untyped receiver has that type. Retaining
                    // every declaration that happens to share a common property
                    // name (`type`, `data`, `state`, ...) creates enormous false
                    // positive hierarchy cascades. Only a receiver resolved by
                    // the source index is evidence that moving this target's
                    // getter to Java affects the smart-cast site.
                    None if site.receiver.is_empty() || site.property.is_empty() => {}
                    None => return false,
                }
            }
        }
        true
    }

    /// Whether `type_name` or one of its indexed supertypes declares a
    /// Java-callable zero-argument instance method. This is used by narrow
    /// lowering paths that must reject unknown Kotlin extension calls.
    pub fn type_declares_zero_arg_instance_method(
        &self,
        source_file: &SourceFile,
        type_name: &str,
        method_name: &str,
    ) -> bool {
        let Some(declaration) = self.resolve_kotlin_type(source_file, type_name) else {
            return false;
        };
        let mut visited = HashSet::new();
        let mut pending = vec![declaration];
        while let Some(current) = pending.pop() {
            if !visited.insert(declaration_key(current)) {
                continue;
            }
            if current.members.iter().any(|member| {
                member.kind == MemberKind::Method
                    && !member.is_static
                    && member.name == method_name
                    && member.parameter_types.is_empty()
            }) {
                return true;
            }
            let current_file = self.declaration_source_file(current).unwrap_or(source_file);
            for supertype in &current.supertypes {
                if let Some(parent) = self.resolve_type(current_file, supertype) {
                    pending.push(parent);
                }
            }
        }
        false
    }

    pub fn nullable_property_narrowing_conflict(
        &self,
        source_file: &SourceFile,
        target: &Declaration,
    ) -> Option<(String, String, String)> {
        target.supertypes.iter().find_map(|supertype| {
            let contract = self.resolve_kotlin_type(source_file, supertype)?;
            let (_, supplied_args) = split_generic(supertype.trim().trim_end_matches("()"));
            let bindings: HashMap<String, String> = contract
                .type_params
                .iter()
                .zip(supplied_args.unwrap_or_default())
                .map(|(parameter, argument)| (parameter.clone(), argument.to_string()))
                .collect();
            contract.members.iter().find_map(|contract_member| {
                if contract_member.kind != MemberKind::Property || !contract_member.is_nullable {
                    return None;
                }
                target.members.iter().find_map(|member| {
                    if member.kind != MemberKind::Property
                        || member.name != contract_member.name
                        || member.is_nullable
                    {
                        return None;
                    }
                    let inherited_type = substitute_type_parameters(
                        contract_member.type_name.as_deref().unwrap_or("<unknown>"),
                        &bindings,
                    );
                    let implementation_type = member.type_name.as_deref().unwrap_or("<unknown>");
                    let inherited_base = inherited_type.trim().trim_end_matches('?').trim();
                    let primitive_unboxing = inherited_base == implementation_type.trim()
                        && matches!(
                            inherited_base,
                            "Boolean"
                                | "Byte"
                                | "Short"
                                | "Int"
                                | "Long"
                                | "Float"
                                | "Double"
                                | "Char"
                        );
                    let classification = match (
                        contract_member.type_name.as_ref(),
                        member.type_name.as_ref(),
                    ) {
                        (Some(_), Some(_)) => classify_member_conflict(
                            self,
                            source_file,
                            contract,
                            &inherited_type,
                            implementation_type,
                        ),
                        _ => MemberConflictClass::UnknownType,
                    };
                    let contract_file = self
                        .declaration_source_file(contract)
                        .unwrap_or(source_file);
                    let collection_covariance = declaration_covariant_return_is_subtype(
                        self,
                        source_file,
                        implementation_type,
                        contract_file,
                        &inherited_type,
                    );
                    if !primitive_unboxing
                        && (collection_covariance
                            || matches!(
                                classification,
                                MemberConflictClass::Exact
                                    | MemberConflictClass::JavaCovariantReturn
                            ))
                    {
                        return None;
                    }
                    Some((
                        member.name.clone(),
                        inherited_type,
                        implementation_type.to_string(),
                    ))
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

    /// Whether a retained-property-interface boundary on `target` is safe for
    /// Java because every inherited retained Kotlin property it implements is
    /// exact, scalar-Java-covariant, or an explicitly repaired read-only List
    /// contract. This is intentionally stricter than ordinary Kotlin
    /// declaration-site variance: generic List covariance is accepted only
    /// when the retained interface source carries the exact fully-qualified
    /// `@kotlin.jvm.JvmWildcard` ABI marker.
    pub fn retained_property_implementation_interfaces_java_compatible(
        &self,
        source_file: &SourceFile,
        target: &Declaration,
        translation_roots: &[PathBuf],
        retained: &HashSet<crate::semantics::SymbolId>,
    ) -> Option<HashSet<crate::semantics::SymbolId>> {
        if target.kind == DeclarationKind::Interface {
            // Interface-to-interface fake overrides can carry physical
            // default-getter and annotation obligations beyond Java return
            // substitutability. Keep the existing specialized bridge
            // analysis responsible for that case.
            return None;
        }
        let own_properties = target
            .members
            .iter()
            .filter(|member| member.kind == MemberKind::Property && !member.is_static)
            .collect::<Vec<_>>();
        if own_properties.is_empty() {
            return None;
        }
        let conflicts = self.retained_supertype_member_conflicts_in_file(
            &source_file.path,
            &target.supertypes,
            &target.name,
        );
        let mut pending = target
            .supertypes
            .iter()
            .cloned()
            .map(|supertype| (source_file, supertype))
            .collect::<Vec<_>>();
        let mut visited = HashSet::new();
        let mut checked_contract = false;
        let mut marker_contract = false;
        let mut checked_interfaces = HashSet::new();
        while let Some((context, supertype)) = pending.pop() {
            let parent = self.resolve_type(context, &supertype)?;
            let key = declaration_key(parent);
            if !visited.insert(key.clone()) {
                continue;
            }
            let parent_file = self.declaration_source_file(parent)?;
            pending.extend(
                parent
                    .supertypes
                    .iter()
                    .cloned()
                    .map(|parent_supertype| (parent_file, parent_supertype)),
            );
            if parent.language != SourceLanguage::Kotlin
                || !self.declaration_retained(parent, retained)
            {
                continue;
            }
            for contract in parent
                .members
                .iter()
                .filter(|member| member.kind == MemberKind::Property && !member.is_static)
            {
                let implementation = own_properties
                    .iter()
                    .find(|member| member.name == contract.name)?;
                checked_contract = true;
                checked_interfaces.insert(crate::semantics::workspace_symbol(self, parent));
                if contract.is_mutable
                    || implementation.is_mutable
                    || contract.is_jvm_field
                    || implementation.is_jvm_field
                    || contract.has_unsupported_property_shape
                    || implementation.has_unsupported_property_shape
                    || contract.has_custom_accessor
                    || implementation.has_custom_accessor
                    || (implementation.has_unsupported_property_annotations
                        || contract.has_unsupported_property_annotations
                            && !readonly_list_wildcard_property_annotations_supported(
                                parent_file,
                                parent,
                                &contract.name,
                            ))
                {
                    return None;
                }
                let conflict = conflicts.iter().find(|conflict| {
                    conflict.member_name == contract.name
                        && conflict.kind == MemberKind::Property
                        && conflict.supertype == key
                })?;
                if conflict.inherited_type.trim() != conflict.implementation_type.trim() {
                    // Only ordinary reference-return covariance is accepted
                    // here. Generic shapes must be exact or pass the explicit
                    // marker path above.
                    let marker_proven = readonly_list_jvm_wildcard_abi_proven(
                        self,
                        parent_file,
                        parent,
                        &contract.name,
                        &conflict.inherited_type,
                    );
                    if marker_proven
                        && conflict.legacy_mismatch
                        && self.selected_covariant_property_conflict_is_safe(
                            &source_file.path,
                            conflict,
                            translation_roots,
                            retained,
                        )
                    {
                        marker_contract = true;
                        continue;
                    }
                    if conflict.inherited_type.contains('<')
                        || conflict.implementation_type.contains('<')
                        || conflict.implementation_type.trim().ends_with('?')
                            && !conflict.inherited_type.trim().ends_with('?')
                        || !self.property_getter_return_compatible_in_files(
                            &source_file.path,
                            &conflict.implementation_type,
                            &parent_file.path,
                            &conflict.inherited_type,
                        )
                    {
                        return None;
                    }
                }
            }
        }
        (checked_contract && marker_contract).then_some(checked_interfaces)
    }

    pub fn retained_property_implementations_are_java_compatible(
        &self,
        source_file: &SourceFile,
        target: &Declaration,
        translation_roots: &[PathBuf],
        retained: &HashSet<crate::semantics::SymbolId>,
    ) -> bool {
        self.retained_property_implementation_interfaces_java_compatible(
            source_file,
            target,
            translation_roots,
            retained,
        )
        .is_some()
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
        let file = self
            .declaration_files
            .get(&(declaration as *const Declaration as usize))
            .and_then(|index| self.files.get(*index));
        default_getters::record_declaration_lookup(declaration);
        if let Some(file) = file {
            default_getters::record_source_path(&file.path);
        }
        file
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
            self.qualified_declarations.get(qualified)?.iter().find_map(
                |(file_index, declaration_index)| {
                    let declaration = self
                        .files
                        .get(*file_index)?
                        .declarations
                        .get(*declaration_index)?;
                    (declaration.language == SourceLanguage::Kotlin).then_some(declaration)
                },
            )
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
        let mut matches = self.declaration_names.get(simple)?.iter().filter_map(
            |(file_index, declaration_index)| {
                let declaration = self
                    .files
                    .get(*file_index)?
                    .declarations
                    .get(*declaration_index)?;
                (declaration.language == SourceLanguage::Kotlin).then_some(declaration)
            },
        );
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    pub fn resolve_type<'a>(
        &'a self,
        source_file: &SourceFile,
        type_name: &str,
    ) -> Option<&'a Declaration> {
        default_getters::record_type_resolution(self, source_file, type_name);
        let base = type_name
            .trim()
            .trim_end_matches('?')
            .split('<')
            .next()
            .unwrap_or("")
            .split('(')
            .next()
            .unwrap_or("")
            .trim();
        let simple = base.rsplit('.').next().unwrap_or("");
        if base.contains('.') {
            return self.find_qualified(base);
        }
        for import in &source_file.imports {
            if import.ends_with(&format!(".{simple}"))
                && let Some(found) = self.find_qualified(import)
            {
                return Some(found);
            }
        }
        if let Some(package) = &source_file.package
            && let Some(found) = self.find_qualified(&format!("{package}.{simple}"))
        {
            return Some(found);
        }
        let mut wildcard_matches = source_file
            .imports
            .iter()
            .filter_map(|import| import.strip_suffix(".*"))
            .filter_map(|package| self.find_qualified(&format!("{package}.{simple}")));
        if let Some(found) = wildcard_matches.next() {
            return wildcard_matches
                .all(|other| declaration_key(other) == declaration_key(found))
                .then_some(found);
        }
        let mut matches = self.declaration_names.get(simple)?.iter().filter_map(
            |(file_index, declaration_index)| {
                self.files
                    .get(*file_index)?
                    .declarations
                    .get(*declaration_index)
            },
        );
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
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
        let (file_index, declaration_index) =
            self.qualified_declarations.get(qualified)?.first()?;
        self.files
            .get(*file_index)?
            .declarations
            .get(*declaration_index)
    }
}

fn source_types_have_same_identity(
    index: &SourceIndex,
    own_file: &SourceFile,
    own_type: &str,
    contract_file: &SourceFile,
    contract_type: &str,
) -> bool {
    let own_resolved = index.resolve_type(own_file, own_type);
    let contract_resolved = index.resolve_type(contract_file, contract_type);
    if own_type == "UUID"
        && contract_type == "UUID"
        && (own_file
            .imports
            .iter()
            .any(|import| import == "java.util.*")
            || contract_file
                .imports
                .iter()
                .any(|import| import == "java.util.*"))
    {
        let explicit_import = |file: &SourceFile| {
            file.imports
                .iter()
                .find(|import| import.rsplit('.').next() == Some("UUID"))
                .cloned()
        };
        let own_explicit = explicit_import(own_file);
        let contract_explicit = explicit_import(contract_file);
        match (&own_explicit, &contract_explicit) {
            (Some(left), Some(right)) if left == right => {
                // The same explicit import wins over all wildcard imports.
                return left == "java.util.UUID"
                    || matches!(
                        (index.resolve_type(own_file, own_type), index.resolve_type(contract_file, contract_type)),
                        (Some(left), Some(right)) if declaration_key(left) == declaration_key(right)
                    );
            }
            (Some(_), Some(_)) => return false,
            (Some(left), None)
                if left == "java.util.UUID"
                    && contract_file
                        .imports
                        .iter()
                        .any(|import| import == "java.util.*") => {}
            (None, Some(right))
                if right == "java.util.UUID"
                    && own_file
                        .imports
                        .iter()
                        .any(|import| import == "java.util.*") => {}
            (None, None) => {}
            _ => return false,
        }
        let indexed_uuid_collision = index
            .declarations()
            .any(|declaration| declaration.name == "UUID");
        if indexed_uuid_collision {
            // Allow a real same-package domain UUID to resolve normally below;
            // never let the unqualified global-name fallback masquerade as a
            // java.util wildcard import.
            let same_package_domain_type = own_file.package == contract_file.package
                && own_file.package.as_ref().is_some_and(|package| {
                    index.declarations().any(|declaration| {
                        declaration.name == "UUID" && declaration.package.as_ref() == Some(package)
                    })
                });
            if !same_package_domain_type {
                return false;
            }
        } else if (own_explicit.as_deref() == Some("java.util.UUID")
            || own_file
                .imports
                .iter()
                .any(|import| import == "java.util.*"))
            && (contract_explicit.as_deref() == Some("java.util.UUID")
                || contract_file
                    .imports
                    .iter()
                    .any(|import| import == "java.util.*"))
        {
            return true;
        }
    }
    if own_resolved.is_some_and(|declaration| {
        !declaration_is_visible_from(index, own_file, own_type, declaration)
    }) || contract_resolved.is_some_and(|declaration| {
        !declaration_is_visible_from(index, contract_file, contract_type, declaration)
    }) {
        return false;
    }
    // Kotlin's scalar builtins resolve in the compiler rather than the source
    // index. Their identical unshadowed spellings are exact JVM types across
    // source files, so retain that exact identity proof without extending the
    // same-name fallback to user declarations.
    let builtin_name = own_type.trim_end_matches('?').trim();
    if own_type == contract_type
        && matches!(
            builtin_name,
            "String" | "Boolean" | "Byte" | "Short" | "Int" | "Long" | "Float" | "Double" | "Char"
        )
        && own_resolved.is_none()
        && contract_resolved.is_none()
        && index.declarations_named(builtin_name).next().is_none()
        && !own_file
            .imports
            .iter()
            .chain(contract_file.imports.iter())
            .any(|import| {
                import.split_once(" as ").map_or_else(
                    || import.rsplit('.').next() == Some(builtin_name),
                    |(_, alias)| alias.trim() == builtin_name,
                )
            })
    {
        return true;
    }
    let (Some(own), Some(contract)) = (own_resolved, contract_resolved) else {
        if own_resolved.is_some() || contract_resolved.is_some() {
            return false;
        }
        if own_type != contract_type {
            return false;
        }
        let imported = |file: &SourceFile, name: &str| {
            file.imports
                .iter()
                .find(|import| import.rsplit('.').next() == Some(name))
                .cloned()
        };
        return (own_type.contains('.')
            && contract_type.contains('.')
            && own_type == contract_type)
            || matches!(
                (imported(own_file, own_type), imported(contract_file, contract_type)),
                (Some(left), Some(right)) if left == right
            );
    };
    declaration_key(own) == declaration_key(contract)
}

fn type_reference_is_visible(index: &SourceIndex, file: &SourceFile, ty: &str) -> bool {
    let base = ty
        .trim()
        .trim_end_matches('?')
        .split('<')
        .next()
        .unwrap_or("")
        .trim();
    if base == "UUID" && file.imports.iter().any(|import| import == "java.util.UUID") {
        return true;
    }
    let Some(declaration) = index.resolve_type(file, base) else {
        return true;
    };
    declaration_is_visible_from(index, file, base, declaration)
}

fn declaration_is_visible_from(
    index: &SourceIndex,
    file: &SourceFile,
    type_name: &str,
    declaration: &Declaration,
) -> bool {
    let base = type_name
        .trim()
        .trim_end_matches('?')
        .split('<')
        .next()
        .unwrap_or("")
        .trim();
    let key = declaration_key(declaration);
    if base.contains('.') {
        return base == key;
    }
    let simple = base.rsplit('.').next().unwrap_or(base);
    let explicit = file
        .imports
        .iter()
        .filter(|import| import.rsplit('.').next() == Some(simple))
        .collect::<Vec<_>>();
    if !explicit.is_empty() {
        return explicit
            .iter()
            .all(|import| import.as_str() == key.as_str());
    }
    let wildcard_matches = file
        .imports
        .iter()
        .filter_map(|import| import.strip_suffix(".*"))
        .filter_map(|package| index.find_qualified(&format!("{package}.{simple}")))
        .collect::<Vec<_>>();
    if !wildcard_matches.is_empty() {
        return wildcard_matches.len() == 1 && declaration_key(wildcard_matches[0]) == key;
    }
    file.package.as_ref() == declaration.package.as_ref()
}

fn declaration_key(declaration: &Declaration) -> String {
    match &declaration.package {
        Some(package) => format!("{package}.{}", declaration.name),
        None => declaration.name.clone(),
    }
}

fn property_getter_name(property: &str) -> String {
    let mut chars = property.chars();
    match chars.next() {
        Some(first) => format!("get{}{}", first.to_uppercase(), chars.as_str()),
        None => "get".to_string(),
    }
}

fn property_name_from_getter(getter: &str) -> Option<String> {
    let suffix = getter.strip_prefix("get")?;
    let mut chars = suffix.chars();
    let first = chars.next()?;
    if !first.is_uppercase() {
        return None;
    }
    let second = chars.clone().next();
    if second.is_some_and(char::is_uppercase) {
        return Some(suffix.to_string());
    }
    Some(first.to_lowercase().chain(chars).collect())
}

fn member_property_contract_name(member: &Member) -> Option<String> {
    match member.kind {
        MemberKind::Property => Some(member.name.clone()),
        MemberKind::Method if member.parameter_types.is_empty() => {
            property_name_from_getter(&member.name)
        }
        _ => None,
    }
}

fn member_is_property_getter(member: &Member, property: &str) -> bool {
    !member.is_static
        && member.visibility.as_deref() != Some("private")
        && (member.kind == MemberKind::Property && member.name == property
            || member.kind == MemberKind::Method
                && member.parameter_types.is_empty()
                && property_name_from_getter(&member.name).as_deref() == Some(property)
                && member.name == property_getter_name(property))
}

fn declaration_declares_property_getter(declaration: &Declaration, property: &str) -> bool {
    declaration
        .members
        .iter()
        .any(|member| member_is_property_getter(member, property))
}

fn paths_match(left: &Path, right: &Path) -> bool {
    normalized_path(left) == normalized_path(right)
}

fn normalized_path(path: &Path) -> String {
    // Indexed files and declaration locations are already absolute canonical
    // paths. Compatibility scans compare them many times; do not repeat a
    // filesystem lookup for every comparison. Relative overlay inputs are
    // canonicalized at ingestion, and source_file canonicalizes lookup inputs.
    let absolute;
    let path = if path.is_absolute() {
        path
    } else {
        absolute = absolute_source_path(path);
        absolute.as_path()
    };
    let value = path.to_string_lossy().replace('\\', "/");
    let value = value.strip_prefix("//?/").unwrap_or(&value);
    if cfg!(windows) {
        value.to_ascii_lowercase()
    } else {
        value.to_string()
    }
}

fn absolute_source_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    })
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
        // A listed entry can vanish between `read_dir` and the open (an
        // overlapping migrator, a concurrent cleanup). Skip it with a warning
        // like the tolerant input read: the file simply drops out of this
        // snapshot and the next run sees it again if it reappears.
        let Some(source) = scan_source(&path, &child_relative, language, previous, stats)? else {
            continue;
        };
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
    relative: &Path,
    language: SourceLanguage,
    previous: Option<&CachedSource>,
    stats: &mut IndexStats,
) -> Result<Option<CachedSource>, String> {
    // A listed entry that is already gone (NotFound on open) vanishes from
    // this snapshot with a warning; any other I/O failure stays fatal.
    let vanished = || {
        eprintln!(
            "warning: source vanished during indexing, skipped: {}",
            crate::paths::display(relative)
        );
    };
    let canonical_path = match fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            vanished();
            return Ok(None);
        }
        Err(error) => return Err(format!("{}: {error}", crate::paths::display(path))),
    };
    let metadata = match fs::metadata(&canonical_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            vanished();
            return Ok(None);
        }
        Err(error) => {
            return Err(format!(
                "{}: {error}",
                crate::paths::display(&canonical_path)
            ));
        }
    };
    let size = metadata.len();
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);

    let bytes = match fs::read(&canonical_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            vanished();
            return Ok(None);
        }
        Err(error) => {
            return Err(format!(
                "{}: {error}",
                crate::paths::display(&canonical_path)
            ));
        }
    };
    let digest = *blake3::hash(&bytes).as_bytes();
    if let Some(previous) = previous
        && previous.digest == digest
    {
        let mut cached = previous.source_file.clone();
        cached.path = canonical_path.clone();
        stats.reused_files += 1;
        return Ok(Some(CachedSource {
            size,
            modified_nanos,
            digest,
            source_file: cached,
        }));
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
        inferred_binding_calls,
        ctor_calls,
        kotlin_declaration_count,
        header_facts,
    ) = parse_declarations(&source, language, package.as_deref())?;
    stats.parsed_files += 1;
    Ok(Some(CachedSource {
        size,
        modified_nanos,
        digest,
        source_file: SourceFile {
            path: canonical_path,
            language,
            source_text: source,
            header_facts,
            kotlin_declaration_count,
            package,
            imports,
            declarations,
            type_aliases,
            identifier_counts,
            enum_entries_qualifiers,
            smart_cast_properties,
            smart_cast_sites,
            bindings,
            inferred_binding_calls,
            ctor_calls,
        },
    }))
}

fn should_skip_directory(name: &Path) -> bool {
    // `.notlin` cache dir, plus gradle/cargo build-output mirrors: indexing
    // them shadows migrated sources (`build/k2j/...` twins) and breaks
    // companion-owner resolution.
    name.to_str() == Some(CACHE_DIR) || name.to_str() == Some("build")
}

fn package_name(source: &str) -> Option<String> {
    source.lines().map(str::trim).find_map(|line| {
        let package = line.strip_prefix("package ")?.split(';').next()?.trim();
        (!package.is_empty()).then(|| package.to_string())
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
    Vec<crate::smart_cast::InferredBindingCall>,
    Vec<CtorCall>,
    usize,
    header_facts::HeaderFacts,
);

fn java_annotation_target(source: &str, name: &str, element_type: &str) -> Option<bool> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "annotation_type_declaration"
            && node
                .child_by_field_name("name")
                .and_then(|name_node| name_node.utf8_text(source.as_bytes()).ok())
                == Some(name)
        {
            let declaration = node.utf8_text(source.as_bytes()).ok()?;
            let Some(target) = declaration.find("Target(") else {
                // Java annotations without @Target are applicable everywhere.
                return Some(true);
            };
            let arguments = &declaration[target + "Target(".len()..];
            let close = arguments.find(')')?;
            let arguments = &arguments[..close];
            return Some(
                arguments
                    .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                    .any(|token| token == element_type),
            );
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    None
}

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
    let mut header_facts = header_facts::HeaderFacts::default();
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
        header_facts.record(node, language, source);
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
        if let Some((kind, name_node)) = declaration_shape(node, language, source) {
            let name = node_text(name_node, source)?;
            if !declarations.iter().any(|d: &Declaration| d.name == name) {
                let ctor_params = constructor_param_names(node, language, source);
                declarations.push(Declaration {
                    name: name.clone(),
                    package: package.map(str::to_string),
                    language,
                    kind,
                    is_jvm_value_class: language == SourceLanguage::Kotlin
                        && crate::semantics::is_kotlin_value_class(source, node),
                    is_abstract: node
                        .children(&mut node.walk())
                        .find(|child| child.kind() == "modifiers")
                        .and_then(|modifiers| modifiers.utf8_text(source.as_bytes()).ok())
                        .is_some_and(|modifiers| {
                            modifiers.split_whitespace().any(|word| word == "abstract")
                        }),
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
    let (smart_cast_sites, bindings, inferred_binding_calls) = if language == SourceLanguage::Kotlin
    {
        (
            crate::smart_cast::sites_in(&tree, source),
            crate::smart_cast::bindings_in(&tree, source),
            crate::smart_cast::inferred_binding_calls_in(&tree, source),
        )
    } else {
        (Vec::new(), HashMap::new(), Vec::new())
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
    let kotlin_declaration_count = if language == SourceLanguage::Kotlin {
        crate::translation_plan::ownership_boundary_count(source, tree.root_node())
    } else {
        0
    };
    Ok((
        declarations,
        type_aliases,
        identifier_counts,
        enum_entries_qualifiers,
        smart_cast_properties,
        smart_cast_sites,
        bindings,
        inferred_binding_calls,
        ctor_calls,
        kotlin_declaration_count,
        header_facts,
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

fn declaration_shape<'a>(
    node: tree_sitter::Node<'a>,
    language: SourceLanguage,
    source: &str,
) -> Option<(DeclarationKind, tree_sitter::Node<'a>)> {
    if language == SourceLanguage::Kotlin
        && let Some(name) = crate::semantics::recovered_annotation_class_name_node(node, source)
    {
        return Some((DeclarationKind::Annotation, name));
    }
    if language == SourceLanguage::Kotlin
        && let Some((name, _)) = recovered_interface_parts(node, source)
    {
        let name_node = recovered_interface_name_node(node, source, &name)?;
        return Some((DeclarationKind::Interface, name_node));
    }
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

/// Recover only the parser's known annotated-interface shape: an
/// `annotated_expression`/`ERROR` containing the infix expression
/// `interface Name { ... }`, represented by direct `interface`, name, and
/// lambda children. The declaration name is independently checked by the
/// shared semantics recovery helper, which rejects multiple interface
/// keywords and annotation identifiers masquerading as the name.
fn recovered_interface_parts<'tree>(
    node: tree_sitter::Node<'tree>,
    source: &str,
) -> Option<(String, tree_sitter::Node<'tree>)> {
    if !matches!(node.kind(), "annotated_expression" | "ERROR") {
        return None;
    }
    let name = crate::semantics::recovered_interface_name(node, source)?;
    let mut candidates = Vec::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.id() != node.id()
            && matches!(
                current.kind(),
                "lambda_literal" | "class_declaration" | "object_declaration"
            )
        {
            continue;
        }
        if current.kind() == "infix_expression" {
            let children = current.children(&mut current.walk()).collect::<Vec<_>>();
            let has_interface = children.iter().any(|child| {
                child.kind() == "identifier"
                    && child.utf8_text(source.as_bytes()).ok() == Some("interface")
            });
            let has_name = children.iter().any(|child| {
                matches!(child.kind(), "identifier" | "simple_identifier")
                    && child.utf8_text(source.as_bytes()).ok() == Some(name.as_str())
            });
            let lambdas = children
                .iter()
                .filter(|child| child.kind() == "lambda_literal")
                .copied()
                .collect::<Vec<_>>();
            if has_interface && has_name && lambdas.len() == 1 {
                candidates.push((name.clone(), lambdas[0]));
            }
        }
        let mut walk = current.walk();
        stack.extend(current.children(&mut walk));
    }
    (candidates.len() == 1).then(|| candidates.remove(0))
}

fn recovered_interface_name_node<'tree>(
    node: tree_sitter::Node<'tree>,
    source: &str,
    name: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "infix_expression" {
            let children = current.children(&mut current.walk()).collect::<Vec<_>>();
            let is_shape = children.iter().any(|child| {
                child.kind() == "identifier"
                    && child.utf8_text(source.as_bytes()).ok() == Some("interface")
            }) && children
                .iter()
                .any(|child| child.kind() == "lambda_literal");
            if is_shape {
                return children.into_iter().find(|child| {
                    matches!(child.kind(), "identifier" | "simple_identifier")
                        && child.utf8_text(source.as_bytes()).ok() == Some(name)
                });
            }
        }
        if current.id() != node.id() && current.kind() == "lambda_literal" {
            continue;
        }
        let mut walk = current.walk();
        stack.extend(current.children(&mut walk));
    }
    None
}

fn supertypes(node: tree_sitter::Node<'_>, language: SourceLanguage, source: &str) -> Vec<String> {
    let fields: &[&str] = match language {
        SourceLanguage::Kotlin => &["delegation_specifiers"],
        SourceLanguage::Java => &[
            "superclass",
            "interfaces",
            "super_interfaces",
            "extends_interfaces",
        ],
    };
    let mut nodes: Vec<tree_sitter::Node<'_>> = fields
        .iter()
        .filter_map(|field| node.child_by_field_name(field))
        .collect();
    if nodes.is_empty() {
        let kinds: &[&str] = match language {
            SourceLanguage::Kotlin => &["delegation_specifiers"],
            SourceLanguage::Java => &[
                "superclass",
                "super_interfaces",
                "interfaces",
                "extends_interfaces",
            ],
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
        .flat_map(|child| {
            let text = child.utf8_text(source.as_bytes()).unwrap_or("").trim();
            if language == SourceLanguage::Java {
                split_java_supertypes(text)
            } else {
                vec![text.to_string()]
            }
        })
        .filter(|text| !text.is_empty())
        .collect()
}

fn split_java_supertypes(text: &str) -> Vec<String> {
    let text = text
        .strip_prefix("extends ")
        .or_else(|| text.strip_prefix("implements "))
        .unwrap_or(text)
        .trim();
    let mut result = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, character) in text.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                result.push(text[start..index].trim().to_string());
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    result.push(text[start..].trim().to_string());
    result
}

/// `Optional<pkg.Holder>` -> `Holder`. This is intentionally not a general
/// generic-unwrapping rule: N6C94 may stabilize an inferred local only when the
/// initializer explicitly calls `Optional.orElseThrow()`.
fn optional_element_type(type_name: &str) -> Option<String> {
    let text = type_name.trim().trim_end_matches('?').trim();
    let open = text.find('<')?;
    let outer = text[..open].trim().rsplit('.').next()?;
    if outer != "Optional" {
        return None;
    }
    let mut depth = 0usize;
    let mut close = None;
    for (offset, character) in text[open + 1..].char_indices() {
        match character {
            '<' => depth += 1,
            '>' if depth == 0 => {
                close = Some(open + 1 + offset);
                break;
            }
            '>' => depth -= 1,
            ',' if depth == 0 => return None,
            _ => {}
        }
    }
    let inner = text[open + 1..close?].trim();
    let simple = crate::smart_cast::simple_type(inner);
    (!simple.is_empty()).then_some(simple)
}

/// Type-parameter names of a declaration (`class Foo<T, I : Bar>` ->
/// `["T", "I"]`), from the `type_parameters` AST child. Java only.
fn type_param_names(node: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    let Some(params) = node
        .children(&mut node.walk())
        .find(|child| child.kind() == "type_parameters")
    else {
        if recovered_interface_parts(node, source).is_some() {
            let mut stack = vec![node];
            while let Some(current) = stack.pop() {
                if current.kind() == "lambda_literal" {
                    continue;
                }
                if current.kind() == "type_parameters" {
                    return current
                        .children(&mut current.walk())
                        .filter(|child| child.kind() == "type_parameter")
                        .filter_map(|child| {
                            child
                                .children(&mut child.walk())
                                .find(|part| {
                                    matches!(part.kind(), "identifier" | "type_identifier")
                                })
                                .and_then(|id| node_text(id, source).ok())
                        })
                        .collect();
                }
                let mut walk = current.walk();
                stack.extend(current.children(&mut walk));
            }
            return Vec::new();
        }
        return Vec::new();
    };
    params
        .children(&mut params.walk())
        .filter(|child| child.kind() == "type_parameter")
        .filter_map(|child| {
            child
                .children(&mut child.walk())
                .find(|inner| matches!(inner.kind(), "identifier" | "type_identifier"))
                .and_then(|id| node_text(id, source).ok())
        })
        .collect()
}

fn members(node: tree_sitter::Node<'_>, language: SourceLanguage, source: &str) -> Vec<Member> {
    let mut result = Vec::new();
    if language == SourceLanguage::Kotlin
        && let Some((_, body)) = recovered_interface_parts(node, source)
    {
        for child in body.children(&mut body.walk()) {
            result.extend(member_from_node(child, language, source));
        }
        return result;
    }
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
                is_mutable: false,
                has_unsupported_property_annotations: false,
                has_unsupported_property_shape: false,
                is_constructor_property: false,
                has_custom_accessor: false,
                has_plain_computed_getter: false,
                has_inline_getter: false,
                type_name: type_node.and_then(|node| node_text(node, source).ok()),
                parameter_types: Vec::new(),
                callable_arity: None,
                is_nullable: false,
                has_body: false,
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
            let parameter_text = node_text(parameter, source).unwrap_or_default();
            let (property_modifiers, is_jvm_field) = member_modifiers(parameter, source);
            result.push(Member {
                name: node_text(name_node, source).unwrap_or_default(),
                kind: MemberKind::Property,
                visibility: ["public", "protected", "internal", "private"]
                    .iter()
                    .find(|modifier| property_modifiers.contains(**modifier))
                    .map(|modifier| (*modifier).to_string())
                    .or_else(|| Some("public".to_string())),
                is_static: false,
                is_jvm_field,
                is_mutable: parameter
                    .children(&mut parameter.walk())
                    .any(|child| child.kind() == "var"),
                // Ordinary constructor-property annotations remain on the
                // field/property when the bridge adds @JvmField. Targeted
                // getter/setter annotations are supported only when their
                // complete argument syntax stays within one lexical token;
                // the source rewriter cannot safely rebuild split arguments.
                has_unsupported_property_annotations: constructor_property_annotations_unsupported(
                    &parameter_text.replace("@JvmField", ""),
                ),
                has_unsupported_property_shape: property_modifiers.contains("open")
                    || property_modifiers.contains("lateinit")
                    || property_modifiers.contains("abstract")
                    || parameter
                        .named_children(&mut parameter.walk())
                        .any(|child| child.kind() == "property_delegate"),
                is_constructor_property: true,
                has_custom_accessor: false,
                has_plain_computed_getter: false,
                has_inline_getter: false,
                type_name: type_node.and_then(|node| node_text(node, source).ok()),
                parameter_types: Vec::new(),
                callable_arity: None,
                is_nullable: type_node.is_some_and(|node| node.kind() == "nullable_type"),
                has_body: true,
            });
        }
    }
    if let Some(body) = node.child_by_field_name("body").or_else(|| {
        node.children(&mut node.walk())
            .find(|child| matches!(child.kind(), "class_body" | "enum_class_body" | "enum_body"))
    }) {
        for child in body.children(&mut body.walk()) {
            if child.kind() == "enum_body_declarations" {
                result.extend(
                    child
                        .children(&mut child.walk())
                        .filter_map(|member| member_from_node(member, language, source)),
                );
            } else {
                result.extend(member_from_node(child, language, source));
            }
        }
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

pub(crate) fn member_callable_arity(
    node: tree_sitter::Node<'_>,
    language: SourceLanguage,
) -> Option<CallableArity> {
    if node.has_error() {
        return None;
    }
    if language == SourceLanguage::Kotlin
        && node
            .child_by_field_name("name")
            .and_then(|name| name.prev_sibling())
            .is_some_and(|previous| previous.kind() == ".")
    {
        // Extension and context-receiver calls need receiver binding beyond
        // the number of written value arguments.
        return None;
    }
    if language == SourceLanguage::Kotlin
        && node
            .children(&mut node.walk())
            .any(|child| child.kind().contains("receiver"))
    {
        return None;
    }
    let parameters = match language {
        SourceLanguage::Kotlin => node
            .children(&mut node.walk())
            .find(|child| child.kind() == "function_value_parameters"),
        SourceLanguage::Java => node.child_by_field_name("parameters").or_else(|| {
            node.children(&mut node.walk())
                .find(|child| child.kind() == "formal_parameters")
        }),
    }?;
    if parameters.has_error() {
        return None;
    }

    match language {
        SourceLanguage::Kotlin => kotlin_callable_arity(parameters),
        SourceLanguage::Java => java_callable_arity(parameters),
    }
}

fn kotlin_callable_arity(parameters: tree_sitter::Node<'_>) -> Option<CallableArity> {
    let children: Vec<_> = parameters.children(&mut parameters.walk()).collect();
    let mut required = 0;
    let mut fixed = 0;
    let mut vararg_modifier_pending = false;
    let mut has_vararg = false;
    let mut index = 0;

    while index < children.len() {
        let child = children[index];
        if !child.is_named() || matches!(child.kind(), "line_comment" | "block_comment") {
            index += 1;
            continue;
        }
        if child.kind() == "parameter_modifiers" {
            vararg_modifier_pending = contains_descendant_kind(child, "vararg");
            index += 1;
            while index < children.len()
                && matches!(children[index].kind(), "line_comment" | "block_comment")
            {
                index += 1;
            }
            if index == children.len() || children[index].kind() != "parameter" {
                return None;
            }
            continue;
        }
        if child.kind() != "parameter" || has_vararg {
            return None;
        }

        let suffix_end = children[index + 1..]
            .iter()
            .position(|next| matches!(next.kind(), "parameter" | "parameter_modifiers"))
            .map(|offset| index + 1 + offset)
            .unwrap_or(children.len());
        let suffix = children[index + 1..suffix_end]
            .iter()
            .filter(|next| !matches!(next.kind(), "," | ")" | "line_comment" | "block_comment"))
            .copied()
            .collect::<Vec<_>>();
        let has_default = match suffix.as_slice() {
            [] => false,
            [equals, expression] if equals.kind() == "=" && expression.is_named() => true,
            _ => return None,
        };
        child
            .child_by_field_name("type")
            .or_else(|| first_type_node(child))?;

        if vararg_modifier_pending {
            if has_default {
                return None;
            }
            has_vararg = true;
        } else {
            fixed += 1;
            if !has_default {
                required += 1;
            }
        }
        vararg_modifier_pending = false;
        index = suffix_end;
    }
    if vararg_modifier_pending {
        return None;
    }
    Some(CallableArity {
        required,
        fixed,
        vararg: has_vararg,
    })
}

fn java_callable_arity(parameters: tree_sitter::Node<'_>) -> Option<CallableArity> {
    let children: Vec<_> = parameters.children(&mut parameters.walk()).collect();
    if children.iter().any(|child| {
        child.is_named()
            && !matches!(
                child.kind(),
                "formal_parameter" | "spread_parameter" | "line_comment" | "block_comment"
            )
    }) {
        return None;
    }

    let mut required = 0;
    let mut fixed = 0;
    let mut has_vararg = false;
    let mut seen_vararg = false;
    for parameter in children.iter() {
        if !parameter.is_named()
            || !matches!(parameter.kind(), "formal_parameter" | "spread_parameter")
        {
            continue;
        }
        let (is_vararg, has_type) = match parameter.kind() {
            "formal_parameter" => (
                false,
                parameter
                    .child_by_field_name("type")
                    .or_else(|| first_type_node(*parameter))
                    .is_some(),
            ),
            "spread_parameter" => (
                true,
                parameter
                    .named_children(&mut parameter.walk())
                    .any(|child| {
                        !matches!(
                            child.kind(),
                            "modifiers"
                                | "annotation"
                                | "marker_annotation"
                                | "variable_declarator"
                        )
                    }),
            ),
            _ => return None,
        };
        if !has_type || (seen_vararg && !is_vararg) {
            return None;
        }
        if is_vararg {
            if seen_vararg {
                return None;
            }
            seen_vararg = true;
            has_vararg = true;
            continue;
        }
        fixed += 1;
        required += 1;
    }
    Some(CallableArity {
        required,
        fixed,
        vararg: has_vararg,
    })
}

fn contains_descendant_kind(node: tree_sitter::Node<'_>, kind: &str) -> bool {
    node.kind() == kind
        || node
            .children(&mut node.walk())
            .any(|child| contains_descendant_kind(child, kind))
}

/// Modifier facts come from syntax tokens, never from annotation arguments or
/// comments: `@Tag("public val x; JvmField") private var x` is still private.
fn member_modifiers<'s>(node: tree_sitter::Node<'_>, source: &'s str) -> (HashSet<&'s str>, bool) {
    let mut words = HashSet::new();
    let mut jvm_field = false;
    let Some(modifiers) = node
        .children(&mut node.walk())
        .find(|child| child.kind() == "modifiers")
    else {
        return (words, jvm_field);
    };
    let mut pending = vec![modifiers];
    while let Some(modifier) = pending.pop() {
        match modifier.kind() {
            "annotation" | "marker_annotation" => {
                let annotation_type =
                    modifier
                        .named_children(&mut modifier.walk())
                        .find_map(|child| match child.kind() {
                            "user_type" => Some(child),
                            "constructor_invocation" => child
                                .named_children(&mut child.walk())
                                .find(|part| part.kind() == "user_type"),
                            _ => None,
                        });
                jvm_field |= annotation_type.is_some_and(|name| {
                    let name = name.utf8_text(source.as_bytes()).unwrap_or("");
                    matches!(name, "JvmField" | "kotlin.jvm.JvmField")
                        || (source.contains("JvmField as ")
                            && import_names(source).iter().any(|import| {
                                import.split_once(" as ").is_some_and(|(target, alias)| {
                                    target == "kotlin.jvm.JvmField" && alias == name
                                })
                            }))
                });
            }
            "line_comment" | "block_comment" => {}
            _ if modifier.child_count() == 0 => {
                words.insert(modifier.utf8_text(source.as_bytes()).unwrap_or("").trim());
            }
            _ => pending.extend(modifier.children(&mut modifier.walk())),
        }
    }
    (words, jvm_field)
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
    let (modifiers, is_jvm_field) = member_modifiers(node, source);
    let visibility = ["public", "protected", "internal", "private"]
        .iter()
        .find(|visibility| modifiers.contains(**visibility))
        .map(|visibility| (*visibility).to_string());
    let type_node = match (language, kind) {
        (SourceLanguage::Kotlin, MemberKind::Method) => kotlin_function_return_type(node),
        (_, MemberKind::Constructor) => None,
        _ => node
            .child_by_field_name("type")
            .or_else(|| first_type_node(node)),
    };
    let type_name = type_node.map(|node| {
        node.utf8_text(source.as_bytes())
            .unwrap_or("")
            .trim()
            .to_string()
    });
    let is_static = modifiers.contains("static");
    let member_text = node.utf8_text(source.as_bytes()).unwrap_or("");
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
        is_mutable: kind == MemberKind::Property
            && language == SourceLanguage::Kotlin
            && node
                .children(&mut node.walk())
                .any(|child| child.kind() == "var"),
        has_unsupported_property_annotations: kind == MemberKind::Property
            && language == SourceLanguage::Kotlin
            && {
                let text = node.utf8_text(source.as_bytes()).unwrap_or("");
                text.contains("@JvmName") || body_property_annotations_unsupported(text)
            },
        has_unsupported_property_shape: kind == MemberKind::Property
            && language == SourceLanguage::Kotlin
            && {
                modifiers.contains("open")
                    || modifiers.contains("lateinit")
                    || modifiers.contains("abstract")
                    || node
                        .named_children(&mut node.walk())
                        .any(|child| child.kind() == "property_delegate")
            },
        is_constructor_property: false,
        has_custom_accessor: kind == MemberKind::Property
            && language == SourceLanguage::Kotlin
            && node
                .named_children(&mut node.walk())
                .any(|child| matches!(child.kind(), "getter" | "setter")),
        has_plain_computed_getter: kind == MemberKind::Property
            && language == SourceLanguage::Kotlin
            && plain_computed_property_getter(node, source).is_some(),
        has_inline_getter: kind == MemberKind::Property
            && language == SourceLanguage::Kotlin
            && member_text
                .lines()
                .next()
                .is_some_and(|line| line.contains("get()")),
        type_name,
        parameter_types: member_parameter_types(node, language, source),
        callable_arity: (kind == MemberKind::Method)
            .then(|| member_callable_arity(node, language))
            .flatten(),
        is_nullable: type_node.is_some_and(|node| node.kind() == "nullable_type"),
        has_body: node.named_children(&mut node.walk()).any(|child| {
            matches!(child.kind(), "function_body" | "block")
                || child.kind() == "getter"
                    && child
                        .named_children(&mut child.walk())
                        .any(|body| body.kind() == "function_body")
        }),
    })
}

fn constructor_property_annotations_unsupported(text: &str) -> bool {
    if !text.contains('@') {
        return false;
    }
    if text.contains("@JvmName") || text.contains("@JvmField") {
        return true;
    }
    // Ordinary constructor-property annotations stay attached to the field
    // when the bridge removes `override` and adds `final @JvmField`; their
    // arguments may span whitespace. Only targeted accessor annotations are
    // moved onto synthesized methods, and that narrow token-based rewrite
    // still requires the complete annotation to be one lexical token.
    text.split_whitespace()
        .filter(|token| token.starts_with("@get:") || token.starts_with("@set:"))
        .any(|token| {
            let mut depth = 0isize;
            let mut in_string = false;
            let mut escaped = false;
            for ch in token.chars() {
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        in_string = false;
                    }
                    continue;
                }
                match ch {
                    '"' => in_string = true,
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth < 0 {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
            depth != 0 || in_string
        })
}

/// Exact structural evidence for replacing a readonly computed property with
/// its accessor method. Annotation targets need their own proof; leave those
/// properties on the existing bridge path. In particular, an initializer plus
/// a getter still owns storage even when the getter never mentions `field`.
pub(crate) struct PlainComputedGetter<'tree> {
    pub body: tree_sitter::Node<'tree>,
    pub return_type: tree_sitter::Node<'tree>,
    pub is_final: bool,
    pub is_public: bool,
}

pub(crate) fn plain_computed_property_getter<'tree>(
    node: tree_sitter::Node<'tree>,
    source: &str,
) -> Option<PlainComputedGetter<'tree>> {
    if node.kind() != "property_declaration" || node.has_error() {
        return None;
    }
    let children = node.children(&mut node.walk()).collect::<Vec<_>>();
    if children
        .iter()
        .any(|child| matches!(child.kind(), "var" | "=" | "property_delegate" | "setter"))
        || !children.iter().any(|child| child.kind() == "val")
    {
        return None;
    }
    let variable = children
        .iter()
        .find(|child| child.kind() == "variable_declaration")?;
    let return_type = first_type_node(*variable)?;
    let modifiers = children.iter().find(|child| child.kind() == "modifiers")?;
    let mut is_final = false;
    let mut is_public = false;
    let mut is_override = false;
    for modifier in modifiers.named_children(&mut modifiers.walk()) {
        if matches!(modifier.kind(), "line_comment" | "multiline_comment") {
            continue;
        }
        match modifier.utf8_text(source.as_bytes()).ok()?.trim() {
            "override" => is_override = true,
            "public" => is_public = true,
            "final" => is_final = true,
            _ => return None,
        }
    }
    if !is_override {
        return None;
    }
    let getters = children
        .iter()
        .filter(|child| child.kind() == "getter")
        .collect::<Vec<_>>();
    let [getter] = getters.as_slice() else {
        return None;
    };
    let body = getter
        .named_children(&mut getter.walk())
        .find(|child| child.kind() == "function_body")?;
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        if current.start_byte() < body.start_byte() && current.kind() == "annotation" {
            return None;
        }
        if current.kind() == "identifier" && current.utf8_text(source.as_bytes()).ok()? == "field" {
            return None;
        }
        pending.extend(current.named_children(&mut current.walk()));
    }
    Some(PlainComputedGetter {
        body,
        return_type,
        is_final,
        is_public,
    })
}

fn body_property_annotations_unsupported(text: &str) -> bool {
    if !text.contains('@') {
        return false;
    }
    let property_at = text
        .find("val ")
        .or_else(|| text.find("var "))
        .unwrap_or(usize::MAX);
    let getter_at = text.find("get()");
    let setter_at = text.find("set(");
    text.match_indices('@').any(|(at, _)| {
        let tail = &text[at..];
        if tail.starts_with("@get:") || tail.starts_with("@set:") {
            return false;
        }
        // Kotlin permits an annotation directly on a custom accessor.  It is
        // inside the property node, after the declaration and before get/set,
        // and can move to the explicit JavaBean bridge method unchanged.
        let on_getter = getter_at.is_some_and(|accessor| at > property_at && at < accessor);
        let on_setter = setter_at.is_some_and(|accessor| at > property_at && at < accessor);
        !(on_getter || on_setter)
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

fn is_physical_zero_arg_getter(member: &Member, accessor: &str) -> bool {
    if member.kind != MemberKind::Method
        || member.name != accessor
        || !member.parameter_types.is_empty()
        || member.visibility.as_deref() == Some("private")
    {
        return false;
    }
    let Some(return_type) = member.type_name.as_deref().map(str::trim) else {
        return false;
    };
    if return_type.is_empty() || matches!(return_type, "Unit" | "kotlin.Unit" | "void") {
        return false;
    }
    let get_shape = accessor
        .strip_prefix("get")
        .is_some_and(|suffix| suffix.chars().next().is_some_and(char::is_uppercase));
    let boolean_is_shape = accessor
        .strip_prefix("is")
        .is_some_and(|suffix| suffix.chars().next().is_some_and(char::is_uppercase))
        && matches!(return_type, "Boolean" | "boolean" | "kotlin.Boolean");
    get_shape || boolean_is_shape
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

/// Kotlin NG does not expose a stable `type` field for every function return.
/// Search only direct children after the value-parameter list: a recursive
/// first-type search sees the first parameter type and records it as the return.
fn kotlin_function_return_type(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let parameters_end = node
        .named_children(&mut node.walk())
        .find(|child| child.kind() == "function_value_parameters")?
        .end_byte();
    node.named_children(&mut node.walk()).find(|child| {
        child.start_byte() >= parameters_end
            && matches!(
                child.kind(),
                "user_type"
                    | "nullable_type"
                    | "function_type"
                    | "parenthesized_type"
                    | "definitely_non_nullable_type"
            )
    })
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
        let Some(own_decl) = index.resolve_type(source_file, own_ty) else {
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
    if projected_return_is_subtype(
        index,
        source_file,
        own_ty,
        index
            .declaration_source_file(super_decl)
            .unwrap_or(source_file),
        super_ty,
    ) {
        return MemberConflictClass::JavaCovariantReturn;
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

    let Some(own_decl) = index.resolve_type(source_file, own_ty) else {
        return MemberConflictClass::UnknownType;
    };
    if member_supertype_closure_contains(index, source_file, own_decl, super_base) {
        MemberConflictClass::JavaCovariantReturn
    } else {
        MemberConflictClass::UnrelatedReturnTypes
    }
}

/// Only explicit use-site projections widen Java generic returns. Invariant
/// arguments stay invariant even for Kotlin's read-only collection types.
fn projected_return_is_subtype(
    index: &SourceIndex,
    own_file: &SourceFile,
    own_ty: &str,
    super_file: &SourceFile,
    super_ty: &str,
) -> bool {
    let (super_base, Some(super_args)) = split_generic(super_ty.trim().trim_end_matches('?'))
    else {
        return false;
    };
    let (own_base, Some(own_args)) = split_generic(own_ty.trim().trim_end_matches('?')) else {
        return false;
    };
    if super_base != own_base || super_args.len() != own_args.len() {
        return false;
    }
    match (
        index.resolve_type(own_file, own_base),
        index.resolve_type(super_file, super_base),
    ) {
        (Some(own), Some(sup)) if declaration_key(own) == declaration_key(sup) => {}
        (None, None) => {}
        _ => return false,
    }
    super_args.iter().zip(own_args.iter()).all(|(sup, own)| {
        if sup == own || *sup == "*" {
            return true;
        }
        if let Some(bound) = sup.strip_prefix("out ") {
            let own = own.strip_prefix("out ").unwrap_or(own);
            return !own.starts_with("in ")
                && own != "*"
                && proven_reference_subtype(index, own_file, own, super_file, bound);
        }
        if let Some(bound) = sup.strip_prefix("in ") {
            let own = own.strip_prefix("in ").unwrap_or(own);
            return !own.starts_with("out ")
                && own != "*"
                && proven_reference_subtype(index, super_file, bound, own_file, own);
        }
        false
    })
}

fn declaration_covariant_return_is_subtype(
    index: &SourceIndex,
    own_file: &SourceFile,
    own_ty: &str,
    super_file: &SourceFile,
    super_ty: &str,
) -> bool {
    let (super_base, Some(super_args)) = split_generic(super_ty.trim().trim_end_matches('?'))
    else {
        return false;
    };
    if let (own_base, Some(own_args)) = split_generic(own_ty.trim().trim_end_matches('?'))
        && covariant_collection_base(own_base, super_base)
        && collection_base_is_unshadowed(index, own_file, own_base)
        && collection_base_is_unshadowed(index, super_file, super_base)
        && covariant_collection_arguments(
            index,
            own_file,
            &own_args,
            super_file,
            &super_args,
            super_base,
        )
    {
        return true;
    }

    // Named collection wrappers are common domain types (`AssetBarcodeList :
    // ArrayList<AssetBarcode>`). Their declared superclass proves the same
    // covariance as a direct `List<AssetBarcode>` return, and Java accepts the
    // wrapper as a covariant return for `List<? extends IBarcode>`.
    let Some(wrapper) = index.resolve_type(own_file, own_ty.trim().trim_end_matches('?')) else {
        return false;
    };
    let wrapper_file = index.declaration_source_file(wrapper).unwrap_or(own_file);
    wrapper.supertypes.iter().any(|candidate| {
        let (candidate_base, Some(candidate_args)) =
            split_generic(candidate.trim().trim_end_matches("()"))
        else {
            return false;
        };
        covariant_collection_base(candidate_base, super_base)
            && collection_base_is_unshadowed(index, wrapper_file, candidate_base)
            && collection_base_is_unshadowed(index, super_file, super_base)
            && covariant_collection_arguments(
                index,
                wrapper_file,
                &candidate_args,
                super_file,
                &super_args,
                super_base,
            )
    })
}

/// The covariance rules below describe Kotlin's standard read-only collection
/// declarations. A user declaration or explicit import with the same simple
/// name can be invariant, so its spelling alone cannot establish that rule.
pub(crate) fn collection_base_is_unshadowed(
    index: &SourceIndex,
    source_file: &SourceFile,
    base: &str,
) -> bool {
    if !matches!(
        base,
        "List"
            | "ArrayList"
            | "MutableList"
            | "Set"
            | "HashSet"
            | "LinkedHashSet"
            | "MutableSet"
            | "Collection"
            | "Iterable"
            | "Iterator"
            | "Sequence"
            | "Map"
            | "HashMap"
            | "LinkedHashMap"
            | "MutableMap"
    ) {
        return false;
    }

    if index.resolve_type(source_file, base).is_some() {
        // The workspace index contains source declarations, not the Kotlin
        // standard library. Any resolved declaration therefore shadows the
        // builtin assumption and must be checked under its own variance.
        return false;
    }

    // Reject explicit external shadows too. Wildcard imports with indexed
    // shadows have already been rejected by resolution above.
    source_file.imports.iter().all(|import| {
        if import == &format!("kotlin.collections.{base}") || import == &format!("java.util.{base}")
        {
            return true;
        }
        if import.ends_with(&format!(".{base}")) {
            return false;
        }
        true
    })
}

fn covariant_collection_base(own: &str, inherited: &str) -> bool {
    match inherited {
        "List" => matches!(own, "List" | "ArrayList" | "MutableList"),
        "Set" => matches!(own, "Set" | "HashSet" | "LinkedHashSet" | "MutableSet"),
        "Collection" | "Iterable" => matches!(
            own,
            "List"
                | "ArrayList"
                | "MutableList"
                | "Set"
                | "HashSet"
                | "LinkedHashSet"
                | "MutableSet"
                | "Collection"
                | "Iterable"
        ),
        "Iterator" | "Sequence" => own == inherited,
        "Map" => matches!(own, "Map" | "HashMap" | "LinkedHashMap" | "MutableMap"),
        _ => false,
    }
}

fn covariant_collection_arguments(
    index: &SourceIndex,
    own_file: &SourceFile,
    own_args: &[&str],
    super_file: &SourceFile,
    super_args: &[&str],
    super_base: &str,
) -> bool {
    own_args.len() == super_args.len()
        && own_args
            .iter()
            .zip(super_args.iter())
            .enumerate()
            .all(|(index_in_type, (own, sup))| {
                if own == sup || *sup == "*" {
                    return true;
                }
                if super_base == "List"
                    && super_file.language == SourceLanguage::Kotlin
                    && let Some(bound) = sup.strip_prefix("@kotlin.jvm.JvmWildcard ")
                {
                    return is_plain_type_reference(bound)
                        && proven_reference_subtype(index, own_file, own, super_file, bound);
                }
                if let Some(bound) = sup.strip_prefix("out ") {
                    let own = own.strip_prefix("out ").unwrap_or(own);
                    return !own.starts_with("in ")
                        && own != "*"
                        && proven_reference_subtype(index, own_file, own, super_file, bound);
                }
                (super_base != "Map" || index_in_type == 1)
                    && proven_reference_subtype(index, own_file, own, super_file, sup)
            })
}

fn is_plain_type_reference(ty: &str) -> bool {
    !ty.is_empty()
        && !ty
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '<' | '>' | '?' | '*' | '(' | ')' | '@'))
        && !ty.starts_with("in ")
        && !ty.starts_with("out ")
}

fn readonly_kotlin_list_base_is_proven(
    index: &SourceIndex,
    file: &SourceFile,
    owner: &Declaration,
) -> bool {
    if index.resolve_type(file, "List").is_some()
        || owner
            .type_params
            .iter()
            .any(|parameter| parameter == "List")
        || index
            .type_aliases_for(&file.path)
            .iter()
            .any(|(alias, _)| alias == "List")
        || file.imports.iter().any(|import| {
            import
                .split_once(" as ")
                .is_some_and(|(_, alias)| alias.trim() == "List")
                || import.ends_with(".*") && import != "kotlin.collections.*"
        })
    {
        return false;
    }
    file.imports
        .iter()
        .all(|import| import == "kotlin.collections.List" || !import.ends_with(".List"))
}

fn readonly_list_jvm_wildcard_abi_proven(
    index: &SourceIndex,
    file: &SourceFile,
    owner: &Declaration,
    property: &str,
    inherited_type: &str,
) -> bool {
    if file.language != SourceLanguage::Kotlin
        || owner.language != SourceLanguage::Kotlin
        || owner.kind != DeclarationKind::Interface
        || !readonly_kotlin_list_base_is_proven(index, file, owner)
    {
        return false;
    }
    let Some(member) = owner
        .members
        .iter()
        .find(|member| member.kind == MemberKind::Property && member.name == property)
    else {
        return false;
    };
    if member.is_mutable
        || member.has_unsupported_property_shape
        || member.type_name.as_deref() != Some(inherited_type)
        || !readonly_list_wildcard_property_annotations_supported(file, owner, property)
    {
        return false;
    }
    let Some(member_type) = member.type_name.as_deref() else {
        return false;
    };
    let (base, Some(args)) = split_generic(member_type.trim().trim_end_matches('?')) else {
        return false;
    };
    base == "List"
        && args.len() == 1
        && args[0]
            .strip_prefix("@kotlin.jvm.JvmWildcard ")
            .is_some_and(is_plain_type_reference)
}

/// `Member::has_unsupported_property_annotations` intentionally treats any
/// unclassified `@` in a property node as unsafe. Here the one exact annotation
/// is on the List type argument (and changes only the JVM generic signature),
/// so inspect the parsed type subtree and remove only that occurrence before
/// applying the ordinary property-annotation guard.
fn readonly_list_wildcard_property_annotations_supported(
    file: &SourceFile,
    owner: &Declaration,
    property: &str,
) -> bool {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let source = file.source_text();
    let Some(tree) = parser.parse(source, None) else {
        return false;
    };
    let mut stack = vec![tree.root_node()];
    let mut matches = Vec::new();
    while let Some(node) = stack.pop() {
        if let Some((kind, name_node)) = declaration_shape(node, SourceLanguage::Kotlin, source)
            && kind == owner.kind
            && node_text(name_node, source).ok().as_deref() == Some(owner.name.as_str())
            && let Some(body) = node.child_by_field_name("body").or_else(|| {
                node.children(&mut node.walk())
                    .find(|child| child.kind() == "class_body")
            })
        {
            for member_node in body.children(&mut body.walk()) {
                if member_node.kind() != "property_declaration" {
                    continue;
                }
                if member_from_node(member_node, SourceLanguage::Kotlin, source).is_some_and(
                    |member| member.kind == MemberKind::Property && member.name == property,
                ) {
                    matches.push(member_node);
                }
            }
        }
        stack.extend(node.children(&mut node.walk()));
    }
    let [property_node] = matches.as_slice() else {
        return false;
    };
    let Some(type_node) = property_node
        .child_by_field_name("type")
        .or_else(|| first_type_node(*property_node))
    else {
        return false;
    };
    let Ok(property_text) = property_node.utf8_text(source.as_bytes()) else {
        return false;
    };
    let Ok(type_text) = type_node.utf8_text(source.as_bytes()) else {
        return false;
    };
    let marker = "@kotlin.jvm.JvmWildcard";
    let marker_positions = type_text.match_indices(marker).collect::<Vec<_>>();
    if marker_positions.len() != 1 {
        return false;
    }
    let relative_start =
        type_node.start_byte() - property_node.start_byte() + marker_positions[0].0;
    let relative_end = relative_start + marker.len();
    let mut without_type_annotation = property_text.to_owned();
    without_type_annotation.replace_range(relative_start..relative_end, "");
    !body_property_annotations_unsupported(&without_type_annotation)
}

/// Locate the exact element type argument in a Kotlin interface property.
/// The edit is anchored to the indexed text snapshot and is rejected if the
/// parser cannot uniquely identify the owner/property/type argument.
fn readonly_list_type_argument_edit(
    file: &SourceFile,
    owner: &Declaration,
    property: &str,
) -> Option<(crate::semantics::SourceLocation, String, bool)> {
    if file.language != SourceLanguage::Kotlin {
        return None;
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(file.source_text(), None)?;
    let source = file.source_text();
    let mut stack = vec![tree.root_node()];
    let mut matches = Vec::new();
    while let Some(node) = stack.pop() {
        if let Some((kind, name_node)) = declaration_shape(node, SourceLanguage::Kotlin, source)
            && kind == owner.kind
            && node_text(name_node, source).ok().as_deref() == Some(owner.name.as_str())
            && let Some(body) = node.child_by_field_name("body").or_else(|| {
                node.children(&mut node.walk())
                    .find(|child| child.kind() == "class_body")
            })
        {
            for member_node in body.children(&mut body.walk()) {
                if member_node.kind() != "property_declaration" {
                    continue;
                }
                let Some(member) = member_from_node(member_node, SourceLanguage::Kotlin, source)
                else {
                    continue;
                };
                if member.kind == MemberKind::Property && member.name == property {
                    matches.push(member_node);
                }
            }
        }
        stack.extend(node.children(&mut node.walk()));
    }
    let [property_node] = matches.as_slice() else {
        return None;
    };
    let type_node = property_node
        .child_by_field_name("type")
        .or_else(|| first_type_node(*property_node))?;
    let type_text = type_node.utf8_text(source.as_bytes()).ok()?.trim();
    let indexed_type = owner
        .members
        .iter()
        .find(|member| member.kind == MemberKind::Property && member.name == property)?
        .type_name
        .as_deref()?;
    if type_text != indexed_type.trim() {
        return None;
    }
    let base_text = type_text.trim_end_matches('?').trim();
    let open = base_text.find('<')?;
    if !base_text[..open].trim().eq("List") || !base_text.ends_with('>') {
        return None;
    }
    let arg = base_text[open + 1..base_text.len() - 1].trim();
    let (already_present, bound) = if let Some(bound) = arg.strip_prefix("@kotlin.jvm.JvmWildcard ")
    {
        (true, bound.trim())
    } else {
        (false, arg)
    };
    if !is_plain_type_reference(bound) || (!already_present && arg.contains('@')) {
        return None;
    }
    let leading = base_text[open + 1..].len() - base_text[open + 1..].trim_start().len();
    let arg_start = type_node.start_byte() + open + 1 + leading;
    let arg_end = arg_start + arg.len();
    let replacement = if already_present {
        arg.to_owned()
    } else {
        format!("@kotlin.jvm.JvmWildcard {arg}")
    };
    Some((
        crate::semantics::SourceLocation {
            file: file.path.clone(),
            snapshot_hash: *blake3::hash(source.as_bytes()).as_bytes(),
            start_byte: arg_start,
            end_byte: arg_end,
        },
        replacement,
        already_present,
    ))
}

fn proven_reference_subtype(
    index: &SourceIndex,
    own_file: &SourceFile,
    own_ty: &str,
    super_file: &SourceFile,
    super_ty: &str,
) -> bool {
    if own_ty == super_ty || matches!(super_ty, "Any" | "Object") {
        return true;
    }
    if projected_return_is_subtype(index, own_file, own_ty, super_file, super_ty) {
        return true;
    }
    // A bare inheritance edge cannot certify different generic instantiations.
    if own_ty.contains('<') || super_ty.contains('<') {
        return false;
    }
    let (Some(own), Some(target)) = (
        index.resolve_type(own_file, own_ty),
        index.resolve_type(super_file, super_ty),
    ) else {
        return false;
    };
    let target_key = declaration_key(target);
    let mut pending = vec![own];
    let mut visited = HashSet::new();
    while let Some(current) = pending.pop() {
        let key = declaration_key(current);
        if key == target_key {
            return true;
        }
        if !visited.insert(key) {
            continue;
        }
        let context = index.declaration_source_file(current).unwrap_or(own_file);
        pending.extend(
            current
                .supertypes
                .iter()
                .filter_map(|sup| index.resolve_type(context, sup)),
        );
    }
    false
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

/// Substitute declaration type parameters in an inherited member signature.
/// The index stores source-level type text, so this deliberately replaces only
/// identifier tokens; a parameter named `T` must not alter `Thing`.
fn substitute_type_parameters(ty: &str, bindings: &HashMap<String, String>) -> String {
    if bindings.is_empty() {
        return ty.to_string();
    }
    let mut out = String::with_capacity(ty.len());
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if token.is_empty() {
            return;
        }
        out.push_str(bindings.get(token).map_or(token.as_str(), String::as_str));
        token.clear();
    };
    for ch in ty.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            token.push(ch);
        } else {
            flush(&mut token, &mut out);
            out.push(ch);
        }
    }
    flush(&mut token, &mut out);
    while out.contains("??") {
        out = out.replace("??", "?");
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CanonicalGetterType {
    ScopedParameter(String),
    Named(String, Vec<CanonicalGetterType>),
    Nullable(Box<CanonicalGetterType>),
    Projection(String, Box<CanonicalGetterType>),
}

fn java_getter_has_no_method_type_parameters(
    file: &SourceFile,
    owner: &Declaration,
    getter: &str,
) -> bool {
    file.language == SourceLanguage::Java
        && getter_has_no_method_type_parameters(file, owner, getter)
}
/// The ancestry graph was resolved while gathering this descendant's getter
/// contracts. Reuse those exact symbol edges instead of resolving names again.
fn getter_owner_ancestors(
    parents: &HashMap<crate::semantics::SymbolId, HashSet<crate::semantics::SymbolId>>,
    owner: &crate::semantics::SymbolId,
) -> Option<HashSet<crate::semantics::SymbolId>> {
    let mut pending = vec![owner];
    let mut seen = HashSet::new();
    while let Some(current) = pending.pop() {
        if !seen.insert(current.clone()) {
            continue;
        }
        if seen.len() > 256 {
            return None;
        }
        if let Some(ancestors) = parents.get(current) {
            pending.extend(ancestors);
        }
    }
    Some(seen)
}

fn getter_has_no_method_type_parameters(
    file: &SourceFile,
    owner: &Declaration,
    getter: &str,
) -> bool {
    file.header_facts
        .getter_has_no_method_type_parameters(&owner.name, getter)
}
fn getter_is_public(member: &Member, owner: &Declaration) -> bool {
    match member.visibility.as_deref() {
        Some("public") => true,
        Some(_) => false,
        // Kotlin members are public by default. Java interface methods are
        // public implicitly; a Java class method with no modifier is
        // package-private and cannot be treated as a public inherited getter.
        None => {
            owner.language == SourceLanguage::Kotlin || owner.kind == DeclarationKind::Interface
        }
    }
}

fn declaration_type_parameter_shadows(owner: &Declaration, source_type: &str) -> bool {
    let base = source_type
        .trim()
        .trim_end_matches('?')
        .split('<')
        .next()
        .unwrap_or("")
        .trim();
    let simple = base.rsplit('.').next().unwrap_or(base);
    owner
        .type_params
        .iter()
        .any(|parameter| parameter == simple)
}

/// Resolve a simple/qualified source reference only when its binding is
/// explicit in that file's package/import scope. Wildcard imports are not
/// enough to establish this proof's nominal identity.
fn exact_source_reference_type<'a>(
    index: &'a SourceIndex,
    file: &SourceFile,
    source_type: &str,
) -> Option<&'a Declaration> {
    let source_type = source_type.trim();
    let base = source_type.strip_suffix('?').unwrap_or(source_type).trim();
    if base.is_empty()
        || base.contains(['<', '>', '&', '(', ')', '[', ']', '*', '@'])
        || base.split_whitespace().count() != 1
    {
        return None;
    }
    let declaration = resolve_unique_getter_type(index, file, base)?;
    if !matches!(
        declaration.kind,
        DeclarationKind::Class
            | DeclarationKind::Interface
            | DeclarationKind::Enum
            | DeclarationKind::Object
    ) {
        return None;
    }
    let qualified = declaration_key(declaration);
    if base.contains('.') {
        return (base == qualified).then_some(declaration);
    }
    let simple = base.rsplit('.').next().unwrap_or(base);
    let imports = file
        .imports
        .iter()
        .filter(|import| import.rsplit('.').next() == Some(simple))
        .collect::<Vec<_>>();
    if !imports.is_empty() {
        return (imports.len() == 1 && imports[0].as_str() == qualified).then_some(declaration);
    }
    (file.package.as_deref() == declaration.package.as_deref()).then_some(declaration)
}

fn scoped_type_parameter_bindings(
    index: &SourceIndex,
    owner: &Declaration,
) -> HashMap<String, CanonicalGetterType> {
    let scope = crate::semantics::workspace_symbol(index, owner).stable_key();
    owner
        .type_params
        .iter()
        .map(|parameter| {
            (
                parameter.clone(),
                CanonicalGetterType::ScopedParameter(format!("{scope}::{parameter}")),
            )
        })
        .collect()
}

fn inherited_type_parameter_bindings(
    index: &SourceIndex,
    edge_owner: &Declaration,
    parent: &Declaration,
    supertype: &str,
    current_bindings: &HashMap<String, CanonicalGetterType>,
) -> HashMap<String, CanonicalGetterType> {
    if parent.type_params.is_empty() {
        return HashMap::new();
    }
    let (_, supplied_arguments) = split_generic(supertype.trim().trim_end_matches("()"));
    let Some(supplied_arguments) = supplied_arguments else {
        return HashMap::new();
    };
    if supplied_arguments.len() != parent.type_params.len() {
        return HashMap::new();
    }
    parent
        .type_params
        .iter()
        .zip(supplied_arguments)
        .filter_map(|(parameter, argument)| {
            canonical_getter_type(index, edge_owner, argument, current_bindings)
                .map(|canonical| (parameter.clone(), canonical))
        })
        .collect()
}

fn canonical_getter_type(
    index: &SourceIndex,
    owner: &Declaration,
    source_type: &str,
    bindings: &HashMap<String, CanonicalGetterType>,
) -> Option<CanonicalGetterType> {
    let file = index.declaration_source_file(owner)?;
    let ty = source_type.trim();
    for variance in ["out ", "in "] {
        if let Some(inner) = ty.strip_prefix(variance) {
            return Some(CanonicalGetterType::Projection(
                variance.trim().to_owned(),
                Box::new(canonical_getter_type(index, owner, inner, bindings)?),
            ));
        }
    }
    if let Some(inner) = ty.strip_suffix('?') {
        let inner = canonical_getter_type(index, owner, inner, bindings)?;
        return Some(match inner {
            nullable @ CanonicalGetterType::Nullable(_) => nullable,
            other => CanonicalGetterType::Nullable(Box::new(other)),
        });
    }
    if ty == "*" || ty.is_empty() || ty.contains("->") || ty.contains('@') {
        return None;
    }
    let (base, arguments) = split_generic(ty);
    let base = base.trim();
    if owner.type_params.iter().any(|parameter| parameter == base) {
        if arguments.is_some() {
            return None;
        }
        return bindings.get(base).cloned();
    }
    let identity = canonical_getter_named_identity(index, file, base)?;
    let arguments = match arguments {
        Some(arguments) => arguments
            .iter()
            .map(|argument| canonical_getter_type(index, owner, argument, bindings))
            .collect::<Option<Vec<_>>>()?,
        None => Vec::new(),
    };
    Some(CanonicalGetterType::Named(identity, arguments))
}

fn canonical_getter_named_identity(
    index: &SourceIndex,
    file: &SourceFile,
    type_name: &str,
) -> Option<String> {
    default_getters::record_alias_lookup(index, type_name);
    let simple = type_name.rsplit('.').next().unwrap_or(type_name);
    if type_name.is_empty()
        || !type_name
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '.'))
    {
        return None;
    }

    if index.alias_names.get(simple).is_some_and(|candidates| {
        candidates.iter().any(|&position| {
            let declaring = &index.files[position];
            let qualified = declaring
                .package
                .as_ref()
                .map(|package| format!("{package}.{simple}"));
            if type_name.contains('.') {
                return qualified.as_deref() == Some(type_name);
            }
            paths_match(&declaring.path, &file.path)
                || declaring.package == file.package
                || qualified
                    .as_ref()
                    .is_some_and(|qualified| file.imports.contains(qualified))
                || declaring.package.as_ref().is_some_and(|package| {
                    file.imports
                        .iter()
                        .any(|import| import == &format!("{package}.*"))
                })
        })
    }) {
        return None;
    }

    let visible = index
        .declarations_named(simple)
        .filter(|declaration| declaration_is_visible_from(index, file, type_name, declaration))
        .collect::<Vec<_>>();
    if visible.len() == 1 {
        return Some(crate::semantics::workspace_symbol(index, visible[0]).stable_key());
    }
    if !visible.is_empty()
        || file.type_aliases.contains_key(simple)
        || file.imports.iter().any(|import| {
            import
                .split_once(" as ")
                .is_some_and(|(_, alias)| alias.trim() == simple)
        })
    {
        return None;
    }

    if type_name.contains('.') {
        if index
            .qualified_declarations
            .get(type_name)
            .is_some_and(|items| !items.is_empty())
        {
            return None;
        }
        return Some(canonical_getter_external_identity(type_name));
    }
    let explicit_imports = file
        .imports
        .iter()
        .filter(|import| import.rsplit('.').next() == Some(simple))
        .collect::<Vec<_>>();
    if !explicit_imports.is_empty() {
        return (explicit_imports.len() == 1)
            .then(|| canonical_getter_external_identity(explicit_imports[0]));
    }
    // Preserve the workspace provider's established standard-type inference:
    // unrelated stars do not erase a known builtin fact. Source collisions,
    // explicit imports and aliases take precedence; arbitrary custom names
    // still have no fallback. Full classpath resolution belongs to a semantic
    // provider with compiler analysis.
    if index.declarations_named(simple).next().is_some() {
        return None;
    }
    if simple == "UUID" && file.imports.iter().any(|import| import == "java.util.*") {
        return Some(canonical_getter_external_identity("java.util.UUID"));
    }
    let builtin = match simple {
        "Any" | "Object" => Some("kotlin.Any"),
        "Nothing" => Some("kotlin.Nothing"),
        "Unit" | "void" => Some("kotlin.Unit"),
        "Boolean" | "boolean" => Some("kotlin.Boolean"),
        "Byte" | "byte" => Some("kotlin.Byte"),
        "Short" | "short" => Some("kotlin.Short"),
        "Int" | "int" => Some("kotlin.Int"),
        "Long" | "long" => Some("kotlin.Long"),
        "Float" | "float" => Some("kotlin.Float"),
        "Double" | "double" => Some("kotlin.Double"),
        "Char" | "char" => Some("kotlin.Char"),
        "String" => Some("kotlin.String"),
        "Comparable" => Some("external:java.lang.Comparable"),
        "List" | "Set" | "Collection" | "Iterable" | "Iterator" | "Sequence" | "Map"
            if collection_base_is_unshadowed(index, file, simple) =>
        {
            Some(match simple {
                "List" => "kotlin.collections.List",
                "Set" => "kotlin.collections.Set",
                "Collection" => "kotlin.collections.Collection",
                "Iterable" => "kotlin.collections.Iterable",
                "Iterator" => "kotlin.collections.Iterator",
                "Sequence" => "kotlin.sequences.Sequence",
                "Map" => "kotlin.collections.Map",
                _ => unreachable!(),
            })
        }
        _ => None,
    }?;
    Some(builtin.to_owned())
}

fn canonical_getter_external_identity(qualified: &str) -> String {
    match qualified {
        "java.lang.String" | "kotlin.String" => "kotlin.String".to_owned(),
        "java.lang.Object" | "kotlin.Any" => "kotlin.Any".to_owned(),
        "java.lang.Comparable" | "kotlin.Comparable" => "external:java.lang.Comparable".to_owned(),
        "kotlin.Nothing" | "kotlin.Unit" | "kotlin.Boolean" | "kotlin.Byte" | "kotlin.Short"
        | "kotlin.Int" | "kotlin.Long" | "kotlin.Float" | "kotlin.Double" | "kotlin.Char" => {
            qualified.to_owned()
        }
        _ => format!("external:{qualified}"),
    }
}

fn resolve_unique_getter_type<'a>(
    index: &'a SourceIndex,
    file: &SourceFile,
    type_name: &str,
) -> Option<&'a Declaration> {
    let base = type_name
        .trim()
        .trim_end_matches('?')
        .split('<')
        .next()?
        .split('(')
        .next()?
        .trim();
    let simple = base.rsplit('.').next()?;
    let visible = index
        .declarations_named(simple)
        .filter(|declaration| declaration_is_visible_from(index, file, base, declaration))
        .collect::<Vec<_>>();
    if visible.len() == 1 {
        Some(visible[0])
    } else {
        None
    }
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

#[cfg(test)]
mod declaration_symbol_cache_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn java_package_ends_before_a_same_line_declaration() {
        assert_eq!(
            package_name("package example.annotations; public @interface Marker {}"),
            Some("example.annotations".into())
        );
    }

    #[test]
    fn readonly_list_wildcard_requirements_are_exact_and_atomic() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "notlin-jvm-wildcard-contract-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("Model.kt");
        fs::write(
            &path,
            r#"
            package sample
            interface Base {
            }
            interface Other {
            }
            class Sub : Base {
            }
            interface Contract {
                val items: List<Base>
            }
            interface SecondContract {
                val items: List<Other>
            }
            class Impl : Contract {
                override val items: List<Sub> = emptyList()
            }
            class MultiImpl : Contract, SecondContract {
                override val items: List<Sub> = emptyList()
            }
            "#,
        )
        .unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        let contract = index
            .declarations()
            .find(|decl| decl.name == "Contract")
            .unwrap();
        let implementation = index
            .declarations()
            .find(|decl| decl.name == "Impl")
            .unwrap();
        let conflicts = index.retained_supertype_member_conflicts_in_file(
            &path,
            &implementation.supertypes,
            &implementation.name,
        );
        let retained = HashSet::from([crate::semantics::workspace_symbol(&index, contract)]);
        let requirements = index
            .readonly_list_jvm_wildcard_requirements(
                implementation,
                "items",
                &conflicts,
                std::slice::from_ref(&root),
                &retained,
            )
            .expect("the readonly List covariance has one exact source contract");
        assert_eq!(requirements.len(), 1);
        let requirement = &requirements[0];
        assert_eq!(
            &index.source_file(&path).unwrap().source_text()
                [requirement.location.start_byte..requirement.location.end_byte],
            "Base"
        );
        assert_eq!(requirement.replacement, "@kotlin.jvm.JvmWildcard Base");
        assert!(!requirement.already_present);
        assert_eq!(requirement.contract.name, "Contract");
        assert_eq!(requirement.implementation.name, "Impl");

        let multi = index
            .declarations()
            .find(|decl| decl.name == "MultiImpl")
            .unwrap();
        let multi_conflicts = index.retained_supertype_member_conflicts_in_file(
            &path,
            &multi.supertypes,
            &multi.name,
        );
        let retained = index
            .declarations()
            .filter(|decl| matches!(decl.name.as_str(), "Contract" | "SecondContract"))
            .map(|decl| crate::semantics::workspace_symbol(&index, decl))
            .collect();
        assert!(
            index
                .readonly_list_jvm_wildcard_requirements(
                    multi,
                    "items",
                    &multi_conflicts,
                    std::slice::from_ref(&root),
                    &retained,
                )
                .is_none(),
            "one incompatible parent rejects the whole edit group"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn readonly_list_wildcard_rejects_nullable_mutable_and_shadowed_contracts() {
        fn assert_rejected(suffix: &str, source: &str) {
            let root = std::env::temp_dir().join(format!(
                "notlin-jvm-wildcard-rejected-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            let path = root.join("Model.kt");
            fs::write(&path, source).unwrap();
            let index = SourceIndex::discover(&root).unwrap();
            let contract = index
                .declarations()
                .find(|decl| decl.name == "Contract")
                .unwrap();
            let implementation = index
                .declarations()
                .find(|decl| decl.name == "Impl")
                .unwrap();
            let conflicts = index.retained_supertype_member_conflicts_in_file(
                &path,
                &implementation.supertypes,
                &implementation.name,
            );
            let retained = HashSet::from([crate::semantics::workspace_symbol(&index, contract)]);
            assert!(
                index
                    .readonly_list_jvm_wildcard_requirements(
                        implementation,
                        "items",
                        &conflicts,
                        std::slice::from_ref(&root),
                        &retained,
                    )
                    .is_none(),
                "{suffix} must not gain a wildcard contract"
            );
            let _ = fs::remove_dir_all(root);
        }

        let base = "package sample\ninterface Base {\n}\nclass Sub : Base {\n}\n";
        assert_rejected(
            "nullable-element",
            &format!(
                "{base}interface Contract {{\n val items: List<Base>\n}}\nclass Impl : Contract {{\n override val items: List<Sub?> = emptyList()\n}}\n"
            ),
        );
        assert_rejected(
            "nullable-outer",
            &format!(
                "{base}interface Contract {{\n val items: List<Base>\n}}\nclass Impl : Contract {{\n override val items: List<Sub>? = null\n}}\n"
            ),
        );
        assert_rejected(
            "mutable",
            &format!(
                "{base}interface Contract {{\n var items: List<Base>\n}}\nclass Impl : Contract {{\n override var items: List<Sub> = emptyList()\n}}\n"
            ),
        );
        assert_rejected(
            "typealias-shadow",
            &format!(
                "{base}typealias List<T> = kotlin.collections.List<T>\ninterface Contract {{\n val items: List<Base>\n}}\nclass Impl : Contract {{\n override val items: List<Sub> = emptyList()\n}}\n"
            ),
        );
        assert_rejected(
            "import-shadow",
            "package sample\nimport external.List\ninterface Base {\n}\nclass Sub : Base {\n}\ninterface Contract {\n val items: List<Base>\n}\nclass Impl : Contract {\n override val items: List<Sub> = emptyList()\n}\n",
        );
    }

    #[test]
    fn fully_qualified_jvm_wildcard_proves_only_readonly_list_abi_covariance() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "notlin-jvm-wildcard-abi-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("Model.kt");
        let source = "package sample\ninterface Base {\n}\nclass Sub : Base {\n}\ninterface Contract {\n val items: List<@kotlin.jvm.JvmWildcard Base>\n}\nclass Impl : Contract {\n override val items: List<Sub> = emptyList()\n}\n";
        fs::write(&path, source).unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        let file = index.source_file(&path).unwrap();
        let contract = index
            .declarations()
            .find(|decl| decl.name == "Contract")
            .unwrap();
        let implementation = index
            .declarations()
            .find(|decl| decl.name == "Impl")
            .unwrap();
        let member_type = contract.members[0].type_name.as_deref().unwrap();
        assert!(readonly_list_jvm_wildcard_abi_proven(
            &index,
            file,
            contract,
            "items",
            member_type,
        ));
        assert!(!readonly_list_jvm_wildcard_abi_proven(
            &index,
            file,
            contract,
            "items",
            "MutableList<@kotlin.jvm.JvmWildcard Base>",
        ));
        let conflicts = index.retained_supertype_member_conflicts_in_file(
            &path,
            &implementation.supertypes,
            &implementation.name,
        );
        let retained = HashSet::from([crate::semantics::workspace_symbol(&index, contract)]);
        let conflict = conflicts
            .iter()
            .find(|conflict| conflict.member_name == "items")
            .expect("the ABI mismatch remains visible to the planner");
        assert!(index.selected_covariant_property_conflict_is_safe(
            &path,
            conflict,
            std::slice::from_ref(&root),
            &retained,
        ));
        let requirements = index
            .readonly_list_jvm_wildcard_requirements(
                implementation,
                "items",
                &conflicts,
                std::slice::from_ref(&root),
                &retained,
            )
            .unwrap();
        assert_eq!(requirements.len(), 1);
        assert!(requirements[0].already_present);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn java_type_parameter_bindings_flow_through_mixed_generic_supertypes() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "notlin-java-generic-edge-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("Base.kt"),
            "package sample\ninterface Identifier {}\ninterface RecordId : Identifier {}\ninterface RecordView {}\ninterface CreatedNotice<T, I> {\n    fun getPayload(): T\n}\nclass ConcreteRecord : RecordView {}\nclass ConcreteNotice : TypedNotice<ConcreteRecord> {\n    override fun getPayload(): ConcreteRecord = ConcreteRecord()\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("GenericNotice.java"),
            "package sample; public interface GenericNotice<V extends RecordView, I extends Identifier> extends CreatedNotice<V, I> {}\n",
        )
        .unwrap();
        fs::write(
            root.join("TypedNotice.java"),
            "package sample; public interface TypedNotice<U extends RecordView> extends GenericNotice<U, RecordId> {}\n",
        )
        .unwrap();

        let index = SourceIndex::discover(&root).unwrap();
        let java_event = index
            .declarations()
            .find(|declaration| declaration.name == "TypedNotice")
            .unwrap();
        assert_eq!(java_event.type_params, ["U"]);
        let implementation_file = root.join("Base.kt");
        let implementation = index
            .declarations()
            .find(|declaration| declaration.name == "ConcreteNotice")
            .unwrap();
        let conflicts = index.retained_supertype_member_conflicts_in_file(
            &implementation_file,
            &implementation.supertypes,
            &implementation.name,
        );
        let payload = conflicts
            .iter()
            .find(|conflict| conflict.member_name == "getPayload")
            .expect("the inherited generic member should be considered");
        assert_eq!(payload.inherited_type, "ConcreteRecord");
        assert_eq!(payload.implementation_type, "ConcreteRecord");
        assert!(
            !payload.blocks_java_override(),
            "the Java U binding must reach the inherited Kotlin member: {payload:?}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recovered_annotated_generic_interface_indexes_its_property_contract() {
        let source = r#"
@JsonTypeInfo(
    use = JsonTypeInfo.Id.NAME,
    include = JsonTypeInfo.As.PROPERTY,
    property = "_class"
)
@JsonSubTypes(
    JsonSubTypes.Type(value = ContextA::class),
    JsonSubTypes.Type(value = ContextB::class),
    JsonSubTypes.Type(value = ContextC::class)
)
interface Example<T> {
    @get:GetterTag
    val contextKind: T
}

@JsonSubTypes(
    JsonSubTypes.Type(value = SiblingRequest::class),
    JsonSubTypes.Type(value = SiblingContext::class),
)
interface SiblingContext {
    val items: List<String>
}
"#;
        let parsed = parse_declarations(source, SourceLanguage::Kotlin, Some("sample"))
            .expect("fixture parses with recovery");
        let declaration = parsed
            .0
            .iter()
            .find(|declaration| declaration.name == "Example")
            .expect("recovered interface is indexed");
        assert_eq!(declaration.kind, DeclarationKind::Interface);
        assert_eq!(declaration.type_params, ["T"]);
        assert!(declaration.members.iter().any(|member| {
            member.name == "contextKind"
                && member.kind == MemberKind::Property
                && member.type_name.as_deref() == Some("T")
        }));
    }

    #[test]
    fn recovered_annotated_interface_with_getter_annotation_indexes_member() {
        let source = r#"
@JsonTypeInfo(
    use = JsonTypeInfo.Id.NAME,
    include = JsonTypeInfo.As.PROPERTY,
    property = "_class"
)
@JsonSubTypes(
    JsonSubTypes.Type(value = ContextA::class),
    JsonSubTypes.Type(value = ContextB::class),
    JsonSubTypes.Type(value = ContextC::class)
)
interface ContextContract {
    @get:GetterTag
    val contextKind: String
}

@JsonSubTypes(
    JsonSubTypes.Type(value = SiblingRequest::class),
    JsonSubTypes.Type(value = SiblingContext::class),
)
interface SiblingContext {
    val items: List<String>
}
"#;
        let parsed = parse_declarations(source, SourceLanguage::Kotlin, Some("sample"))
            .expect("fixture parses with recovery");
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut stack = vec![tree.root_node()];
        let mut has_recovered_context_contract = false;
        while let Some(node) = stack.pop() {
            has_recovered_context_contract |= recovered_interface_parts(node, source)
                .is_some_and(|(name, _)| name == "ContextContract");
            let mut walk = node.walk();
            stack.extend(node.named_children(&mut walk));
        }
        assert!(
            has_recovered_context_contract,
            "the getter-annotated fixture must exercise parser recovery"
        );
        let declaration = parsed
            .0
            .iter()
            .find(|declaration| declaration.name == "ContextContract")
            .expect("annotated interface is indexed");
        assert_eq!(declaration.kind, DeclarationKind::Interface);
        assert!(declaration.members.iter().any(|member| {
            member.name == "contextKind"
                && member.kind == MemberKind::Property
                && member.type_name.as_deref() == Some("String")
        }));
        assert!(
            declaration
                .members
                .iter()
                .all(|member| member.name != "items")
        );
    }

    #[test]
    fn per_file_symbols_match_direct_resolution_for_nested_overloaded_and_recovered_sources() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "notlin-symbol-cache-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        let kotlin = root.join("Types.kt");
        fs::write(
            &kotlin,
            "package sample\nclass Outer { class Nested; fun convert(x: String): String = x; fun convert(x: Int): Int = x }\nclass Duplicate\nclass Duplicate\nclass Recovered(\n",
        )
        .unwrap();
        let java = root.join("Types.java");
        fs::write(
            &java,
            "package sample; interface Api { String getName(); String getName(int n); class Child {} }\n",
        )
        .unwrap();

        let index = SourceIndex::discover(&root).unwrap();
        for file in &index.files {
            let batched = crate::semantics::workspace_symbols_for_file(file);
            assert_eq!(batched.len(), file.declarations.len());
            for (declaration, cached) in file.declarations.iter().zip(batched) {
                let direct = crate::semantics::workspace_symbol_uncached(&index, declaration);
                assert_eq!(
                    cached,
                    direct,
                    "{}: {}",
                    file.path.display(),
                    declaration.name
                );
                let retained = HashSet::from([direct.clone()]);
                assert!(index.declaration_retained(declaration, &retained));
                assert!(!index.declaration_retained(declaration, &HashSet::new()));
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn member_name_index_preserves_candidate_order_enum_preference_and_file_priority() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "notlin-member-index-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("A.kt"),
            "package sample\nenum class Stage { READY }\nclass A { val status: String = \"a\"; fun getThing(): String = \"a\" }\n",
        )
        .unwrap();
        fs::write(
            root.join("B.kt"),
            "package sample\nclass B { val status: Stage = Stage.READY; fun thing(): Stage = status }\n",
        )
        .unwrap();
        fs::write(
            root.join("C.kt"),
            "package sample\nclass C { val status: String = \"local\"; fun local(): Int = 1 }\n",
        )
        .unwrap();
        fs::write(root.join("D.kt"), "package sample\nclass D\n").unwrap();

        let index = SourceIndex::discover(&root).unwrap();
        assert_eq!(
            index.bare_property_type("status").as_deref(),
            Some("String")
        );
        assert_eq!(
            index.property_type_of_getter("getStatus").as_deref(),
            Some("String")
        );
        assert_eq!(
            index
                .property_type_in_file(&root.join("C.kt"), "status")
                .as_deref(),
            Some("String")
        );
        assert_eq!(
            index
                .property_type_in_file(&root.join("D.kt"), "status")
                .as_deref(),
            Some("Stage")
        );
        assert_eq!(
            index
                .method_return_type_in_file(&root.join("D.kt"), "getThing()")
                .as_deref(),
            Some("String")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn nullable_builtin_getter_identity_preserves_nullability_and_import_scope() {
        let left_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("facts/Left.kt");
        let right_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("facts/Right.kt");
        let index = SourceIndex::from_files(Vec::new())
            .with_overlays(&[
                SourceOverlay::Replace {
                    path: left_path.clone(),
                    language: SourceLanguage::Kotlin,
                    source: "package left\ninterface Left\n".into(),
                },
                SourceOverlay::Replace {
                    path: right_path.clone(),
                    language: SourceLanguage::Kotlin,
                    source: "package right\ninterface Right\n".into(),
                },
            ])
            .unwrap();
        for builtin in ["String", "Boolean", "Int", "Char"] {
            let nullable = format!("{builtin}?");
            assert!(index.property_getter_return_compatible_in_files(
                &left_path,
                &nullable,
                &right_path,
                &nullable,
            ));
            assert!(!index.property_getter_return_compatible_in_files(
                &left_path,
                &nullable,
                &right_path,
                builtin,
            ));
        }
        for import in ["import external.String", "import external.Text as String"] {
            let shadowed = index
                .with_overlays(&[SourceOverlay::Replace {
                    path: left_path.clone(),
                    language: SourceLanguage::Kotlin,
                    source: format!("package left\n{import}\ninterface Left\n"),
                }])
                .unwrap();
            assert!(
                !shadowed.property_getter_return_compatible_in_files(
                    &left_path,
                    "String?",
                    &right_path,
                    "String?",
                ),
                "conflicting import: {import}"
            );
        }
    }

    #[test]
    fn java_util_uuid_wildcards_share_identity_across_packages() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "notlin-jdk-uuid-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        let left_path = root.join("Left.kt");
        let right_path = root.join("Right.kt");
        fs::write(
            &left_path,
            "package left\nimport java.util.*\ninterface Left<T> { val id: T }\ninterface LeftEvent : Left<UUID>\n",
        )
        .unwrap();
        fs::write(
            &right_path,
            "package right\nimport java.util.*\ninterface Right { val id: UUID }\n",
        )
        .unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        let left = index.source_file(&left_path).unwrap();
        let right = index.source_file(&right_path).unwrap();
        assert!(source_types_have_same_identity(
            &index, left, "UUID", right, "UUID"
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn wildcard_jdk_uuid_does_not_match_an_indexed_domain_uuid_collision() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "notlin-jdk-uuid-collision-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        let left_path = root.join("Left.kt");
        let right_path = root.join("Right.kt");
        fs::write(
            &left_path,
            "package left\nimport java.util.*\ninterface Left { val id: UUID }\n",
        )
        .unwrap();
        fs::write(
            &right_path,
            "package right\nimport java.util.*\ninterface Right { val id: UUID }\n",
        )
        .unwrap();
        fs::write(root.join("Domain.kt"), "package domain\nclass UUID\n").unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        let left = index.source_file(&left_path).unwrap();
        let right = index.source_file(&right_path).unwrap();
        assert!(!source_types_have_same_identity(
            &index, left, "UUID", right, "UUID"
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn default_property_provider_is_retained_only_for_a_retained_conflicting_descendant() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        for (
            retained_descendant,
            explicit_getter,
            explicit_overload,
            repaired_method,
            provider_type,
            expected,
        ) in [
            (true, false, false, false, "NarrowRef", false),
            (false, false, false, false, "NarrowRef", false),
            (true, true, false, false, "NarrowRef", false),
            (true, false, true, false, "NarrowRef", false),
            (true, false, false, false, "BaseRef", false),
            (true, false, false, true, "NarrowRef", false),
            (true, false, false, false, "AliasRef", true),
            (false, false, false, false, "AliasRef", false),
        ] {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join(format!(
                    "notlin-default-provider-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&root).unwrap();
            let provider_member = if repaired_method {
                format!("override fun getRef(): {provider_type} = NarrowRef()")
            } else {
                format!("override val ref: {provider_type} get() = NarrowRef()")
            };
            fs::write(
                root.join("Contracts.kt"),
                format!("package sample\ninterface BaseRef\nclass NarrowRef : BaseRef\ntypealias AliasRef = NarrowRef\ninterface BroadContract {{\n    val ref: BaseRef\n}}\ninterface DefaultContract : BroadContract {{\n    {provider_member}\n}}\n"),
            )
            .unwrap();
            let consumer_source = if explicit_getter {
                "package sample\nclass Consumer : DefaultContract {\n    override val ref: NarrowRef get() = NarrowRef()\n}\n"
            } else if explicit_overload {
                "package sample\nclass Consumer : DefaultContract {\n    fun getRef(other: String): NarrowRef = NarrowRef()\n}\n"
            } else {
                "package sample\nclass Consumer : DefaultContract\n"
            };
            fs::write(root.join("Consumer.kt"), consumer_source).unwrap();

            let index = SourceIndex::discover(&root).unwrap();
            let provider = index
                .declarations()
                .find(|declaration| declaration.name == "DefaultContract")
                .unwrap();
            let consumer = index
                .declarations()
                .find(|declaration| declaration.name == "Consumer")
                .unwrap();
            let retained = if retained_descendant {
                HashSet::from([crate::semantics::workspace_symbol(&index, consumer)])
            } else {
                HashSet::new()
            };
            let reason = index.default_property_provider_needs_physical_getter(
                provider,
                &retained,
                std::slice::from_ref(&root),
            );
            assert_eq!(reason.is_some(), expected, "{reason:?}");
            let _ = fs::remove_dir_all(root);
        }
    }

    fn reference_direct_subtypes<'a>(
        index: &'a SourceIndex,
        target: &Declaration,
    ) -> Vec<&'a Declaration> {
        let target_path = index.declaration_source_file(target).map(|file| &file.path);
        index
            .direct_subtype_names(target)
            .into_iter()
            .filter_map(|name| index.declaration_names.get(&name))
            .flatten()
            .filter_map(|(file_index, declaration_index)| {
                let file = index.files.get(*file_index)?;
                let declaration = file.declarations.get(*declaration_index)?;
                declaration
                    .supertypes
                    .iter()
                    .any(|supertype| {
                        index.resolve_type(file, supertype).is_some_and(|resolved| {
                            resolved.name == target.name
                                && index.declaration_source_file(resolved).is_some_and(
                                    |resolved_file| target_path == Some(&resolved_file.path),
                                )
                        })
                    })
                    .then_some(declaration)
            })
            .collect()
    }

    fn subtype_identities(
        index: &SourceIndex,
        declarations: Vec<&Declaration>,
    ) -> Vec<(String, Option<String>, String)> {
        declarations
            .into_iter()
            .map(|declaration| {
                let file = index.declaration_source_file(declaration).unwrap();
                (
                    file.path.to_string_lossy().into_owned(),
                    declaration.package.clone(),
                    declaration.name.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn direct_subtype_cache_matches_legacy_order_collisions_dedup_and_overlays() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "notlin-direct-subtype-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("00-q.kt"),
            "package q\ninterface Hub {}\nclass Alpha : p.Hub {}\nclass Beta : Hub {}\n",
        )
        .unwrap();
        let p_path = root.join("10-p.kt");
        fs::write(
            &p_path,
            "package p\ninterface Hub {}\ninterface Other {}\nclass Alpha : Hub, Hub {}\nclass Zulu : Hub {}\nclass Moved : Other {}\n",
        )
        .unwrap();

        let index = SourceIndex::discover(&root).unwrap();
        let compare = |index: &SourceIndex| {
            for target in index.declarations().filter(|item| item.name == "Hub") {
                let cached = index.direct_subtypes(target);
                let reference = reference_direct_subtypes(index, target);
                assert_eq!(
                    subtype_identities(index, cached),
                    subtype_identities(index, reference),
                    "target {:?}",
                    crate::semantics::workspace_symbol(index, target)
                );
            }
        };
        compare(&index);

        let p_hub = index
            .declarations()
            .find(|item| item.name == "Hub" && item.package.as_deref() == Some("p"))
            .unwrap();
        let cached = index.direct_subtypes(p_hub);
        assert_eq!(
            cached
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "Alpha", "Zulu"],
            "same-name descendants retain file order and repeated supertypes deduplicate"
        );
        let cloned_target = p_hub.clone();
        assert_eq!(
            subtype_identities(&index, index.direct_subtypes(&cloned_target)),
            subtype_identities(&index, reference_direct_subtypes(&index, &cloned_target)),
            "non-index declarations preserve the prior fallback behavior"
        );

        let rebuilt = index
            .with_overlays(&[SourceOverlay::Replace {
                path: p_path,
                language: SourceLanguage::Kotlin,
                source: "package p\ninterface Hub {}\ninterface Other {}\nclass Alpha : Hub, Hub {}\nclass Zulu : Other {}\nclass Moved : Hub {}\n".into(),
            }])
            .unwrap();
        compare(&rebuilt);
        let rebuilt_hub = rebuilt
            .declarations()
            .find(|item| item.name == "Hub" && item.package.as_deref() == Some("p"))
            .unwrap();
        assert_eq!(
            rebuilt
                .direct_subtypes(rebuilt_hub)
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "Alpha", "Moved"]
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod kotlin_declaration_count_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!(
            "notlin-kotlin-declaration-count-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn cached_count_tracks_top_level_ownership_boundaries_only() {
        let root = scratch();
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("sample.kt"),
            "package sample\n\
             class Outer {\n\
                 class Nested {\n\
                     fun nestedMethod() = 1\n\
                 }\n\
                 fun memberMethod() = 2\n\
                 val memberProperty = 3\n\
             }\n\
             object Singleton {\n\
                 fun memberMethod() = 4\n\
             }\n\
             fun topLevelFunction() = 5\n\
             val topLevelProperty = 6\n\
             typealias Alias = String\n",
        )
        .unwrap();
        fs::write(
            root.join("Sample.java"),
            "package sample; public class Sample { public void method() {} }\n",
        )
        .unwrap();

        let (index, first_stats) = SourceIndex::discover_with_stats(&root).unwrap();
        assert_eq!(first_stats.parsed_files, 2);
        let kotlin = index
            .source_file(&root.join("sample.kt"))
            .expect("Kotlin source indexed");
        let java = index
            .source_file(&root.join("Sample.java"))
            .expect("Java source indexed");
        assert_eq!(kotlin.kotlin_declaration_count(), 5);
        assert_eq!(java.kotlin_declaration_count(), 0);

        let (warm, warm_stats) = SourceIndex::discover_with_stats(&root).unwrap();
        assert_eq!(warm_stats.parsed_files, 0);
        assert_eq!(warm_stats.reused_files, 2);
        assert_eq!(
            warm.source_file(&root.join("sample.kt"))
                .unwrap()
                .kotlin_declaration_count(),
            5,
            "warm cache reads reuse the serialized count without reparsing"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn overlays_recompute_counts_from_the_overlay_parser_tree() {
        let root = scratch();
        fs::create_dir_all(&root).unwrap();
        let path = root.join("sample.kt");
        fs::write(
            &path,
            "package sample\nclass Before {\n    fun member() = 1\n}\n",
        )
        .unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        assert_eq!(
            index.source_file(&path).unwrap().kotlin_declaration_count(),
            1
        );

        let overlay = index
            .with_overlays(&[SourceOverlay::Replace {
                path: path.clone(),
                language: SourceLanguage::Kotlin,
                source: "package sample\nclass After {\n    class Nested {\n        fun nested() = 2\n    }\n}\nfun top() = 3\nval property = 4\ntypealias Alias = String\n".into(),
            }])
            .unwrap();
        assert_eq!(
            overlay
                .source_file(&path)
                .unwrap()
                .kotlin_declaration_count(),
            4
        );

        let java_overlay = index
            .with_overlays(&[SourceOverlay::Replace {
                path: path.clone(),
                language: SourceLanguage::Java,
                source: "package sample; class Replacement { void method() {} }".into(),
            }])
            .unwrap();
        assert_eq!(
            java_overlay
                .source_file(&path)
                .unwrap()
                .kotlin_declaration_count(),
            0
        );
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod member_modifier_tests {
    use super::*;

    #[test]
    fn annotation_arguments_do_not_become_member_modifiers() {
        let source = r#"package sample
import kotlin.jvm.JvmField as Storage
class Sample(
    @Tag("public val bogus; JvmField open lateinit abstract by") private var id: String,
    @JvmField val stable: String,
    @kotlin.jvm.JvmField var qualified: String,
    @Storage val aliased: String
) {
    @Tag(" public open static lateinit JvmField ") private val value: String = ""
    @Tag(" public static ") private fun inspect() {}
}
"#;
        let declarations = parse_declarations(source, SourceLanguage::Kotlin, Some("sample"))
            .unwrap()
            .0;
        let owner = declarations
            .iter()
            .find(|owner| owner.name == "Sample")
            .unwrap();
        for name in ["id", "value", "inspect"] {
            let member = owner
                .members
                .iter()
                .find(|member| member.name == name)
                .unwrap();
            assert_eq!(member.visibility.as_deref(), Some("private"), "{member:?}");
            assert!(!member.is_static && !member.is_jvm_field, "{member:?}");
            assert!(!member.has_unsupported_property_shape, "{member:?}");
        }
        for name in ["stable", "qualified", "aliased"] {
            let member = owner
                .members
                .iter()
                .find(|member| member.name == name)
                .unwrap();
            assert!(member.is_jvm_field, "{member:?}");
        }
    }
}
