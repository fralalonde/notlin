use notlin::semantics::{
    FactStatus, ReferenceStringCallKind, SemanticProvider, SyntaxSemanticProvider,
    plan_required_calls, plan_syntax_lowerings,
};
use std::path::{Path, PathBuf};

fn indexed(files: &[(&str, &str)]) -> SyntaxSemanticProvider {
    for (path, source) in files {
        let extension = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str());
        let language = match extension {
            Some("kt" | "kts") => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Some("java") => tree_sitter_java::LANGUAGE.into(),
            _ => continue,
        };
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
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

fn required_calls(
    source: &str,
    file: &str,
    provider: &SyntaxSemanticProvider,
) -> Vec<(String, FactStatus<notlin::semantics::SymbolId>)> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture:\n{source}");
    let path = Path::new(file);
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), path);
    plan_required_calls(source, &tree, path, provider, false, &mut plan);
    plan.dependencies
        .into_iter()
        .map(|dependency| (dependency.spelling, dependency.resolution))
        .collect()
}

fn reference_string_plan(
    source: &str,
    file: &str,
    provider: &SyntaxSemanticProvider,
) -> notlin::translation_plan::TranslationPlan {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture:\n{source}");
    let path = Path::new(file);
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), path);
    plan_syntax_lowerings(source, &tree, path, provider, &mut plan);
    plan
}

fn no_to_string_dependency(source: &str, file: &str, provider: &SyntaxSemanticProvider) {
    let calls = required_calls(source, file, provider);
    assert!(
        !calls.iter().any(|(name, _)| name == "toString"),
        "expected Object.toString result fact, but found dependency: {calls:#?}"
    );
}

#[test]
fn canonical_object_parameters_record_external_dispatch_without_synthetic_symbols() {
    for (imports, ty, canonical) in [
        ("", "Any", "kotlin.Any"),
        ("", "kotlin.Any", "kotlin.Any"),
        ("import kotlin.Any\nimport unknown.*", "Any", "kotlin.Any"),
    ] {
        let source = format!(
            "package reference\n{imports}\nfun render(value: {ty}): String = value.toString()\n"
        );
        let provider = indexed(&[("Render.kt", &source)]);
        no_to_string_dependency(&source, "Render.kt", &provider);
        let plan = reference_string_plan(&source, "Render.kt", &provider);
        assert_eq!(plan.reference_string_calls.len(), 1, "{source}: {plan:#?}");
        let notlin::semantics::ReferenceStringReceiver::Standard {
            canonical_type,
            expression,
        } = &plan.reference_string_calls[0].receiver
        else {
            panic!("{plan:#?}")
        };
        assert_eq!(canonical_type, canonical);
        assert_eq!(&source[expression.start_byte..expression.end_byte], "value");
        assert_eq!(expression.snapshot_hash, plan.source_hash);
        assert!(
            plan.dependencies.is_empty(),
            "external classifiers have no source SymbolId: {plan:#?}"
        );
        let json = serde_json::to_string(&plan.reference_string_calls[0]).unwrap();
        let restored: notlin::semantics::ReferenceStringCall = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, plan.reference_string_calls[0]);
    }
}

#[test]
fn canonical_object_proof_rejects_nullable_shadowed_ambiguous_and_stale_types() {
    for source in [
        "package reference\nfun render(value: Any?): String = value.toString()",
        "package reference\nfun render(value: Any): String { if (value is java.lang.Object) return value.toString(); return \"other\" }",
        "package reference\nfun render(value: Any): String { value as java.lang.Object; return value.toString() }",
        "package reference\nfun render(value: Any): String = when(value) { is java.lang.Object -> value.toString(); else -> \"other\" }",
        "package reference\nfun narrow(value: Any): Boolean = true\nfun render(value: Any): String { if (narrow(value)) return value.toString(); return \"other\" }",
        "package reference\nfun render(value: java.lang.Object): String = value.toString()",
        "package reference\nimport java.lang.Object\nfun render(value: Object): String = value.toString()",
        "package reference\nfun <Any> render(value: Any): String = value.toString()",
        "package reference\nimport other.Any\nfun render(value: Any): String = value.toString()",
        "package reference\nimport other.Type as Any\nfun render(value: Any): String = value.toString()",
        "package reference\nimport unknown.*\nfun render(value: Any): String = value.toString()",
        "package reference\n/*\nimport kotlin.Any\n*/\nimport unknown.*\nfun render(value: Any): String = value.toString()",
        "/*\npackage falsepackage\n*/\npackage reference\nfun render(value: Any): String = value.toString()",
        "package reference\ntypealias Any = kotlin.Any?\nfun render(value: Any): String = value.toString()",
    ] {
        let provider = indexed(&[("Reject.kt", source)]);
        let plan = reference_string_plan(source, "Reject.kt", &provider);
        assert!(
            plan.reference_string_calls.is_empty(),
            "{source}: {plan:#?}"
        );
    }
    let source = "package reference\nfun render(value: Any): String = value.toString()";
    let provider = indexed(&[("Stale.kt", source)]);
    let changed = source.replace("value: Any", "value: Any?");
    assert!(
        reference_string_plan(&changed, "Stale.kt", &provider)
            .reference_string_calls
            .is_empty()
    );
}

#[test]
fn nonnull_object_member_wins_over_extensions_and_custom_any_keeps_source_identity() {
    let source = "package reference\nfun Any.toString(): String = \"extension\"\nfun render(value: Any): String = value.toString()";
    let provider = indexed(&[("Member.kt", source)]);
    no_to_string_dependency(source, "Member.kt", &provider);
    let source = "package reference\nclass Any\nfun render(value: Any): String = value.toString()";
    let provider = indexed(&[("Shadow.kt", source)]);
    let plan = reference_string_plan(source, "Shadow.kt", &provider);
    assert_eq!(plan.reference_string_calls.len(), 1);
    assert!(
        matches!(&plan.reference_string_calls[0].receiver, notlin::semantics::ReferenceStringReceiver::Indexed(id) if id.package == "reference" && id.name == "Any")
    );
}

fn todo_string_dependency(source: &str, file: &str, provider: &SyntaxSemanticProvider) {
    let calls = required_calls(source, file, provider);
    assert!(
        calls.iter().any(|(name, _)| name == "toString"),
        "expected unproven toString call to remain required: {calls:#?}"
    );
}

#[test]
fn indexed_nonnull_interface_parameters_prove_to_string_return_type() {
    let file = "ObjectString.kt";
    let source = r#"package objectstring
interface EntityId
fun render(value: EntityId): String = value.toString()
"#;
    let provider = indexed(&[(file, source)]);
    no_to_string_dependency(source, file, &provider);
}

#[test]
fn uppercase_named_parameters_are_instance_receivers_not_classifiers() {
    let file = "ObjectStringUppercaseParameter.kt";
    let source = r#"package objectstring
class EntityId
fun render(EntityId: EntityId): String = EntityId.toString()
"#;
    let provider = indexed(&[(file, source)]);
    no_to_string_dependency(source, file, &provider);
}

#[test]
fn secondary_constructor_parameters_and_generic_reference_parameters_are_supported() {
    let file = "ObjectStringParams.kt";
    let source = r#"package objectstring
interface EntityId
class Holder {
    constructor(value: EntityId) {
        val text = value.toString()
    }
}
class Box<T>
fun render(box: Box<String>): String = box.toString()
"#;
    let provider = indexed(&[(file, source)]);
    no_to_string_dependency(source, file, &provider);
}

#[test]
fn directly_typed_properties_prove_reference_to_string() {
    let file = "ObjectStringProperty.kt";
    let source = r#"package objectstring
class EntityId
class Holder(val own: EntityId) {
    fun render(): String = own.toString()
    fun renderThis(): String = this.own.toString()
}
fun render(holder: Holder): String = holder.own.toString()
"#;
    let provider = indexed(&[(file, source)]);
    no_to_string_dependency(source, file, &provider);
}

#[test]
fn string_result_fact_composes_with_following_string_calls() {
    let file = "ObjectStringChain.kt";
    let source = r#"package objectstring
class EntityId
fun render(value: EntityId): String = value.toString().lowercase()
"#;
    let provider = indexed(&[(file, source)]);
    let calls = required_calls(source, file, &provider);
    assert!(
        calls.is_empty(),
        "the proven String result should support the following lowercase call: {calls:#?}"
    );
}

#[test]
fn reference_string_plan_records_exact_locations_receiver_identity_and_dependencies() {
    let file = "ReferenceStringPlan.kt";
    let source = r#"package objectstring
class EntityId
fun render(value: EntityId): String = value.toString()
fun renderLowercase(value: EntityId): String = value.toString().lowercase()
"#;
    let provider = indexed(&[(file, source)]);
    let plan = reference_string_plan(source, file, &provider);
    assert_eq!(plan.reference_string_calls.len(), 3);

    let expected_owner = provider
        .symbols_named("EntityId")
        .into_iter()
        .find(|symbol| symbol.id.kind == "class")
        .expect("fixture classifier should be indexed")
        .id
        .clone();
    let mut facts = plan.reference_string_calls.clone();
    facts.sort_by_key(|fact| (fact.location.start_byte, fact.location.end_byte));
    let expected = [
        ("value.toString()", ReferenceStringCallKind::ObjectToString),
        ("value.toString()", ReferenceStringCallKind::ObjectToString),
        (
            "value.toString().lowercase()",
            ReferenceStringCallKind::InvariantLowercase,
        ),
    ];
    let snapshot_hash = *blake3::hash(source.as_bytes()).as_bytes();
    for (fact, (spelling, kind)) in facts.iter().zip(expected) {
        assert_eq!(fact.location.file, PathBuf::from(file));
        assert_eq!(fact.location.snapshot_hash, snapshot_hash);
        assert_eq!(
            &source[fact.location.start_byte..fact.location.end_byte],
            spelling
        );
        assert_eq!(
            fact.receiver,
            notlin::semantics::ReferenceStringReceiver::Indexed(expected_owner.clone())
        );
        assert_eq!(fact.kind, kind);
    }
    assert!(plan.dependencies.iter().any(|dependency| {
        dependency.spelling == "EntityId"
            && dependency.resolution == FactStatus::Established(expected_owner.clone())
    }));
    assert_eq!(plan.dependencies.len(), 3, "{:#?}", plan.dependencies);
    assert!(plan.dependencies.iter().all(|dependency| {
        dependency.spelling == "EntityId"
            && dependency.resolution == FactStatus::Established(expected_owner.clone())
    }));
}

#[test]
fn returned_reference_facts_keep_producer_identity_and_platform_dispatch_separate() {
    let file = "Returned.kt";
    let source = "package objectstring\ndata class Snapshot(val value: String) {\n    constructor(provider: Provider): this(provider.readId().toString())\n}\n";
    let java = "package objectstring;\nimport java.util.UUID;\npublic interface Provider { UUID readId(); }";
    let provider = indexed(&[(file, source), ("Provider.java", java)]);
    let plan = reference_string_plan(source, file, &provider);
    assert_eq!(plan.reference_string_calls.len(), 1, "{plan:#?}");
    let notlin::semantics::ReferenceStringReceiver::Returned {
        producer,
        producer_location,
        expression,
        canonical_type,
        nullability,
    } = &plan.reference_string_calls[0].receiver
    else {
        panic!("expected a producer fact");
    };
    assert_eq!(producer.name, "readId");
    assert_eq!(producer.file, PathBuf::from("Provider.java"));
    assert_eq!(
        producer_location.snapshot_hash,
        *blake3::hash(java.as_bytes()).as_bytes()
    );
    assert_eq!(
        &source[expression.start_byte..expression.end_byte],
        "provider.readId()"
    );
    assert_eq!(canonical_type, "java.util.UUID");
    assert_eq!(
        *nullability,
        notlin::semantics::ReferenceReturnNullability::Platform
    );
    assert!(
        plan.dependencies
            .iter()
            .all(|dependency| dependency.resolution == FactStatus::Established(producer.clone()))
    );
    no_to_string_dependency(source, file, &provider);
}

#[test]
fn nullable_reference_facts_reject_extensions_unknown_stars_and_qualifier_shadows() {
    let java = "package objectstring;\nimport java.util.UUID;\npublic interface Provider { @org.jetbrains.annotations.Nullable UUID readId(); }";
    for source in [
        "package objectstring\ndata class Snapshot(val value: String) {\n    constructor(provider: Provider, java: Any): this(provider.readId().toString())\n}\n",
        "package objectstring\nclass Snapshot<java>(val value: String) {\n    constructor(provider: Provider): this(provider.readId().toString())\n}\n",
        "package objectstring\nimport unknown.*\ndata class Snapshot(val value: String) {\n    constructor(provider: Provider): this(provider.readId().toString())\n}\n",
        "package objectstring\nimport unknown.toString\ndata class Snapshot(val value: String) {\n    constructor(provider: Provider): this(provider.readId().toString())\n}\n",
        "package objectstring\nfun java.util.UUID?.toString(): String = \"custom\"\ndata class Snapshot(val value: String) {\n    constructor(provider: Provider): this(provider.readId().toString())\n}\n",
    ] {
        let provider = indexed(&[("Returned.kt", source), ("Provider.java", java)]);
        let plan = reference_string_plan(source, "Returned.kt", &provider);
        assert!(
            plan.reference_string_calls.is_empty(),
            "{source}\n{plan:#?}"
        );
    }
}

#[test]
fn direct_reference_members_keep_priority_over_caller_imports_extensions_and_java_shadows() {
    for annotation in ["", "@org.jetbrains.annotations.NotNull"] {
        let java = format!(
            "package objectstring;\nimport java.util.UUID;\npublic interface Provider {{ {annotation} UUID readId(); }}"
        );
        for source in [
            "package objectstring\nimport unknown.*\ndata class Snapshot(val value: String) {\n constructor(provider: Provider): this(provider.readId().toString())\n}\n",
            "package objectstring\nimport unknown.toString\ndata class Snapshot(val value: String) {\n constructor(provider: Provider): this(provider.readId().toString())\n}\n",
            "package objectstring\nfun java.util.UUID?.toString(): String = \"custom\"\ndata class Snapshot(val value: String) {\n constructor(provider: Provider): this(provider.readId().toString())\n}\n",
            "package objectstring\ndata class Snapshot(val value: String) {\n constructor(provider: Provider, java: Any): this(provider.readId().toString())\n}\n",
            "package objectstring\nclass Snapshot<java>(val value: String) {\n constructor(provider: Provider): this(provider.readId().toString())\n}\n",
        ] {
            let provider = indexed(&[("Returned.kt", source), ("Provider.java", &java)]);
            let plan = reference_string_plan(source, "Returned.kt", &provider);
            assert_eq!(
                plan.reference_string_calls.len(),
                1,
                "{annotation}: {source}\n{plan:#?}"
            );
            assert!(matches!(
                &plan.reference_string_calls[0].receiver,
                notlin::semantics::ReferenceStringReceiver::Returned { .. }
            ));
        }
    }
}

#[test]
fn direct_reference_members_still_need_the_producers_import_scope() {
    let source = "package objectstring\nimport java.util.UUID\ndata class Snapshot(val value: String) {\n constructor(provider: Provider): this(provider.readId().toString())\n}\n";
    for java in [
        "package objectstring;\nimport unknown.*;\npublic interface Provider { UUID readId(); }",
        "package objectstring;\nimport unknown.UUID;\npublic interface Provider { UUID readId(); }",
    ] {
        let provider = indexed(&[("Returned.kt", source), ("Provider.java", java)]);
        assert!(
            reference_string_plan(source, "Returned.kt", &provider)
                .reference_string_calls
                .is_empty(),
            "{java}"
        );
    }
}

#[test]
fn returned_reference_facts_do_not_generalize_constructor_dispatch_to_other_contexts() {
    let java = "package objectstring;\nimport java.util.UUID;\npublic interface Provider { UUID readId(); }";
    for expression in [
        "provider.readId().toString()",
        "\"value=\" + provider.readId().toString()",
        "\"${provider.readId().toString()}\"",
    ] {
        let source = format!(
            "package objectstring\nfun render(provider: Provider): String = {expression}\n"
        );
        let provider = indexed(&[("Returned.kt", &source), ("Provider.java", java)]);
        assert!(
            reference_string_plan(&source, "Returned.kt", &provider)
                .reference_string_calls
                .is_empty(),
            "{expression}"
        );
    }
}

#[test]
fn nullable_returned_reference_fact_keeps_the_enhanced_return_contract() {
    let source = "package objectstring\ndata class Snapshot(val value: String) {\n constructor(provider: Provider): this(provider.readId().toString())\n}\n";
    let java = "package objectstring;\nimport java.util.UUID;\npublic interface Provider { @org.jetbrains.annotations.Nullable UUID readId(); }";
    let provider = indexed(&[("Returned.kt", source), ("Provider.java", java)]);
    let method = provider.symbols_named("readId")[0];
    assert_eq!(
        provider.reference_return_nullability(&method.id),
        Some(notlin::semantics::ReferenceReturnNullability::Nullable),
        "{method:#?}"
    );
    let plan = reference_string_plan(source, "Returned.kt", &provider);
    assert_eq!(
        plan.reference_string_calls.len(),
        1,
        "public={} callable={} {plan:#?}",
        provider.is_public_instance_callable(&method.id),
        provider.is_java_callable_member(&method.id)
    );
}

#[test]
fn arbitrary_reference_results_need_a_separate_to_string_dispatch_contract() {
    let source = "package objectstring\ndata class Snapshot(val value: String) {\n constructor(provider: Provider): this(provider.readId().toString())\n}\n";
    let provider = indexed(&[
        ("Returned.kt", source),
        (
            "Provider.java",
            "package objectstring;\npublic interface Provider { Opaque readId(); }",
        ),
        (
            "Opaque.java",
            "package objectstring;\npublic class Opaque {}",
        ),
    ]);
    assert!(
        reference_string_plan(source, "Returned.kt", &provider)
            .reference_string_calls
            .is_empty()
    );
}

#[test]
fn return_nullability_contracts_reject_defaults_exceptions_and_stale_symbols() {
    use notlin::semantics::ReferenceReturnNullability::{NonNull, Nullable, Platform};
    for (annotation, expected) in [
        ("", Some(Platform)),
        ("@NotNull", Some(NonNull)),
        ("@Nullable", Some(Nullable)),
        ("@Custom", None),
        ("@NotNull @Nullable", None),
    ] {
        let java = format!(
            "package objectstring;\nimport java.util.UUID;\nimport org.jetbrains.annotations.NotNull;\nimport org.jetbrains.annotations.Nullable;\npublic interface Provider {{ {annotation} UUID readId(); }}"
        );
        let provider = indexed(&[("Provider.java", &java)]);
        let member = &provider.symbols_named("readId")[0].id;
        assert_eq!(
            provider.reference_return_nullability(member),
            expected,
            "{annotation}"
        );
    }
    let source = "package objectstring\ndata class Snapshot(val value: String) {\n    constructor(provider: Provider): this(provider.readId().toString())\n}\n";
    for java in [
        "package objectstring;\nimport java.util.UUID;\n@NullMarked public interface Provider { UUID readId(); }",
        "package objectstring;\nimport java.util.UUID;\npublic interface Provider { UUID readId() throws java.io.IOException; }",
        "package objectstring;\npublic interface Provider { <T> T readId(); }",
        "package objectstring;\npublic interface Provider { java.util.@org.jetbrains.annotations.NotNull UUID readId(); }",
    ] {
        let provider = indexed(&[("Returned.kt", source), ("Provider.java", java)]);
        assert!(
            reference_string_plan(source, "Returned.kt", &provider)
                .reference_string_calls
                .is_empty(),
            "{java}"
        );
    }
    let java = "package objectstring;\nimport java.util.UUID;\npublic interface Provider { UUID readId(); }";
    let provider = indexed(&[
        ("Returned.kt", source),
        ("Provider.java", java),
        (
            "package-info.java",
            "/* package decoy */\n@NullMarked package objectstring;\n",
        ),
    ]);
    let member = &provider.symbols_named("readId")[0].id;
    assert_eq!(provider.reference_return_nullability(member), None);
    let module_provider = indexed(&[
        ("Returned.kt", source),
        ("Provider.java", java),
        ("module-info.java", "@NullMarked module sample { }"),
    ]);
    let member = &module_provider.symbols_named("readId")[0].id;
    assert_eq!(module_provider.reference_return_nullability(member), None);
    let mut stale = indexed(&[("Returned.kt", source), ("Provider.java", java)]);
    stale.index_file(
        Path::new("Provider.java"),
        &java.replace("UUID readId()", "String readId()"),
    );
    assert!(
        reference_string_plan(source, "Returned.kt", &stale)
            .reference_string_calls
            .is_empty()
    );
    let duplicate = indexed(&[
        ("Returned.kt", source),
        ("Provider.java", java),
        ("Provider.java", java),
    ]);
    assert!(
        reference_string_plan(source, "Returned.kt", &duplicate)
            .reference_string_calls
            .is_empty()
    );
}

#[test]
fn return_nullability_contracts_follow_overridden_slots() {
    use notlin::semantics::ReferenceReturnNullability::{NonNull, Nullable, Platform};
    let source = "package objectstring\ndata class Snapshot(val value: String) {\n constructor(provider: Provider): this(provider.readId().toString())\n}\n";
    for (direct, parent, grandparent, expected) in [
        ("", "", "", Some(Platform)),
        ("@NotNull", "", "", Some(NonNull)),
        ("@Nullable", "", "", Some(Nullable)),
        ("@NotNull", "@Nullable", "@Nullable", Some(NonNull)),
        ("@Nullable", "@NotNull", "@NotNull", None),
        ("", "@Nullable", "@Nullable", None),
        ("", "@NotNull", "@NotNull", None),
        ("", "", "@NotNull", None),
        ("", "", "@Nullable", None),
    ] {
        let java = format!(
            "package objectstring;\nimport java.util.UUID;\nimport org.jetbrains.annotations.NotNull;\nimport org.jetbrains.annotations.Nullable;\ninterface Root {{ {grandparent} UUID readId(); }}\ninterface Parent extends Root {{ {parent} UUID readId(); }}\npublic interface Provider extends Parent {{ {direct} UUID readId(); }}"
        );
        let provider = indexed(&[("Returned.kt", source), ("Provider.java", &java)]);
        let member = &provider
            .symbols_named("readId")
            .into_iter()
            .find(|member| member.id.owner_path == ["Provider"])
            .unwrap()
            .id;
        assert_eq!(
            provider.reference_return_nullability(member),
            expected,
            "{java}"
        );
        assert_eq!(
            reference_string_plan(source, "Returned.kt", &provider)
                .reference_string_calls
                .len(),
            usize::from(expected.is_some()),
            "{java}"
        );
    }
}

#[test]
fn reference_string_plan_rejects_nullable_static_aliased_and_stale_receivers() {
    let cases = [
        (
            "ReferenceStringNullable.kt",
            "package objectstring\nclass EntityId\nfun render(value: EntityId?): String = value.toString()\n",
            vec![(
                "ReferenceStringNullable.kt",
                "package objectstring\nclass EntityId\nfun render(value: EntityId?): String = value.toString()\n",
            )],
        ),
        (
            "ReferenceStringStatic.kt",
            "package objectstring\nclass EntityId\nfun render(): String = EntityId.toString()\n",
            vec![(
                "ReferenceStringStatic.kt",
                "package objectstring\nclass EntityId\nfun render(): String = EntityId.toString()\n",
            )],
        ),
    ];
    for (file, source, files) in cases {
        let provider = indexed(&files);
        let plan = reference_string_plan(source, file, &provider);
        assert!(plan.reference_string_calls.is_empty(), "{file}: {plan:#?}");
        assert!(
            plan.dependencies
                .iter()
                .all(|dependency| dependency.spelling != "EntityId")
        );
    }

    let model = "package model\nclass EntityId\n";
    let alias_file = "ReferenceStringAlias.kt";
    let alias_source = "package objectstring\nimport model.EntityId as EntityAlias\nfun render(value: EntityAlias): String = value.toString()\n";
    let alias_provider = indexed(&[("EntityId.kt", model), (alias_file, alias_source)]);
    let alias_plan = reference_string_plan(alias_source, alias_file, &alias_provider);
    assert!(
        alias_plan.reference_string_calls.is_empty(),
        "{alias_plan:#?}"
    );

    let fresh = "package objectstring\nclass EntityId\nfun render(value: EntityId): String = value.toString()\n";
    let stale_provider = indexed(&[("ReferenceStringStale.kt", fresh)]);
    let changed = fresh.replace("EntityId", "DifferentId");
    let stale_plan = reference_string_plan(&changed, "ReferenceStringStale.kt", &stale_provider);
    assert!(
        stale_plan.reference_string_calls.is_empty(),
        "{stale_plan:#?}"
    );
}

#[test]
fn delegation_chain_has_a_separate_accepted_call_fact() {
    let file = "ConstructorChain.kt";
    let source = "package objectstring\nclass Display private constructor(private val text: String) {\n constructor(label: Label) : this(label.toString().lowercase())\n fun render(): String = text\n}\n";
    let label = "package objectstring\ninterface Label {\n suspend fun retainedHook()\n}\n";
    let provider = indexed(&[(file, source), ("Label.kt", label)]);
    let plan = reference_string_plan(source, file, &provider);
    assert_eq!(plan.reference_string_calls.len(), 2, "{plan:#?}");
}

#[test]
fn inherited_properties_and_indexed_lowercase_extensions_do_not_gain_facts() {
    let inherited_file = "ReferenceStringInherited.kt";
    let inherited_source = r#"package objectstring
class EntityId
open class Base {
    val id: EntityId = EntityId()
}
class Derived : Base()
fun render(value: Derived): String = value.id.toString()
"#;
    let inherited_provider = indexed(&[(inherited_file, inherited_source)]);
    let inherited_plan =
        reference_string_plan(inherited_source, inherited_file, &inherited_provider);
    assert!(
        inherited_plan.reference_string_calls.is_empty(),
        "{inherited_plan:#?}"
    );

    let extension_file = "ReferenceStringExtension.kt";
    let extension_source = r#"package objectstring
class EntityId
fun String.lowercase(): String = this
fun render(value: EntityId): String = value.toString().lowercase()
"#;
    let extension_provider = indexed(&[(extension_file, extension_source)]);
    let extension_plan =
        reference_string_plan(extension_source, extension_file, &extension_provider);
    assert!(
        extension_plan
            .reference_string_calls
            .iter()
            .all(|fact| fact.kind != ReferenceStringCallKind::InvariantLowercase),
        "an indexed source lowercase extension must not receive the invariant JDK fact: {extension_plan:#?}"
    );
}

#[test]
fn nullable_primitive_array_valueclass_typeparameter_and_unknown_receivers_remain_required() {
    let cases = [
        "fun test(value: EntityId?): String = value.toString()",
        "fun test(value: Int): String = value.toString()",
        "fun test(value: Array<EntityId>): String = value.toString()",
        "fun test(value: WrappedId): String = value.toString()",
        "fun <T> test(value: T): String = value.toString()",
        "fun test(value: MissingType): String = value.toString()",
        "typealias EntityAlias = EntityId\nfun test(value: EntityAlias): String = value.toString()",
    ];
    for (index, declaration) in cases.into_iter().enumerate() {
        let file = format!("ObjectStringRejected{index}.kt");
        let source = format!(
            "package objectstring\nclass EntityId\n@JvmInline value class WrappedId(val raw: String)\n{declaration}\n"
        );
        let provider = indexed(&[(&file, &source)]);
        todo_string_dependency(&source, &file, &provider);
    }
}

#[test]
fn nullable_properties_do_not_fall_back_to_standard_library_member_facts() {
    for ty in ["String?", "java.util.UUID?", "EntityId?"] {
        let file = "NullableProperty.kt";
        let source = format!(
            "package objectstring\nclass EntityId\nclass Holder(val value: {ty}) {{\n fun render(): String = value.toString()\n}}\n"
        );
        let provider = indexed(&[(file, &source)]);
        todo_string_dependency(&source, file, &provider);
    }
}

#[test]
fn annotated_property_accessors_need_a_separate_abi_proof() {
    for annotation in [
        "@get:JvmName(\"renamed\")",
        "@get:JvmSynthetic",
        "@JvmField",
    ] {
        let file = "AnnotatedProperty.kt";
        let source = format!(
            "package objectstring\nclass EntityId\nclass Holder {{\n {annotation} val value: EntityId = EntityId()\n fun render(): String = value.toString()\n}}\n"
        );
        let provider = indexed(&[(file, &source)]);
        let plan = reference_string_plan(&source, file, &provider);
        assert!(
            plan.reference_string_calls.is_empty(),
            "{annotation}: {plan:#?}"
        );
    }
}

#[test]
fn inferred_properties_and_locally_shadowed_parameters_are_not_receiver_proofs() {
    let file = "ObjectStringShadows.kt";
    let source = r#"package objectstring
class EntityId
class Holder {
    val inferred = EntityId()
    fun inferredCall(): String = inferred.toString()
}
fun localShadow(value: EntityId): String {
    val value = 7
    return value.toString()
}
"#;
    let provider = indexed(&[(file, source)]);
    let calls = required_calls(source, file, &provider);
    assert!(
        calls.iter().any(|(name, _)| name == "toString"),
        "inferred property and local shadow should remain unresolved: {calls:#?}"
    );
}

#[test]
fn classifier_qualifiers_and_ambiguous_types_are_not_instance_receivers() {
    let left = "package left\nclass EntityId\n";
    let right = "package right\nclass EntityId\n";
    let file = "ObjectStringAmbiguous.kt";
    let source = r#"package objectstring
import left.EntityId
import right.EntityId
fun ambiguous(value: EntityId): String = value.toString()
"#;
    let provider = indexed(&[("Left.kt", left), ("Right.kt", right), (file, source)]);
    todo_string_dependency(source, file, &provider);

    let static_file = "ObjectStringStatic.kt";
    let static_source =
        "package objectstring\nclass EntityId\nfun staticCall(): String = EntityId.toString()\n";
    let static_provider = indexed(&[(static_file, static_source)]);
    todo_string_dependency(static_source, static_file, &static_provider);
}

#[test]
fn stale_provider_and_source_to_string_overrides_keep_their_own_resolution() {
    let file = "ObjectStringStale.kt";
    let source = "package objectstring\nclass EntityId\nfun render(value: EntityId): String = value.toString()\n";
    let provider = indexed(&[(file, source)]);
    let changed = source.replace("EntityId", "OtherId");
    todo_string_dependency(&changed, file, &provider);

    let override_file = "ObjectStringOverride.kt";
    let override_source = r#"package objectstring
class EntityId {
    override fun toString(): String = "entity"
}
fun render(value: EntityId): String = value.toString()
"#;
    let override_provider = indexed(&[(override_file, override_source)]);
    let calls = required_calls(override_source, override_file, &override_provider);
    assert!(
        calls.iter().any(|(name, resolution)| {
            name == "toString" && matches!(resolution, FactStatus::Established(_))
        }),
        "an indexed override must retain its ordinary source dispatch: {calls:#?}"
    );
}
