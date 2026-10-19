use notlin::semantics::SyntaxSemanticProvider;
use notlin::translation_plan::{
    BackendOwner, PreparationBlockerKind, RetentionReason, TranslationPlan,
};
use std::path::{Path, PathBuf};

fn provider(files: &[(&str, &str)]) -> SyntaxSemanticProvider {
    for (path, source) in files {
        if !path.ends_with(".kt") {
            continue;
        }
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "invalid fixture {path}:\n{source}"
        );
    }
    SyntaxSemanticProvider::new(
        files
            .iter()
            .map(|(path, source)| (PathBuf::from(path), (*source).to_owned()))
            .collect::<Vec<_>>(),
    )
}

fn analyze(source: &str, file: &str, index: &SyntaxSemanticProvider) -> TranslationPlan {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let path = Path::new(file);
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), path);
    notlin::semantics::plan_bottom_return_losses(source, &tree, path, index, false, &mut plan);
    plan
}

fn decision<'a>(
    plan: &'a TranslationPlan,
    name: &str,
) -> &'a notlin::translation_plan::DeclarationDecision {
    plan.declarations
        .iter()
        .find(|entry| entry.symbol_id.name == name)
        .unwrap_or_else(|| {
            panic!(
                "missing planned declaration {name}: {:#?}",
                plan.declarations
            )
        })
}

#[test]
fn explicit_nothing_callable_property_and_established_callers_stay_kotlin() {
    let file = "BottomReturns.kt";
    let source = r#"package bottomplan
fun stop(): Nothing = throw IllegalStateException()
fun invoke(): String = stop()
class Holder {
    val halted: Nothing
        get() = throw IllegalStateException()
}
"#;
    let index = provider(&[(file, source)]);
    let plan = analyze(source, file, &index);
    for name in ["stop", "invoke", "Holder"] {
        assert_eq!(decision(&plan, name).candidate_owner, BackendOwner::Kotlin);
    }
    assert!(
        decision(&plan, "stop")
            .retention_reasons
            .iter()
            .any(|reason| {
                matches!(reason, RetentionReason::PreparationBlocker {
            kind: PreparationBlockerKind::SemanticLoss,
            message,
        } if message.contains("Nothing"))
            })
    );
    assert!(plan.declarations.iter().all(|entry| {
        entry.candidate_owner == BackendOwner::Kotlin
            || entry.retention_reasons.iter().all(|reason| {
                !matches!(reason, RetentionReason::PreparationBlocker { message, .. } if message.contains("Nothing"))
            })
    }));
}

#[test]
fn fully_qualified_nothing_is_proven_even_with_an_unrelated_unknown_star() {
    let file = "QualifiedBottom.kt";
    let source = r#"package bottomplan
import sample.extensions.*
fun stop(): kotlin.Nothing = throw IllegalStateException()
"#;
    let index = provider(&[(file, source)]);
    let plan = analyze(source, file, &index);
    assert_eq!(
        decision(&plan, "stop").candidate_owner,
        BackendOwner::Kotlin
    );
}

#[test]
fn simple_nothing_return_with_unknown_star_binding_is_not_assumed_canonical() {
    let file = "UnknownBottom.kt";
    let source = r#"package bottomplan
import sample.extensions.*
fun stop(): Nothing = throw IllegalStateException()
"#;
    let index = provider(&[(file, source)]);
    let plan = analyze(source, file, &index);
    assert_eq!(
        decision(&plan, "stop").candidate_owner,
        BackendOwner::Kotlin
    );
    assert!(
        decision(&plan, "stop")
            .retention_reasons
            .iter()
            .any(|reason| {
                matches!(reason, RetentionReason::PreparationBlocker {
            kind: PreparationBlockerKind::UnresolvedAssumption,
            message,
        } if message.contains("unknown or ambiguous"))
            })
    );
}

#[test]
fn source_classifier_named_nothing_is_not_a_canonical_bottom_type() {
    let file = "ShadowedBottom.kt";
    let source = r#"package bottomplan
class Nothing
fun make(): Nothing = Nothing()
"#;
    let index = provider(&[(file, source)]);
    let plan = analyze(source, file, &index);
    assert_eq!(decision(&plan, "make").candidate_owner, BackendOwner::Java);
}

#[test]
fn approximation_mode_reports_nothing_loss_without_changing_the_candidate_owner() {
    let file = "ApproxBottom.kt";
    let source = "package bottomplan\nfun stop(): Nothing = throw IllegalStateException()\n";
    let index = provider(&[(file, source)]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let path = Path::new(file);
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), path);
    notlin::semantics::plan_bottom_return_losses(source, &tree, path, &index, true, &mut plan);
    assert_eq!(decision(&plan, "stop").candidate_owner, BackendOwner::Java);
    assert!(
        plan.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "A001")
    );
}

#[test]
fn caller_is_retained_only_when_source_binding_proves_nothing() {
    let file = "AmbiguousBottom.kt";
    let sources = [
        (
            file,
            "package bottomplan\nfun stop(): Nothing = throw IllegalStateException()\nfun invoke(): String = stop()\n",
        ),
        (
            "Other.kt",
            "package other\nfun stop(): String = \"ready\"\n",
        ),
    ];
    let index = provider(&sources);
    let source = sources[0].1;
    let plan = analyze(source, file, &index);
    assert_eq!(
        decision(&plan, "stop").candidate_owner,
        BackendOwner::Kotlin
    );
    assert_eq!(
        decision(&plan, "invoke").candidate_owner,
        BackendOwner::Kotlin
    );
}

#[test]
fn unrelated_todo_does_not_create_a_false_bottom_return_fact() {
    let file = "TodoBottom.kt";
    let source = "package bottomplan\nfun missing(): String = TODO()\n";
    let index = provider(&[(file, source)]);
    let plan = analyze(source, file, &index);
    assert_eq!(
        decision(&plan, "missing").candidate_owner,
        BackendOwner::Java
    );
}

#[test]
fn qualified_and_explicitly_imported_builtin_nothing_survive_local_name_collisions() {
    for source in [
        "package bottomplan\nclass Nothing\nfun stop(): kotlin.Nothing = throw IllegalStateException()\n",
        "package bottomplan\nimport kotlin.Nothing as Nothing\nfun stop(): Nothing = throw IllegalStateException()\n",
    ] {
        let index = provider(&[("Qualified.kt", source)]);
        let result = analyze(source, "Qualified.kt", &index);
        assert_eq!(
            decision(&result, "stop").candidate_owner,
            BackendOwner::Kotlin
        );
    }
}

#[test]
fn nullable_nothing_preserves_its_null_only_assignability_contract() {
    let source =
        "package bottomplan\nfun absent(): Nothing? = null\nfun value(): String? = absent()\n";
    let index = provider(&[("NullableBottom.kt", source)]);
    let result = analyze(source, "NullableBottom.kt", &index);
    assert_eq!(
        decision(&result, "absent").candidate_owner,
        BackendOwner::Kotlin
    );
    assert_eq!(
        decision(&result, "value").candidate_owner,
        BackendOwner::Kotlin
    );
}
