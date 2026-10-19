use notlin::function_callsite::{
    functions_requiring_kotlin_retention, functions_requiring_kotlin_retention_with_provider,
    repair_virtual_calls, repair_virtual_calls_with_emitted_owners,
};
use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider};
use notlin::translation_plan::{self, BackendOwner};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn java_owned_plan(path: &str, source: &str) -> translation_plan::TranslationPlan {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = translation_plan::analyze(source, tree.root_node(), &PathBuf::from(path));
    for declaration in &mut plan.declarations {
        declaration.final_owner = Some(BackendOwner::Java);
    }
    plan
}

#[test]
fn facade_collision_changes_the_generated_name_and_caller_import_together() {
    let target = "package p\nclass Functions\nclass FunctionsKt\nfun work(value: Int) = value\n";
    let caller = "package q\nimport p.work\nfun use() = work(1)\n";
    let targets = BTreeMap::from([
        (PathBuf::from("Functions.kt"), target.to_owned()),
        (PathBuf::from("Caller.kt"), caller.to_owned()),
    ]);
    let mut residual = targets.clone();
    let report = repair_virtual_calls(
        &targets,
        &[java_owned_plan("Functions.kt", target)],
        &mut residual,
    );
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
    assert!(residual[&PathBuf::from("Caller.kt")].contains("import p.FunctionsKtKt.work"));
    assert_eq!(
        notlin::function_callsite::facade_name(target, std::path::Path::new("Functions.kt")),
        "FunctionsKtKt"
    );
}

#[test]
fn unique_imported_call_and_import_rewrite_to_file_facade() {
    let target = "package p\nfun work(value: Int) = value\n";
    let caller = "package q\nimport p.work\nfun use() = work(1)\n";
    let targets = BTreeMap::from([
        (PathBuf::from("Functions.kt"), target.to_owned()),
        (PathBuf::from("Caller.kt"), caller.to_owned()),
    ]);
    let plans = vec![java_owned_plan("Functions.kt", target)];
    let mut residual = targets.clone();
    let report = repair_virtual_calls(&targets, &plans, &mut residual);
    assert_eq!(
        residual[&PathBuf::from("Caller.kt")],
        "package q\nimport p.Functions.work\nfun use() = p.Functions.work(1)\n"
    );
    assert!(report.edits.iter().all(|edit| edit.speculative));
    assert_eq!(report.edits.len(), 2);
    assert!(
        report
            .edits
            .iter()
            .all(|edit| edit.location.snapshot_hash == *blake3::hash(caller.as_bytes()).as_bytes())
    );
}

#[test]
fn honors_jvm_name_and_alias_but_leaves_local_shadow_and_named_arguments() {
    let target = "@file:JvmName(\"Facade\")\npackage p\nfun work(value: Int) = value\n";
    let caller = "package q\nimport p.work as invokeWork\nfun use(invokeWork: () -> Int) = invokeWork()\nfun named() = invokeWork(value = 1)\nfun normal() = invokeWork()\n";
    let targets = BTreeMap::from([
        (PathBuf::from("Functions.kt"), target.to_owned()),
        (PathBuf::from("Caller.kt"), caller.to_owned()),
    ]);
    let plans = vec![java_owned_plan("Functions.kt", target)];
    let mut residual = targets.clone();
    let report = repair_virtual_calls(&targets, &plans, &mut residual);
    let output = &residual[&PathBuf::from("Caller.kt")];
    assert!(output.contains("import p.Facade.work as invokeWork"));
    assert!(output.contains("invokeWork: () -> Int) = invokeWork()"));
    assert!(output.contains("invokeWork(value = 1)"));
    assert!(output.contains("p.Facade.work()"));
    assert!(report.requires_replan);
    assert!(report.diagnostics.iter().any(|d| d.code == "C002"));
}

#[test]
fn local_call_shadowing_obeys_initializer_and_lexical_scope() {
    let target = "package p\nfun singletonNull() = \"top\"\n";
    let caller = "package q\nimport p.singletonNull\nfun initializer() {\n val singletonNull = singletonNull()\n}\nfun previousInitializer() {\n val previous = singletonNull()\n singletonNull()\n}\nfun earlierLocalFunction() {\n fun singletonNull() = \"local\"\n singletonNull()\n}\nfun laterLocalFunction() {\n singletonNull()\n fun singletonNull() = \"local\"\n singletonNull()\n}\nfun siblingBlock() {\n if (true) { val singletonNull = { \"local\" } }\n singletonNull()\n}\nfun nestedBlock() {\n val inner = run { val singletonNull = { \"local\" }; singletonNull() }\n singletonNull()\n}\nfun parameter(singletonNull: () -> Unit) = singletonNull()\nfun lambdaParameter() = ({ \"local\" }).let { singletonNull -> singletonNull() }\nfun loop(xs: List<() -> Unit>) { for (singletonNull in xs) singletonNull() }\nfun destructured() { val (singletonNull, other) = Pair({ \"local\" }, 1); singletonNull() }\n";
    let targets = BTreeMap::from([
        (PathBuf::from("Functions.kt"), target.to_owned()),
        (PathBuf::from("Caller.kt"), caller.to_owned()),
    ]);
    let plans = vec![java_owned_plan("Functions.kt", target)];
    let mut residual = targets.clone();
    let report = repair_virtual_calls(&targets, &plans, &mut residual);
    assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);
    let output = &residual[&PathBuf::from("Caller.kt")];
    assert!(
        output.contains("val singletonNull = p.Functions.singletonNull()"),
        "the initializer call must resolve before its own local binding: {output}"
    );
    assert!(
        output.contains("fun previousInitializer() {\n val previous = p.Functions.singletonNull()\n p.Functions.singletonNull()"),
        "a prior initializer's RHS call is not a binding for later calls: {output}"
    );
    assert!(
        output.contains(
            "fun earlierLocalFunction() {\n fun singletonNull() = \"local\"\n singletonNull()"
        ),
        "a preceding local function shadows the imported function: {output}"
    );
    assert!(
        output.contains(
            "fun laterLocalFunction() {\n p.Functions.singletonNull()\n fun singletonNull()"
        ),
        "Kotlin local functions do not shadow calls before their declaration: {output}"
    );
    assert!(
        output.contains("fun siblingBlock() {\n if (true) { val singletonNull = { \"local\" } }\n p.Functions.singletonNull()"),
        "a binding in a nested sibling block does not escape: {output}"
    );
    assert!(
        output.contains("val inner = run { val singletonNull = { \"local\" }; singletonNull() }\n p.Functions.singletonNull()"),
        "nested lambda bindings shadow only within their lambda: {output}"
    );
    assert!(output.contains("fun parameter(singletonNull: () -> Unit) = singletonNull()"));
    assert!(
        output.contains("({ \"local\" }).let { singletonNull -> singletonNull() }"),
        "a lambda parameter shadows the imported function inside its lambda: {output}"
    );
    assert!(output.contains("run { val singletonNull = { \"local\" }; singletonNull() }"));
    assert!(output.contains("for (singletonNull in xs) singletonNull()"));
    assert!(
        output.contains("val (singletonNull, other) = Pair({ \"local\" }, 1); singletonNull()")
    );
}

#[test]
fn package_mismatch_and_overloads_are_not_guessed() {
    let one = "package p\nfun work(value: Int) = value\n";
    let two = "package p\nfun work(value: String) = value\n";
    let caller = "package q\nimport p.work\nfun use() = work(1)\n";
    let targets = BTreeMap::from([
        (PathBuf::from("One.kt"), one.to_owned()),
        (PathBuf::from("Two.kt"), two.to_owned()),
        (PathBuf::from("Caller.kt"), caller.to_owned()),
    ]);
    let plans = vec![
        java_owned_plan("One.kt", one),
        java_owned_plan("Two.kt", two),
    ];
    let mut residual = targets.clone();
    let report = repair_virtual_calls(&targets, &plans, &mut residual);
    assert_eq!(residual[&PathBuf::from("Caller.kt")], caller);
    assert!(report.edits.is_empty());
}

#[test]
fn changed_residual_snapshot_is_used_and_edit_spans_match_it() {
    let target = "package p\nfun work() = 1\n";
    let original = "package p\nfun use() = work()\n";
    let current = "package p\n// prior speculative repair\nfun use() = work()\n";
    let sources = BTreeMap::from([
        (PathBuf::from("Fns.kt"), target.to_owned()),
        (PathBuf::from("Use.kt"), original.to_owned()),
    ]);
    let mut residual = BTreeMap::from([
        (PathBuf::from("Fns.kt"), target.to_owned()),
        (PathBuf::from("Use.kt"), current.to_owned()),
    ]);
    let plans = vec![java_owned_plan("Fns.kt", target)];
    let report = repair_virtual_calls(&sources, &plans, &mut residual);
    let edit = report
        .edits
        .iter()
        .find(|e| e.location.file == *"Use.kt")
        .unwrap();
    assert_eq!(
        edit.location.snapshot_hash,
        *blake3::hash(current.as_bytes()).as_bytes()
    );
    assert!(residual[&PathBuf::from("Use.kt")].contains("p.Fns.work()"));
}

#[test]
fn facade_calls_are_rewritten_only_for_accepted_emitted_function_identities() {
    let target = "package boundary\nfun readRootId(value: RootEvent) = value.id.text()\nfun readDisplay(value: BroadEvent<Id>) = value.display\n";
    let caller = "package boundary\nfun run(root: RootEvent, broad: BroadEvent<Id>) = \"${readRootId(root)}:${readDisplay(broad)}\"\n";
    let sources = BTreeMap::from([
        (PathBuf::from("Callers.kt"), target.to_owned()),
        (PathBuf::from("Runner.kt"), caller.to_owned()),
    ]);
    let plan = java_owned_plan("Callers.kt", target);
    let emitted_root_id = plan
        .declarations
        .iter()
        .find(|decision| decision.symbol_id.name == "readRootId")
        .unwrap()
        .symbol_id
        .clone();
    let mut residual = sources.clone();
    let report = repair_virtual_calls_with_emitted_owners(
        &sources,
        &[plan],
        &mut residual,
        &[emitted_root_id],
    );
    let output = &residual[&PathBuf::from("Runner.kt")];
    assert!(output.contains("Callers.readRootId(root)"));
    assert!(output.contains("readDisplay(broad)"));
    assert!(!output.contains("Callers.readDisplay"));
    assert_eq!(report.repairs.len(), 1);
}

#[test]
fn unsupported_named_callable_and_java_facade_references_keep_target_kotlin() {
    let target = "package p\nfun work(value: Int) = value\n";
    let target_plan = java_owned_plan("Functions.kt", target);
    let symbol = target_plan.declarations[0].symbol_id.clone();
    let caller = "package q\nimport p.work\nfun named() = work(value = 1)\nval action = ::work\n";
    let java = "class Uses { int call() { return FunctionsKt.work(1); } }\n";
    let sources = BTreeMap::from([
        (PathBuf::from("Functions.kt"), target.to_owned()),
        (PathBuf::from("Caller.kt"), caller.to_owned()),
        (PathBuf::from("Uses.java"), java.to_owned()),
    ]);
    let retained = functions_requiring_kotlin_retention(&sources, std::slice::from_ref(&symbol));
    assert!(
        retained.contains_key(&symbol),
        "unsupported references were not surfaced"
    );
    let diagnostics = &retained[&symbol];
    assert!(diagnostics.iter().any(|d| d.code == "C002"));
    assert!(diagnostics.iter().any(|d| d.code == "C007"));
    assert!(diagnostics.iter().any(|d| d.code == "C003"));
}

#[test]
fn qualified_call_retention_checks_only_the_callee_selector() {
    let target = "package p\nfun label() = \"ok\"\n";
    let plan = java_owned_plan("Labels.kt", target);
    let symbol = plan.declarations[0].symbol_id.clone();
    let caller = "package q\nimport p.label\nfun use() = println(\"Device.Companion.label\") + Device.Companion.create(\"label\")\n";
    let sources = BTreeMap::from([
        (PathBuf::from("Labels.kt"), target.to_owned()),
        (PathBuf::from("Use.kt"), caller.to_owned()),
    ]);
    let retained = functions_requiring_kotlin_retention(&sources, &[symbol]);
    assert!(
        retained.is_empty(),
        "a string argument was mistaken for a qualified call: {retained:?}"
    );
}

#[test]
fn java_facade_references_are_collected_once_and_match_multiple_targets() {
    let one = "package p\nfun alpha() = 1\n";
    let two = "package p\nfun beta() = 2\n";
    let sources = BTreeMap::from([
        (PathBuf::from("One.kt"), one.to_owned()),
        (PathBuf::from("Two.kt"), two.to_owned()),
        (
            PathBuf::from("Use.java"),
            "class Use { int value() { return OneKt.alpha() + TwoKt.beta(); } }\n".to_owned(),
        ),
    ]);
    let provider = SyntaxSemanticProvider::new(
        sources
            .iter()
            .map(|(path, source)| (path.clone(), source.clone())),
    );
    let targets = provider
        .symbols()
        .iter()
        .filter(|symbol| symbol.id.kind == "function")
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    let shared = functions_requiring_kotlin_retention_with_provider(&sources, &targets, &provider);
    let wrapper = functions_requiring_kotlin_retention(&sources, &targets);
    assert_eq!(shared, wrapper);
    assert_eq!(shared.len(), 2);
    assert!(
        shared
            .values()
            .all(|diagnostics| diagnostics.iter().any(|d| d.code == "C003"))
    );
}

#[test]
fn java_facade_scan_preserves_conservative_comment_and_literal_matches() {
    let target = "package p\nfun work() = 1\n";
    let java = "class Use { String text = \"FunctionsKt.work()\"; // FunctionsKt.work()\n }\n";
    let sources = BTreeMap::from([
        (PathBuf::from("Functions.kt"), target.to_owned()),
        (PathBuf::from("Use.java"), java.to_owned()),
    ]);
    let provider = SyntaxSemanticProvider::new(
        sources
            .iter()
            .map(|(path, source)| (path.clone(), source.clone())),
    );
    let targets = provider
        .symbols()
        .iter()
        .filter(|symbol| symbol.id.kind == "function")
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    let report = functions_requiring_kotlin_retention_with_provider(&sources, &targets, &provider);
    assert!(
        report
            .values()
            .any(|diagnostics| diagnostics.iter().any(|d| d.code == "C003"))
    );
}

#[test]
fn facade_retention_lookup_scales_across_many_candidate_names() {
    let mut sources = BTreeMap::new();
    for index in 0..64 {
        sources.insert(
            PathBuf::from(format!("Function{index}.kt")),
            format!("package p\nfun work{index}() = {index}\n"),
        );
    }
    sources.insert(
        PathBuf::from("Use.java"),
        "class Use { int value() { return Function63Kt.work63(); } }\n".to_owned(),
    );
    let provider = SyntaxSemanticProvider::new(
        sources
            .iter()
            .map(|(path, source)| (path.clone(), source.clone())),
    );
    let targets = provider
        .symbols()
        .iter()
        .filter(|symbol| symbol.id.kind == "function")
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    let report = functions_requiring_kotlin_retention_with_provider(&sources, &targets, &provider);
    assert_eq!(report.len(), 1);
    assert_eq!(report.keys().next().unwrap().name, "work63");
}
