use notlin::semantics::{FactStatus, SymbolId, SyntaxSemanticProvider, plan_required_calls};
use std::path::{Path, PathBuf};

fn checked_provider(files: Vec<(PathBuf, String)>) -> SyntaxSemanticProvider {
    for (path, source) in &files {
        let mut parser = tree_sitter::Parser::new();
        let language = if path
            .extension()
            .is_some_and(|extension| extension == "java")
        {
            tree_sitter_java::LANGUAGE.into()
        } else {
            tree_sitter_kotlin_ng::LANGUAGE.into()
        };
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "invalid fixture {}: {source}",
            path.display()
        );
    }
    SyntaxSemanticProvider::new(files)
}

fn dependencies(
    source: &str,
    file: &str,
    provider: &SyntaxSemanticProvider,
) -> Vec<FactStatus<SymbolId>> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), Path::new(file));
    plan_required_calls(source, &tree, Path::new(file), provider, true, &mut plan);
    plan.dependencies
        .into_iter()
        .map(|dependency| dependency.resolution)
        .collect()
}

fn selected_type(source: &str, provider: &SyntaxSemanticProvider) -> FactStatus<SymbolId> {
    dependencies(source, "Consumer.kt", provider)
        .into_iter()
        .find(|fact| matches!(fact, FactStatus::Established(symbol) if symbol.name == "create"))
        .unwrap_or(FactStatus::Unknown)
}

fn factory_provider(consumer: &str) -> SyntaxSemanticProvider {
    let factory = r#"package overloads
class Factory {
    companion object {
        fun create(value: String): String = value
        fun create(value: java.math.BigDecimal): String = value.toPlainString()
        fun create(value: Long): String = value.toString()
    }
}
"#;
    checked_provider(vec![
        (PathBuf::from("Factory.kt"), factory.to_owned()),
        (PathBuf::from("Consumer.kt"), consumer.to_owned()),
    ])
}

fn java_factory_provider(java_factory: &str, consumer: &str) -> SyntaxSemanticProvider {
    checked_provider(vec![
        (PathBuf::from("JavaFactory.java"), java_factory.to_owned()),
        (PathBuf::from("Consumer.kt"), consumer.to_owned()),
    ])
}

fn java_factory() -> &'static str {
    r#"package overloads;
import java.math.BigDecimal;
public final class JavaFactory {
    public static String create(String value) { return value; }
    public static String create(BigDecimal value) { return value.toString(); }
    public static String create(long value) { return Long.toString(value); }
}
"#
}

#[test]
fn exact_string_literal_selects_the_string_overload() {
    let consumer = "package overloads\nfun use() = Factory.create(\"value\")\n";
    let provider = factory_provider(consumer);
    let FactStatus::Established(symbol) = selected_type(consumer, &provider) else {
        panic!("exact String argument should establish one overload");
    };
    assert_eq!(symbol.parameters, ["String"]);
}

#[test]
fn exact_lexical_parameter_selects_primitive_overload() {
    let consumer = "package overloads\nfun use(value: Long) = Factory.create(value)\n";
    let provider = factory_provider(consumer);
    let FactStatus::Established(symbol) = selected_type(consumer, &provider) else {
        panic!("exact Long parameter should establish one overload");
    };
    assert_eq!(symbol.parameters, ["Long"]);
}

#[test]
fn object_to_string_result_selects_string_overload() {
    let consumer = "package overloads\nfun use(value: Any) = Factory.create(value.toString())\n";
    let provider = factory_provider(consumer);
    let FactStatus::Established(symbol) = selected_type(consumer, &provider) else {
        panic!("proven Any.toString result should select String overload");
    };
    assert_eq!(symbol.parameters, ["String"]);
}

#[test]
fn unsupported_supertype_and_nullable_candidates_preserve_ambiguity() {
    for overload in ["Any", "java.math.BigDecimal?"] {
        let factory = format!(
            "package overloads\nclass Factory {{\n    companion object {{\n        fun create(value: String): String = value\n        fun create(value: {overload}): String = value.toString()\n    }}\n}}\n"
        );
        let consumer = "package overloads\nfun use() = Factory.create(\"value\")\n";
        let provider = checked_provider(vec![
            (PathBuf::from("Factory.kt"), factory),
            (PathBuf::from("Consumer.kt"), consumer.to_owned()),
        ]);
        assert!(
            dependencies(consumer, "Consumer.kt", &provider).iter().any(
                |fact| matches!(fact, FactStatus::Ambiguous(candidates) if candidates.len() == 2)
            ),
            "unsupported `{overload}` candidate must not be discarded: {:#?}",
            dependencies(consumer, "Consumer.kt", &provider)
        );
    }
}

#[test]
fn named_arguments_do_not_use_positional_exact_filtering() {
    let consumer = "package overloads\nfun use() = Factory.create(value = \"value\")\n";
    let provider = factory_provider(consumer);
    assert!(
        dependencies(consumer, "Consumer.kt", &provider)
            .iter()
            .any(|fact| matches!(fact, FactStatus::Ambiguous(candidates) if candidates.len() == 3)),
        "named arguments are outside the exact positional proof"
    );
}

#[test]
fn multiple_exact_same_type_import_candidates_remain_ambiguous() {
    let left = "package left\nfun create(value: String): String = value\n";
    let right = "package right\nfun create(value: String): String = value\n";
    let consumer =
        "package consumer\nimport left.*\nimport right.*\nfun use() = create(\"value\")\n";
    let provider = checked_provider(vec![
        (PathBuf::from("Left.kt"), left.to_owned()),
        (PathBuf::from("Right.kt"), right.to_owned()),
        (PathBuf::from("Consumer.kt"), consumer.to_owned()),
    ]);
    assert!(
        dependencies(consumer, "Consumer.kt", &provider)
            .iter()
            .any(|fact| matches!(fact, FactStatus::Ambiguous(candidates) if candidates.len() == 2)),
        "both exact applicable candidates must remain ambiguous"
    );
}

#[test]
fn exact_string_argument_selects_java_static_string_overload() {
    let consumer = "package overloads\nfun use() = JavaFactory.create(\"value\")\n";
    let provider = java_factory_provider(java_factory(), consumer);
    let FactStatus::Established(symbol) = selected_type(consumer, &provider) else {
        panic!("exact String argument should select Java String overload");
    };
    assert_eq!(symbol.file, PathBuf::from("JavaFactory.java"));
    assert_eq!(symbol.parameters, ["String"]);
}

#[test]
fn java_supertype_candidate_is_not_discarded_by_string_proof() {
    let java = java_factory().replace(
        "    public static String create(long value) { return Long.toString(value); }\n",
        "    public static String create(long value) { return Long.toString(value); }\n    public static String create(Object value) { return \"object\"; }\n",
    );
    let consumer = "package overloads\nfun use() = JavaFactory.create(\"value\")\n";
    let provider = java_factory_provider(&java, consumer);
    assert!(
        dependencies(consumer, "Consumer.kt", &provider)
            .iter()
            .any(|fact| matches!(fact, FactStatus::Ambiguous(candidates) if candidates.len() == 4)),
        "Object is a possible Java supertype candidate and must preserve ambiguity"
    );
}

#[test]
fn package_nullness_defaults_prevent_exact_java_overload_selection() {
    let consumer = "package overloads\nfun use() = JavaFactory.create(\"value\")\n";
    let provider = checked_provider(vec![
        (PathBuf::from("JavaFactory.java"), java_factory().to_owned()),
        (
            PathBuf::from("package-info.java"),
            "@NullMarked package overloads;\n".to_owned(),
        ),
        (PathBuf::from("Consumer.kt"), consumer.to_owned()),
    ]);
    assert!(
        dependencies(consumer, "Consumer.kt", &provider)
            .iter()
            .any(|fact| matches!(fact, FactStatus::Ambiguous(candidates) if candidates.len() == 3)),
        "package-level nullness defaults must keep the Java return contract unresolved"
    );
}

#[test]
fn java_numeric_widening_candidates_are_not_filtered() {
    let java = r#"package overloads;
public final class JavaFactory {
    public static String create(int value) { return Integer.toString(value); }
    public static String create(long value) { return Long.toString(value); }
}
"#;
    let consumer = "package overloads\nfun use(value: Int) = JavaFactory.create(value)\n";
    let provider = java_factory_provider(java, consumer);
    assert!(
        dependencies(consumer, "Consumer.kt", &provider)
            .iter()
            .any(|fact| matches!(fact, FactStatus::Ambiguous(candidates) if candidates.len() == 2)),
        "Java primitive widening must be left to the real compiler"
    );
}

#[test]
fn primitive_spelling_shadow_is_not_treated_as_the_builtin_type() {
    for prefix in ["class Long\n", "import unknown.*\n"] {
        let consumer =
            format!("package overloads\n{prefix}fun use(value: Long) = Factory.create(value)\n");
        let provider = factory_provider(&consumer);
        assert!(dependencies(&consumer, "Consumer.kt", &provider).iter().any(|fact| matches!(fact, FactStatus::Ambiguous(candidates) if candidates.len() == 3)));
    }
}

#[test]
fn syntax_reuse_never_reuses_old_binding_or_source_snapshot_proofs() {
    let consumer = "package overloads\nfun use() = JavaFactory.create(\"value\")\n";
    let first = java_factory_provider(java_factory(), consumer);
    assert!(matches!(
        selected_type(consumer, &first),
        FactStatus::Established(_)
    ));
    let changed = java_factory().replace("create(String value)", "create(CharSequence value)");
    let second = java_factory_provider(&changed, consumer);
    assert!(matches!(
        selected_type(consumer, &second),
        FactStatus::Unknown
    ));
    let files = vec![
        (PathBuf::from("JavaFactory.java"), java_factory().to_owned()),
        (PathBuf::from("Consumer.kt"), consumer.to_owned()),
        (
            PathBuf::from("Shadow.kt"),
            "package unrelated\nclass String\n".to_owned(),
        ),
    ];
    let rebound = checked_provider(files);
    assert!(matches!(
        selected_type(consumer, &rebound),
        FactStatus::Unknown
    ));
    let stale = consumer.replace("\"value\"", "\"changed\"");
    assert!(matches!(selected_type(&stale, &first), FactStatus::Unknown));
}
