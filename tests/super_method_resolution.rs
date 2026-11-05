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
fn qualified_super_selects_the_exact_direct_branch() {
    let source = r#"package p
interface Left {
    fun value(): String = "left"
}
interface Right {
    fun value(): String = "right"
}
class Child : Left, Right {
    override fun value(): String = super<p.Right>.value()
}
"#;
    assert_eq!(
        resolve_super(source, "value"),
        FactStatus::Established(expected_member(source, "Right"))
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
fn rejects_missing_methods_and_wrong_qualified_parent() {
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
    assert_eq!(
        resolve_super(arguments, "value"),
        FactStatus::Established(expected_member(arguments, "Parent"))
    );
}

#[test]
fn argument_super_lookup_preserves_inherited_overloads_and_exact_branch_selection() {
    let source = r#"package p
open class Root {
    open fun value(x: String): String = x
}
open class Parent : Root() {
    fun value(x: Int): String = "int"
}
class Child : Parent() {
    fun run(): String = super.value("root")
}
"#;
    assert_eq!(
        resolve_super(source, "value"),
        FactStatus::Established(expected_member(source, "Root"))
    );
    let qualified = r#"package p
interface Left { fun value(x: Int): String = "left" }
interface Right { fun value(x: Int): String = "right" }
class Child : Left, Right {
    override fun value(x: Int): String = super<Right>.value(x)
}
"#;
    assert_eq!(
        resolve_super(qualified, "value"),
        FactStatus::Established(expected_member(qualified, "Right"))
    );
    let ambiguous = qualified.replace("super<Right>", "super");
    assert!(matches!(
        resolve_super(&ambiguous, "value"),
        FactStatus::Ambiguous(_)
    ));
}

#[test]
fn argument_super_lookup_rejects_unproved_call_shapes() {
    for (parameter, argument) in [
        ("x: Int = 2", "1"),
        ("vararg x: Int", "1"),
        ("x: Int", "x = 1"),
    ] {
        let source = format!(
            "package p\nopen class Parent {{ open fun value({parameter}): String = \"parent\" }}\nclass Child : Parent() {{ fun run():String = super.value({argument}) }}\n"
        );
        assert_eq!(
            resolve_super(&source, "value"),
            FactStatus::Unknown,
            "{source}"
        );
    }
}

#[test]
fn nearest_override_uses_bound_nominal_parameters() {
    let source = r#"package p
interface Input
open class Root { open fun value(x: Input): String = "root" }
open class Parent : Root() { override fun value(x: Input): String = "parent" }
class Child : Parent() { fun run(x: Input): String = super.value(x) }
"#;
    assert_eq!(
        resolve_super(source, "value"),
        FactStatus::Established(expected_member(source, "Parent"))
    );
}

#[test]
fn contextual_narrow_super_arguments_require_a_proven_in_range_cast() {
    for (parameter, argument) in [
        ("Byte", "128"),
        ("Short", "32768"),
        ("Byte", "1L"),
        ("Byte", "1 + 2"),
    ] {
        let source = format!(
            "package p\nopen class Parent {{fun value(x:{parameter}):Int=1}}\nclass Child:Parent(){{fun run():Int=super.value({argument})}}\n"
        );
        assert_eq!(
            resolve_super(&source, "value"),
            FactStatus::Unknown,
            "{source}"
        );
    }
    for (parameter, argument, cast) in [
        ("Byte", "127", "byte"),
        ("Byte", "-128", "byte"),
        ("Short", "32767", "short"),
        ("Short", "(-32768)", "short"),
    ] {
        let source = format!(
            "package p\nopen class Parent {{fun value(x:{parameter}):Int=1}}\nclass Child:Parent(){{fun run():Int=super.value({argument})}}\n"
        );
        let file = Path::new("super_resolution.kt");
        let provider = SyntaxSemanticProvider::new([(file.to_path_buf(), source.clone())]);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(&source, None).unwrap();
        let mut plan = notlin::translation_plan::analyze(&source, tree.root_node(), file);
        plan_required_calls(&source, &tree, file, &provider, false, &mut plan);
        let [call] = plan.ordinary_source_calls.as_slice() else {
            panic!("expected planned narrow argument: {source}");
        };
        assert_eq!(call.argument_casts, [Some(cast.into())]);
    }
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
