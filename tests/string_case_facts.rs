//! One typed fact for Kotlin `String.lowercase`/`uppercase` calls.
//!
//! The direction and the locale contract are properties of the *resolved*
//! callee, so the same fact drives both retention planning and Java emission:
//!
//! * with Track B call-site facts, the stdlib identity (`kotlin.text.StringsKt`,
//!   `kotlin.String` result, exact erased descriptor) decides direction and
//!   arity — an import alias such as `import kotlin.text.lowercase as fold`
//!   lowers to `toLowerCase(Locale.ROOT)`, never to `value.fold()`;
//! * without them, the source shape proves the same fact, which keeps the
//!   helper-off floor byte-identical;
//! * an authoritative site with a foreign owner or a shape this lowering does
//!   not implement is retained — spelling is never resurrected as evidence;
//! * the caller's snapshot hash and the callee selector's offset are the only
//!   keys: a speculative revision or a shifted offset gets no fact.

use clap::Parser;
use notlin::facts::protocol::{CallSite, SiteStatus};
use notlin::semantics::{
    FactStatus, SemanticProvider, SemanticSymbol, SymbolId, SyntaxSemanticProvider,
    kotlin_call_site_offset,
};
use notlin::transpiler::{PlannedTranslation, WorkspaceScope, plan_with_tree_hint};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A provider with the syntax index plus a scripted call-site map that enforces
/// the same hash guard `FactsRuntime::call_site_at` enforces: a site answers
/// only for a snapshot whose blake3 equals the bytes the helper saw.
struct ScriptedProvider {
    inner: SyntaxSemanticProvider,
    sites: HashMap<(PathBuf, u64), CallSite>,
    disk_hash: [u8; 32],
}

impl ScriptedProvider {
    fn new(
        files: &[(PathBuf, String)],
        disk_source: &str,
        sites: HashMap<(PathBuf, u64), CallSite>,
    ) -> Self {
        Self {
            inner: SyntaxSemanticProvider::new(files.to_vec()),
            sites,
            disk_hash: *blake3::hash(disk_source.as_bytes()).as_bytes(),
        }
    }
}

impl SemanticProvider for ScriptedProvider {
    fn resolve(&self, name: &str) -> FactStatus<SymbolId> {
        self.inner.resolve(name)
    }

    fn symbols(&self) -> &[SemanticSymbol] {
        self.inner.symbols()
    }

    fn source_for_file(&self, file: &Path) -> Option<&str> {
        self.inner.source_for_file(file)
    }

    fn established_call_site(
        &self,
        file: &Path,
        byte_offset: u64,
        source_hash: &[u8; 32],
    ) -> Option<CallSite> {
        if *source_hash != self.disk_hash {
            return None;
        }
        self.sites.get(&(file.to_path_buf(), byte_offset)).cloned()
    }
}

fn parse(source: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture:\n{source}");
    tree
}

/// Byte offset of `member` inside the first occurrence of `call_text`, checked
/// against the planner's own selector convention (`kotlin_call_site_offset`).
fn selector_offset(source: &str, call_text: &str, member: &str) -> u64 {
    let start = source
        .find(call_text)
        .unwrap_or_else(|| panic!("fixture has no {call_text:?}"));
    let offset = (start + call_text.find(member).expect("member inside call")) as u64;
    let tree = parse(source);
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression"
            && node.utf8_text(source.as_bytes()).ok() == Some(call_text)
        {
            assert_eq!(
                kotlin_call_site_offset(node),
                Some(offset as usize),
                "selector offset disagrees with the planner for {call_text:?}"
            );
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    offset
}

/// The helper contract a fixture call site is scripted with.
struct SiteSpec<'a> {
    call_text: &'a str,
    member: &'a str,
    callee_name: &'a str,
    parameter_types: &'a [&'a str],
    result_type: &'a str,
    nullable: bool,
    descriptor: &'a str,
}

impl<'a> SiteSpec<'a> {
    /// The zero-argument invariant conversion.
    fn invariant(call_text: &'a str, member: &'a str, callee_name: &'a str) -> Self {
        Self {
            call_text,
            member,
            callee_name,
            parameter_types: &[],
            result_type: "kotlin.String",
            nullable: false,
            descriptor: "()Ljava/lang/String;",
        }
    }

    /// The explicit `java.util.Locale` overload.
    fn explicit_locale(call_text: &'a str, member: &'a str, callee_name: &'a str) -> Self {
        Self {
            call_text,
            member,
            callee_name,
            parameter_types: &["java.util.Locale"],
            result_type: "kotlin.String",
            nullable: false,
            descriptor: "(Ljava/util/Locale;)Ljava/lang/String;",
        }
    }
}

fn framework_site(source: &str, spec: &SiteSpec<'_>) -> ((PathBuf, u64), CallSite) {
    let offset = selector_offset(source, spec.call_text, spec.member);
    (
        (PathBuf::new(), offset),
        CallSite {
            byte_offset: offset,
            text: spec.call_text[spec.call_text.find(spec.member).unwrap()..].to_string(),
            status: SiteStatus::Established,
            callee_owner: Some("kotlin.text.StringsKt".into()),
            callee_name: Some(spec.callee_name.into()),
            parameter_types: spec
                .parameter_types
                .iter()
                .map(|ty| (*ty).to_string())
                .collect(),
            result_type: Some(spec.result_type.into()),
            result_nullable: spec.nullable,
            jvm_descriptor: Some(spec.descriptor.into()),
        },
    )
}

fn sites_for(
    file: &Path,
    entries: Vec<((PathBuf, u64), CallSite)>,
) -> HashMap<(PathBuf, u64), CallSite> {
    entries
        .into_iter()
        .map(|((_, offset), mut site)| {
            site.byte_offset = offset;
            ((file.to_path_buf(), offset), site)
        })
        .collect()
}

fn translate(source: &str, file: &Path, provider: &dyn SemanticProvider) -> PlannedTranslation {
    plan_with_tree_hint(
        source,
        &parse(source),
        file,
        &notlin::cli::Cli::parse_from(["notlin", "--annotations", "none", "Fixture.kt"]),
        WorkspaceScope {
            semantic_provider: Some(provider),
            ..Default::default()
        },
        true,
    )
}

fn java(result: &PlannedTranslation) -> String {
    assert!(!result.emission_failed, "{:#?}", result.plan.diagnostics);
    assert_eq!(result.errors, 0, "{:#?}", result.plan.diagnostics);
    result
        .java_files
        .iter()
        .map(|(_, source)| source.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The call was retained: the owning class has no emitted Java and the plan
/// says why. Other declarations in the same fixture may still translate.
fn retained(result: &PlannedTranslation, class: &str) -> bool {
    !result
        .java_files
        .iter()
        .any(|(_, source)| source.contains(&format!("class {class} ")))
        && result
            .plan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "S001")
}

const CANONICAL: &str = r#"package stringcase
import java.util.Locale
class Casing {
    fun lower(value: String): String = value.lowercase()
    fun upper(value: String): String = value.uppercase()
    fun lowerLocale(value: String): String = value.lowercase(Locale.getDefault())
    fun upperLocal(value: String): String {
        val locale = Locale.getDefault()
        return value.uppercase(locale)
    }
}
"#;

fn canonical_sites(file: &Path) -> HashMap<(PathBuf, u64), CallSite> {
    sites_for(
        file,
        vec![
            framework_site(
                CANONICAL,
                &SiteSpec::invariant("value.lowercase()", "lowercase", "lowercase"),
            ),
            framework_site(
                CANONICAL,
                &SiteSpec::invariant("value.uppercase()", "uppercase", "uppercase"),
            ),
            framework_site(
                CANONICAL,
                &SiteSpec::explicit_locale(
                    "value.lowercase(Locale.getDefault())",
                    "lowercase",
                    "lowercase",
                ),
            ),
            framework_site(
                CANONICAL,
                &SiteSpec::explicit_locale("value.uppercase(locale)", "uppercase", "uppercase"),
            ),
        ],
    )
}

fn assert_canonical_java(result: &PlannedTranslation) {
    let java = java(result);
    assert_eq!(result.warnings, 0, "{java}");
    assert!(
        java.contains("(value).toLowerCase(java.util.Locale.ROOT)"),
        "{java}"
    );
    assert!(
        java.contains("(value).toUpperCase(java.util.Locale.ROOT)"),
        "{java}"
    );
    assert!(
        java.contains("(value).toLowerCase(Locale.getDefault())"),
        "{java}"
    );
    assert!(java.contains("(value).toUpperCase(locale)"), "{java}");
    assert!(!java.contains(".lowercase("), "{java}");
    assert!(!java.contains(".uppercase("), "{java}");
}

#[test]
fn resolved_stdlib_calls_emit_root_for_invariant_and_pass_explicit_locale() {
    let source = CANONICAL;
    let file = Path::new("Casing.kt");
    let provider = ScriptedProvider::new(
        &[(file.to_path_buf(), source.to_owned())],
        source,
        canonical_sites(file),
    );
    let result = translate(source, file, &provider);
    assert_canonical_java(&result);
}

#[test]
fn aliased_stdlib_imports_lower_by_resolved_identity_not_spelling() {
    // `fold`/`raise` are import aliases for the stdlib overloads, so the
    // callee's Kotlin name is `lowercase`/`uppercase` while every call-site
    // spelling differs. Emitting `value.fold()` is invalid Java; emitting
    // `toLowerCase()` without the invariant Locale silently changes results.
    let source = r#"package stringcase.alias
import java.util.Locale
import kotlin.text.lowercase as fold
import kotlin.text.uppercase as raise
class Aliasing {
    fun lower(value: String): String = value.fold()
    fun upper(value: String): String = value.raise()
    fun lowerLocale(value: String): String = value.fold(locale = Locale.getDefault())
}
"#;
    let file = Path::new("Aliasing.kt");
    let sites = sites_for(
        file,
        vec![
            framework_site(
                source,
                &SiteSpec::invariant("value.fold()", "fold", "lowercase"),
            ),
            framework_site(
                source,
                &SiteSpec::invariant("value.raise()", "raise", "uppercase"),
            ),
            framework_site(
                source,
                &SiteSpec::explicit_locale(
                    "value.fold(locale = Locale.getDefault())",
                    "fold",
                    "lowercase",
                ),
            ),
        ],
    );
    let provider = ScriptedProvider::new(&[(file.to_path_buf(), source.to_owned())], source, sites);
    let result = translate(source, file, &provider);
    let java = java(&result);
    assert_eq!(result.warnings, 0, "{java}");
    assert!(
        java.contains("(value).toLowerCase(java.util.Locale.ROOT)"),
        "{java}"
    );
    assert!(
        java.contains("(value).toUpperCase(java.util.Locale.ROOT)"),
        "{java}"
    );
    assert!(
        java.contains("(value).toLowerCase(Locale.getDefault())"),
        "{java}"
    );
    assert!(!java.contains(".fold("), "{java}");
    assert!(!java.contains(".raise("), "{java}");
}

#[test]
fn contradictory_authoritative_alias_sites_are_retained() {
    // The call is spelt `fold`, but `import kotlin.text.lowercase as fold`
    // binds it to the stdlib conversion. An authoritative fact that resolves
    // it to anything else — another owner, another stdlib function, a
    // different result contract, or a shape this lowering does not implement —
    // must retain exactly like a contradictory `lowercase()` site, never emit
    // the Kotlin alias into Java.
    let source = r#"package stringcase.alias
import kotlin.text.lowercase as fold
class Aliasing {
    fun lower(value: String): String = value.fold()
}
"#;
    let file = Path::new("Aliasing.kt");
    /// One authoritative fact for the aliased call that must not become the
    /// case lowering.
    struct Fact<'a> {
        label: &'a str,
        owner: &'a str,
        callee_name: &'a str,
        result_type: &'a str,
        nullable: bool,
        parameter_types: &'a [&'a str],
        descriptor: &'a str,
    }
    let cases = [
        Fact {
            label: "an unrelated owner answers the aliased site",
            owner: "external.CustomKt",
            callee_name: "lowercase",
            result_type: "kotlin.String",
            nullable: false,
            parameter_types: &[],
            descriptor: "()Ljava/lang/String;",
        },
        Fact {
            label: "the stdlib façade but a descriptor this lowering has no form for",
            owner: "kotlin.text.StringsKt",
            callee_name: "lowercase",
            result_type: "kotlin.String",
            nullable: false,
            parameter_types: &[],
            descriptor: "(I)Ljava/lang/String;",
        },
        Fact {
            label: "a different stdlib function behind the alias",
            owner: "kotlin.text.StringsKt",
            callee_name: "toString",
            result_type: "kotlin.String",
            nullable: false,
            parameter_types: &[],
            descriptor: "()Ljava/lang/String;",
        },
        Fact {
            label: "the aliased site declares a non-String result",
            owner: "kotlin.text.StringsKt",
            callee_name: "lowercase",
            result_type: "kotlin.CharSequence",
            nullable: false,
            parameter_types: &[],
            descriptor: "()Ljava/lang/String;",
        },
        Fact {
            label: "the aliased site declares a nullable result",
            owner: "kotlin.text.StringsKt",
            callee_name: "lowercase",
            result_type: "kotlin.String",
            nullable: true,
            parameter_types: &[],
            descriptor: "()Ljava/lang/String;",
        },
        Fact {
            label: "the aliased site declares the locale overload for a zero-arg call",
            owner: "kotlin.text.StringsKt",
            callee_name: "lowercase",
            result_type: "kotlin.String",
            nullable: false,
            parameter_types: &["java.util.Locale"],
            descriptor: "(Ljava/util/Locale;)Ljava/lang/String;",
        },
    ];
    let offset = selector_offset(source, "value.fold()", "fold");
    let text_start = source.find("value.fold()").unwrap();
    let selector =
        source[text_start + "value.".len()..text_start + "value.fold()".len()].to_string();
    for Fact {
        label,
        owner,
        callee_name,
        result_type,
        nullable,
        parameter_types,
        descriptor,
    } in cases
    {
        let site = CallSite {
            byte_offset: offset,
            text: selector.clone(),
            status: SiteStatus::Established,
            callee_owner: Some(owner.to_string()),
            callee_name: Some(callee_name.to_string()),
            parameter_types: parameter_types.iter().map(|ty| (*ty).to_string()).collect(),
            result_type: Some(result_type.to_string()),
            result_nullable: nullable,
            jvm_descriptor: Some(descriptor.to_string()),
        };
        let sites = HashMap::from([((file.to_path_buf(), offset), site)]);
        let provider =
            ScriptedProvider::new(&[(file.to_path_buf(), source.to_string())], source, sites);
        let result = translate(source, file, &provider);
        assert!(
            retained(&result, "Aliasing"),
            "{label} must retain: owner={owner}, callee={callee_name}, result={result_type}, \
             nullable={nullable}, descriptor={descriptor}, diagnostics={:?}, Java={:?}",
            result.plan.diagnostics,
            result.java_files
        );
    }
}

#[test]
fn aliased_spelling_without_facts_keeps_the_syntax_floor() {
    // No helper facts at all: the alias resolves to nothing the index knows, so
    // the ordinary unresolved-call retention applies. The claim predicate must
    // not need a fact (it cannot establish one) and must not silence this.
    let source = r#"package stringcase.alias
import kotlin.text.lowercase as fold
class Aliasing {
    fun lower(value: String): String = value.fold()
}
"#;
    let file = Path::new("Aliasing.kt");
    let provider = SyntaxSemanticProvider::new([(file.to_path_buf(), source.to_owned())]);
    let result = translate(source, file, &provider);
    assert!(
        retained(&result, "Aliasing"),
        "an unresolved aliased call stays retained: {:#?}",
        result.plan.diagnostics
    );
    assert!(
        result
            .java_files
            .iter()
            .all(|(_, java)| !java.contains(".fold")),
        "{:?}",
        result.java_files
    );
}

#[test]
fn resolved_named_locale_emits_the_argument_value_without_a_java_assignment() {
    let source = r#"package stringcase.named
import java.util.Locale
class NamedCase {
    fun lower(value: String): String = value.lowercase(locale = Locale.getDefault())
}
"#;
    let file = Path::new("NamedCase.kt");
    let sites = sites_for(
        file,
        vec![framework_site(
            source,
            &SiteSpec::explicit_locale(
                "value.lowercase(locale = Locale.getDefault())",
                "lowercase",
                "lowercase",
            ),
        )],
    );
    let provider =
        ScriptedProvider::new(&[(file.to_path_buf(), source.to_string())], source, sites);
    let result = translate(source, file, &provider);
    let output = java(&result);
    assert!(
        output.contains("(value).toLowerCase(Locale.getDefault())"),
        "named Kotlin argument must emit only its value: {output}"
    );
    assert!(!output.contains("locale ="), "{output}");
}

#[test]
fn contradictory_alias_facts_retain_with_legal_import_comments_and_tabs() {
    let file = Path::new("FormattedAlias.kt");
    let mut missed = Vec::new();
    for import in [
        "import kotlin.text.lowercase as fold // alias comment",
        "import kotlin.text.lowercase\tas\tfold",
        "import kotlin.text.lowercase /* target */ as /* alias */ fold",
    ] {
        let source = format!(
            "package stringcase.formatted\n{import}\nclass FormattedAlias {{\n    fun lower(value: String): String = value.fold()\n}}\n"
        );
        let mut site = framework_site(
            &source,
            &SiteSpec::invariant("value.fold()", "fold", "lowercase"),
        )
        .1;
        site.callee_owner = Some("external.CustomKt".into());
        let sites = HashMap::from([((file.to_path_buf(), site.byte_offset), site)]);
        let provider =
            ScriptedProvider::new(&[(file.to_path_buf(), source.clone())], &source, sites);
        let result = translate(&source, file, &provider);
        if !retained(&result, "FormattedAlias") {
            missed.push(format!("{import:?}: {:?}", result.java_files));
        }
    }
    assert!(
        missed.is_empty(),
        "legal alias import formatting bypassed retention: {missed:?}"
    );
}

#[test]
fn source_shape_proves_the_same_fact_without_helper_facts() {
    let source = CANONICAL;
    let file = Path::new("Casing.kt");
    let provider = SyntaxSemanticProvider::new([(file.to_path_buf(), source.to_owned())]);
    let result = translate(source, file, &provider);
    assert_canonical_java(&result);
}

#[test]
fn authoritative_foreign_or_misshapen_sites_never_fall_back_to_spelling() {
    let file = Path::new("Casing.kt");
    let offset = selector_offset(CANONICAL, "value.lowercase()", "lowercase");
    let contract = |owner: &str,
                    callee_name: &str,
                    parameter_types: &[&str],
                    result_type: &str,
                    nullable: bool,
                    descriptor: &str| {
        let mut site = framework_site(
            CANONICAL,
            &SiteSpec {
                call_text: "value.lowercase()",
                member: "lowercase",
                callee_name,
                parameter_types,
                result_type,
                nullable,
                descriptor,
            },
        )
        .1;
        site.callee_owner = Some(owner.to_string());
        site.byte_offset = offset;
        site
    };
    let cases: Vec<(&str, CallSite)> = vec![
        (
            "a workspace extension owns the call",
            contract(
                "fixture.CasingKt",
                "lowercase",
                &[],
                "kotlin.String",
                false,
                "()Ljava/lang/String;",
            ),
        ),
        (
            "a different stdlib function is resolved",
            contract(
                "kotlin.text.StringsKt",
                "toString",
                &[],
                "kotlin.String",
                false,
                "()Ljava/lang/String;",
            ),
        ),
        (
            "the declared result is not a non-null String",
            contract(
                "kotlin.text.StringsKt",
                "lowercase",
                &[],
                "kotlin.CharSequence",
                false,
                "()Ljava/lang/String;",
            ),
        ),
        (
            "the declared result is nullable",
            contract(
                "kotlin.text.StringsKt",
                "lowercase",
                &[],
                "kotlin.String",
                true,
                "()Ljava/lang/String;",
            ),
        ),
        (
            "the descriptor is not the invariant overload",
            contract(
                "kotlin.text.StringsKt",
                "lowercase",
                &["kotlin.Int"],
                "kotlin.String",
                false,
                "(I)Ljava/lang/String;",
            ),
        ),
        (
            "the owner is the stdlib façade but the name has no zero-arg form",
            contract(
                "kotlin.text.StringsKt",
                "lowercase",
                &["java.util.Locale"],
                "kotlin.String",
                false,
                "(Ljava/util/Locale;)Ljava/lang/String;",
            ),
        ),
    ];
    for (label, site) in cases {
        let sites = HashMap::from([((file.to_path_buf(), site.byte_offset), site)]);
        let provider = ScriptedProvider::new(
            &[(file.to_path_buf(), CANONICAL.to_owned())],
            CANONICAL,
            sites,
        );
        let result = translate(CANONICAL, file, &provider);
        assert!(
            retained(&result, "Casing"),
            "{label} must retain instead of lowering: {:#?}",
            result.plan.diagnostics
        );
    }
}

#[test]
fn safe_calls_propagate_null_while_unproven_receivers_and_wrong_arity_retain() {
    let file = Path::new("Casing.kt");
    // Safe-call lowering guards the member conversion before Elvis handling.
    let safe = r#"package stringcase
class Casing {
    fun lower(value: String?): String = value?.lowercase() ?: ""
}
"#;
    let provider = SyntaxSemanticProvider::new([(file.to_path_buf(), safe.to_owned())]);
    let result = translate(safe, file, &provider);
    assert!(
        !retained(&result, "Casing"),
        "{:?}",
        result.plan.diagnostics
    );
    let output = java(&result);
    assert!(
        output.contains("value != null") && output.contains("toLowerCase(java.util.Locale.ROOT)"),
        "{output}"
    );

    let wrong_arity = r#"package stringcase
import java.util.Locale
class Other
class Casing {
    fun lower(value: String): String = value.lowercase(Locale.getDefault(), true)
}
"#;
    let provider = SyntaxSemanticProvider::new([(file.to_path_buf(), wrong_arity.to_owned())]);
    assert!(
        retained(&translate(wrong_arity, file, &provider), "Casing"),
        "a two-argument case conversion is not this fact"
    );

    let not_a_locale = r#"package stringcase
class Other
class Casing {
    fun lower(value: String): String = value.lowercase(Other())
    fun upper(value: String): String = value.lowercase(Other())
}
"#;
    let provider = SyntaxSemanticProvider::new([(file.to_path_buf(), not_a_locale.to_owned())]);
    assert!(
        retained(&translate(not_a_locale, file, &provider), "Casing"),
        "an argument without a proven Locale type is not the explicit overload"
    );
}

#[test]
fn indexed_case_extensions_keep_precedence_over_a_resolved_stdlib_site() {
    let source = r#"package stringcase
class Casing {
    fun render(value: String): String = value.lowercase()
}
fun String.lowercase(): String = this
"#;
    let file = Path::new("Casing.kt");
    // Even a stale site naming the stdlib façade must not win over the
    // workspace declaration that actually owns this spelling.
    let sites = sites_for(
        file,
        vec![framework_site(
            source,
            &SiteSpec::invariant("value.lowercase()", "lowercase", "lowercase"),
        )],
    );
    let provider = ScriptedProvider::new(&[(file.to_path_buf(), source.to_owned())], source, sites);
    assert!(
        retained(&translate(source, file, &provider), "Casing"),
        "the indexed extension owns `lowercase`"
    );
}

#[test]
fn hash_guard_and_selector_offset_keep_the_syntax_floor() {
    let file = Path::new("Casing.kt");
    let sites = canonical_sites(file);

    // The helper saw `CANONICAL`; the planned snapshot adds a trailing comment.
    // Offsets are unchanged on purpose — the refusal must come from the hash.
    let overlay = format!("{CANONICAL}// candidate revision\n");
    let provider = ScriptedProvider::new(
        &[(file.to_path_buf(), CANONICAL.to_owned())],
        CANONICAL,
        sites.clone(),
    );
    let result = translate(&overlay, file, &provider);
    assert_canonical_java(&result);

    // A site one byte off is not this call.
    let shifted: HashMap<_, _> = sites
        .into_iter()
        .map(|((path, offset), site)| ((path, offset + 1), site))
        .collect();
    let provider = ScriptedProvider::new(
        &[(file.to_path_buf(), CANONICAL.to_owned())],
        CANONICAL,
        shifted,
    );
    let result = translate(CANONICAL, file, &provider);
    assert_canonical_java(&result);
}
