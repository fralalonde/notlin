use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider, plan_syntax_lowerings};
use std::path::{Path, PathBuf};

const TYPES: &str = "package sample\ninterface Context\nclass Entry : Context\n";
const STORE: &str = r#"package sample
class Store {
    fun <T : Context> find(type: Class<T>): T? = null
    inline fun <reified T : Context> find(): T? = find(T::class.java)
}
"#;
const CALLER: &str = "package sample\nfun read(store: Store): Entry? = store.find<Entry>()\n";

fn plan(
    store: &str,
    caller: &str,
    indexed_caller: &str,
    types: &str,
) -> notlin::translation_plan::TranslationPlan {
    let provider = SyntaxSemanticProvider::new([
        (PathBuf::from("Types.kt"), types.to_owned()),
        (PathBuf::from("Store.kt"), store.to_owned()),
        (PathBuf::from("Caller.kt"), indexed_caller.to_owned()),
    ]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    for source in [store, caller, types] {
        assert!(
            !parser.parse(source, None).unwrap().root_node().has_error(),
            "invalid fixture: {source}"
        );
    }
    let tree = parser.parse(caller, None).unwrap();
    let path = Path::new("Caller.kt");
    let mut plan = notlin::translation_plan::analyze(caller, tree.root_node(), path);
    plan_syntax_lowerings(caller, &tree, path, &provider, &mut plan);
    assert!(provider.source_for_file(path).is_some());
    plan
}

#[test]
fn reified_fact_records_the_source_and_java_targets_and_nullable_substitution() {
    let plan = plan(STORE, CALLER, CALLER, TYPES);
    assert_eq!(plan.reified_class_calls.len(), 1);
    let fact = &plan.reified_class_calls[0];
    assert!(fact.source_target.parameters.is_empty());
    assert_eq!(fact.java_target.parameters, ["Class<T>"]);
    assert_eq!(fact.type_argument.name, "Entry");
    assert!(fact.return_type.nullable);
    assert_eq!(
        &CALLER[fact.receiver.start_byte..fact.receiver.end_byte],
        "store"
    );
    assert_eq!(
        &CALLER[fact.location.start_byte..fact.location.end_byte],
        "store.find<Entry>()"
    );
    assert_eq!(
        fact.location.snapshot_hash,
        *blake3::hash(CALLER.as_bytes()).as_bytes()
    );
    assert_eq!(fact.receiver.snapshot_hash, fact.location.snapshot_hash);
    let decoded: notlin::semantics::ReifiedClassCall =
        serde_json::from_str(&serde_json::to_string(fact).unwrap()).unwrap();
    assert_eq!(*fact, decoded);
    for target in [&fact.source_target, &fact.java_target, &fact.type_argument] {
        assert!(plan.dependencies.iter().any(|dependency| matches!(&dependency.resolution, notlin::semantics::FactStatus::Established(symbol) if symbol == target)));
    }
}

#[test]
fn reified_facts_reject_non_forwarders_and_unproven_type_bindings() {
    for store in [
        STORE.replace(
            "find(T::class.java)",
            "run { println(\"effect\"); find(T::class.java) }",
        ),
        STORE.replace("inline fun", "private inline fun"),
        STORE.replace("inline fun", "@kotlin.jvm.JvmName(\"renamed\") inline fun"),
        STORE.replace("find(type: Class<T>): T?", "find(type: Class<T>): String?"),
        STORE.replace("package sample", "package sample\nimport unknown.library.*"),
        STORE.replace("package sample", "package sample\nimport custom.java"),
        STORE.replace("inline fun", "suspend inline fun"),
    ] {
        assert!(
            plan(&store, CALLER, CALLER, TYPES)
                .reified_class_calls
                .is_empty(),
            "{store}"
        );
    }
    let alias = "package sample\ntypealias Alias = Entry\nfun read(store: Store): Entry? = store.find<Alias>()\n";
    assert!(
        plan(STORE, alias, alias, TYPES)
            .reified_class_calls
            .is_empty()
    );
    let nullable_receiver = CALLER
        .replace("store: Store", "store: Store?")
        .replace("store.find", "store?.find");
    assert!(
        plan(STORE, &nullable_receiver, &nullable_receiver, TYPES)
            .reified_class_calls
            .is_empty()
    );
    let changed = format!("{CALLER}\n");
    assert!(
        plan(STORE, &changed, CALLER, TYPES)
            .reified_class_calls
            .is_empty()
    );
    let safe = CALLER.replace("store.find", "store?.find");
    assert!(
        plan(STORE, &safe, &safe, TYPES)
            .reified_class_calls
            .is_empty()
    );
    let fake_package = format!("/*\npackage other\n*/\n{CALLER}");
    assert!(
        plan(STORE, &fake_package, &fake_package, TYPES)
            .reified_class_calls
            .is_empty()
    );
}

#[test]
fn class_token_forwarding_excludes_only_a_proven_enum_alternative() {
    let enum_overload = STORE.replace(
        "class Store {",
        "enum class Kind { ENTRY }\nclass Store {\nfun <T : Context> find(type: Kind): T? = null",
    );
    assert_eq!(
        plan(&enum_overload, CALLER, CALLER, TYPES)
            .reified_class_calls
            .len(),
        1
    );
    let unknown_overload = STORE.replace(
        "class Store {",
        "class Store {\nfun <T : Context> find(type: Any): T? = null",
    );
    assert!(
        plan(&unknown_overload, CALLER, CALLER, TYPES)
            .reified_class_calls
            .is_empty()
    );
    let unrelated = TYPES.replace("class Entry : Context", "class Entry");
    assert!(
        plan(STORE, CALLER, CALLER, &unrelated)
            .reified_class_calls
            .is_empty()
    );
}

#[test]
fn literal_and_return_must_bind_to_the_reified_parameter_not_a_source_classifier() {
    let unrelated_parameter = STORE
        .replace("<T : Context>", "<U : Context>")
        .replace("<reified T : Context>", "<reified U : Context>")
        .replace("Class<T>", "Class<U>");
    let types = format!("{TYPES}\nclass T : Context\n");
    let caller = CALLER.replace("Entry?", "Context?");
    assert!(
        plan(&unrelated_parameter, &caller, &caller, &types)
            .reified_class_calls
            .is_empty()
    );
    let renamed = STORE
        .replace(
            "<T : Context> find(type: Class<T>): T?",
            "<Value : Context> find(token: Class<Value>): Value?",
        )
        .replace(
            "<reified T : Context> find(): T? = find(T::class.java)",
            "<reified Result : Context> find(): Result? = find(Result::class.java)",
        );
    assert_eq!(
        plan(&renamed, CALLER, CALLER, TYPES)
            .reified_class_calls
            .len(),
        1
    );
}

#[test]
fn property_receivers_require_a_proven_getter_abi_and_record_its_dependency() {
    let caller = "package sample\nclass Reader(val store: Store) {\nfun read(): Entry? = store.find<Entry>()\nfun explicit(): Entry? = this.store.find<Entry>()\n}\n";
    let accepted = plan(STORE, caller, caller, TYPES);
    assert_eq!(accepted.reified_class_calls.len(), 2);
    assert!(
        accepted
            .reified_class_calls
            .iter()
            .all(|fact| fact.receiver_dependencies.len() == 1
                && fact.receiver_dependencies[0].name == "store")
    );
    let renamed = caller.replace(
        "val store: Store",
        "@get:kotlin.jvm.JvmName(\"renamed\") val store: Store",
    );
    assert!(
        plan(STORE, &renamed, &renamed, TYPES)
            .reified_class_calls
            .is_empty()
    );
}
