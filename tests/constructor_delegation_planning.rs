use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider, plan_constructor_delegations};
use notlin::translation_plan::TranslationPlan;
use std::path::{Path, PathBuf};

const SOURCE: &str = r#"package chains
class Chain(val first: String, val second: String, val third: String) {
    constructor(first: String, second: String) : this(first, second, second)
    constructor(first: String) : this(first, first)
}
"#;

fn plan(source: &str, snapshots: &[(&str, &str)]) -> TranslationPlan {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "{source}");
    let provider = SyntaxSemanticProvider::new(
        snapshots
            .iter()
            .map(|(file, text)| (PathBuf::from(file), text.to_string())),
    );
    let file = Path::new("Chain.kt");
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), file);
    plan_constructor_delegations(source, &tree, file, &provider, &mut plan);
    plan
}

#[test]
fn delegation_plan_records_the_target_owner_and_exact_snapshot() {
    let plan = plan(SOURCE, &[("Chain.kt", SOURCE)]);
    assert_eq!(plan.constructor_delegations.len(), 1, "{plan:#?}");
    let fact = &plan.constructor_delegations[0];
    assert_eq!(fact.owner.name, "Chain");
    assert_eq!(fact.owner.package, "chains");
    assert_eq!(fact.owner.file, Path::new("Chain.kt"));
    assert_eq!(fact.symbol_id.kind, "constructor");
    assert_eq!(fact.symbol_id.parameters, ["String"]);
    assert_eq!(fact.target_symbol_id.parameters, ["String", "String"]);
    assert_eq!(fact.symbol_id.owner_path, ["Chain"]);
    assert_eq!(fact.location.file, fact.owner.file);
    assert_eq!(fact.target.file, fact.owner.file);
    assert_eq!(fact.location.snapshot_hash, plan.source_hash);
    assert_eq!(fact.target.snapshot_hash, plan.source_hash);
    assert_eq!(fact.parameter_types, ["String", "String"]);
    assert_eq!(
        &SOURCE[fact.location.start_byte..fact.location.end_byte],
        "constructor(first: String) : this(first, first)"
    );
    assert_eq!(
        &SOURCE[fact.target.start_byte..fact.target.end_byte],
        "constructor(first: String, second: String) : this(first, second, second)"
    );
    let encoded = serde_json::to_string(&plan).unwrap();
    assert_eq!(
        serde_json::from_str::<TranslationPlan>(&encoded).unwrap(),
        plan
    );
}

#[test]
fn stale_or_duplicate_owner_facts_cannot_select_a_constructor() {
    let changed = SOURCE.replace("this(first, first)", "this(first, \"edited\")");
    assert!(
        plan(&changed, &[("Chain.kt", SOURCE)])
            .constructor_delegations
            .is_empty()
    );
    assert!(
        plan(SOURCE, &[("Chain.kt", SOURCE), ("Chain.kt", SOURCE)])
            .constructor_delegations
            .is_empty()
    );
}

#[test]
fn unsupported_overload_and_argument_shapes_do_not_gain_a_target() {
    for source in [
        SOURCE.replace(
            "constructor(first: String) : this(first, first)",
            "constructor(first: String) : this(second = first, first = first)",
        ),
        SOURCE.replace(
            "constructor(first: String) : this(first, first)",
            "constructor(first: String) : this(first, first)\n    constructor(first: String, second: Int) : this(first, first, first)",
        ),
        SOURCE.replace("val third: String", "val third: String = \"default\""),
        SOURCE.replace("class Chain(", "class Chain<T>("),
        SOURCE.replace("second: String)", "second: String?)"),
        SOURCE.replace("second: String)", "vararg second: String)"),
    ] {
        assert!(
            plan(&source, &[("Chain.kt", &source)]).constructor_delegations.is_empty(),
            "{source}"
        );
    }
}

#[test]
fn cyclic_constructor_chains_cannot_be_accepted() {
    let source = SOURCE.replace(": this(first, second, second)", ": this(first)");
    assert!(
        plan(&source, &[("Chain.kt", &source)])
            .constructor_delegations
            .is_empty()
    );
}

#[test]
fn unknown_alias_and_changed_visibility_signatures_are_not_planned() {
    for source in [
        SOURCE.replace("String", "Missing"),
        SOURCE.replace("String", "Text").replacen(
            "package chains",
            "package chains\ntypealias Text = String",
            1,
        ),
        SOURCE.replace("package chains", "package chains\nimport opaque.*"),
        SOURCE.replace(
            "constructor(first: String)",
            "private constructor(first: String)",
        ),
        SOURCE.replace(
            "class Chain(",
            "class Chain @Throws(Exception::class) constructor(",
        ),
    ] {
        assert!(
            plan(&source, &[("Chain.kt", &source)])
                .constructor_delegations
                .is_empty(),
            "{source}"
        );
    }
}

#[test]
fn workspace_builtin_facts_require_all_contributing_snapshots() {
    let root = std::env::temp_dir().join(format!(
        "notlin-constructor-type-facts-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _scratch = Scratch(root.clone());
    std::fs::write(
        root.join("Chain.kt"),
        SOURCE.replace("package chains", "package chains\nimport support.*"),
    )
    .unwrap();
    std::fs::write(root.join("Marker.kt"), "package support\nclass Marker\n").unwrap();
    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let chain = index
        .files
        .iter()
        .find(|file| file.path.file_name().unwrap() == "Chain.kt")
        .unwrap();
    let snapshots = || {
        index
            .files
            .iter()
            .map(|file| (file.path.clone(), file.source_text().to_owned()))
            .collect::<Vec<_>>()
    };
    let mut provider = SyntaxSemanticProvider::new(snapshots());
    provider.enrich_workspace_type_facts(&index);
    assert_eq!(
        provider.builtin_jvm_type_descriptor(&chain.path, "String"),
        Some("Ljava/lang/String;")
    );
    provider.index_file(Path::new("Additional.kt"), "class String");
    provider.enrich_workspace_type_facts(&index);
    assert_eq!(
        provider.builtin_jvm_type_descriptor(&chain.path, "String"),
        None
    );

    let mut stale = snapshots();
    stale
        .iter_mut()
        .find(|(file, _)| file.file_name().unwrap() == "Marker.kt")
        .unwrap()
        .1
        .push_str("class Changed\n");
    let mut provider = SyntaxSemanticProvider::new(stale);
    provider.enrich_workspace_type_facts(&index);
    assert_eq!(
        provider.builtin_jvm_type_descriptor(&chain.path, "String"),
        None
    );

    std::fs::write(root.join("Shadow.kt"), "package support\nclass String\n").unwrap();
    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let mut provider = SyntaxSemanticProvider::new(
        index
            .files
            .iter()
            .map(|file| (file.path.clone(), file.source_text().to_owned())),
    );
    provider.enrich_workspace_type_facts(&index);
    assert_eq!(
        provider.builtin_jvm_type_descriptor(&chain.path, "String"),
        None
    );
}
