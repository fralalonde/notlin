//! Middle default arguments (`N87CB`).
//!
//! Java cannot express Kotlin default arguments as such, but it can express the
//! call shapes callers actually use: a literal default can be written into the
//! call, and any other omission pattern can be given a delegating overload. These
//! tests pin both, and — for the declarations that must NOT translate — the three
//! cases that genuinely have no Java form: a default naming a parameter the
//! omission pattern does not supply, a delegating signature that collides after
//! erasure, and a call shape that could not be read.

use notlin::ctor_defaults::{
    CtorShape, call_sites, is_neutral_literal, lower_call_args, plan_ctor_defaults, resolve_args,
    rewrite, trailing_patterns,
};

fn defaults(entries: &[Option<&str>]) -> Vec<Option<String>> {
    entries
        .iter()
        .map(|entry| entry.map(str::to_string))
        .collect()
}

fn names(entries: &[&str]) -> Vec<String> {
    entries.iter().map(|entry| entry.to_string()).collect()
}

#[test]
fn only_language_neutral_literals_may_be_written_into_a_call() {
    for literal in [
        "10", "-3", "1.5", "10L", "0xFF", "true", "false", "null", "\"x\"", "'c'",
    ] {
        assert!(
            is_neutral_literal(literal),
            "`{literal}` reads the same in Java"
        );
    }
    for other in [
        "\"$name\"",    // Kotlin interpolation
        "listOf()",     // a Kotlin call
        "Dimensions()", // a constructor
        "OrganizationType.INSTANCE",
        "referenceNo", // another parameter
        "10u",         // no Java unsigned literal
    ] {
        assert!(
            !is_neutral_literal(other),
            "`{other}` belongs inside a delegating overload, not in the call"
        );
    }
}

#[test]
fn named_arguments_resolve_to_declared_positions() {
    let params = names(&["first", "optional", "last"]);
    let resolved = resolve_args(
        &params,
        &[
            (Some("first".to_string()), "\"x\"".to_string()),
            (Some("last".to_string()), "true".to_string()),
        ],
    )
    .expect("every name matches a parameter");
    assert_eq!(
        resolved.slots,
        vec![Some("\"x\"".to_string()), None, Some("true".to_string())]
    );
    assert_eq!(resolved.omitted, vec![1]);
}

#[test]
fn a_positional_argument_after_a_named_one_is_not_resolvable() {
    let params = names(&["first", "last"]);
    assert!(
        resolve_args(
            &params,
            &[
                (Some("first".to_string()), "\"x\"".to_string()),
                (None, "true".to_string()),
            ],
        )
        .is_none(),
        "Kotlin puts positional arguments first; anything else is not this shape"
    );
}

#[test]
fn an_unknown_name_or_an_overflowing_position_leaves_the_call_alone() {
    let params = names(&["first", "last"]);
    assert!(
        resolve_args(&params, &[(Some("other".to_string()), "1".to_string())]).is_none(),
        "a name that matches no parameter is not this constructor"
    );
    assert!(
        resolve_args(
            &params,
            &[
                (None, "1".to_string()),
                (None, "2".to_string()),
                (None, "3".to_string())
            ]
        )
        .is_none(),
        "more positional arguments than parameters is a vararg call, not an omission"
    );
}

#[test]
fn a_literal_default_is_written_into_the_call_and_the_rest_are_left_to_the_overload() {
    let slots = vec![
        Some("\"x\"".to_string()),
        None,
        None,
        Some("true".to_string()),
    ];
    let defaults = defaults(&[None, Some("10"), Some("listOf()"), None]);
    let (args, left) = lower_call_args(&slots, &defaults);
    assert_eq!(
        args,
        vec!["\"x\"".to_string(), "10".to_string(), "true".to_string()],
        "the literal default goes in; the non-literal one is not invented"
    );
    assert_eq!(left, vec![2], "the pattern keeps its delegating overload");
}

#[test]
fn a_middle_default_a_caller_omits_gets_a_delegating_overload() {
    let params = names(&["first", "optional", "last"]);
    let plan = plan_ctor_defaults(&CtorShape {
        param_names: &params,
        defaults: &defaults(&[None, Some("10"), None]),
        param_types: &names(&["String", "Int", "Boolean"]),
        patterns: &[vec![1]],
        ladder: &[],
        has_secondary_constructor: false,
    });
    assert_eq!(
        plan.overloads,
        vec![vec![1]],
        "one overload for the used pattern"
    );
    assert!(plan.blocked.is_none());
}

#[test]
fn a_declaration_nobody_omits_needs_no_overload_at_all() {
    let params = names(&["first", "optional", "last"]);
    let plan = plan_ctor_defaults(&CtorShape {
        param_names: &params,
        defaults: &defaults(&[None, Some("10"), None]),
        param_types: &names(&["String", "Int", "Boolean"]),
        patterns: &[],
        ladder: &[],
        has_secondary_constructor: false,
    });
    assert!(plan.overloads.is_empty());
    assert!(
        plan.blocked.is_none(),
        "a middle default is not a blocker on its own"
    );
}

#[test]
fn a_pattern_the_trailing_ladder_already_covers_is_not_written_twice() {
    let params = names(&["host", "port", "secure"]);
    let defaults = defaults(&[None, Some("8080"), Some("false")]);
    let ladder = trailing_patterns(&defaults);
    assert_eq!(ladder, vec![vec![2], vec![1, 2]]);
    let plan = plan_ctor_defaults(&CtorShape {
        param_names: &params,
        defaults: &defaults,
        param_types: &names(&["String", "Int", "Boolean"]),
        patterns: &[vec![1, 2]],
        ladder: &ladder,
        has_secondary_constructor: false,
    });
    assert!(plan.overloads.is_empty(), "the ladder rung already exists");
    assert!(plan.blocked.is_none());
}

#[test]
fn a_default_naming_a_parameter_the_pattern_omits_blocks_the_translation() {
    let params = names(&["a", "b"]);
    let plan = plan_ctor_defaults(&CtorShape {
        param_names: &params,
        // `b = a + 1` cannot be evaluated in an overload that does not take `a`.
        defaults: &defaults(&[None, Some("a + 1")]),
        param_types: &names(&["Int", "Int"]),
        patterns: &[vec![0, 1]],
        ladder: &[],
        has_secondary_constructor: false,
    });
    assert_eq!(plan.overloads.len(), 0);
    assert!(
        plan.blocked
            .as_deref()
            .is_some_and(|reason| reason.contains("`a`")),
        "the reason names the parameter the pattern cannot supply: {:?}",
        plan.blocked
    );
}

#[test]
fn overloads_that_erase_to_the_same_signature_block_the_translation() {
    let params = names(&["first", "left", "right", "last"]);
    let plan = plan_ctor_defaults(&CtorShape {
        param_names: &params,
        defaults: &defaults(&[None, Some("listOf()"), Some("listOf()"), None]),
        // Two omission patterns of the same arity whose kept types erase alike.
        param_types: &names(&["String", "List<A>", "List<B>", "Boolean"]),
        patterns: &[vec![1], vec![2]],
        ladder: &[],
        has_secondary_constructor: false,
    });
    assert!(plan.overloads.is_empty());
    assert!(
        plan.blocked
            .as_deref()
            .is_some_and(|reason| reason.contains("erasure")),
        "the reason names the collision: {:?}",
        plan.blocked
    );
}

#[test]
fn a_class_that_declares_another_constructor_blocks_new_overloads() {
    let params = names(&["first", "optional", "last"]);
    let plan = plan_ctor_defaults(&CtorShape {
        param_names: &params,
        defaults: &defaults(&[None, Some("10"), None]),
        param_types: &names(&["String", "Int", "Boolean"]),
        patterns: &[vec![1]],
        ladder: &[],
        has_secondary_constructor: true,
    });
    assert!(plan.overloads.is_empty());
    assert!(
        plan.blocked
            .as_deref()
            .is_some_and(|reason| reason.contains("another constructor")),
        "the reason is the unprovable collision: {:?}",
        plan.blocked
    );
}

#[test]
fn call_sites_read_names_positions_and_unreadable_shapes() {
    let source = "fun build(): Payload = Payload(\"x\", last = true)\n\
                  fun spread(): Payload = Payload(*args)\n\
                  fun other(): Payload = build { it }\n";
    let sites = call_sites(source);
    let shapes: Vec<(String, usize, Vec<String>, bool)> = sites
        .iter()
        .map(|site| {
            (
                site.callee.clone(),
                site.args
                    .iter()
                    .filter(|arg| arg.name.is_none() && !arg.spread)
                    .count(),
                site.args
                    .iter()
                    .filter_map(|arg| arg.name.clone())
                    .collect(),
                site.unknown,
            )
        })
        .collect();
    assert_eq!(
        shapes,
        vec![
            ("Payload".to_string(), 1, vec!["last".to_string()], false),
            ("Payload".to_string(), 0, vec![], true),
        ],
        "a spread is recorded as unreadable; a trailing lambda is not a constructor call at all"
    );
}

#[test]
fn a_named_argument_call_is_lowered_against_a_translated_constructor() {
    let source = "fun make(): Payload = Payload(first = \"y\", last = false)\n";
    let params = names(&["first", "optional", "last"]);
    let (text, rewrites) = rewrite(source, &|callee| {
        (callee == "Payload").then(|| params.clone())
    });
    assert_eq!(
        text, "fun make(): Payload = Payload(\"y\", false)\n",
        "names dropped, declared order kept, the omitted middle parameter left to the overload"
    );
    assert_eq!(rewrites, 1);
}

#[test]
fn a_constructor_with_no_translated_counterpart_is_left_alone() {
    let source = "fun make(): Payload = Payload(first = \"y\", last = false)\n";
    let (text, rewrites) = rewrite(source, &|_callee| None);
    assert_eq!(text, source);
    assert_eq!(rewrites, 0);
}

#[test]
fn an_already_positional_call_is_not_rewritten_again() {
    let source = "fun make(): Payload = Payload(\"y\", false)\n";
    let params = names(&["first", "last"]);
    let (text, rewrites) = rewrite(source, &|callee| {
        (callee == "Payload").then(|| params.clone())
    });
    assert_eq!(text, source, "the pass is idempotent");
    assert_eq!(rewrites, 0);
}

/// Run the CLI over `files` in a scratch root and hand back `(stderr, root)`.
fn run_cli(name: &str, files: &[(&str, &str)]) -> (String, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("notlin-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for (file, text) in files {
        std::fs::write(root.join(file), text).unwrap();
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    (String::from_utf8_lossy(&output.stderr).to_string(), root)
}

#[test]
fn cli_gives_a_retained_caller_the_delegating_overload_it_needs() {
    // `inline fun <reified T>` is itself retained in Kotlin, so this call site is
    // still compiled by kotlinc against the TRANSLATED `Payload`: named arguments
    // have no Java form, and the omitted middle parameter has to come from a
    // constructor the Java class carries.
    let (stderr, root) = run_cli(
        "ctor-defaults-retained",
        &[
            (
                "shapes.kt",
                "package p\n\nclass Payload(val first: String, val optional: Int = 10, val last: Boolean)\n",
            ),
            (
                "consumer.kt",
                "package p\n\ninline fun <reified T> make(): Payload = Payload(first = \"y\", last = false)\n",
            ),
        ],
    );
    let payload = std::fs::read_to_string(root.join("Payload.java")).unwrap_or_default();
    assert!(
        payload.contains(
            "public Payload(String first, boolean last) {\n        this(first, 10, last);"
        ),
        "one delegating overload for the pattern the caller uses:\n{payload}\nstderr:\n{stderr}"
    );
    let consumer = std::fs::read_to_string(root.join("consumer.kt")).unwrap_or_default();
    assert!(
        consumer.contains("Payload(\"y\", false)"),
        "the retained caller's named arguments are lowered to positional order:\n{consumer}"
    );
    assert!(
        !stderr.contains("omits a default argument"),
        "the declaration must not be retained for an omission an overload serves:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cli_translates_a_middle_default_no_caller_omits() {
    let (stderr, root) = run_cli(
        "ctor-defaults-unused",
        &[(
            "shapes.kt",
            "package p\n\nclass Lonely(val a: Int, val b: String = \"b\", val c: Boolean)\n",
        )],
    );
    let lonely = std::fs::read_to_string(root.join("Lonely.java")).unwrap_or_default();
    assert!(
        lonely.contains("public final class Lonely"),
        "a middle default nobody omits is not a blocker:\n{lonely}\nstderr:\n{stderr}"
    );
    assert!(
        !lonely.contains("public Lonely("),
        "no caller omits it, so no delegating overload is written:\n{lonely}"
    );
    assert!(
        !root.join("shapes.kt").exists(),
        "the source is migrated in place"
    );
    assert!(
        !stderr.contains("omits a default argument"),
        "no retention is reported:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cli_writes_a_literal_default_into_a_translated_call() {
    let (stderr, root) = run_cli(
        "ctor-defaults-translated-caller",
        &[(
            "shapes.kt",
            "package p\n\nclass Payload(val first: String, val optional: Int = 10, val last: Boolean)\n\nfun build(): Payload = Payload(first = \"x\", last = true)\n",
        )],
    );
    let shapes = std::fs::read_to_string(root.join("Shapes.java")).unwrap_or_default();
    assert!(
        shapes.contains("new Payload(\"x\", 10, true)"),
        "the canonical all-arguments call carries the literal default:\n{shapes}\nstderr:\n{stderr}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
