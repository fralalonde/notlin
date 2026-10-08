use notlin::planning::{AcceptedTranslationPlan, PreparedJavaFile};
use notlin::semantics::SymbolId;
use notlin::semantics::SyntaxSemanticProvider;
use notlin::translation_plan::TranslationPlan;
use notlin::translation_plan::{
    BackendOwner, DeclarationDecision, DeclarationId, DeclarationKind, PreparationOutcome,
};
use std::path::PathBuf;

fn plan_with_java_owner() -> TranslationPlan {
    let file = PathBuf::from("Model.kt");
    let symbol_id = SymbolId {
        module: "demo".into(),
        package: "demo".into(),
        file: file.clone(),
        owner_path: vec![],
        kind: "class".into(),
        name: "Box".into(),
        receiver: None,
        parameters: vec![],
    };
    TranslationPlan {
        source_hash: [1; 32],
        declarations: vec![DeclarationDecision {
            id: DeclarationId {
                file: file.clone(),
                source_hash: [1; 32],
                start_byte: 0,
                end_byte: 1,
            },
            symbol_id,
            qualified_name: Some("demo.Box".into()),
            kind: DeclarationKind::Class,
            candidate_owner: BackendOwner::Java,
            retention_reasons: vec![],
            preparation_outcome: Some(PreparationOutcome::Prepared),
            final_owner: Some(BackendOwner::Java),
        }],
        ..TranslationPlan::default()
    }
}

#[test]
fn accepted_plan_owns_ir_and_emits_from_it() {
    let path = "demo/Box.java".to_string();
    let source = "package demo;\npublic record Box(int value) { public int twice() { return value * 2; } public int call() { return takes(helper()); } }\n";
    let provider = SyntaxSemanticProvider::new([(
        PathBuf::from("Model.kt"),
        "package demo\nclass Box { fun twice() = 2; fun helper() = 1; fun takes(x: Int) = x }"
            .to_string(),
    )]);
    let accepted = AcceptedTranslationPlan::accept(
        plan_with_java_owner(),
        vec![PreparedJavaFile {
            path: path.clone(),
            source: source.to_string(),
            owners: vec![plan_with_java_owner().declarations[0].symbol_id.clone()],
            snapshot_hash: [1; 32],
        }],
        Some(&provider),
    )
    .expect("syntactically valid candidate should be accepted");

    assert_eq!(accepted.files().len(), 1);
    assert_eq!(accepted.files()[0].path, path);
    assert_eq!(accepted.emit(), vec![(path, source.to_string())]);
    assert_eq!(accepted.translation().outputs.len(), 1);
    assert_eq!(accepted.translation().outputs[0].owner, BackendOwner::Java);
    let mut resolved_calls = Vec::new();
    accepted.files()[0]
        .unit
        .root
        .clone()
        .visit_mut(&mut |node| {
            if node.kind == "method_invocation"
                && let Some(target) = &node.origin_target
            {
                resolved_calls.push(target.name.clone());
            }
        });
    resolved_calls.sort();
    assert_eq!(
        resolved_calls,
        vec!["helper", "takes"],
        "calls resolve their own names, not argument names"
    );
}

#[test]
fn invalid_candidate_aborts_acceptance_instead_of_emitting_partial_files() {
    let result = AcceptedTranslationPlan::accept(
        plan_with_java_owner(),
        vec![
            PreparedJavaFile {
                path: "Good.java".into(),
                source: "class Good {}".into(),
                owners: vec![plan_with_java_owner().declarations[0].symbol_id.clone()],
                snapshot_hash: [1; 32],
            },
            PreparedJavaFile {
                path: "Bad.java".into(),
                source: "class Bad { void f( { }".into(),
                owners: vec![plan_with_java_owner().declarations[0].symbol_id.clone()],
                snapshot_hash: [1; 32],
            },
        ],
        None,
    );
    let error = result.expect_err("malformed Java must be rejected");
    assert_eq!(error.path, "Bad.java");
}

#[test]
fn acceptance_rejects_stale_snapshot_and_retained_ownership() {
    let mut plan = plan_with_java_owner();
    let owner = plan.declarations[0].symbol_id.clone();
    let prepared = |hash| PreparedJavaFile {
        path: "Box.java".into(),
        source: "class Box {}".into(),
        owners: vec![owner.clone()],
        snapshot_hash: hash,
    };
    assert!(
        AcceptedTranslationPlan::accept(plan.clone(), vec![prepared([2; 32])], None)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    plan.declarations[0].final_owner = Some(BackendOwner::Kotlin);
    assert!(
        AcceptedTranslationPlan::accept(plan, vec![prepared([1; 32])], None)
            .unwrap_err()
            .to_string()
            .contains("not Java-owned")
    );
}

#[test]
fn acceptance_rejects_unassigned_and_mismatched_generated_types() {
    let plan = plan_with_java_owner();
    assert!(AcceptedTranslationPlan::accept(plan.clone(), vec![], None).is_err());
    let candidate = PreparedJavaFile {
        path: "Other.java".into(),
        source: "class Other {}".into(),
        owners: vec![plan.declarations[0].symbol_id.clone()],
        snapshot_hash: plan.source_hash,
    };
    assert!(
        AcceptedTranslationPlan::accept(plan, vec![candidate], None)
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
}

#[test]
fn annotations_do_not_replace_the_generated_type_name() {
    let plan = plan_with_java_owner();
    let candidate = PreparedJavaFile {
        path: "Box.java".into(),
        source: "@Marker(value = 1) class Box { @Other void f() {} }".into(),
        owners: vec![plan.declarations[0].symbol_id.clone()],
        snapshot_hash: plan.source_hash,
    };
    let accepted = AcceptedTranslationPlan::accept(plan, vec![candidate], None).unwrap();
    assert!(accepted.emit()[0].1.contains("class Box"));
}

#[test]
fn constructor_overloads_have_constructor_symbol_origins() {
    let plan = plan_with_java_owner();
    let provider = SyntaxSemanticProvider::new([(
        PathBuf::from("Model.kt"),
        "package demo\nclass Box(val value: Int = 1)\n".to_string(),
    )]);
    let candidate = PreparedJavaFile {
        path: "Box.java".into(),
        source: "class Box { Box(int value) {} Box() { this(1); } }".into(),
        owners: vec![plan.declarations[0].symbol_id.clone()],
        snapshot_hash: plan.source_hash,
    };
    let accepted = AcceptedTranslationPlan::accept(plan, vec![candidate], Some(&provider)).unwrap();
    assert!(
        accepted
            .translation()
            .bridges
            .iter()
            .any(|bridge| bridge.kind.starts_with("constructor-overload:")
                && bridge.origin.kind == "constructor")
    );
}

#[test]
fn facade_calls_keep_the_planned_top_level_symbol_target() {
    let mut plan = plan_with_java_owner();
    let symbol = &mut plan.declarations[0].symbol_id;
    symbol.kind = "function".into();
    symbol.name = "work".into();
    plan.declarations[0].kind = DeclarationKind::Function;
    let owner = plan.declarations[0].symbol_id.clone();
    let provider = SyntaxSemanticProvider::new([(
        PathBuf::from("Model.kt"),
        "package demo\nfun work(): Int = 1\n".to_string(),
    )]);
    let candidate = PreparedJavaFile {
        path: "Model.java".into(),
        source: "class Model { static int work() { return Model.work(); } }".into(),
        owners: vec![owner.clone()],
        snapshot_hash: plan.source_hash,
    };
    let accepted = AcceptedTranslationPlan::accept(plan, vec![candidate], Some(&provider)).unwrap();
    let mut targets = Vec::new();
    accepted.files()[0]
        .unit
        .root
        .clone()
        .visit_mut(&mut |node| {
            if node.kind == "method_invocation" {
                targets.push(node.origin_target.clone());
            }
        });
    assert_eq!(targets, vec![Some(owner)]);
}

#[test]
fn lombok_generated_members_have_unique_property_and_class_origins() {
    let cases = [
        (
            "import lombok.Data; @Data class Box { private int count; private final String label; }",
            "package demo\ndata class Box(var count: Int, val label: String)",
            vec![
                ("property", "count", "lombok:getter:getCount()"),
                ("property", "count", "lombok:setter:setCount(int)"),
                ("property", "label", "lombok:getter:getLabel()"),
                ("class", "Box", "lombok:equals(java.lang.Object)"),
                ("class", "Box", "lombok:hashCode()"),
                ("class", "Box", "lombok:toString()"),
            ],
        ),
        (
            "import lombok.Value; @Value class Box { int count; }",
            "package demo\ndata class Box(val count: Int)",
            vec![
                ("property", "count", "lombok:getter:getCount()"),
                ("class", "Box", "lombok:equals(java.lang.Object)"),
                ("class", "Box", "lombok:hashCode()"),
                ("class", "Box", "lombok:toString()"),
                (
                    "constructor",
                    "<init>",
                    "lombok:all-args-constructor:Box(int)",
                ),
            ],
        ),
        (
            "import lombok.AllArgsConstructor; @AllArgsConstructor class Box { static int ignored; int count; }",
            "package demo\nclass Box(val count: Int)",
            vec![(
                "constructor",
                "<init>",
                "lombok:all-args-constructor:Box(int)",
            )],
        ),
        (
            "import lombok.Data; @Data class Box { private boolean isEnabled; }",
            "package demo\ndata class Box(var isEnabled: Boolean)",
            vec![
                ("property", "isEnabled", "lombok:getter:isEnabled()"),
                ("property", "isEnabled", "lombok:setter:setEnabled(boolean)"),
            ],
        ),
    ];
    for (java, kotlin, expected) in cases {
        let mut plan = plan_with_java_owner();
        plan.declarations[0].symbol_id.module.clear();
        let owner = plan.declarations[0].symbol_id.clone();
        let provider = SyntaxSemanticProvider::new([(PathBuf::from("Model.kt"), kotlin.into())]);
        let accepted = AcceptedTranslationPlan::accept(
            plan,
            vec![PreparedJavaFile {
                path: "Box.java".into(),
                source: java.into(),
                owners: vec![owner],
                snapshot_hash: [1; 32],
            }],
            Some(&provider),
        )
        .unwrap();
        for (origin_kind, origin_name, bridge_kind) in expected {
            assert!(
                accepted.translation().bridges.iter().any(|bridge| {
                    bridge.origin.kind == origin_kind
                        && bridge.origin.name == origin_name
                        && bridge.kind == bridge_kind
                }),
                "missing Lombok bridge {bridge_kind} for {origin_kind} {origin_name}: {:?}",
                accepted.translation().bridges
            );
        }
    }

    let mut plan = plan_with_java_owner();
    plan.declarations[0].symbol_id.module.clear();
    let owner = plan.declarations[0].symbol_id.clone();
    let provider = SyntaxSemanticProvider::new([(
        PathBuf::from("Model.kt"),
        "package demo\ndata class Box(var count: Int)".into(),
    )]);
    let accepted = AcceptedTranslationPlan::accept(
        plan,
        vec![PreparedJavaFile {
            path: "Box.java".into(),
            source: "import lombok.Data; @Data class Box { private int count; public int getCount() { return count; } }".into(),
            owners: vec![owner],
            snapshot_hash: [1; 32],
        }],
        Some(&provider),
    )
    .unwrap();
    assert!(
        !accepted
            .translation()
            .bridges
            .iter()
            .any(|bridge| { bridge.kind == "lombok:getter:getCount()" })
    );
    assert!(
        accepted
            .translation()
            .bridges
            .iter()
            .any(|bridge| { bridge.kind == "lombok:setter:setCount(int)" })
    );

    let mut plan = plan_with_java_owner();
    plan.declarations[0].symbol_id.module.clear();
    let owner = plan.declarations[0].symbol_id.clone();
    let provider = SyntaxSemanticProvider::new([(
        PathBuf::from("Model.kt"),
        "package demo\ndata class Box(var count: Int)".into(),
    )]);
    let accepted = AcceptedTranslationPlan::accept(
        plan,
        vec![PreparedJavaFile {
            path: "Box.java".into(),
            source: "import lombok.Data; @Data class Box { int count; public int GETCOUNT() { return count; } public void setCount(String value) {} }".into(),
            owners: vec![owner],
            snapshot_hash: [1; 32],
        }],
        Some(&provider),
    )
    .unwrap();
    assert!(!accepted.translation().bridges.iter().any(|bridge| {
        bridge.kind == "lombok:getter:getCount()" || bridge.kind == "lombok:setter:setCount(int)"
    }));

    for (java, kotlin, absent_bridges) in [
        (
            "import lombok.Data; @Data class Box { boolean enabled; public boolean getEnabled() { return enabled; } }",
            "package demo\nclass Box(var enabled: Boolean)",
            vec!["lombok:getter:isEnabled()"],
        ),
        (
            "import lombok.Data; @Data class Box { boolean isEnabled; public boolean getIsEnabled() { return isEnabled; } public void setIsEnabled(boolean value) {} }",
            "package demo\nclass Box(var isEnabled: Boolean)",
            vec![
                "lombok:getter:isEnabled()",
                "lombok:setter:setEnabled(boolean)",
            ],
        ),
    ] {
        let mut plan = plan_with_java_owner();
        plan.declarations[0].symbol_id.module.clear();
        let owner = plan.declarations[0].symbol_id.clone();
        let provider = SyntaxSemanticProvider::new([(PathBuf::from("Model.kt"), kotlin.into())]);
        let accepted = AcceptedTranslationPlan::accept(
            plan,
            vec![PreparedJavaFile {
                path: "Box.java".into(),
                source: java.into(),
                owners: vec![owner],
                snapshot_hash: [1; 32],
            }],
            Some(&provider),
        )
        .unwrap();
        for bridge_kind in absent_bridges {
            assert!(
                !accepted
                    .translation()
                    .bridges
                    .iter()
                    .any(|bridge| bridge.kind == bridge_kind),
                "unexpected Lombok bridge {bridge_kind}: {:?}",
                accepted.translation().bridges
            );
        }
    }
}
