use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider};
use notlin::top_level_property_callsite::{
    properties_requiring_kotlin_retention, repair_emitted_property_references,
};
use notlin::translation_plan::{self, BackendOwner};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

fn java_plan(path: &str, source: &str) -> translation_plan::TranslationPlan {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = translation_plan::analyze(source, tree.root_node(), &PathBuf::from(path));
    for decision in &mut plan.declarations {
        decision.final_owner = Some(BackendOwner::Java);
    }
    plan
}

#[test]
fn repairs_keep_distinct_same_named_property_identities_and_snapshot_spans() {
    let left = "package left\nvar calls = 0\n";
    let right = "package right\nvar calls = 0\n";
    let caller = "package caller\nsuspend fun use() { left.calls++; right.calls += 2 }\n";
    let original = BTreeMap::from([
        (PathBuf::from("Left.kt"), left.into()),
        (PathBuf::from("Right.kt"), right.into()),
    ]);
    let plans = [java_plan("Left.kt", left), java_plan("Right.kt", right)];
    let owners = plans
        .iter()
        .flat_map(|plan| plan.declarations.iter().map(|d| d.symbol_id.clone()))
        .collect::<Vec<_>>();
    let mut residual = BTreeMap::from([(PathBuf::from("Caller.kt"), caller.to_owned())]);
    let report = repair_emitted_property_references(&original, &plans, &mut residual, &owners);
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
    assert_eq!(
        residual[&PathBuf::from("Caller.kt")],
        "package caller\nsuspend fun use() { left.Left.calls++; right.Right.calls += 2 }\n"
    );
    assert_eq!(
        report
            .repairs
            .iter()
            .map(|r| r.target.clone())
            .collect::<HashSet<_>>(),
        owners.into_iter().collect()
    );
    assert_eq!(report.edits.len(), 2);
    for edit in report.edits {
        assert_eq!(
            edit.location.snapshot_hash,
            *blake3::hash(caller.as_bytes()).as_bytes()
        );
        assert!(matches!(
            &caller[edit.location.start_byte..edit.location.end_byte],
            "left.calls" | "right.calls"
        ));
    }
}

#[test]
fn stale_or_unemitted_property_ownership_cannot_rewrite_callers() {
    let source = "package sample\nvar calls = 0\n";
    let plan = java_plan("State.kt", source);
    let owner = plan.declarations[0].symbol_id.clone();
    let caller = BTreeMap::from([(
        PathBuf::from("Caller.kt"),
        "package sample\nsuspend fun use() { calls++ }\n".into(),
    )]);
    let mut residual = caller.clone();
    let report = repair_emitted_property_references(
        &BTreeMap::from([(PathBuf::from("State.kt"), source.into())]),
        std::slice::from_ref(&plan),
        &mut residual,
        &[],
    );
    assert!(report.edits.is_empty());
    assert_eq!(residual, caller);
    let changed = BTreeMap::from([(PathBuf::from("State.kt"), source.replace("= 0", "= 1"))]);
    let report = repair_emitted_property_references(&changed, &[plan], &mut residual, &[owner]);
    assert!(report.edits.is_empty());
    assert!(report.diagnostics.iter().any(|d| d.code == "P005"));
    assert_eq!(residual, caller);
}

fn retained(sources: &[(&str, &str)]) -> Vec<String> {
    let sources = sources
        .iter()
        .map(|(path, source)| (PathBuf::from(path), (*source).into()))
        .collect::<BTreeMap<_, _>>();
    let provider = SyntaxSemanticProvider::new(sources.clone());
    properties_requiring_kotlin_retention(
        &sources,
        &provider
            .symbols()
            .iter()
            .map(|s| s.id.clone())
            .collect::<Vec<_>>(),
        &provider,
    )
    .keys()
    .map(|id| id.name.clone())
    .collect()
}

#[test]
fn short_templates_member_receivers_and_arbitrary_receiver_lambdas_retain_properties() {
    for body in [
        "println(\"$calls\")",
        "println(calls.toString())",
        "customScope { calls++ }",
    ] {
        let caller = format!("package sample\nsuspend fun use() {{ {body} }}\n");
        assert!(
            retained(&[
                ("State.kt", "package sample\nvar calls = 0\n"),
                ("Caller.kt", &caller)
            ])
            .contains(&"calls".into()),
            "{body}"
        );
    }
}

#[test]
fn file_facade_collision_and_unknown_star_resolution_remain_kotlin_owned() {
    assert!(
        retained(&[("State.kt", "package sample\nclass State\nvar calls = 0\n")])
            .contains(&"calls".into())
    );
    assert!(
        retained(&[(
            "State.kt",
            "@file:JvmName(\"Renamed\")\npackage sample\nvar calls = 0\n"
        )])
        .contains(&"calls".into())
    );
    assert!(
        retained(&[
            ("State.kt", "package state\nvar calls = 0\n"),
            (
                "Caller.kt",
                "package caller\nimport state.*\nimport external.*\nsuspend fun use() { calls++ }\n"
            )
        ])
        .contains(&"calls".into())
    );
}

#[test]
fn package_and_jvm_name_text_in_comments_cannot_redirect_field_references() {
    let source = "// @file:JvmName(\"Wrong\")\npackage sample\nvar calls = 0\n";
    let caller = "/*\npackage wrong\n*/\npackage sample\nsuspend fun use() { calls++ }\n";
    let plan = java_plan("State.kt", source);
    let owner = plan.declarations[0].symbol_id.clone();
    let mut residual = BTreeMap::from([(PathBuf::from("Caller.kt"), caller.into())]);
    let report = repair_emitted_property_references(
        &BTreeMap::from([(PathBuf::from("State.kt"), source.into())]),
        &[plan],
        &mut residual,
        &[owner],
    );
    assert!(report.diagnostics.is_empty());
    assert!(residual[&PathBuf::from("Caller.kt")].contains("sample.State.calls++"));
    assert!(!residual[&PathBuf::from("Caller.kt")].contains("Wrong.calls"));
}

#[test]
fn qualified_callable_values_and_package_root_value_shadows_are_not_field_proofs() {
    assert!(
        retained(&[
            (
                "State.kt",
                "package sample\nval callback: () -> Int = { 1 }\n"
            ),
            (
                "Caller.kt",
                "package other\nsuspend fun use(): Int = sample.callback()\n"
            )
        ])
        .contains(&"callback".into())
    );
    assert!(retained(&[("State.kt", "package sample\nvar calls = 0\n"), ("Caller.kt", "package other\nclass Holder(var calls: Int)\nval sample = Holder(5)\nsuspend fun use() { sample.calls++ }\n")]).contains(&"calls".into()));
}
