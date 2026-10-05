//! Retained-Kotlin smart-cast repair (`N6C94`).
//!
//! Translating a property's OWNER turns its Kotlin read into a Java getter call,
//! and Kotlin does not carry a smart cast across two such calls. The repair binds
//! the read into a local before the narrowing control flow. These tests pin the
//! supported shapes, the collision-free local name, and — just as important — the
//! shapes the pass must REFUSE, because a planner that trusts an unrepairable
//! site translates an owner that then breaks the build.

use notlin::smart_cast::{SmartCastShape, bindings, rewrite, sites};

/// Every site in `source`, as `(shape, property, repairable, uses)`.
fn shapes(source: &str) -> Vec<(SmartCastShape, String, bool, usize)> {
    sites(source)
        .into_iter()
        .map(|site| (site.shape, site.property, site.repairable, site.uses))
        .collect()
}

fn repair(source: &str) -> String {
    let (text, _) = rewrite(source, &|_owner, _property| true);
    text
}

#[test]
fn if_is_branch_binds_the_property_once() {
    let source = "fun read(h: Holder): String {\n    if (h.payload is Detail) {\n        return h.payload.text\n    }\n    return \"\"\n}\n";
    let text = repair(source);
    assert!(
        text.contains("    val payload = h.payload\n    if (payload is Detail) {"),
        "{text}"
    );
    assert!(text.contains("return payload.text"), "{text}");
    assert!(!text.contains("h.payload.text"), "{text}");
}

#[test]
fn positive_test_without_a_narrowed_use_is_left_alone() {
    // Nothing in the branch reads the chain: one getter call narrows nothing, so
    // the owner can translate untouched.
    let source = "fun read(h: Holder) {\n    if (h.payload is Detail) {\n        log()\n    }\n}\n";
    assert_eq!(repair(source), source);
    assert_eq!(shapes(source)[0].3, 0, "no narrowed use");
}

#[test]
fn not_is_early_exit_binds_and_rewrites_the_rest() {
    let source = "fun read2(h: Holder): String {\n    if (h.payload !is Detail) return \"\"\n    return h.payload.text\n}\n";
    let text = repair(source);
    assert!(
        text.contains("    val payload = h.payload\n    if (payload !is Detail) return \"\""),
        "{text}"
    );
    assert!(text.contains("return payload.text"), "{text}");
}

#[test]
fn not_is_that_falls_through_is_refused() {
    // The `if` can fall through, so the code after it is NOT narrowed.
    let source = "fun read(h: Holder) {\n    if (h.payload !is Detail) {\n        log(h.payload)\n    }\n    use(h.payload.text)\n}\n";
    assert_eq!(
        repair(source),
        source,
        "no repair for a falling-through !is"
    );
    assert!(
        shapes(source).iter().all(|site| !site.2),
        "unrepairable: {:?}",
        shapes(source)
    );
}

#[test]
fn not_is_with_else_narrows_the_else_branch() {
    let source = "fun read(h: Holder): String {\n    if (h.payload !is Detail) {\n        return \"\"\n    } else {\n        return h.payload.text\n    }\n}\n";
    let text = repair(source);
    assert!(text.contains("val payload = h.payload"), "{text}");
    assert!(text.contains("return payload.text"), "{text}");
}

#[test]
fn when_subject_binds_across_is_entries() {
    let source = "fun read3(h: Holder): String = when (h.payload) {\n    is Detail -> h.payload.text\n    else -> \"\"\n}\n";
    let text = repair(source);
    assert!(text.contains("val payload = h.payload"), "{text}");
    assert!(text.contains("when (payload) {"), "{text}");
    assert!(text.contains("is Detail -> payload.text"), "{text}");
}

#[test]
fn subjectless_when_binds_first_positive_condition() {
    let source = "fun read(h: Holder) {\n    when {\n        h.payload is Detail -> h.payload.text\n        else -> \"\"\n    }\n}\n";
    let text = repair(source);
    assert!(
        text.contains(
            "    val payload = h.payload\n    when {\n        payload is Detail -> payload.text"
        ),
        "{text}"
    );
    assert_eq!(shapes(source)[0].0, SmartCastShape::WhenCondition);
}

#[test]
fn subjectless_when_does_not_hoist_past_an_earlier_entry() {
    let source = "fun read(h: Holder): String = when {\n    ready() -> \"ready\"\n    h.payload is Detail -> h.payload.text\n    else -> \"\"\n}\n";
    assert_eq!(repair(source), source);
    assert!(
        shapes(source).iter().any(|site| !site.2),
        "later type tests remain visible to retention: {:?}",
        shapes(source)
    );
}

#[test]
fn subjectless_when_in_an_expression_body_is_refused() {
    let source = "fun read(h: Holder): String = when {\n    h.payload is Detail -> h.payload.text\n    else -> \"\"\n}\n";
    assert_eq!(repair(source), source);
    assert!(!shapes(source)[0].2);
}

#[test]
fn conjunction_rewrites_the_dominated_operand() {
    let source = "fun read4(h: Holder): Boolean {\n    if (h.payload is Detail && h.payload.text.isNotEmpty()) return true\n    return false\n}\n";
    let text = repair(source);
    assert!(text.contains("val payload = h.payload"), "{text}");
    assert!(
        text.contains("if (payload is Detail && payload.text.isNotEmpty())"),
        "{text}"
    );
}

#[test]
fn conjunction_test_that_is_not_first_is_refused() {
    // Hoisting the binding would move the receiver's evaluation in front of the
    // operand that precedes it.
    let source = "fun read4(h: Holder): Boolean {\n    if (other(h) && h.payload is Detail && h.payload.text.isNotEmpty()) return true\n    return false\n}\n";
    assert_eq!(repair(source), source);
}

#[test]
fn local_name_never_collides_with_an_existing_identifier() {
    let source = "fun read(h: Holder): String {\n    val payload = 1\n    if (h.payload is Detail) {\n        return h.payload.text + payload\n    }\n    return \"\"\n}\n";
    let text = repair(source);
    assert!(text.contains("val payloadNotlin1 = h.payload"), "{text}");
    assert!(text.contains("if (payloadNotlin1 is Detail)"), "{text}");
    assert!(text.contains("payloadNotlin1.text + payload"), "{text}");
}

#[test]
fn unresolved_receiver_is_left_alone() {
    // The rewrite pass only repairs a site whose owner it can PROVE: an
    // unresolved receiver belongs to the retention decision, not to a guess.
    let source = "fun read(h: Holder): String {\n    if (h.payload is Detail) {\n        return h.payload.text\n    }\n    return \"\"\n}\n";
    let (text, repaired) = rewrite(source, &|_owner, _property| false);
    assert_eq!(repaired, 0);
    assert_eq!(text, source);
}

#[test]
fn safe_call_and_deep_chains_are_refused() {
    for source in [
        "fun read(h: Holder): String {\n    if (h?.payload is Detail) {\n        return h.payload.text\n    }\n    return \"\"\n}\n",
        "fun read(a: Outer): String {\n    if (a.b.payload is Detail) {\n        return a.b.payload.text\n    }\n    return \"\"\n}\n",
    ] {
        assert_eq!(repair(source), source, "{source}");
    }
}

#[test]
fn receiver_shadowed_inside_the_branch_is_not_rewritten() {
    // The inner `h` is a different value: rewriting it would change the program.
    let source = "fun read(h: Holder): String {\n    if (h.payload is Detail) {\n        return listOf(1).map { h -> h.payload.text }.first()\n    }\n    return \"\"\n}\n";
    assert_eq!(repair(source), source, "{source}");
}

#[test]
fn rewrite_is_idempotent() {
    let source = "fun read(h: Holder): String {\n    if (h.payload is Detail) {\n        return h.payload.text\n    }\n    return \"\"\n}\n";
    let once = repair(source);
    let (twice, repaired) = rewrite(&once, &|_owner, _property| true);
    assert_eq!(repaired, 0, "the binding is already there: {twice}");
    assert_eq!(twice, once);
}

#[test]
fn crlf_sources_keep_their_line_endings() {
    let source = "fun read(h: Holder): String {\r\n    if (h.payload is Detail) {\r\n        return h.payload.text\r\n    }\r\n    return \"\"\r\n}\r\n";
    let text = repair(source);
    assert!(
        text.contains("    val payload = h.payload\r\n    if (payload is Detail) {"),
        "{text}"
    );
    assert!(
        !text
            .as_bytes()
            .iter()
            .enumerate()
            .any(|(i, byte)| *byte == b'\n' && (i == 0 || text.as_bytes()[i - 1] != b'\r')),
        "no bare LF introduced: {text}"
    );
}

#[test]
fn nested_sites_are_repaired_by_their_outer_site() {
    let source = "fun read(h: Holder): String {\n    if (h.payload is Detail) {\n        if (h.payload.text.isEmpty()) {\n            return h.payload.text\n        }\n    }\n    return \"\"\n}\n";
    let text = repair(source);
    assert_eq!(text.matches("val payload = h.payload").count(), 1, "{text}");
    // The chain survives only in the binding itself: every narrowed use, the
    // nested test included, reads the local.
    assert_eq!(text.matches("h.payload").count(), 1, "{text}");
}

/// The whole chain, through the binary: the owner translates, and the caller that
/// had to stay Kotlin comes back out with the read bound once.
#[test]
fn cli_run_translates_the_owner_and_repairs_the_retained_caller() {
    let root = std::env::temp_dir().join(format!("notlin-smart-cast-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(root.join("p")).unwrap();
    std::fs::write(
        root.join("p/Holder.kt"),
        "package neutral.smartcast2\n\ninterface Value\nclass Detail(val text: String) : Value\ndata class Holder(val payload: Value)\n",
    )
    .unwrap();
    // `inline fun <reified T>` is retained in Kotlin, so this declaration is
    // still compiled by kotlinc against the translated `Holder` — exactly the
    // boundary the rewrite pass exists for.
    std::fs::write(
        root.join("p/Consumer.kt"),
        "package neutral.smartcast2\n\ninline fun <reified T> render(holder: Holder): String {\n    if (holder.payload is Detail) return holder.payload.text\n    return \"\"\n}\n",
    )
    .unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("p/Holder.kt"))
        .arg(root.join("p/Consumer.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");

    let holder_java = std::fs::read_to_string(root.join("p/Holder.java")).unwrap_or_default();
    assert!(
        holder_java.contains("Holder"),
        "the property owner translates:\n{stderr}"
    );

    let consumer = std::fs::read_to_string(root.join("p/Consumer.kt")).unwrap_or_default();
    assert!(
        consumer.contains(
            "    val payload = holder.payload\n    if (payload is Detail) return payload.text"
        ),
        "the retained caller is repaired:\n{consumer}"
    );
    assert!(
        !stderr.contains("smart-casts one of its properties"),
        "no smart-cast retention is reported:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cli_run_infers_a_local_receiver_from_a_method_return() {
    let root = std::env::temp_dir().join(format!(
        "notlin-smart-cast-inferred-cli-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("p")).unwrap();
    std::fs::write(
        root.join("p/Holder.kt"),
        "package neutral.smartcast3\n\ninterface Value\nclass Detail(val text: String) : Value\ndata class Holder(val payload: Value)\n",
    )
    .unwrap();
    std::fs::write(
        root.join("p/Lookup.kt"),
        "package neutral.smartcast3\n\nimport java.util.Optional\n\ninterface Lookup {\n    fun getHolder(id: String): Optional<Holder> = Optional.of(Holder(Detail(id)))\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("p/Consumer.kt"),
        "package neutral.smartcast3\n\nclass Consumer : Lookup {\n    fun unrelated(holder: Value) = Unit\n\n    inline fun <reified T> render(): String {\n        val holder = getHolder(\"ok\").orElseThrow()\n        if (holder.payload is Detail) return holder.payload.text\n        return \"\"\n    }\n}\n",
    )
    .unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("p/Holder.kt"))
        .arg(root.join("p/Lookup.kt"))
        .arg(root.join("p/Consumer.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");

    let holder_java = std::fs::read_to_string(root.join("p/Holder.java")).unwrap_or_default();
    assert!(
        holder_java.contains("Holder"),
        "the inferred receiver must not retain its property owner:\n{stderr}"
    );

    let consumer = std::fs::read_to_string(root.join("p/Consumer.kt")).unwrap_or_default();
    assert!(
        consumer.contains(
            "        val payload = holder.payload\n        if (payload is Detail) return payload.text"
        ),
        "the retained caller is repaired:\n{consumer}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_nested_site_on_another_chain_keeps_its_own_binding() {
    // The enclosing site's first and last read straddle the nested site without
    // touching any of its reads: skipping it because it sits inside that span
    // would leave `g.other` reading a Java getter under a smart cast.
    let source = "fun read(h: Holder, g: Gear): String {\n\
         \x20   if (h.payload is Detail) {\n\
         \x20       if (g.other is Thing) {\n\
         \x20           use(g.other.name)\n\
         \x20       }\n\
         \x20       use(h.payload.text)\n\
         \x20   }\n\
         \x20   return \"\"\n\
         }\n";
    let text = repair(source);
    assert_eq!(text.matches("val payload = h.payload").count(), 1, "{text}");
    assert_eq!(text.matches("val other = g.other").count(), 1, "{text}");
    assert!(text.contains("if (other is Thing)"), "{text}");
    assert!(text.contains("use(other.name)"), "{text}");
    assert!(text.contains("use(payload.text)"), "{text}");
}

#[test]
fn bindings_resolve_parameters_properties_and_class_parameters() {
    let source = "class Consumer(val holder: Holder) {\n    val other: Detail? = null\n    fun read(h: Holder, raw: String): String {\n        val local: Detail = h.payload\n        return local.text\n    }\n}\n";
    let table = bindings(source);
    assert_eq!(table.get("holder").map(String::as_str), Some("Holder"));
    assert_eq!(table.get("other").map(String::as_str), Some("Detail"));
    assert_eq!(table.get("h").map(String::as_str), Some("Holder"));
    assert_eq!(table.get("local").map(String::as_str), Some("Detail"));
    // A name whose type never names an owner (`String`) is still a binding; it
    // simply never resolves to a declaration that could move to Java.
    assert_eq!(table.get("raw").map(String::as_str), Some("String"));
}

#[test]
fn ambiguous_binding_is_dropped_rather_than_guessed() {
    let source = "fun one(a: Holder) {\n}\nfun two(a: Detail) {\n}\n";
    assert!(!bindings(source).contains_key("a"));
}
