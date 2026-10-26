//! Constructor applicability from the *same* classpath signature model as
//! instance and static calls.
//!
//! Before this slice, `established_platform_constructor` always handed the
//! resolver `SignatureSource::Table`: even with a live Track A helper, a
//! constructor was answered by four hand-declared owner spellings and a guessed
//! overload subset. These tests pin the sourced behaviour:
//!
//! * a real constructor overload the table does not declare (`StringBuilder(String)`,
//!   `BigDecimal(String, MathContext)`) is established from facts;
//! * an owner the fact source covers is the whole truth for that owner, so "no
//!   applicable constructor" is a **final negative**, never a table fallback;
//! * no usable fact for the owner (`members: null`, dead/unavailable, all-null)
//!   keeps the exact pre-helper table floor, unsupported shapes included;
//! * every spelling/import/shadow/nullability/arity guard still runs *before*
//!   the fact source, so facts can never resurrect a call the syntax layer
//!   already owns;
//! * the four-owner family is not widened: an authoritative answer for an owner
//!   outside it (`java.time.LocalDate`) does not make that constructor
//!   establishable.
//!
//! The fixtures in `tests/fixtures/classpath_facts/*_constructors.json` are
//! subsets of what the real slim helper answers for those owners: every `<init>`
//! row is the complete reflection list (verified with `javap -public`), while
//! non-constructor members are trimmed to the ones these fixtures call.
//!
//! The scripted source files below never exist on disk. That is deliberate:
//! `FactsRuntime::call_site_at` refuses a snapshot that does not hash to the
//! file's bytes, so Track B is never consulted (and never fabricated) here; the
//! fake speaks `Ping` and `ClassMembers` only, and each test that cares asserts
//! what the Rust side actually asked.

use notlin::facts::decorator::FactsProvider;
use notlin::facts::runtime::FactsRuntime;
use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider, plan_required_calls};
use std::path::{Path, PathBuf};
use std::process::Command;

const STRING_BUILDER: &str =
    include_str!("fixtures/classpath_facts/string_builder_constructors.json");
const BIG_DECIMAL: &str = include_str!("fixtures/classpath_facts/big_decimal_constructors.json");
const ILLEGAL_ARGUMENT: &str =
    include_str!("fixtures/classpath_facts/illegal_argument_constructors.json");
const ILLEGAL_STATE: &str =
    include_str!("fixtures/classpath_facts/illegal_state_constructors.json");

/// The four owner families this checkpoint is bounded to, with the real
/// reflection answers as the fact source's per-owner content.
fn real_owners() -> Vec<(&'static str, &'static str)> {
    vec![
        ("java.lang.StringBuilder", STRING_BUILDER),
        ("java.math.BigDecimal", BIG_DECIMAL),
        ("java.lang.IllegalArgumentException", ILLEGAL_ARGUMENT),
        ("java.lang.IllegalStateException", ILLEGAL_STATE),
    ]
}

/// A JSON member list with only the rows a case needs, so "this owner is
/// covered but has no such constructor" is spelled out rather than implied.
fn rows(spec: &[(&str, &[&str])]) -> String {
    let members = spec
        .iter()
        .map(|(name, parameters)| {
            // `<init>` rows carry the helper's real `<void>` result: the shared
            // resolver maps a constructor's semantic result to its owner, so a
            // scripted row must not pretend the wire says otherwise.
            let result = if *name == "<init>" {
                "<void>"
            } else {
                "java.lang.Object"
            };
            format!(
                r#"{{"name":"{}","static_member":false,"parameter_types":[{}],"result_type":"{}","parameter_nullability":[{}],"result_nullability":"unknown"}}"#,
                name,
                parameters
                    .iter()
                    .map(|parameter| format!("\"{parameter}\""))
                    .collect::<Vec<_>>()
                    .join(","),
                result,
                parameters
                    .iter()
                    .map(|_| "\"unknown\"")
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("[{members}]")
}

fn one_line(members: &str) -> String {
    members.lines().map(str::trim).collect()
}

/// A `sh` helper speaking the line protocol: `Ping`, and `ClassMembers` for the
/// owners in `owners`. Every other owner gets `members: null` ("cannot load
/// this type"), which the Rust side must treat as *no facts*, never as an empty
/// member list. `ResolveCalls` is answered with EOF (the arm does not exist), so
/// a test that somehow provoked a Track B query would fail loudly instead of
/// being handed a fabricated empty batch.
///
/// When `log` is given, each `ClassMembers` owner queried is appended to it
/// *before* the answer for that request is written. The journal is therefore
/// complete for every answer the client has already received; a caller may
/// read it immediately after a query returns instead of racing the helper.
fn fake_helper_script(owners: &[(&str, &str)], log: Option<&Path>) -> String {
    let mut arms = String::new();
    for (owner, members) in owners {
        arms.push_str(&format!(
            r#"    *'"owner":"{owner}"'*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Members":{{"owner":"{owner}","members":{members}}}}}}}\n' "$id"
      ;;
"#,
            members = one_line(members)
        ));
    }
    let log_line = log
        .map(|path| {
            format!(
                "  case \"$line\" in *'\"owner\"'*) printf '%s\\n' \"$owner\" >> '{}' ;; esac\n",
                path.display()
            )
        })
        .unwrap_or_default();
    let script = format!(
        r#"while IFS= read -r line; do
  owner=$(printf '%s' "$line" | sed -n 's/.*"owner":"\([^"]*\)".*/\1/p')
{log_line}  case "$line" in
    *'"Ping"'*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Pong":{{"version":1}}}}}}\n' "$id"
      ;;
{arms}    *'"ClassMembers"'*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Members":{{"owner":"unknown","members":null}}}}}}\n' "$id"
      ;;
    *'"ResolveCalls"'*)
      exit 3
      ;;
  esac
done
"#
    );
    script
}

fn fake_helper(owners: &[(&str, &str)], log: Option<&Path>) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(fake_helper_script(owners, log));
    command
}

/// The journal invariant the owner-scoping test relies on: a request is
/// recorded before its answer is written, so a client that has the answer can
/// read the journal immediately. Asserted on the generated script because a
/// behavioural observation would only re-create the race it guards against.
#[test]
fn the_helper_journals_a_request_before_it_answers() {
    let script = fake_helper_script(&real_owners(), Some(Path::new("/tmp/notlin-journal")));
    let journal = script
        .find(">> '/tmp/notlin-journal'")
        .expect("the helper appends to the journal");
    let dispatch = script
        .find(r#"*'"Ping"'*)"#)
        .expect("the helper dispatches requests");
    let answer = script
        .find(r#""answer""#)
        .expect("the helper writes an answer");
    assert!(
        journal < dispatch && dispatch < answer,
        "the journal must be appended before the request is dispatched and answered:\n{script}"
    );
}

fn syntax(source: &str) -> SyntaxSemanticProvider {
    let file = fixture_file();
    SyntaxSemanticProvider::new([(file, source.to_owned())])
}

/// A provider with a live Track A helper. The source file is virtual, so this
/// is a Track A-only run in the same sense as a slim-jar CLI run: `ClassMembers`
/// is answered, `ResolveCalls` is never asked.
fn facts(owners: &[(&str, &str)], source: &str, log: Option<&Path>) -> FactsProvider {
    let runtime = FactsRuntime::with_command(fake_helper(owners, log)).unwrap();
    FactsProvider::with_runtime(syntax(source), runtime)
}

/// The same source through the helper-less wrapper: the pre-helper floor.
fn helper_off(source: &str) -> FactsProvider {
    FactsProvider::transparent(syntax(source))
}

fn fixture_file() -> PathBuf {
    PathBuf::from("ConstructorCall.kt")
}

/// Calls the run left as required calls (i.e. it could not establish them).
fn retained(provider: &(impl SemanticProvider + ?Sized), source: &str) -> Vec<String> {
    let file = fixture_file();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture: {source}");
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), &file);
    plan_required_calls(source, &tree, &file, provider, false, &mut plan);
    let mut spellings = plan
        .dependencies
        .into_iter()
        .map(|dependency| dependency.spelling)
        .collect::<Vec<_>>();
    spellings.sort();
    spellings.dedup();
    spellings
}

fn asserted(provider: &(impl SemanticProvider + ?Sized), source: &str, name: &str) {
    assert!(
        !retained(provider, source)
            .iter()
            .any(|spelling| spelling == name),
        "`{name}` must be established from classpath constructor facts: {source}"
    );
}

fn retained_call(provider: &(impl SemanticProvider + ?Sized), source: &str, name: &str) {
    assert!(
        retained(provider, source)
            .iter()
            .any(|spelling| spelling == name),
        "`{name}` must stay a required call: {source}"
    );
}

#[test]
fn classpath_constructor_overloads_are_established_beside_the_declared_ones() {
    let owners = real_owners();
    // Shapes the pre-helper table refused outright.
    let sourced = [
        "fun build(): StringBuilder = StringBuilder(\"seed\")",
        "fun build(): StringBuilder = StringBuilder(16)",
        "import java.math.BigDecimal\nimport java.math.MathContext\nfun build(text: String, context: MathContext) = BigDecimal(text, context)",
    ];
    for source in sourced {
        let provider = facts(&owners, source, None);
        let name = if source.contains("BigDecimal") {
            "BigDecimal"
        } else {
            "StringBuilder"
        };
        asserted(&provider, source, name);
    }
    // Every overload the old table declared still resolves, now from facts.
    for (source, name) in [
        (
            "fun build(): StringBuilder = StringBuilder()",
            "StringBuilder",
        ),
        (
            "import java.math.BigDecimal\nfun build() = BigDecimal(\"1\")",
            "BigDecimal",
        ),
        (
            "import java.math.BigDecimal\nfun build() = BigDecimal(1)",
            "BigDecimal",
        ),
        (
            "import java.math.BigDecimal\nfun build() = BigDecimal(1L)",
            "BigDecimal",
        ),
        (
            "import java.math.BigDecimal\nfun build() = BigDecimal(1.5)",
            "BigDecimal",
        ),
        (
            "fun build() = IllegalStateException()",
            "IllegalStateException",
        ),
        (
            "fun build() = IllegalArgumentException(\"message\")",
            "IllegalArgumentException",
        ),
    ] {
        let provider = facts(&owners, source, None);
        asserted(&provider, source, name);
    }
}

#[test]
fn an_owner_covered_by_facts_without_an_applicable_constructor_is_final() {
    // Each fact answer covers the owner, so the table rows for the same owner
    // are stale bootstrapping, not additional candidates.
    let cases = [
        // The table's only StringBuilder constructor is zero-argument.
        (
            ("java.lang.StringBuilder", rows(&[("<init>", &["int"])])),
            "fun build(): StringBuilder = StringBuilder()",
            "StringBuilder",
        ),
        (
            ("java.lang.StringBuilder", rows(&[("<init>", &["int"])])),
            "fun build(): StringBuilder = StringBuilder(\"seed\")",
            "StringBuilder",
        ),
        // No `<init>` at all: the owner loads, the class simply declares none
        // in this answer.
        (
            (
                "java.math.BigDecimal",
                rows(&[("add", &["java.math.BigDecimal"])]),
            ),
            "import java.math.BigDecimal\nfun build() = BigDecimal(\"1\")",
            "BigDecimal",
        ),
        // A right-arity, wrong-type answer: the table's kotlin.Int row would
        // have accepted `1`.
        (
            (
                "java.math.BigDecimal",
                rows(&[("<init>", &["java.lang.String"])]),
            ),
            "import java.math.BigDecimal\nfun build() = BigDecimal(1)",
            "BigDecimal",
        ),
        // The zero-argument exception constructors are table-only guesses.
        (
            (
                "java.lang.IllegalArgumentException",
                rows(&[("<init>", &["java.lang.String"])]),
            ),
            "fun build() = IllegalArgumentException()",
            "IllegalArgumentException",
        ),
        (
            (
                "java.lang.IllegalStateException",
                rows(&[("<init>", &["java.lang.String"])]),
            ),
            "fun build() = IllegalStateException()",
            "IllegalStateException",
        ),
    ];
    for ((owner, members), source, name) in cases {
        let provider = facts(&[(owner, members.as_str())], source, None);
        retained_call(&provider, source, name);
    }

    // Control: the same contradictory answers accept the overload they *do*
    // declare, so the negatives above are about applicability, not about the
    // owner lookup failing.
    for ((owner, members), source, name) in [
        (
            ("java.lang.StringBuilder", rows(&[("<init>", &["int"])])),
            "fun build(): StringBuilder = StringBuilder(16)",
            "StringBuilder",
        ),
        (
            (
                "java.lang.IllegalArgumentException",
                rows(&[("<init>", &["java.lang.String"])]),
            ),
            "fun build() = IllegalArgumentException(\"message\")",
            "IllegalArgumentException",
        ),
    ] {
        let provider = facts(&[(owner, members.as_str())], source, None);
        asserted(&provider, source, name);
    }
}

/// The floor: no usable class fact for an owner means the old helper-free
/// table behaviour, byte for byte, unsupported shapes included.
#[test]
fn no_usable_class_fact_keeps_the_exact_helper_free_table_floor() {
    let sources = [
        "fun build(): StringBuilder = StringBuilder()",
        "fun build(): StringBuilder = StringBuilder(\"seed\")",
        "fun build(): StringBuilder = StringBuilder(16)",
        "import java.math.BigDecimal\nfun build() = BigDecimal(\"1\")",
        "import java.math.BigDecimal\nfun build() = BigDecimal(1)",
        "import java.math.BigDecimal\nfun build() = BigDecimal()",
        "import java.math.BigDecimal\nfun build() = BigDecimal(\"1\", \"2\")",
        "import java.math.BigDecimal\nfun build() = BigDecimal(value = \"1\")",
        "import java.math.BigDecimal\nfun build(values: Array<String>) = BigDecimal(*values)",
        "import java.math.BigDecimal\nfun build(value: Float) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun build(value: Any) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun build(value: Double?) = BigDecimal(value)",
        "import java.math.BigDecimal\nimport java.math.MathContext\nfun build(text: String, context: MathContext) = BigDecimal(text, context)",
        "import java.math.*\nfun build() = BigDecimal(\"1\")",
        "import java.math.BigDecimal\nimport custom.BigDecimal\nfun build() = BigDecimal(\"1\")",
        "fun build() = IllegalArgumentException()",
        "fun build() = IllegalStateException(\"message\", null)",
    ];
    let owners = real_owners();
    for source in sources {
        let off = helper_off(source);
        let all_null = facts(&[], source, None);
        assert_eq!(
            retained(&off, source),
            retained(&all_null, source),
            "an all-null fact answer must reproduce the helper-free floor: {source}"
        );
        // A live helper that answers for the *other* owners but cannot load the
        // one this call names is the same "no usable fact" case per owner.
        let partial = facts(&owners[..1], source, None);
        if !source.contains("StringBuilder") {
            assert_eq!(
                retained(&off, source),
                retained(&partial, source),
                "an unloadable owner must keep the table floor: {source}"
            );
        }
    }
}

/// Guards run before the fact source: an authoritative positive answer must not
/// resurrect a call the syntax layer already gives to a source declaration or
/// refuses to bind.
#[test]
fn spelling_import_and_shadow_guards_still_precede_classpath_facts() {
    let owners = real_owners();
    let source_owned = [
        "import java.math.BigDecimal\nclass BigDecimal\nfun build() = BigDecimal(\"1\")",
        "import java.math.BigDecimal\ntypealias BigDecimal = String\nfun build() = BigDecimal(\"1\")",
        "import java.math.BigDecimal\nfun BigDecimal(value: String) = value\nfun build() = BigDecimal(\"1\")",
        "import java.math.BigDecimal\nfun build() { fun BigDecimal(value: String) = value; BigDecimal(\"1\") }",
        "import java.math.BigDecimal\nfun build(BigDecimal: (String) -> String) = BigDecimal(\"1\")",
        "import java.math.BigDecimal\nfun build() { val BigDecimal: (String) -> String = { it }; BigDecimal(\"1\") }",
        "import java.math.BigDecimal\nclass Factory(val BigDecimal: (String) -> String) {\n fun build() = BigDecimal(\"1\")\n}\n",
        "import java.math.BigDecimal\nopen class Factory {\n fun BigDecimal(value: String) = value\n}\nclass Child : Factory() {\n fun build() = BigDecimal(\"1\")\n}\n",
        "class StringBuilder\nfun build() = StringBuilder()",
        "fun build(StringBuilder: () -> String) = StringBuilder()",
        "fun build() { val StringBuilder: () -> String = { \"x\" }; StringBuilder() }",
    ];
    for source in source_owned {
        let provider = facts(&owners, source, None);
        let name = if source.contains("StringBuilder") {
            "StringBuilder"
        } else {
            "BigDecimal"
        };
        retained_call(&provider, source, name);
    }

    let unbound = [
        "import java.math.BigDecimal\nimport custom.BigDecimal\nfun build() = BigDecimal(\"1\")",
        "import java.math.BigDecimal as Decimal\nfun build() = BigDecimal(\"1\")",
        "import java.math.BigDecimal as Decimal\nfun build() = Decimal(\"1\")",
        "import java.math.BigDecimal\nimport java.math.BigDecimal\nfun build() = BigDecimal(\"1\")",
        "import java.math.*\nfun build() = BigDecimal(\"1\")",
        "import java.math.*\nimport foreign.*\nfun build() = BigDecimal(\"1\")",
        "import java.util.UUID as BigDecimal\nfun build() = BigDecimal(\"1\")",
        "import foreign.*\nfun build() = java.math.BigDecimal(\"1\")",
        "fun build(java: Int) = java.math.BigDecimal(\"1\")",
        "import custom.Root as java\nfun build() = java.math.BigDecimal(\"1\")",
        "object java {\n object lang {\n fun StringBuilder(): String = \"x\"\n }\n}\nfun build() = java.lang.StringBuilder()",
        "import foreign.*\nfun build() = IllegalStateException(\"message\")",
        "import custom.IllegalArgumentException\nfun build() = IllegalArgumentException()",
    ];
    for source in unbound {
        let provider = facts(&owners, source, None);
        let name = if source.contains("StringBuilder") {
            "StringBuilder"
        } else if source.contains("IllegalStateException") {
            "IllegalStateException"
        } else if source.contains("IllegalArgumentException") {
            "IllegalArgumentException"
        } else {
            "BigDecimal"
        };
        let spellings = retained(&provider, source);
        assert!(
            !spellings.is_empty(),
            "an unbound spelling must not be established from facts: {source}"
        );
        if name == "BigDecimal" && source.contains("fun build() = BigDecimal(\"1\")") {
            retained_call(&provider, source, name);
        }
    }

    // Nullable / unproven / generic argument types: exact proven type only.
    for source in [
        "import java.math.BigDecimal\nfun build(value: Float) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun build(value: Number) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun build(value: Double?) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun build(value: Any) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun <T> build(value: T) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun build() { val value = obtain(); BigDecimal(value) }",
        "import java.math.BigDecimal\nfun build(value: Double) { listOf(\"x\").forEach { value -> BigDecimal(value) } }",
        "import java.math.BigDecimal\nfun build(values: Array<String>) = BigDecimal(*values)",
        "import java.math.BigDecimal\nfun build() = BigDecimal(value = \"1\", other = \"2\")",
        "fun build(): StringBuilder = StringBuilder(\"a\") { }\n",
        // An explicit cast is a proven *Kotlin* type but not one of the argument
        // shapes this checkpoint's resolver proves, so the `CharSequence`
        // overload stays unproven rather than being inferred from the cast.
        "fun build(): StringBuilder = StringBuilder(\"seed\" as CharSequence)",
    ] {
        let provider = facts(&owners, source, None);
        let name = if source.contains("StringBuilder") {
            "StringBuilder"
        } else {
            "BigDecimal"
        };
        retained_call(&provider, source, name);
    }
}

/// Owner-family bound: facts may be authoritative for an owner this checkpoint
/// does not own, without making that owner's constructor establishable.
#[test]
fn classpath_constructors_do_not_widen_the_four_owner_families() {
    let local_date = rows(&[
        ("<init>", &["int", "int", "int"]),
        ("of", &["int", "int", "int"]),
    ]);
    let uuid = rows(&[("<init>", &["long", "long"])]);
    for (owner, members, source, name, established) in [
        (
            "java.time.LocalDate",
            local_date.as_str(),
            "import java.time.LocalDate\nfun build() = LocalDate(2024, 1, 2)",
            "LocalDate",
            false,
        ),
        (
            "java.util.UUID",
            uuid.as_str(),
            "import java.util.UUID\nfun build(value: Long) = UUID(value, value)",
            "UUID",
            false,
        ),
        // Control: the same owner inside the bounded family does establish the
        // fully-qualified spelling.
        (
            "java.lang.StringBuilder",
            STRING_BUILDER,
            "fun build(): java.lang.CharSequence = java.lang.StringBuilder(\"seed\")",
            "StringBuilder",
            true,
        ),
    ] {
        let provider = facts(&[(owner, members)], source, None);
        if established {
            asserted(&provider, source, name);
            continue;
        }
        assert!(
            retained(&provider, source).iter().any(|s| s == name),
            "an owner outside the bounded families must stay retained: {source}"
        );
    }
}

/// The fact source is asked per canonical owner, and never for a spelling the
/// syntax layer already refuses.
#[test]
fn constructor_queries_are_owner_scoped_and_skipped_for_shadowed_spellings() {
    let log = std::env::temp_dir().join(format!(
        "notlin-constructor-queries-{}.log",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&log);
    let owners = real_owners();

    let proven = "fun build(): StringBuilder = StringBuilder(\"seed\")";
    let provider = facts(&owners, proven, Some(&log));
    asserted(&provider, proven, "StringBuilder");
    let asked = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        asked
            .lines()
            .any(|owner| owner.trim() == "java.lang.StringBuilder"),
        "the canonical owner must be the query key: {asked:?}"
    );

    let _ = std::fs::remove_file(&log);
    // A shadowed spelling beside a proven constructor in the *same* run: the
    // proven owner must be journaled, so the negative below cannot pass just
    // because the helper never recorded anything.
    let shadowed = "fun build(StringBuilder: () -> String) = StringBuilder(\"seed\")\nfun amount(text: String) = java.math.BigDecimal(text)";
    let provider = facts(&owners, shadowed, Some(&log));
    retained_call(&provider, shadowed, "StringBuilder");
    let asked = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        asked
            .lines()
            .any(|owner| owner.trim() == "java.math.BigDecimal"),
        "the control owner must be journaled, or the shadowed negative is vacuous: {asked:?}"
    );
    assert!(
        !asked
            .lines()
            .any(|owner| owner.trim() == "java.lang.StringBuilder"),
        "a shadowed spelling must never reach the helper: {asked:?}"
    );
}
