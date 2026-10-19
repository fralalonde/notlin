use notlin::semantics::{
    FactStatus, SemanticProvider, SyntaxSemanticProvider, plan_required_calls,
};
use std::path::{Path, PathBuf};

fn resolve_super(source: &str, method: &str) -> FactStatus<notlin::semantics::SymbolId> {
    let file = Path::new("super_resolution.kt");
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture:\n{source}");
    let provider = SyntaxSemanticProvider::new([(PathBuf::from(file), source.to_owned())]);
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), file);
    plan_required_calls(source, &tree, file, &provider, false, &mut plan);
    let facts = plan
        .dependencies
        .into_iter()
        .filter(|dependency| dependency.spelling == method)
        .map(|dependency| dependency.resolution)
        .collect::<Vec<_>>();
    assert_eq!(
        facts.len(),
        1,
        "expected one super.{method} dependency: {facts:?}"
    );
    facts.into_iter().next().unwrap()
}

fn expected_member(source: &str, owner_name: &str) -> notlin::semantics::SymbolId {
    let file = Path::new("super_resolution.kt");
    let provider = SyntaxSemanticProvider::new([(PathBuf::from(file), source.to_owned())]);
    let mut matches = provider
        .symbols_named("value")
        .into_iter()
        .filter(|symbol| symbol.id.owner_path == [owner_name.to_owned()])
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "expected unique {owner_name}.value");
    matches.pop().unwrap()
}

#[test]
fn resolves_plain_super_to_nearest_inherited_method() {
    let source = r#"package p
open class Root {
    open fun value(): String = "root"
}
open class Parent : Root()
class Child : Parent() {
    fun run(): String = super.value()
}
"#;
    assert_eq!(
        resolve_super(source, "value"),
        FactStatus::Established(expected_member(source, "Root"))
    );
}

#[test]
fn resolves_direct_parent_override_instead_of_grandparent() {
    let source = r#"package p
open class Root {
    open fun value(): String = "root"
}
open class Parent : Root() {
    override fun value(): String = "parent"
}
class Child : Parent() {
    override fun value(): String = super.value()
}
"#;
    assert_eq!(
        resolve_super(source, "value"),
        FactStatus::Established(expected_member(source, "Parent"))
    );
}

#[test]
fn rejects_unrelated_parent_candidates() {
    let source = r#"package p
interface Left {
    fun value(): String = "left"
}
interface Right {
    fun value(): String = "right"
}
class Child : Left, Right {
    fun run(): String = super.value()
}
"#;
    assert!(
        matches!(resolve_super(source, "value"), FactStatus::Ambiguous(candidates) if candidates.len() == 2)
    );
}

#[test]
fn rejects_mixed_depth_candidates_across_parent_branches() {
    let source = r#"package p
interface Root {
    fun value(): String = "root"
}
interface Middle : Root
interface Direct {
    fun value(): String = "direct"
}
class Child : Middle, Direct {
    fun run(): String = super.value()
}
"#;
    assert!(
        matches!(resolve_super(source, "value"), FactStatus::Ambiguous(candidates) if candidates.len() == 2)
    );
}

#[test]
fn rejects_missing_methods_wrong_qualified_parent_and_arguments() {
    let absent = r#"package p
open class Parent
class Child : Parent() {
    fun run(): String = super.absent()
}
"#;
    assert_eq!(resolve_super(absent, "absent"), FactStatus::Unknown);

    let wrong_parent = r#"package p
open class Parent {
    open fun value(): String = "parent"
}
class Other {
    fun value(): String = "other"
}
class Child : Parent() {
    fun run(): String = super<Other>.value()
}
"#;
    assert_eq!(resolve_super(wrong_parent, "value"), FactStatus::Unknown);

    let arguments = r#"package p
open class Parent {
    open fun value(x: Int): String = x.toString()
}
class Child : Parent() {
    fun run(): String = super.value(1)
}
"#;
    assert_eq!(resolve_super(arguments, "value"), FactStatus::Unknown);
}

#[test]
fn rejects_same_package_parent_name_collisions() {
    let source = r#"package p
open class Parent {
    open fun value(): String = "parent"
}
interface Parent {
    fun other(): String
}
class Child : Parent() {
    fun run(): String = super.value()
}
"#;
    assert_eq!(resolve_super(source, "value"), FactStatus::Unknown);
}

#[test]
fn checks_forks_above_the_direct_parent() {
    let source = r#"package p
interface Root {
    fun value(): String = "root"
}
interface Middle : Root
interface Direct {
    fun value(): String = "direct"
}
interface Parent : Middle, Direct
class Child : Parent {
    fun run(): String = super.value()
}
"#;
    assert!(
        matches!(resolve_super(source, "value"), FactStatus::Ambiguous(candidates) if candidates.len() == 2)
    );
}

#[test]
fn shared_diamond_preserves_one_target_identity() {
    let source = r#"package p
interface Root {
    fun value(): String = "root"
}
interface Left : Root
interface Right : Root
interface Parent : Left, Right
class Child : Parent {
    fun run(): String = super.value()
}
"#;
    assert_eq!(
        resolve_super(source, "value"),
        FactStatus::Established(expected_member(source, "Root"))
    );
}

#[test]
fn incomplete_parent_does_not_prove_an_unambiguous_target() {
    let source = r#"package p
interface Known {
    fun value(): String = "known"
}
class Child : Known, Missing {
    fun run(): String = super.value()
}
"#;
    assert_eq!(resolve_super(source, "value"), FactStatus::Unknown);
}
