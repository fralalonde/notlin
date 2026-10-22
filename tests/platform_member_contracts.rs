//! What an external JVM signature may and may not answer.
//!
//! The resolver reads one table of hand-declared rows plus, when a helper is
//! present, this run's classpath facts. Both sources are gated by the same
//! proofs: the *identity* of the receiver (not its spelling), the exact proven
//! type of every argument (not arity, not assignability), and unique
//! applicability. These tests pin the holes that a name-only dispatch would
//! open: local shadowing, import aliases, foreign same-named classes,
//! receiver-typed instance calls, erased `(Object)` overloads and duplicate
//! rows.

use notlin::facts::decorator::FactsProvider;
use notlin::facts::runtime::FactsRuntime;
use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider, plan_required_calls};
use std::path::{Path, PathBuf};
use std::process::Command;

const DURATION_MEMBERS: &str = include_str!("fixtures/classpath_facts/duration_members.json");
const COMPARE_TO_MEMBERS: &str = include_str!("fixtures/classpath_facts/compare_to_members.json");
const DUPLICATE_APPEND_MEMBERS: &str =
    include_str!("fixtures/classpath_facts/duplicate_append_members.json");

/// Calls the fact path left unproven in `file`, by spelling.
fn retained(provider: &(impl SemanticProvider + ?Sized), file: &Path, source: &str) -> Vec<String> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture: {source}");
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), file);
    plan_required_calls(source, &tree, file, provider, false, &mut plan);
    plan.dependencies
        .into_iter()
        .map(|dependency| dependency.spelling)
        .collect()
}

fn syntax_provider(sources: &[(&str, &str)]) -> SyntaxSemanticProvider {
    SyntaxSemanticProvider::new(
        sources
            .iter()
            .map(|(file, source)| (PathBuf::from(file), (*source).to_owned()))
            .collect::<Vec<_>>(),
    )
}

fn unresolved(source: &str) -> Vec<String> {
    let file = Path::new("Contracts.kt");
    retained(&syntax_provider(&[("Contracts.kt", source)]), file, source)
}

fn one_line(members: &str) -> String {
    members.lines().map(str::trim).collect()
}

/// A `sh` helper that speaks the line protocol.
///
/// `owners` maps an owner to the members JSON it answers with; every other
/// owner gets `members: null` ("cannot load this type"). When `log` is given,
/// each queried owner is appended to it, so a test can assert what the Rust
/// side actually asked for instead of inferring it.
fn fake_helper(owners: &[(&str, &str)], log: Option<&Path>) -> Command {
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
        .map(|path| format!("  printf '%s\\n' \"$owner\" >> '{}'\n", path.display()))
        .unwrap_or_default();
    let script = format!(
        r#"while IFS= read -r line; do
  owner=$(printf '%s' "$line" | sed -n 's/.*"owner":"\([^"]*\)".*/\1/p')
{log_line}  case "$line" in
    *'"Ping"'*)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Pong":{{"version":1}}}}}}\n' "$id"
      ;;
{arms}    *)
      id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/')
      printf '{{"version":1,"id":%s,"answer":{{"Members":{{"owner":"unknown","members":null}}}}}}\n' "$id"
      ;;
  esac
done
"#
    );
    let mut command = Command::new("sh");
    command.arg("-c").arg(script);
    command
}

fn facts_provider(
    owners: &[(&str, &str)],
    sources: &[(&str, &str)],
    log: Option<&Path>,
) -> FactsProvider {
    let runtime = FactsRuntime::with_command(fake_helper(owners, log)).unwrap();
    FactsProvider::with_runtime(syntax_provider(sources), runtime)
}

#[test]
fn static_field_values_are_instance_receivers_not_type_qualifiers() {
    for (imports, expression) in [
        ("import java.math.BigDecimal", "BigDecimal.ONE.abs()"),
        (
            "import java.math.BigDecimal as Decimal",
            "Decimal.ZERO.abs()",
        ),
        ("", "java.math.BigDecimal.TEN.abs()"),
    ] {
        let source = format!("{imports}\nfun magnitude(): java.math.BigDecimal = {expression}");
        assert!(
            unresolved(&source).is_empty(),
            "field receiver was misclassified: {source}: {:?}",
            unresolved(&source)
        );
    }
}

#[test]
fn static_field_receiver_proofs_reject_unknown_and_shadowed_values() {
    for source in [
        "import java.math.BigDecimal\nfun magnitude() = BigDecimal.UNKNOWN.abs()",
        "import foreign.math.BigDecimal as Decimal\nfun magnitude() = Decimal.ONE.abs()",
        "import java.math.BigDecimal\nfun magnitude(BigDecimal: Any) = BigDecimal.ONE.abs()",
        "import java.util.Locale\nfun locale() = Locale.ROOT.getDefault()",
    ] {
        assert!(
            !unresolved(source).is_empty(),
            "unproven field receiver gained a call fact: {source}"
        );
    }
}

#[test]
fn signatures_compose_across_constructors_members_and_factories() {
    for source in [
        "fun text(n: Int): String = StringBuilder().append(n).append(\"!\").toString()",
        "import java.math.BigDecimal\nfun text(n: BigDecimal): String = StringBuilder().append(n.add(BigDecimal.ONE).toPlainString()).toString()",
        "import java.util.UUID\nfun text(): String = UUID.randomUUID().toString()",
        "import java.util.Locale\nfun locale(): Locale = Locale.getDefault()",
        "import java.time.Instant\nfun instant(): Instant = Instant.now()",
    ] {
        assert!(
            unresolved(source).is_empty(),
            "{source}: {:?}",
            unresolved(source)
        );
    }
}

#[test]
fn arity_alone_cannot_prove_an_overload_or_a_chain_result() {
    for argument in ["value", "null", "value = 1", "*values", "1, 2"] {
        let source = format!(
            "fun text(value: Any, values: Array<String>): String = StringBuilder().append({argument}).toString()"
        );
        assert!(
            unresolved(&source).iter().any(|name| name == "append"),
            "{source}"
        );
    }
    for source in [
        "import java.math.BigDecimal\nfun text(n: BigDecimal): String = n.add(value = n).toPlainString()",
        "import java.math.BigDecimal\nfun text(n: BigDecimal): String = n.add(n) { n }.toPlainString()",
        "fun text(builder: StringBuilder?): String = builder.append(1).toString()",
    ] {
        assert!(!unresolved(source).is_empty(), "{source}");
    }
}

#[test]
fn instance_values_cannot_authorize_static_dispatch() {
    for (imports, parameters, expression, name) in [
        (
            "import java.util.UUID",
            "value: UUID",
            "value.randomUUID()",
            "randomUUID",
        ),
        (
            "import java.util.Locale",
            "value: Locale",
            "value.getDefault()",
            "getDefault",
        ),
        (
            "import java.time.Instant",
            "value: Instant",
            "value.now()",
            "now",
        ),
        (
            "import java.util.UUID",
            "UUID: Any",
            "UUID.randomUUID()",
            "randomUUID",
        ),
    ] {
        let source = format!("{imports}\nfun run({parameters}) = {expression}");
        assert!(
            unresolved(&source)
                .iter()
                .any(|unresolved| unresolved == name),
            "{source}"
        );
    }
}

/// Pin 1 — a workspace declaration outranks a row that merely shares its name.
///
/// `abs` is declared locally, so `abs(3)` is a call to that function: it has no
/// `java.math.BigDecimal` result, and the row must not answer it. A local call
/// still shows up as a dependency of the enclosing unit, so retention of `abs`
/// itself proves nothing; the discriminator is the chained `toPlainString()`.
/// If the row answered `abs`, that call would carry a DECIMAL result and the
/// chain would establish itself with nothing retained.
#[test]
fn local_declarations_shadow_rows_of_the_same_name() {
    let file = Path::new("Shadow.kt");
    let cases = [
        // Top-level function.
        "package sample\nfun abs(x: Int): Int = if (x < 0) -x else x\nfun text(): String = abs(3).toPlainString()\n",
        // Member function reached through the implicit receiver.
        "package sample\nclass Holder {\n fun abs(): Holder = this\n fun text(): String = abs().toPlainString()\n}\n",
        // Extension function on the same receiver type.
        "package sample\nfun Int.abs(): Int = this\nfun text(): String = 3.abs().toPlainString()\n",
    ];
    for source in cases {
        let unproven = retained(&syntax_provider(&[("Shadow.kt", source)]), file, source);
        assert!(
            unproven.iter().any(|name| name == "toPlainString"),
            "a BigDecimal result would have chained: {source} -> {unproven:?}"
        );
    }
    // Positive control: the same expression shape on a genuinely proven
    // BigDecimal receiver does chain, so the assertions above are meaningful.
    let proven = "package sample\nimport java.math.BigDecimal\nfun text(b: BigDecimal): String = b.abs().toPlainString()\n";
    let unproven = retained(&syntax_provider(&[("Shadow.kt", proven)]), file, proven);
    assert!(
        unproven.is_empty(),
        "a proven BigDecimal receiver must still chain through abs(): {unproven:?}"
    );
}

/// Pin 2 — dispatch resolves the receiver by identity, not by spelling.
#[test]
fn receivers_resolve_by_identity_not_by_spelling() {
    let file = Path::new("Alias.kt");
    // An alias names the same class, so the same rows apply.
    let aliased = "package sample\nimport java.math.BigDecimal as Decimal\nfun add(a: Decimal, b: Decimal): Decimal = a.add(b)\n";
    let unproven = retained(&syntax_provider(&[("Alias.kt", aliased)]), file, aliased);
    assert!(
        !unproven.iter().any(|name| name == "add"),
        "an import alias is the same type: {unproven:?}"
    );

    // A different class that merely spells `BigDecimal` is a different type.
    let foreign = "package sample\nfun add(a: BigDecimal, b: BigDecimal): BigDecimal = a.add(b)\n";
    let provider = syntax_provider(&[
        ("Foreign.kt", "package other\n\nclass BigDecimal\n"),
        ("Alias.kt", foreign),
    ]);
    assert!(
        retained(&provider, file, foreign)
            .iter()
            .any(|name| name == "add"),
        "a foreign class named BigDecimal must not receive the JDK rows"
    );

    // An alias that points at another class must not borrow the rows either.
    let misdirected = "package sample\nimport java.util.UUID as BigDecimal\nfun add(a: BigDecimal, b: BigDecimal): BigDecimal = a.add(b)\n";
    assert!(
        retained(
            &syntax_provider(&[("Alias.kt", misdirected)]),
            file,
            misdirected
        )
        .iter()
        .any(|name| name == "add"),
        "an alias of java.util.UUID is not a BigDecimal"
    );

    // Positive control: the plain import still establishes the same call.
    let plain = "package sample\nimport java.math.BigDecimal\nfun add(a: BigDecimal, b: BigDecimal): BigDecimal = a.add(b)\n";
    assert!(
        !retained(&syntax_provider(&[("Alias.kt", plain)]), file, plain)
            .iter()
            .any(|name| name == "add"),
        "the direct import must keep working"
    );
}

/// Pin 3 — instance dispatch needs a proven receiver, not just a name match.
#[test]
fn instance_dispatch_requires_a_proven_receiver() {
    let file = Path::new("Receiver.kt");
    for parameter in ["value: Any", "value: String", "value: CharSequence"] {
        let source =
            format!("package sample\nfun text({parameter}): String = value.append(\"x\")\n");
        let unproven = retained(&syntax_provider(&[("Receiver.kt", &source)]), file, &source);
        assert!(
            unproven.iter().any(|name| name == "append"),
            "only a StringBuilder receiver may answer append: {parameter} -> {unproven:?}"
        );
    }
    // Positive control: the identical call on a proven receiver is established.
    let proven = "package sample\nfun text(b: StringBuilder): String = b.append(\"x\")\n";
    assert!(
        !retained(&syntax_provider(&[("Receiver.kt", proven)]), file, proven)
            .iter()
            .any(|name| name == "append"),
        "a proven StringBuilder receiver must establish append"
    );
}

/// Pin 4 — an erased `(Object)` row never captures a typed argument, and a
/// typed row never captures an unproven one.
#[test]
fn erased_object_rows_never_capture_a_typed_argument() {
    let file = Path::new("Compare.kt");
    let owners = [("java.math.BigDecimal", COMPARE_TO_MEMBERS)];
    for source in [
        // A String argument matches neither compareTo(BigDecimal) nor the
        // erased compareTo(Object): applicability is exact proven-type
        // equality, not Java assignability.
        "package sample\nimport java.math.BigDecimal\nfun cmp(a: BigDecimal, text: String): Int = a.compareTo(text)\n",
        // An Any-typed argument is not a proven java.lang.Object.
        "package sample\nimport java.math.BigDecimal\nfun cmp(a: BigDecimal, other: Any): Int = a.compareTo(other)\n",
        // A nullable argument proves nothing.
        "package sample\nimport java.math.BigDecimal\nfun cmp(a: BigDecimal, other: BigDecimal?): Int = a.compareTo(other)\n",
    ] {
        let provider = facts_provider(&owners, &[("Compare.kt", source)], None);
        assert!(
            retained(&provider, file, source)
                .iter()
                .any(|name| name == "compareTo"),
            "unproven argument must not be answered by an erased row: {source}"
        );
    }
    // Control and exclusivity proof: the typed row is the single match. If the
    // table's own compareTo row were merged in beside the classpath rows there
    // would be two matches and applicability would refuse.
    let typed = "package sample\nimport java.math.BigDecimal\nfun cmp(a: BigDecimal, b: BigDecimal): Int = a.compareTo(b)\n";
    let provider = facts_provider(&owners, &[("Compare.kt", typed)], None);
    assert!(
        !retained(&provider, file, typed)
            .iter()
            .any(|name| name == "compareTo"),
        "the typed row alone must establish compareTo"
    );
}

/// Pin 5 — two applicable rows are not a fact.
#[test]
fn duplicate_rows_refuse_unique_applicability() {
    let file = Path::new("Duplicate.kt");
    let source = "package sample\nfun text(b: StringBuilder) = b.append(\"x\")\n";
    // Declared + inherited copies of the same member (what a supertype walk
    // yields) are two candidate rows, not one stronger fact.
    let provider = facts_provider(
        &[("java.lang.StringBuilder", DUPLICATE_APPEND_MEMBERS)],
        &[("Duplicate.kt", source)],
        None,
    );
    assert!(
        retained(&provider, file, source)
            .iter()
            .any(|name| name == "append"),
        "matches > 1 must refuse, not pick a winner"
    );
    // Control: the same call without classpath facts is unique in the table,
    // so the fixture's duplicate is what removed the fact.
    assert!(
        !retained(&syntax_provider(&[("Duplicate.kt", source)]), file, source)
            .iter()
            .any(|name| name == "append"),
        "the fallback table has exactly one append row for this call"
    );
}

/// Pin 6 — Kotlin-owned callees are never answered, nor even queried, as
/// synthetic classpath owners.
#[test]
fn kotlin_owned_callees_are_never_queried_as_synthetic_owners() {
    let log = std::env::temp_dir().join(format!(
        "notlin-platform-member-queries-{}.log",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&log);
    let owners = [
        ("java.time.Duration", DURATION_MEMBERS),
        // Answered, but never askable: a synthetic `kotlin.*` facade is not a
        // reflection address, so a row here must stay unreachable.
        (
            "kotlin.collections.ListKt",
            r#"[{"name":"let","static_member":true,"parameter_types":["kotlin.Function1"],"result_type":"java.lang.Object","parameter_nullability":["unknown"],"result_nullability":"unknown"}]"#,
        ),
    ];
    let hof = "package sample\nfun pick(x: kotlin.Any): kotlin.Any? = x.takeIf { it != null }\nfun mapIt(x: kotlin.Any) = x.let { it }\nfun runIt(x: kotlin.Any) = x.run { it }\nfun anyOf(x: kotlin.Any) = x.any { it != null }\n";
    let control = "package sample\nimport java.time.Duration\nfun later(base: Duration): Duration = base.plusDays(2L)\n";
    let provider = facts_provider(
        &owners,
        &[("Hof.kt", hof), ("Control.kt", control)],
        Some(&log),
    );

    // The control proves the log is wired: it must record a real query.
    assert!(
        !retained(&provider, Path::new("Control.kt"), control)
            .iter()
            .any(|name| name == "plusDays"),
        "classpath facts must answer plusDays"
    );
    let unproven = retained(&provider, Path::new("Hof.kt"), hof);
    // `let`/`run` are lowered by the scope-function path, so they never reach
    // this resolver at all; `takeIf`/`any` do reach it and must stay unknown.
    for name in ["takeIf", "any"] {
        assert!(
            unproven.iter().any(|unresolved| unresolved == name),
            "{name} is Kotlin-owned and must stay unknown: {unproven:?}"
        );
    }

    let asked = std::fs::read_to_string(&log).unwrap_or_default();
    let asked = asked
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert!(
        asked.contains(&"java.time.Duration"),
        "the helper log must record the control query: {asked:?}"
    );
    assert!(
        !asked.iter().any(|owner| owner.starts_with("kotlin.")),
        "kotlin.* owners are not reflection addresses: {asked:?}"
    );
}
