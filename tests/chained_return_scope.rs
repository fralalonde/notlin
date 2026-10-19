use notlin::semantics::{FactStatus, SyntaxSemanticProvider, plan_required_calls};
use std::path::{Path, PathBuf};

fn dependency(
    source: &str,
    file: &Path,
    provider: &SyntaxSemanticProvider,
    spelling: &str,
) -> FactStatus<notlin::semantics::SymbolId> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), file);
    plan_required_calls(source, &tree, file, provider, false, &mut plan);
    plan.dependencies
        .into_iter()
        .find(|dependency| dependency.spelling == spelling)
        .map(|dependency| dependency.resolution)
        .unwrap_or(FactStatus::Unknown)
}

#[test]
fn chained_return_uses_the_producers_import_context() {
    let provider_file = PathBuf::from("Provider.kt");
    let payload_file = PathBuf::from("Payload.kt");
    let consumer_file = PathBuf::from("Consumer.kt");
    let provider_source = "package producer\nimport model.Payload\nclass Provider { fun payload(): Payload = Payload() }\n";
    let payload_source = "package model\nclass Payload { fun getId(): String = \"id\" }\n";
    let consumer_source = "package consumer\nimport producer.Provider\nclass Payload { fun getId(): Int = 1 }\nfun read(provider: Provider) = provider.payload().getId()\n";
    let index = SyntaxSemanticProvider::new(vec![
        (provider_file, provider_source.into()),
        (payload_file.clone(), payload_source.into()),
        (consumer_file.clone(), consumer_source.into()),
    ]);

    let resolved = dependency(consumer_source, &consumer_file, &index, "getId");
    let FactStatus::Established(id) = resolved else {
        panic!("expected producer-scoped return type to resolve, got {resolved:?}");
    };
    assert_eq!(id.file, payload_file);
    assert_eq!(id.package, "model");
    assert_eq!(id.owner_path, vec!["Payload"]);
}

#[test]
fn unknown_or_ambiguous_producer_types_do_not_bind_to_consumer_names() {
    let unknown_file = PathBuf::from("UnknownProvider.kt");
    let consumer_file = PathBuf::from("UnknownConsumer.kt");
    let unknown_provider =
        "package producer\nclass Provider { fun payload(): MissingPayload = TODO() }\n";
    let consumer = "package consumer\nimport producer.Provider\nclass MissingPayload { fun getId(): Int = 1 }\nfun read(provider: Provider) = provider.payload().getId()\n";
    let index = SyntaxSemanticProvider::new(vec![
        (unknown_file, unknown_provider.into()),
        (consumer_file.clone(), consumer.into()),
    ]);
    assert!(matches!(
        dependency(consumer, &consumer_file, &index, "getId"),
        FactStatus::Unknown
    ));

    let provider_file = PathBuf::from("AmbiguousProvider.kt");
    let left_file = PathBuf::from("Left.kt");
    let right_file = PathBuf::from("Right.kt");
    let ambiguous_provider = "package producer\nimport left.*\nimport right.*\nclass Provider { fun payload(): Payload = TODO() }\n";
    let left = "package left\nclass Payload { fun getId(): String = \"left\" }\n";
    let right = "package right\nclass Payload { fun getId(): String = \"right\" }\n";
    let consumer = "package consumer\nimport producer.Provider\nfun read(provider: Provider) = provider.payload().getId()\n";
    let consumer_file = PathBuf::from("AmbiguousConsumer.kt");
    let index = SyntaxSemanticProvider::new(vec![
        (provider_file, ambiguous_provider.into()),
        (left_file, left.into()),
        (right_file, right.into()),
        (consumer_file.clone(), consumer.into()),
    ]);
    assert!(matches!(
        dependency(consumer, &consumer_file, &index, "getId"),
        FactStatus::Unknown
    ));
}

#[test]
fn nullable_and_generic_returns_remain_conservative() {
    let payload = "package model\nclass Payload { fun getId(): String = \"id\" }\n";
    let nullable_provider = "package producer\nimport model.Payload\nclass Provider { fun payload(): Payload? = null }\n";
    let nullable_consumer = "package consumer\nimport producer.Provider\nfun read(provider: Provider) = provider.payload()?.getId()\n";
    let files = vec![
        (PathBuf::from("Payload.kt"), payload.to_owned()),
        (
            PathBuf::from("NullableProvider.kt"),
            nullable_provider.to_owned(),
        ),
        (
            PathBuf::from("NullableConsumer.kt"),
            nullable_consumer.to_owned(),
        ),
    ];
    let index = SyntaxSemanticProvider::new(files);
    assert!(matches!(
        dependency(
            nullable_consumer,
            Path::new("NullableConsumer.kt"),
            &index,
            "getId"
        ),
        FactStatus::Unknown
    ));

    let generic_provider =
        "package producer\nclass Box<T>(val value: T) { fun payload(): T = value }\n";
    let generic_consumer = "package consumer\nimport producer.Box\nimport model.Payload\nfun read(box: Box<Payload>) = box.payload().getId()\n";
    let index = SyntaxSemanticProvider::new(vec![
        (PathBuf::from("Payload.kt"), payload.to_owned()),
        (PathBuf::from("Box.kt"), generic_provider.to_owned()),
        (
            PathBuf::from("GenericConsumer.kt"),
            generic_consumer.to_owned(),
        ),
    ]);
    assert!(matches!(
        dependency(
            generic_consumer,
            Path::new("GenericConsumer.kt"),
            &index,
            "getId"
        ),
        FactStatus::Unknown
    ));
}

#[test]
fn producer_standard_return_types_keep_their_existing_facts() {
    let producer = "package producer\nclass Provider { fun text(): String = \"ok\" }\n";
    let consumer = "package consumer\nimport producer.Provider\nfun read(provider: Provider) = provider.text()\n";
    let index = SyntaxSemanticProvider::new(vec![
        (PathBuf::from("Provider.kt"), producer.into()),
        (PathBuf::from("Consumer.kt"), consumer.into()),
    ]);
    let result = dependency(consumer, Path::new("Consumer.kt"), &index, "text");
    assert!(matches!(result, FactStatus::Established(_)));
}

#[test]
fn generic_named_return_keeps_its_classifier_without_substituting_parameters() {
    let producer = "package producer\nclass Box<T> {\n fun label(): String = \"label\"\n}\nclass Provider {\n fun box(): Box<String> = Box()\n}\n";
    let consumer = "package consumer\nimport producer.Provider\nfun read(provider: Provider) = provider.box().label()\n";
    let provider_file = PathBuf::from("Provider.kt");
    let consumer_file = PathBuf::from("Consumer.kt");
    let index = SyntaxSemanticProvider::new(vec![
        (provider_file.clone(), producer.into()),
        (consumer_file.clone(), consumer.into()),
    ]);
    let result = dependency(consumer, &consumer_file, &index, "label");
    let FactStatus::Established(id) = result else {
        panic!("known generic receiver classifier should resolve: {result:?}");
    };
    assert_eq!(id.file, provider_file);
    assert_eq!(id.owner_path, vec!["Box"]);
}

#[test]
fn chained_java_returns_use_the_java_producers_import_context() {
    let provider_file = PathBuf::from("Provider.java");
    let payload_file = PathBuf::from("Payload.java");
    let consumer_file = PathBuf::from("Reader.kt");
    let provider = "package api;\nimport model.Payload;\npublic class Provider { public Payload getPayload() { return null; } }\n";
    let payload =
        "package model;\npublic class Payload { public String getId() { return \"id\"; } }\n";
    let consumer = "package consumer\nimport api.Provider\nimport other.Payload\nfun read(provider: Provider): String = provider.getPayload().getId()\n";
    let conflict =
        "package other;\npublic class Payload { public int getId(int value) { return value; } }\n";
    let index = SyntaxSemanticProvider::new(vec![
        (provider_file, provider.into()),
        (payload_file.clone(), payload.into()),
        (PathBuf::from("Reader.kt"), consumer.into()),
        (PathBuf::from("OtherPayload.java"), conflict.into()),
    ]);
    let resolved = dependency(consumer, &consumer_file, &index, "getId");
    let FactStatus::Established(id) = resolved else {
        panic!("expected Java producer-scoped return type, got {resolved:?}");
    };
    assert_eq!(id.file, payload_file);
    assert_eq!(id.package, "model");
    assert_eq!(id.owner_path, vec!["Payload"]);
}

#[test]
fn aliased_producer_import_does_not_rebind_to_same_package_type() {
    let provider = "package producer\nimport model.Payload as Body\nclass Provider { fun payload(): Body = TODO() }\n";
    let local_body = "package producer\nclass Body { fun getId(): Int = 1 }\n";
    let payload = "package model\nclass Payload { fun getId(): String = \"id\" }\n";
    let consumer = "package consumer\nimport producer.Provider\nfun read(provider: Provider) = provider.payload().getId()\n";
    let consumer_file = PathBuf::from("AliasConsumer.kt");
    let index = SyntaxSemanticProvider::new(vec![
        (PathBuf::from("AliasProvider.kt"), provider.into()),
        (PathBuf::from("LocalBody.kt"), local_body.into()),
        (PathBuf::from("AliasPayload.kt"), payload.into()),
        (consumer_file.clone(), consumer.into()),
    ]);
    assert!(matches!(
        dependency(consumer, &consumer_file, &index, "getId"),
        FactStatus::Unknown
    ));
}

#[test]
fn producer_type_alias_does_not_become_a_platform_receiver() {
    let producer = "package producer\ntypealias String = Payload\nclass Payload { fun trim(): Payload = this }\nclass Provider { fun text(): String = Payload() }\n";
    let consumer = "package consumer\nimport producer.Provider\nfun read(provider: Provider) = provider.text().trim()\n";
    let file = PathBuf::from("Consumer.kt");
    let provider = SyntaxSemanticProvider::new([
        (PathBuf::from("Provider.kt"), producer.into()),
        (file.clone(), consumer.into()),
    ]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(consumer, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(consumer, tree.root_node(), &file);
    plan_required_calls(consumer, &tree, &file, &provider, false, &mut plan);
    assert!(
        plan.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "S001" && diagnostic.message.contains("`trim`")),
        "{plan:?}"
    );
}
