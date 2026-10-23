//! The nongeneric-declaration API answers only for *declarations*.
//!
//! [`SemanticProvider::explicit_nongeneric_kotlin_return`] is the shared
//! declaration contract: a visible Kotlin source member whose return type is
//! spelled out and nongeneric. A Track B call-site fact is an observation at an
//! offset, not a declaration, so it answers nothing here — whatever its K2
//! result type happens to say.
//!
//! Three things are pinned:
//!
//! * fact identities — scalar, substituted-generic and nullable renderings —
//!   stay reachable through `established_call_site` and answer `None` here;
//! * an indexed explicit-nongeneric member answers the same `TypeRef` through
//!   the transparent wrapper and through a wrapper with a live helper;
//! * the production CLI keeps the reviewed chained-call shapes at their
//!   baseline outcome, which is what treating a K2 rendering as a declaration
//!   used to break.

use notlin::facts::decorator::FactsProvider;
use notlin::facts::runtime::FactsRuntime;
use notlin::semantics::{
    SemanticProvider, SymbolId, SyntaxSemanticProvider, TypeRef, kotlin_call_site_offset,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Declaration-only fixture: the three members differ in what the syntax index
/// can prove about their return type, never in where they come from.
const DECLARATIONS: &str = r#"
package sample
import java.util.Optional

class Item
class Api {
    fun explicit(): Optional<Item> = TODO()
    fun inferred() = explicit()
    fun <T> methodGeneric(): Optional<T> = TODO()
}
"#;

/// Call-site fixture for the API half. The helper's rows below describe these
/// three calls exactly as the real Analysis API reports them.
const CALLS: &str = r#"
package sample

class Chain {
    fun scalar(value: String): String = value.split(",").last().uppercase()
    fun substituted(value: String) = value.split(",").iterator()
    fun nullable(value: String): String? = value.split(",").firstOrNull()
    fun bare(value: String): String = requireNotNull(value).uppercase()
}
"#;

/// The four reviewed chain shapes. Each is ordinary Kotlin over `String`; only a
/// *typing* claim about the inner call can make the emission wrong, because the
/// emitted Java differs from Kotlin's result type in representation
/// (`split` → `String[]`) or semantics (`hashCode` → `int`, `trim` → ASCII).
const SPLIT_LAST: &str = "package chain\n\
\n\
class Chain {\n\
    fun f(value: String): String = value.replace(\"a\", \"b\").split(\",\").last().uppercase()\n\
}\n";

const FIRST_TO_STRING: &str = "package chain\n\
\n\
class Chain {\n\
    fun f(value: String): String = value.first().toString().uppercase()\n\
}\n";

const HASH_CODE_TO_STRING: &str = "package chain\n\
\n\
class Chain {\n\
    fun f(value: String): String = value.replace(\"a\", \"b\").uppercase().hashCode().toString()\n\
}\n";

const TRIM_UPPER: &str = "package chain\n\
\n\
class Chain {\n\
    fun f(value: String): String = value.replace(\"a\", \"b\").trim().uppercase()\n\
}\n";

/// Alias escape: the chain result is bound to a local before it is consumed, so
/// a receiver proof has to survive a variable, not just a nested expression.
const ALIAS_ESCAPE: &str = "package chain\n\
\n\
class Chain {\n\
    fun f(value: String): String {\n\
        val z = value.uppercase().lowercase()\n\
        return z.hashCode().toString()\n\
    }\n\
}\n";

/// Binary escape: the fact-bearing call is one operand of a `+`, and the
/// consumer above it is the parenthesised sum.
const BINARY_ESCAPE: &str = "package chain\n\
\n\
class Chain {\n\
    fun f(value: String): String = (value.trim() + value.uppercase()).hashCode().toString()\n\
}\n";

/// Argument escape: the fact-bearing call is passed to another function, so no
/// member is selected on it at all.
const ARGUMENT_ESCAPE: &str = "package chain\n\
\n\
class Chain {\n\
    fun take(v: String): String = v\n\
    fun f(value: String): String = take(value.uppercase().trim())\n\
}\n";

// ---------------------------------------------------------------------------
// Scripted helper
// ---------------------------------------------------------------------------

struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> Scratch {
    let root = std::env::temp_dir().join(format!(
        "notlin-declaration-returns-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    Scratch(root)
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

/// The byte offset the planner asks about, computed by the planner's own rule
/// (the callee selector's start) rather than by a string search, and checked
/// against the call text the row claims.
fn selector_offset(source: &str, call_text: &str, selector: &str) -> u64 {
    let start = source
        .find(call_text)
        .unwrap_or_else(|| panic!("fixture has no {call_text:?}"));
    let end = start + call_text.len();
    let tree = parse(source);
    let mut stack = vec![tree.root_node()];
    let mut found = None;
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression"
            && node.utf8_text(source.as_bytes()).ok() == Some(call_text)
        {
            found = kotlin_call_site_offset(node);
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    let offset = found.unwrap_or_else(|| panic!("no selector offset for {call_text:?}")) as u64;
    assert!(
        source[offset as usize..end].starts_with(&format!("{selector}(")),
        "the planner's offset does not land on {selector:?} for {call_text:?}"
    );
    offset
}

/// One captured `ResolveCalls` row, replayed verbatim.
struct Site<'a> {
    /// The whole call expression as it appears in the fixture.
    call_text: &'a str,
    selector: &'a str,
    callee_owner: &'a str,
    callee_name: &'a str,
    parameter_types: &'a [&'a str],
    result_type: &'a str,
    result_nullable: bool,
    jvm_descriptor: &'a str,
}

impl<'a> Site<'a> {
    /// The captured `replace("a", "b")` row: a `kotlin.String` result whose
    /// lowering this crate does not implement.
    fn replace() -> Self {
        Self {
            call_text: "value.replace(\"a\", \"b\")",
            selector: "replace",
            callee_owner: "kotlin.text.StringsKt",
            callee_name: "replace",
            parameter_types: &["kotlin.String", "kotlin.String", "kotlin.Boolean"],
            result_type: "kotlin.String",
            result_nullable: false,
            jvm_descriptor: "(Ljava/lang/String;Ljava/lang/String;Z)Ljava/lang/String;",
        }
    }

    /// The captured invariant case-conversion row.
    fn case(selector: &'a str, call_text: &'a str) -> Self {
        Self {
            call_text,
            selector,
            callee_owner: "kotlin.text.StringsKt",
            callee_name: selector,
            parameter_types: &[],
            result_type: "kotlin.String",
            result_nullable: false,
            jvm_descriptor: "()Ljava/lang/String;",
        }
    }

    fn json(&self, source: &str) -> String {
        let offset = selector_offset(source, self.call_text, self.selector);
        let end = source.find(self.call_text).unwrap() + self.call_text.len();
        let text = &source[offset as usize..end];
        let parameters = self
            .parameter_types
            .iter()
            .map(|ty| format!("\"{ty}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"byte_offset":{offset},"text":"{}","status":"Established","callee_owner":"{}","callee_name":"{}","parameter_types":[{parameters}],"result_type":"{}","result_nullable":{},"jvm_descriptor":"{}"}}"#,
            text.replace('\\', "\\\\").replace('"', "\\\""),
            self.callee_owner,
            self.callee_name,
            self.result_type,
            self.result_nullable,
            self.jvm_descriptor,
        )
    }
}

/// The fake helper: answers `ResolveCalls` with the scripted batch, `Ping` with
/// a pong, and every other query with the explicit per-request absence — so
/// Track A behaves exactly like a helper without classpath facts.
fn install_helper(root: &Path, sites: &str) -> PathBuf {
    let helper = root.join("helper.sh");
    let requests = root.join("requests.log");
    fs::write(
        &helper,
        format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "{requests}"
  case "$line" in
    *'"ResolveCalls"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      file=$(printf '%s' "$line" | sed -n 's/.*"file":"\([^"]*\)".*/\1/p')
      printf '{{"version":1,"id":%s,"answer":{{"CallSites":{{"file":"%s","sites":%s}}}}}}\n' "$id" "$file" '{sites}'
      ;;
    *'"ClassMembers"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{{"version":1,"id":%s,"answer":{{"NoEvidence":{{"query":"ClassMembers","reason":"scripted helper"}}}}}}\n' "$id"
      ;;
    *)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{{"version":1,"id":%s,"answer":{{"Pong":{{"version":1}}}}}}\n' "$id"
      ;;
  esac
done
"#,
            requests = requests.display(),
            sites = sites,
        ),
    )
    .unwrap();
    // The `java` stand-in: the client launches `<java> -jar <jar> ...`.
    let java = root.join("java");
    fs::write(
        &java,
        format!("#!/bin/sh\nexec sh \"{}\"\n", helper.display()),
    )
    .unwrap();
    for script in [&helper, &java] {
        let mut permissions = fs::metadata(script).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o755);
        }
        fs::set_permissions(script, permissions).unwrap();
    }
    java
}

/// Ambient fact-source configuration. An inherited value would silently change
/// which query the client is allowed to make, so every child is started without
/// them and given only the settings it is meant to own.
const SCRUBBED: &[&str] = &[
    "NOTLIN_JVM_FACTS_JAR",
    "NOTLIN_JVM_FACTS_KOTLIN",
    "NOTLIN_KOTLIN_SOURCES",
    "NOTLIN_CLASSPATH",
    "NOTLIN_JAVA",
    "NOTLIN_KOTLIN_STDLIB",
    "NOTLIN_KOTLINC",
    "NOTLIN_JAVAC",
    "NOTLIN_KOTLIN_LOMBOK_PLUGIN",
];

/// A helper process running `script`, with the ambient fact-source environment
/// removed first.
///
/// Spawned as `sh <script>` rather than `Command::new(<script>)`: the script was
/// written by this process moments ago, and exec'ing a freshly written file
/// races with the forks other tests are making (ETXTBSY). The wrapper ignores
/// the arguments the client appends.
fn scripted_runtime(script: &Path) -> std::sync::Arc<FactsRuntime> {
    let mut command = Command::new("sh");
    command.arg(script);
    for variable in SCRUBBED {
        command.env_remove(variable);
    }
    FactsRuntime::with_command(command).expect("scripted helper starts")
}

// ---------------------------------------------------------------------------
// The declaration contract, at the provider API
// ---------------------------------------------------------------------------

/// A provider over `CALLS` on disk plus the scripted rows, so `established_call_site`
/// sees the same hash-guarded snapshot the production decorator sees.
fn call_site_provider(name: &str, rows: &[Site<'_>]) -> (FactsProvider, PathBuf, Scratch) {
    let root = scratch(name);
    let file = root.join("Chain.kt");
    fs::write(&file, CALLS).unwrap();
    let java = install_helper(
        &root,
        &format!(
            "[{}]",
            rows.iter()
                .map(|row| row.json(CALLS))
                .collect::<Vec<_>>()
                .join(",")
        ),
    );
    let runtime = scripted_runtime(&java);
    let inner = SyntaxSemanticProvider::new([(file.clone(), CALLS.to_owned())]);
    (FactsProvider::with_runtime(inner, runtime), file, root)
}

fn fact_id(provider: &FactsProvider, file: &Path, root: &Path, offset: u64) -> SymbolId {
    let hash = *blake3::hash(CALLS.as_bytes()).as_bytes();
    let site = provider
        .established_call_site(file, offset, &hash)
        .unwrap_or_else(|| {
            panic!(
                "the scripted row at {offset} must be reachable; helper saw:\n{}",
                fs::read_to_string(root.join("requests.log")).unwrap_or_default()
            )
        });
    notlin::facts::call_site_symbol(file, &site).expect("the row names a callee")
}

#[test]
fn a_scalar_call_site_fact_answers_no_nongeneric_declaration() {
    // Wire truth: `last()` really is `kotlin.String` per K2. The emitter writes
    // a stream reduce over `String[]`, so treating this as a declaration is
    // exactly the mistake the API must refuse to enable.
    let row = Site {
        call_text: "value.split(\",\").last()",
        selector: "last",
        callee_owner: "kotlin.collections.CollectionsKt",
        callee_name: "last",
        parameter_types: &[],
        result_type: "kotlin.String",
        result_nullable: false,
        jvm_descriptor: "()Ljava/lang/String;",
    };
    let offset = selector_offset(CALLS, row.call_text, row.selector);
    let (provider, file, root) = call_site_provider("scalar", &[row]);
    let id = fact_id(&provider, &file, &root, offset);
    assert!(notlin::facts::is_fact_symbol(&id), "{id:?}");
    assert_eq!(provider.explicit_nongeneric_kotlin_return(&id), None);
}

#[test]
fn a_substituted_generic_call_site_fact_answers_no_nongeneric_declaration() {
    // A parameterized K2 rendering is a *substituted* type argument, which is
    // not what an explicit nongeneric Kotlin return type looks like.
    let row = Site {
        call_text: "value.split(\",\").iterator()",
        selector: "iterator",
        callee_owner: "kotlin.collections.CollectionsKt",
        callee_name: "iterator",
        parameter_types: &[],
        result_type: "kotlin.collections.Iterator<kotlin.String>",
        result_nullable: false,
        jvm_descriptor: "()Ljava/util/Iterator;",
    };
    let offset = selector_offset(CALLS, row.call_text, row.selector);
    let (provider, file, root) = call_site_provider("substituted", &[row]);
    let id = fact_id(&provider, &file, &root, offset);
    assert_eq!(provider.explicit_nongeneric_kotlin_return(&id), None);
}

#[test]
fn a_nullable_call_site_fact_answers_no_nongeneric_declaration() {
    // `containing_type`/payload consumers read a `Some` here as "this call
    // yields that declaration's type"; nullability alone must never license it.
    let row = Site {
        call_text: "value.split(\",\").firstOrNull()",
        selector: "firstOrNull",
        callee_owner: "kotlin.collections.CollectionsKt",
        callee_name: "firstOrNull",
        parameter_types: &[],
        result_type: "kotlin.String",
        result_nullable: true,
        jvm_descriptor: "()Ljava/lang/Object;",
    };
    let offset = selector_offset(CALLS, row.call_text, row.selector);
    let (provider, file, root) = call_site_provider("nullable", &[row]);
    let id = fact_id(&provider, &file, &root, offset);
    assert_eq!(provider.explicit_nongeneric_kotlin_return(&id), None);
}

#[test]
fn regression_an_established_row_without_a_callee_name_is_refused() {
    // Contradictory evidence, not a captured positive: preserve the decorator's
    // existing identity-validation boundary when removing its observation memo.
    let row = Site {
        call_text: "value.split(\",\").last()",
        selector: "last",
        callee_owner: "kotlin.collections.CollectionsKt",
        callee_name: "   ",
        parameter_types: &[],
        result_type: "kotlin.String",
        result_nullable: false,
        jvm_descriptor: "()Ljava/lang/String;",
    };
    let offset = selector_offset(CALLS, row.call_text, row.selector);
    let (provider, file, _root) = call_site_provider("invalid-callee", &[row]);
    let hash = *blake3::hash(CALLS.as_bytes()).as_bytes();
    assert!(
        provider
            .established_call_site(&file, offset, &hash)
            .is_none()
    );
}

#[test]
fn regression_owned_scratch_is_removed_when_its_guard_drops() {
    let root = scratch("cleanup");
    let path = root.to_path_buf();
    assert!(path.is_dir());
    drop(root);
    assert!(!path.exists(), "owned test scratch must not accumulate");
}

#[test]
fn an_indexed_explicit_nongeneric_member_answers_identically_with_a_helper() {
    // The declaration half must not depend on whether a helper is attached:
    // this is the same answer the helper-free floor gives.
    let source = DECLARATIONS.to_owned();
    let file = PathBuf::from("sample/Api.kt");
    let inner = SyntaxSemanticProvider::new([(file, source)]);
    let explicit = inner
        .symbols_named("explicit")
        .into_iter()
        .find(|symbol| symbol.id.kind == "function")
        .expect("fixture declares explicit()")
        .id
        .clone();
    let generic = inner
        .symbols_named("methodGeneric")
        .into_iter()
        .find(|symbol| symbol.id.kind == "function")
        .expect("fixture declares methodGeneric()")
        .id
        .clone();

    let expected = Some(TypeRef::parse("Optional<Item>"));
    assert_eq!(inner.explicit_nongeneric_kotlin_return(&explicit), expected);

    let transparent = FactsProvider::transparent(inner.clone());
    assert_eq!(
        transparent.explicit_nongeneric_kotlin_return(&explicit),
        expected
    );
    assert_eq!(
        transparent.explicit_nongeneric_kotlin_return(&generic),
        None
    );

    // A helper that answers nothing new: start it from a script with no sites.
    let root = scratch("declarations");
    let java = install_helper(&root, "[]");
    let with_helper = FactsProvider::with_runtime(inner, scripted_runtime(&java));
    assert!(with_helper.has_helper());
    assert_eq!(
        with_helper.explicit_nongeneric_kotlin_return(&explicit),
        expected
    );
    assert_eq!(
        with_helper.explicit_nongeneric_kotlin_return(&generic),
        None
    );
}

// ---------------------------------------------------------------------------
// The same shapes through the production CLI
// ---------------------------------------------------------------------------

/// What one CLI run produced.
struct Run {
    java: Option<String>,
    retained: bool,
    asked_call_sites: bool,
    stderr: String,
}

impl Run {
    fn assert_asked(&self, name: &str) {
        assert!(
            self.asked_call_sites,
            "{name}: the run must consult Track B for this fixture:\n{}",
            self.stderr
        );
    }
}

fn run(name: &str, source: &str, sites: &str) -> Run {
    run_with_policy(name, source, sites, "--jvm-facts=on")
}

fn run_with_policy(name: &str, source: &str, sites: &str, policy: &str) -> Run {
    let root = scratch(name);
    let file = root.join("Chain.kt");
    fs::write(&file, source).unwrap();
    let java_wrapper = install_helper(&root, sites);
    let out = root.join("out");
    fs::create_dir_all(&out).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command.current_dir(&root).args([
        policy,
        "--annotations",
        "none",
        "-o",
        out.to_str().unwrap(),
        file.to_str().unwrap(),
    ]);
    for variable in SCRUBBED {
        command.env_remove(variable);
    }
    // The jar's file name is the helper's capability signal: only a `-full` jar
    // carries Track B, so only these runs are allowed to answer call sites.
    command.env("NOTLIN_JAVA", &java_wrapper).env(
        "NOTLIN_JVM_FACTS_JAR",
        root.join("notlin-jvm-facts-full.jar"),
    );
    let output = command.output().expect("production binary runs");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "{name}: CLI failed\n{stderr}\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let java = fs::read_dir(&out)
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.path().extension().is_some_and(|ext| ext == "java"))
        .map(|entry| fs::read_to_string(entry.path()).unwrap());
    let asked = fs::read_to_string(root.join("requests.log")).unwrap_or_default();
    Run {
        retained: java.is_none(),
        java,
        asked_call_sites: asked.contains("ResolveCalls"),
        stderr,
    }
}

/// A retained shape emits nothing; that is the baseline outcome these fixtures
/// had before a fact-backed receiver type was proposed.
fn assert_retained(name: &str, source: &str, sites: &str) {
    let run = run(name, source, sites);
    run.assert_asked(name);
    assert!(
        run.retained,
        "{name}: the chained call has no proved Java lowering, so it must stay \
         retained — a K2 result type is not a Java expression type:\n{:?}",
        run.java
    );
}

#[test]
fn a_split_last_chain_stays_retained() {
    // The emitter writes a stream reduce over `String[]`, so typing the
    // receiver would emit `String[].stream()`.
    let sites = format!(
        "[{},{},{}]",
        Site::replace().json(SPLIT_LAST),
        Site {
            call_text: "value.replace(\"a\", \"b\").split(\",\")",
            selector: "split",
            callee_owner: "kotlin.text.StringsKt",
            callee_name: "split",
            parameter_types: &["kotlin.String", "kotlin.Boolean", "kotlin.Int"],
            result_type: "kotlin.collections.List<kotlin.String>",
            result_nullable: false,
            jvm_descriptor: "(Ljava/lang/String;ZI)Ljava/util/List;",
        }
        .json(SPLIT_LAST),
        Site {
            call_text: "value.replace(\"a\", \"b\").split(\",\").last()",
            selector: "last",
            callee_owner: "kotlin.collections.CollectionsKt",
            callee_name: "last",
            parameter_types: &[],
            result_type: "kotlin.String",
            result_nullable: false,
            jvm_descriptor: "()Ljava/lang/String;",
        }
        .json(SPLIT_LAST)
    );
    assert_retained("cli-split-last", SPLIT_LAST, &sites);
}

#[test]
fn a_first_char_to_string_chain_stays_retained() {
    // `first()` is emitted through the collection lowering and `toString()` on
    // a `Char` has no mapped Java lowering here.
    let sites = format!(
        "[{},{}]",
        Site {
            call_text: "value.first()",
            selector: "first",
            callee_owner: "kotlin.text.StringsKt",
            callee_name: "first",
            parameter_types: &[],
            result_type: "kotlin.Char",
            result_nullable: false,
            jvm_descriptor: "()C",
        }
        .json(FIRST_TO_STRING),
        Site {
            call_text: "value.first().toString()",
            selector: "toString",
            callee_owner: "kotlin.Char",
            callee_name: "toString",
            parameter_types: &[],
            result_type: "kotlin.String",
            result_nullable: false,
            jvm_descriptor: "()Ljava/lang/String;",
        }
        .json(FIRST_TO_STRING)
    );
    assert_retained("cli-first", FIRST_TO_STRING, &sites);
}

#[test]
fn a_hash_code_to_string_chain_stays_retained() {
    // `hashCode()` is a Java `int`; `int.toString()` is not Java.
    let sites = format!(
        "[{},{},{}]",
        Site::replace().json(HASH_CODE_TO_STRING),
        Site::case("uppercase", "value.replace(\"a\", \"b\").uppercase()")
            .json(HASH_CODE_TO_STRING),
        Site {
            call_text: "value.replace(\"a\", \"b\").uppercase().hashCode()",
            selector: "hashCode",
            callee_owner: "kotlin.String",
            callee_name: "hashCode",
            parameter_types: &[],
            result_type: "kotlin.Int",
            result_nullable: false,
            jvm_descriptor: "()I",
        }
        .json(HASH_CODE_TO_STRING)
    );
    assert_retained("cli-hash-code", HASH_CODE_TO_STRING, &sites);
}

#[test]
fn a_trim_chain_stays_retained() {
    // Kotlin `trim` strips Unicode whitespace; Java `String.trim` stops at
    // `\u0020`, so the two are not the same function.
    let sites = format!(
        "[{},{},{}]",
        Site::replace().json(TRIM_UPPER),
        Site {
            call_text: "value.replace(\"a\", \"b\").trim()",
            selector: "trim",
            callee_owner: "kotlin.text.StringsKt",
            callee_name: "trim",
            parameter_types: &[],
            result_type: "kotlin.String",
            result_nullable: false,
            jvm_descriptor: "()Ljava/lang/String;",
        }
        .json(TRIM_UPPER),
        Site::case(
            "uppercase",
            "value.replace(\"a\", \"b\").trim().uppercase()"
        )
        .json(TRIM_UPPER)
    );
    assert_retained("cli-trim", TRIM_UPPER, &sites);
}

#[test]
fn an_alias_escape_stays_retained() {
    let sites = format!(
        "[{},{}]",
        Site::case("uppercase", "value.uppercase()").json(ALIAS_ESCAPE),
        Site::case("lowercase", "value.uppercase().lowercase()").json(ALIAS_ESCAPE)
    );
    let run = run("cli-alias", ALIAS_ESCAPE, &sites);
    run.assert_asked("cli-alias");
    // The alias itself may translate (the case chain is a supported lowering);
    // what must not happen is `z` acquiring a type that lets `hashCode()` or
    // `toString()` through as if they were Kotlin declarations.
    if let Some(java) = &run.java {
        assert!(
            !java.contains("z.hashCode().toString()"),
            "cli-alias: a fact typed the alias receiver:\n{java}"
        );
    }
}

#[test]
fn a_binary_escape_stays_retained() {
    let sites = format!(
        "[{},{}]",
        Site {
            call_text: "value.trim()",
            selector: "trim",
            callee_owner: "kotlin.text.StringsKt",
            callee_name: "trim",
            parameter_types: &[],
            result_type: "kotlin.String",
            result_nullable: false,
            jvm_descriptor: "()Ljava/lang/String;",
        }
        .json(BINARY_ESCAPE),
        Site::case("uppercase", "value.uppercase()").json(BINARY_ESCAPE)
    );
    let run = run("cli-binary", BINARY_ESCAPE, &sites);
    run.assert_asked("cli-binary");
    if let Some(java) = &run.java {
        for artifact in ["int.", "(int)"] {
            assert!(
                !java.contains(artifact),
                "cli-binary: emitted Java contains {artifact:?}:\n{java}"
            );
        }
    }
}

#[test]
fn an_argument_escape_keeps_the_baseline_outcome() {
    let sites = format!(
        "[{},{}]",
        Site::case("uppercase", "value.uppercase()").json(ARGUMENT_ESCAPE),
        Site {
            call_text: "value.uppercase().trim()",
            selector: "trim",
            callee_owner: "kotlin.text.StringsKt",
            callee_name: "trim",
            parameter_types: &[],
            result_type: "kotlin.String",
            result_nullable: false,
            jvm_descriptor: "()Ljava/lang/String;",
        }
        .json(ARGUMENT_ESCAPE)
    );
    let run = run("cli-argument", ARGUMENT_ESCAPE, &sites);
    run.assert_asked("cli-argument");
    // With facts the case conversion is a supported lowering, so this shape
    // translates at the baseline too — the assertion is that it still does, and
    // that nothing above the argument gets a fact-derived type. The emitted
    // `String.trim()` for the Kotlin `trim()` (Unicode vs ASCII whitespace) is
    // a pre-existing lowering question, not something a receiver proof may
    // decide.
    let java = run
        .java
        .unwrap_or_else(|| panic!("cli-argument: baseline translates this shape"));
    assert!(
        java.contains("toUpperCase(java.util.Locale.ROOT)"),
        "cli-argument: the case conversion is not the proved lowering:\n{java}"
    );
    for artifact in ["String[].", "int.", "(int)"] {
        assert!(
            !java.contains(artifact),
            "cli-argument: emitted Java contains {artifact:?}:\n{java}"
        );
    }
}

#[test]
fn without_a_helper_the_reviewed_shapes_keep_their_baseline_outcome() {
    for (name, source) in [
        ("off-split-last", SPLIT_LAST),
        ("off-first", FIRST_TO_STRING),
        ("off-hash-code", HASH_CODE_TO_STRING),
        ("off-trim", TRIM_UPPER),
    ] {
        let run = run_with_policy(name, source, "[]", "--jvm-facts=off");
        assert!(
            !run.asked_call_sites,
            "an off run must not query the helper"
        );
        assert!(
            run.retained,
            "{name}: the helper-free floor retains this shape:\n{:?}",
            run.java
        );
    }
}

/// The fixtures are ordinary Kotlin: compile them with the real compiler so a
/// retention assertion cannot be satisfied by an invalid program.
#[test]
#[ignore = "needs NOTLIN_KOTLINC"]
fn the_reviewed_fixtures_compile_as_kotlin() {
    let kotlinc = std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC");
    let root = scratch("kotlinc");
    let out = root.join("classes");
    fs::create_dir_all(&out).unwrap();
    for (index, (name, source)) in [
        ("SplitLast", SPLIT_LAST),
        ("First", FIRST_TO_STRING),
        ("HashCode", HASH_CODE_TO_STRING),
        ("Trim", TRIM_UPPER),
        ("Alias", ALIAS_ESCAPE),
        ("Binary", BINARY_ESCAPE),
        ("Argument", ARGUMENT_ESCAPE),
    ]
    .into_iter()
    .enumerate()
    {
        let file = root.join(format!("{index}-{name}.kt"));
        fs::write(&file, source).unwrap();
        let output = Command::new(&kotlinc)
            .current_dir(&root)
            .arg("-d")
            .arg(&out)
            .arg(&file)
            .output()
            .expect("kotlinc runs");
        assert!(
            output.status.success(),
            "{name} is not valid Kotlin:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
