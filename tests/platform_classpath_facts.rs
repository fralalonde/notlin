//! Classpath facts feed the *same* applicability resolver the fallback table
//! uses; they do not add a second algorithm.
//!
//! The fact source here is a scripted line-protocol process (`sh`), not a JVM,
//! so these tests need no java/javac and pin only the Rust path: owner
//! resolution, `MemberFacts` → row mapping, unique applicability, and the fact
//! that a jar-less provider keeps today's retention.

use notlin::facts::decorator::FactsProvider;
use notlin::facts::runtime::FactsRuntime;
use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider, plan_required_calls};
use std::path::{Path, PathBuf};
use std::process::Command;

const MEMBERS: &str = include_str!("fixtures/classpath_facts/duration_members.json");

/// The helper answers on a single line; the fixture is pretty-printed, so the
/// fake has to join it before writing a response.
fn members_on_one_line() -> String {
    MEMBERS.lines().map(str::trim).collect()
}

/// Answers `Ping`, and `ClassMembers` for `java.time.Duration` from the fixture.
/// Any other owner gets `members: null`, which is the helper's "cannot load
/// this type" answer — the Rust side must treat that as "no facts", not as an
/// empty member list.
fn fake_helper() -> Command {
    let script = format!(
        r#"
while IFS= read -r line; do
  case "$line" in
    *'"Ping"'*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Pong":{{"version":1}}}}}}\n' "$id"
      ;;
    *'"owner":"java.time.Duration"'*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Members":{{"owner":"java.time.Duration","members":{members}}}}}}}\n' "$id"
      ;;
    *)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Members":{{"owner":"unknown","members":null}}}}}}\n' "$id"
      ;;
  esac
done
"#,
        members = members_on_one_line()
    );
    let mut command = Command::new("sh");
    command.arg("-c").arg(script);
    command
}

fn provider_with_facts(source: &str, file: &Path) -> FactsProvider {
    let runtime = FactsRuntime::with_command(fake_helper()).unwrap();
    FactsProvider::with_runtime(
        SyntaxSemanticProvider::new([(file.to_path_buf(), source.to_owned())]),
        runtime,
    )
}

fn provider_without_facts(source: &str, file: &Path) -> FactsProvider {
    FactsProvider::transparent(SyntaxSemanticProvider::new([(
        file.to_path_buf(),
        source.to_owned(),
    )]))
}

/// Whether planning retains the declaration whose body calls `name`.
fn retains(provider: &dyn SemanticProvider, source: &str, file: &Path, name: &str) -> bool {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture: {source}");
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), file);
    plan_required_calls(source, &tree, file, provider, false, &mut plan);
    plan.dependencies
        .iter()
        .any(|dependency| dependency.spelling == name)
}

fn fixture_file() -> PathBuf {
    PathBuf::from("Extend.kt")
}

#[test]
fn a_unique_classpath_applicability_match_establishes_the_call() {
    let file = fixture_file();
    let source = include_str!("fixtures/classpath_facts/extend.kt");
    let provider = provider_with_facts(source, &file);
    assert!(
        !retains(&provider, source, &file, "plusDays"),
        "plusDays(long) is a unique classpath match and must be established"
    );
    assert!(
        !retains(&provider, source, &file, "plus"),
        "plus(Duration) must be provable from another classpath-typed argument"
    );
}

#[test]
fn without_a_fact_source_the_same_calls_stay_retained() {
    let file = fixture_file();
    let source = include_str!("fixtures/classpath_facts/extend.kt");
    let provider = provider_without_facts(source, &file);
    for name in ["plusDays", "plus"] {
        assert!(
            retains(&provider, source, &file, name),
            "{name} is outside the fallback table and must stay unknown without facts"
        );
    }
}

#[test]
fn argument_proofs_still_gate_applicability() {
    let file = fixture_file();
    let provider = provider_with_facts("", &file);
    for source in [
        // The only `plusDays` row takes a long; a String argument is no match.
        "package sample\nimport java.time.Duration\nfun bad(base: Duration): Duration = base.plusDays(\"1\")\n",
        // Arity: no two-argument `plus`.
        "package sample\nimport java.time.Duration\nfun bad(base: Duration, other: Duration): Duration = base.plus(other, other)\n",
        // A nullable receiver is unproven.
        "package sample\nimport java.time.Duration\nfun bad(base: Duration?): Duration? = base?.plusDays(2L)\n",
    ] {
        assert!(
            retains(&provider, source, &file, "plusDays")
                || retains(&provider, source, &file, "plus"),
            "unproven call must stay retained: {source}"
        );
    }
}

#[test]
fn kotlin_owned_callees_are_never_answered_from_classpath_facts() {
    let file = fixture_file();
    // `takeIf` is a Kotlin stdlib inline function: reflection cannot see it, and
    // its call-site semantics are Track B. Track A must leave it unknown rather
    // than answer from a kotlin.* class that happens to carry a similar symbol.
    let source = "package sample\nfun pick(x: Int): Int? = x.takeIf { it > 0 }\n";
    let provider = provider_with_facts(source, &file);
    assert!(retains(&provider, source, &file, "takeIf"));
}

#[test]
fn an_owner_the_fact_source_cannot_load_falls_back_to_the_table() {
    let file = fixture_file();
    // The fake answers `members: null` for BigDecimal, so the run must behave
    // exactly as it does with no helper: the fallback row still applies.
    let source = "package sample\nimport java.math.BigDecimal\nfun total(a: BigDecimal, b: BigDecimal): BigDecimal = a.add(b)\n";
    let provider = provider_with_facts(source, &file);
    assert!(!retains(&provider, source, &file, "add"));
}
