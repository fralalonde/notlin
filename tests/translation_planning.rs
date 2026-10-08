use clap::Parser;
use notlin::cli::Cli;
use notlin::migrate::{MigrationProposal, propose_speculative_migration};
use notlin::translation_plan::{BackendOwner, RetentionReason};
use notlin::transpiler::{PlannedTranslation, WorkspaceScope, plan_with_tree_hint};
use std::path::Path;

fn translate(source: &str) -> PlannedTranslation {
    let file = Path::new("Planning.kt");
    let cli = Cli::parse_from(["notlin", "--annotations", "none", "Planning.kt"]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    plan_with_tree_hint(source, &tree, file, &cli, WorkspaceScope::default(), true)
}

#[test]
fn coroutine_boundary_stays_kotlin_while_independent_class_translates() {
    let source = "class Worker {\n suspend fun fetch(): String = \"value\"\n}\nclass Plain {\n fun count(): Int = 1\n}\n";
    let result = translate(source);
    assert_eq!(result.errors, 0);
    assert!(result.warnings > 0);
    assert_eq!(
        result.java_files.len(),
        1,
        "plan={:?}; coverage={:?}",
        result.plan,
        result.coverage
    );
    assert_eq!(result.java_files[0].0, "Plain.java");
    assert_eq!(
        result.plan.declarations[0].final_owner,
        Some(BackendOwner::Kotlin)
    );
    assert_eq!(
        result.plan.declarations[1].final_owner,
        Some(BackendOwner::Java)
    );
    assert!(matches!(
        result.plan.declarations[0].retention_reasons[0],
        RetentionReason::SuspendConstruct { .. }
    ));
    let MigrationProposal::Rewrite(residue) =
        propose_speculative_migration(source, &result.coverage)
    else {
        panic!("partial translation must keep Kotlin residue");
    };
    assert!(residue.contains("suspend fun fetch"));
    assert!(!residue.contains("class Plain"));
}

#[test]
fn blocked_overload_does_not_own_its_clean_sibling() {
    for source in [
        "suspend fun convert(x: String): String = x\nfun convert(x: Int): Int = x\n",
        "fun convert(x: Int): Int = x\nsuspend fun convert(x: String): String = x\n",
    ] {
        let result = translate(source);
        assert_eq!(result.java_files.len(), 1);
        let java = &result.java_files[0].1;
        assert!(java.contains("int convert(int x)"), "{java}");
        assert!(
            !java.contains("String convert"),
            "blocked overload leaked: {java}"
        );
        let java_decisions = result
            .plan
            .declarations
            .iter()
            .filter(|d| d.final_owner == Some(BackendOwner::Java))
            .count();
        assert_eq!(java_decisions, 1);
        let MigrationProposal::Rewrite(residue) =
            propose_speculative_migration(source, &result.coverage)
        else {
            panic!("one overload must remain Kotlin");
        };
        assert!(residue.contains("suspend fun convert"));
        assert!(!residue.contains("x: Int"));
    }
}

#[test]
fn late_lowering_blocker_discards_the_entire_facade_member() {
    let source = "fun broken(): Int { val value by unknown; return 7 }\nfun good(): Int = 3\n";
    let result = translate(source);
    assert_eq!(result.java_files.len(), 1);
    let java = &result.java_files[0].1;
    assert!(java.contains("int good()"), "{java}");
    assert!(!java.contains("broken("), "rejected method leaked: {java}");
    assert_eq!(
        result.plan.declarations[0].final_owner,
        Some(BackendOwner::Kotlin)
    );
    assert!(matches!(
        result.plan.declarations[0].retention_reasons[0],
        RetentionReason::PreparationBlocker { .. }
    ));
}

#[test]
fn forward_call_to_retained_function_is_not_emitted_as_unresolved_java() {
    let source = "class Caller { fun call(): String = fetch() }\nsuspend fun fetch(): String = \"value\"\nclass Plain\n";
    let result = translate(source);
    assert!(
        !result
            .java_files
            .iter()
            .any(|(name, _)| name == "Caller.java")
    );
    assert!(
        result
            .java_files
            .iter()
            .any(|(name, _)| name == "Plain.java")
    );
    assert_eq!(
        result.plan.declarations[0].final_owner,
        Some(BackendOwner::Kotlin)
    );
}

#[test]
fn ordinary_method_can_call_non_suspend_api_of_retained_class() {
    let source = "class Worker {\n fun label(): String = \"value\"\n suspend fun fetch(): String = label()\n}\nclass Caller {\n fun call(): String = Worker().label()\n}\n";
    let result = translate(source);
    assert!(
        !result
            .java_files
            .iter()
            .any(|(name, _)| name == "Worker.java")
    );
    assert!(
        result
            .java_files
            .iter()
            .any(|(name, _)| name == "Caller.java"),
        "plan={:?}; coverage={:?}",
        result.plan,
        result.coverage
    );
}
