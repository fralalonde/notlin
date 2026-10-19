use notlin::semantics::{FactStatus, SymbolId, SyntaxSemanticProvider, plan_required_calls};
use notlin::translation_plan::analyze;
use std::path::{Path, PathBuf};

fn compare_target(source: &str) -> FactStatus<SymbolId> {
    compare_target_files(&[("Contracts.kt", source)])
}

fn compare_target_files(files: &[(&str, &str)]) -> FactStatus<SymbolId> {
    let source = files
        .iter()
        .find(|(name, _)| *name == "Contracts.kt")
        .map(|(_, source)| *source)
        .unwrap();
    let provider = SyntaxSemanticProvider::new(
        files
            .iter()
            .map(|(name, source)| (PathBuf::from(name), (*source).to_owned()))
            .collect::<Vec<_>>(),
    );
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = analyze(source, tree.root_node(), Path::new("Contracts.kt"));
    plan_required_calls(
        source,
        &tree,
        Path::new("Contracts.kt"),
        &provider,
        false,
        &mut plan,
    );
    plan.dependencies
        .into_iter()
        .find(|dependency| dependency.spelling == "compareToken")
        .map(|dependency| dependency.resolution)
        .unwrap_or(FactStatus::Unknown)
}

const TOKEN: &str = r#"package sample
class RankToken {
    fun compareToken(other: RankToken): Int {
        return 0
    }
}
"#;

#[test]
fn secondary_constructor_parameters_prove_direct_property_receivers() {
    for body in [
        "constructor(position: Position) : this(position.token.compareToken(position.token))",
        "constructor(position: Position) : this(0) { position.token.compareToken(position.token) }",
    ] {
        let source = format!(
            "{TOKEN}\nclass Position(val token: RankToken)\nclass Result(val rank: Int) {{\n {body}\n }}\n"
        );
        assert!(
            matches!(compare_target(&source), FactStatus::Established(_)),
            "{body}"
        );
    }
}

#[test]
fn secondary_constructor_property_receivers_keep_scope_and_type_guards() {
    for (parameter, body) in [
        (
            "position: Position?",
            "position.token.compareToken(position.token)",
        ),
        (
            "vararg position: Position",
            "position.token.compareToken(position.token)",
        ),
        (
            "position: Position",
            "run { position.token.compareToken(position.token) }",
        ),
        (
            "position: Position",
            "val position = Other(); position.token.compareToken(position.token)",
        ),
    ] {
        let source = format!(
            "{TOKEN}\nclass Position(val token: RankToken)\nclass Other\nclass Result(val rank: Int) {{\n constructor({parameter}) : this(0) {{ {body} }}\n }}\n"
        );
        assert!(
            matches!(compare_target(&source), FactStatus::Unknown),
            "{source}"
        );
    }
}

#[test]
fn direct_this_property_proves_receiver_type() {
    let source = format!(
        "{TOKEN}{}",
        r#"class Position(val positionName: RankToken) {
    fun before(other: Position): Int {
        return this.positionName.compareToken(other.positionName)
    }
}
"#
    );
    let FactStatus::Established(target) = compare_target(&source) else {
        panic!("explicitly typed direct property receiver should resolve")
    };
    assert_eq!(target.name, "compareToken");
    assert_eq!(target.owner_path, vec!["RankToken"]);
}

#[test]
fn typed_parameter_property_is_proven_without_a_this_receiver() {
    let source = format!(
        "{TOKEN}{}",
        r#"class Position(val positionName: RankToken) {
    fun before(other: Position): Int {
        return other.positionName.compareToken(other.positionName)
    }
}
"#
    );
    assert!(matches!(
        compare_target(&source),
        FactStatus::Established(_)
    ));
}

#[test]
fn nullable_and_type_parameter_properties_remain_unresolved() {
    let nullable = format!(
        "{TOKEN}{}",
        r#"class Position(val positionName: RankToken?) {
    fun before(other: Position): Int {
        return this.positionName.compareToken(other.positionName)
    }
}
"#
    );
    assert!(matches!(compare_target(&nullable), FactStatus::Unknown));

    let type_parameter = r#"package sample
class Position<T>(val positionName: T) {
    fun before(other: Position<T>): Int {
        return this.positionName.compareToken(other.positionName)
    }
}
"#;
    assert!(matches!(
        compare_target(type_parameter),
        FactStatus::Unknown
    ));
}

#[test]
fn aliases_and_inherited_properties_remain_unresolved_in_direct_only_proof() {
    let alias = format!(
        "{TOKEN}{}",
        r#"typealias TokenAlias = RankToken
class Position(val positionName: TokenAlias) {
    fun before(other: Position): Int {
        return this.positionName.compareToken(other.positionName)
    }
}
"#
    );
    assert!(matches!(compare_target(&alias), FactStatus::Unknown));

    let inherited = format!(
        "{TOKEN}{}",
        r#"interface PositionBase {
    val positionName: RankToken
}
class Position : PositionBase {
    fun before(other: Position): Int {
        return this.positionName.compareToken(other.positionName)
    }
}
"#
    );
    assert!(matches!(compare_target(&inherited), FactStatus::Unknown));
}

#[test]
fn lexical_shadows_lambdas_and_extension_this_remain_unresolved() {
    let local_shadow = format!(
        "{TOKEN}{}",
        r#"class Position(val positionName: RankToken) {
    fun before(other: Position): Int {
        val other = Position(RankToken())
        return other.positionName.compareToken(other.positionName)
    }
}
"#
    );
    assert!(matches!(compare_target(&local_shadow), FactStatus::Unknown));

    let lambda_receiver = format!(
        "{TOKEN}{}",
        r#"class Position(val positionName: RankToken) {
    fun before(other: Position): Int {
        return listOf(other).map { other ->
            other.positionName.compareToken(other.positionName)
        }.first().hashCode()
    }
}
"#
    );
    assert!(matches!(
        compare_target(&lambda_receiver),
        FactStatus::Unknown
    ));

    let extension_this = format!(
        "{TOKEN}{}",
        r#"class Position(val positionName: RankToken)
fun Position.before(other: Position): Int {
    return this.positionName.compareToken(other.positionName)
}
"#
    );
    assert!(matches!(
        compare_target(&extension_this),
        FactStatus::Unknown
    ));
}

#[test]
fn type_parameter_spelling_cannot_resolve_as_an_indexed_classifier() {
    let source = format!(
        "{TOKEN}{}",
        r#"class Position {
    fun <RankToken> before(other: RankToken): Int {
        return other.positionName.compareToken(other.positionName)
    }
}
"#
    );
    assert!(matches!(compare_target(&source), FactStatus::Unknown));
}

#[test]
fn duplicate_owner_classifier_is_not_accepted() {
    let primary = format!(
        "{TOKEN}{}",
        r#"class Position(val positionName: RankToken) {
    fun before(other: Position): Int {
        return this.positionName.compareToken(other.positionName)
    }
}


"#
    );
    let duplicate = "package sample\nclass Position(val unrelated: Int)\n";
    assert!(matches!(
        compare_target_files(&[("Contracts.kt", &primary), ("Duplicate.kt", duplicate)]),
        FactStatus::Unknown
    ));
}

#[test]
fn loop_catch_destructuring_and_anonymous_bindings_do_not_use_outer_parameter_types() {
    let declarations = r#"package sample
class Alpha { fun compareToken(other: Alpha): Int = 1 }
class Beta { fun compareToken(other: Beta): Int = 2 }
class HolderA(val key: Alpha)
class HolderB(val key: Beta) : RuntimeException()
"#;
    for body in [
        "for (other in values) { other.key.compareToken(other.key) }",
        "try { throw values[0] } catch (other: HolderB) { other.key.compareToken(other.key) }",
        "val (_, other) = Pair(1, values[0]); other.key.compareToken(other.key)",
        "val invoke = fun(other: HolderB): Int { return other.key.compareToken(other.key) }",
    ] {
        let source = format!(
            "{declarations}\nfun run(other: HolderA, values: List<HolderB>) {{ {body} }}\n"
        );
        assert!(
            matches!(compare_target(&source), FactStatus::Unknown),
            "must not use the outer HolderA parameter for {body}"
        );
    }
}

#[test]
fn companion_this_does_not_use_the_enclosing_instance_property() {
    let source = r#"package sample
class Alpha { fun compareToken(other: Alpha): Int = 1 }
class Beta { fun compareToken(other: Beta): Int = 2 }
class Host(val key: Alpha) {
    companion object {
        val key: Beta = Beta()
        fun run(other: Beta): Int = this.key.compareToken(other)
    }
}
"#;
    assert!(matches!(compare_target(source), FactStatus::Unknown));
}

#[test]
fn parameter_type_parameter_does_not_bind_to_a_same_named_class_with_that_property() {
    let source = format!(
        "{TOKEN}\nclass Holder(val positionName: RankToken)\n\
         fun <Holder> run(other: Holder): Int = other.positionName.compareToken(other.positionName)\n"
    );
    assert!(matches!(compare_target(&source), FactStatus::Unknown));
}

#[test]
fn property_classifier_uses_its_declaring_files_imports() {
    let source = "package consumer\nimport producer.Holder\nclass RankToken\n\
                  fun run(other: Holder): Int = other.key.compareToken(other.key)\n";
    let holder = "package producer\nimport model.RankToken\nclass Holder(val key: RankToken)\n";
    let token = "package model\nclass RankToken { fun compareToken(other: RankToken): Int = 1 }\n";
    let FactStatus::Established(target) = compare_target_files(&[
        ("Contracts.kt", source),
        ("Holder.kt", holder),
        ("Token.kt", token),
    ]) else {
        panic!("the property type must resolve in the producer's import context")
    };
    assert_eq!(target.package, "model");
    assert_eq!(target.owner_path, vec!["RankToken"]);
}

#[test]
fn imported_alias_does_not_fall_through_to_a_same_package_property_classifier() {
    let source = "package sample\nimport model.Other as RankToken\n\
                  class RankToken { fun compareToken(other: RankToken): Int = 1 }\n\
                  class Holder(val key: RankToken) {\n\
                  fun run(other: Holder): Int = this.key.compareToken(other.key)\n}\n";
    let other = "package model\nclass Other\n";
    assert!(matches!(
        compare_target_files(&[("Contracts.kt", source), ("Other.kt", other)]),
        FactStatus::Unknown
    ));
}

#[test]
fn parameterized_owner_can_supply_an_independently_typed_property() {
    let source = format!(
        "{TOKEN}\nclass Holder<T>(val key: RankToken)\nfun run(other: Holder<String>): Int = other.key.compareToken(other.key)\n"
    );
    assert!(matches!(
        compare_target(&source),
        FactStatus::Established(_)
    ));
    let generic_property = format!(
        "{TOKEN}\nclass Holder<T>(val key: T)\nfun run(other: Holder<RankToken>): Int = other.key.compareToken(other.key)\n"
    );
    assert!(matches!(
        compare_target(&generic_property),
        FactStatus::Unknown
    ));
}

#[test]
fn anonymous_object_this_does_not_use_the_enclosing_instance_property() {
    let source = "package sample\nclass Alpha { fun compareToken(other: Alpha): Int = 1 }\nclass Beta { fun compareToken(other: Beta): Int = 2 }\nclass Host(val key: Alpha) {\n fun run() = object {\n val key: Beta = Beta()\n fun compare(other: Beta): Int = this.key.compareToken(other)\n }\n}\n";
    assert!(matches!(compare_target(source), FactStatus::Unknown));
}

#[test]
fn method_return_and_nested_properties_keep_their_declaring_contexts() {
    let source = "package consumer\nimport api.Event\nclass Token\nfun read(event: Event): Int = event.payload().item.token.compareToken(event.payload().item.token)\n";
    let event = "package api\nimport model.Payload\nclass Event(private val value: Payload) { fun payload(): Payload = value }\n";
    let model = "package model\nclass Token { fun compareToken(other: Token): Int = 1 }\nclass Item(val token: Token)\nclass Payload(val item: Item)\n";
    let FactStatus::Established(target) = compare_target_files(&[
        ("Contracts.kt", source),
        ("Event.kt", event),
        ("Model.kt", model),
    ]) else {
        panic!("each projection should use its producer's type");
    };
    assert_eq!(target.package, "model");
    assert_eq!(target.owner_path, vec!["Token"]);
}

#[test]
fn method_property_projection_refuses_nullable_and_generic_hops() {
    for payload_type in ["Payload<RankToken>?", "Payload<RankToken>"] {
        for property_type in ["RankToken?", "T"] {
            let source = format!(
                "{TOKEN}\nclass Payload<T>(val key: {property_type})\nclass Event {{ fun payload(): {payload_type} = TODO() }}\nfun read(event: Event): Int = event.payload().key.compareToken(event.payload().key)\n"
            );
            assert!(matches!(compare_target(&source), FactStatus::Unknown));
        }
    }
}

#[test]
fn nested_projection_can_start_from_an_unqualified_direct_instance_property() {
    let source = format!(
        "{TOKEN}\nclass Item(val token: RankToken)\nclass Holder(val item: Item) {{ fun run(other: RankToken): Int = item.token.compareToken(other) }}\n"
    );
    assert!(matches!(
        compare_target(&source),
        FactStatus::Established(_)
    ));
    let source = format!(
        "{TOKEN}\nclass Item(val token: RankToken)\nclass Holder(val item: Item) {{ fun run(other: RankToken): Int {{ val item = Other(); return item.token.compareToken(other) }} }}\nclass Other\n"
    );
    assert!(matches!(compare_target(&source), FactStatus::Unknown));
}

#[test]
fn java_synthetic_property_requires_a_unique_physical_public_getter() {
    let consumer = "package consumer\nimport model.Holder\nfun run(other: Holder): Int = other.item.compareToken(other.item)\n";
    let token =
        "package model\nclass RankToken {\n fun compareToken(other: RankToken): Int = 1\n}\n";
    let java = "package model; public class Holder { public RankToken getItem() { return new RankToken(); } }";
    let resolved = compare_target_files(&[
        ("Contracts.kt", consumer),
        ("Holder.java", java),
        ("Token.kt", token),
    ]);
    assert!(matches!(resolved, FactStatus::Established(_)));
    for member in [
        "private RankToken getItem() { return null; }",
        "protected RankToken getItem() { return null; }",
        "RankToken getItem() { return null; }",
        "public static RankToken getItem() { return null; }",
        "public RankToken getItem(int index) { return null; }",
        "public void getItem() {}",
        "@org.jetbrains.annotations.Nullable public RankToken getItem() { return null; }",
        "public RankToken getItem() { return null; } public RankToken getItem() { return null; }",
        "@lombok.Getter private RankToken item;",
        "public RankToken item; public RankToken getItem() { return new RankToken(); }",
        "protected RankToken item; public RankToken getItem() { return new RankToken(); }",
        "RankToken item; public RankToken getItem() { return new RankToken(); }",
    ] {
        let java = format!("package model; public class Holder {{ {member} }}");
        assert!(
            matches!(
                compare_target_files(&[
                    ("Contracts.kt", consumer),
                    ("Holder.java", &java),
                    ("Token.kt", token)
                ]),
                FactStatus::Unknown
            ),
            "{member}"
        );
    }
}
