use notlin::semantics::{
    FactStatus, SemanticProvider, SymbolId, SyntaxSemanticProvider, plan_required_calls,
};
use std::path::{Path, PathBuf};

fn provider(files: &[(&str, &str)]) -> SyntaxSemanticProvider {
    for (path, source) in files {
        let mut parser = tree_sitter::Parser::new();
        let language = if path.ends_with(".java") {
            tree_sitter_java::LANGUAGE.into()
        } else {
            tree_sitter_kotlin_ng::LANGUAGE.into()
        };
        parser.set_language(&language).unwrap();
        assert!(
            !parser.parse(source, None).unwrap().root_node().has_error(),
            "invalid fixture {path}:\n{source}"
        );
    }
    SyntaxSemanticProvider::new(
        files
            .iter()
            .map(|(path, source)| (PathBuf::from(path), (*source).to_owned())),
    )
}

fn owner(index: &SyntaxSemanticProvider, name: &str) -> SymbolId {
    let matches = index
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| matches!(symbol.id.kind.as_str(), "class" | "interface" | "object"))
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "expected unique type {name}: {matches:?}");
    matches.into_iter().next().unwrap()
}

fn member(index: &SyntaxSemanticProvider, owner_name: &str, name: &str) -> SymbolId {
    let owner = owner(index, owner_name);
    let mut expected_path = owner.owner_path;
    expected_path.push(owner.name);
    let matches = index
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| {
            symbol.id.file == owner.file
                && symbol.id.owner_path == expected_path
                && symbol.id.kind == "function"
        })
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "expected unique {owner_name}.{name}: {matches:?}"
    );
    matches.into_iter().next().unwrap()
}

fn inherited(index: &SyntaxSemanticProvider, owner_name: &str, name: &str) -> FactStatus<SymbolId> {
    index.resolve_inherited_noarg_call(&owner(index, owner_name), name)
}

#[test]
fn required_generic_overload_does_not_hide_ancestor_noarg_member() {
    let source = r#"package inherited
interface Root {
    fun getId(): String
}
interface Middle : Root {
    fun <T> getId(clazz: Class<T>): T
}
class Child : Middle
"#;
    let index = provider(&[("Chain.kt", source)]);
    assert_eq!(
        inherited(&index, "Child", "getId"),
        FactStatus::Established(member(&index, "Root", "getId"))
    );
}

#[test]
fn shared_diamond_deduplicates_one_inherited_contract() {
    let source = r#"package inherited
interface Root {
    fun getId(): String
}
interface Left : Root
interface Right : Root
class Child : Left, Right
"#;
    let index = provider(&[("Diamond.kt", source)]);
    assert_eq!(
        inherited(&index, "Child", "getId"),
        FactStatus::Established(member(&index, "Root", "getId"))
    );
}

#[test]
fn same_return_override_dominates_ancestor_declaration() {
    let source = r#"package inherited
open class Root {
    open fun getId(): String {
        return "root"
    }
}
open class Middle : Root() {
    override fun getId(): String {
        return "middle"
    }
}
class Child : Middle()
"#;
    let index = provider(&[("Override.kt", source)]);
    assert_eq!(
        inherited(&index, "Child", "getId"),
        FactStatus::Established(member(&index, "Middle", "getId"))
    );
}

#[test]
fn covariant_override_uses_the_declared_return_hierarchy() {
    let source = r#"package inherited
open class Id
class SpecificId : Id()
open class Root {
    open fun getId(): Id {
        return Id()
    }
}
open class Middle : Root() {
    override fun getId(): SpecificId {
        return SpecificId()
    }
}
class Child : Middle()
"#;
    let index = provider(&[("Covariant.kt", source)]);
    assert_eq!(
        inherited(&index, "Child", "getId"),
        FactStatus::Established(member(&index, "Middle", "getId"))
    );
}

#[test]
fn covariant_override_without_indexed_return_hierarchy_remains_unproven() {
    let source = r#"package inherited
interface Root {
    fun getId(): library.Id
}
interface Middle : Root {
    override fun getId(): library.SpecificId
}
abstract class Child : Middle
"#;
    let index = provider(&[("UnknownCovariance.kt", source)]);
    let actual = inherited(&index, "Child", "getId");
    assert!(!matches!(actual, FactStatus::Established(_)), "{actual:?}");
}

#[test]
fn zero_argument_defaults_and_varargs_are_not_treated_as_required_overloads() {
    let cases = [
        r#"package inherited
interface Root {
    fun getId(): String
}
interface Middle : Root {
    fun getId(clazz: Class<*> = String::class.java): String
}
class Child : Middle
"#,
        r#"package inherited
interface Root {
    fun getId(): String
}
interface Middle : Root {
    fun getId(vararg classes: Class<*>): String
}
class Child : Middle
"#,
    ];
    for source in cases {
        let index = provider(&[("Optional.kt", source)]);
        assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);
    }
}

#[test]
fn incomplete_ancestry_does_not_prove_a_missing_member() {
    let source = "package inherited\nclass Child : MissingParent()\n";
    let index = provider(&[("Incomplete.kt", source)]);
    assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);
}

#[test]
fn unrelated_same_name_members_and_package_collisions_do_not_leak() {
    let index = provider(&[
        (
            "RootA.kt",
            "package first\ninterface Root {\n    fun getId(): String\n}\n",
        ),
        (
            "RootB.kt",
            "package second\ninterface Root {\n    fun getId(): Int\n}\nclass Unrelated {\n    fun getId(): Boolean {\n        return true\n    }\n}\n",
        ),
        (
            "Child.kt",
            "package consumer\nimport first.Root\nclass Child : Root\n",
        ),
    ]);
    let expected = index
        .symbols_named("getId")
        .into_iter()
        .find(|symbol| symbol.id.package == "first")
        .unwrap()
        .id
        .clone();
    assert_eq!(
        inherited(&index, "Child", "getId"),
        FactStatus::Established(expected)
    );
}

#[test]
fn duplicate_owner_declarations_do_not_create_a_unique_inherited_fact() {
    let index = provider(&[
        (
            "RootOne.kt",
            "package inherited\ninterface Root {\n    fun getId(): String\n}\n",
        ),
        (
            "RootTwo.kt",
            "package inherited\ninterface Root {\n    fun getId(): String\n}\n",
        ),
        ("Child.kt", "package inherited\nclass Child : Root\n"),
    ]);
    assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);
}

#[test]
fn private_static_and_changed_jvm_name_members_are_not_callable_facts() {
    let private = r#"package inherited
open class Root {
    private fun getId(): String {
        return "root"
    }
}
class Child : Root()
"#;
    let index = provider(&[("Private.kt", private)]);
    assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);

    let renamed = r#"package inherited
open class Root {
    @kotlin.jvm.JvmName("renamed")
    open fun getId(): String {
        return "root"
    }
}
class Child : Root()
"#;
    let index = provider(&[("Renamed.kt", renamed)]);
    assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);

    let java_static = r#"package inherited;
public class Root {
    public static String getId() {
        return "root";
    }
}
"#;
    let index = provider(&[
        ("Root.java", java_static),
        ("Child.kt", "package inherited\nclass Child : Root()\n"),
    ]);
    assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);
}

#[test]
fn incomparable_sibling_methods_remain_ambiguous() {
    let source = r#"package inherited
interface Left {
    fun getId(): String
}
interface Right {
    fun getId(): String
}
class Child : Left, Right
"#;
    let index = provider(&[("Siblings.kt", source)]);
    assert!(matches!(
        inherited(&index, "Child", "getId"),
        FactStatus::Ambiguous(candidates) if candidates.len() == 2
    ));
}

#[test]
fn cyclic_ancestry_does_not_prove_a_member() {
    let source = r#"package inherited
interface Left : Right
interface Right : Left
class Child : Left
"#;
    let index = provider(&[("Cycle.kt", source)]);
    assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);
}

#[test]
fn same_file_duplicate_callable_ids_are_not_collapsed_to_one_fact() {
    let source = r#"package inherited
interface Root {
    fun getId(): String
    fun getId(): String
}
class Child : Root
"#;
    let index = provider(&[("DuplicateMethod.kt", source)]);
    assert!(matches!(
        inherited(&index, "Child", "getId"),
        FactStatus::Unknown | FactStatus::Ambiguous(_)
    ));
}

#[test]
fn child_property_shadow_blocks_inherited_callable_fact() {
    let source = r#"package inherited
interface Root {
    fun getId(): String
}
class Child : Root {
    val getId: () -> String = { "property" }
}
"#;
    let index = provider(&[("PropertyShadow.kt", source)]);
    assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);
}

#[test]
fn unbounded_generic_return_is_not_a_stable_callable_fact() {
    let source = r#"package inherited
interface Root {
    fun <T> getId(): T
}
class Child : Root
"#;
    let index = provider(&[("GenericReturn.kt", source)]);
    assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);
}

#[test]
fn required_call_planning_uses_ancestor_noarg_method_past_required_overload() {
    let base = r#"package base
interface Root {
    fun getId(): String
}
interface Middle : Root {
    fun <T> getId(clazz: Class<T>): T
}
"#;
    let caller = r#"package consumer
import base.Middle
class Child : Middle {
    fun read(): String {
        return getId()
    }
}
"#;
    let index = provider(&[("Base.kt", base), ("Caller.kt", caller)]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(caller, None).unwrap();
    let path = Path::new("Caller.kt");
    let mut plan = notlin::translation_plan::analyze(caller, tree.root_node(), path);
    plan_required_calls(caller, &tree, path, &index, false, &mut plan);
    let dependencies = plan
        .dependencies
        .iter()
        .filter(|dependency| dependency.spelling == "getId")
        .collect::<Vec<_>>();
    assert_eq!(dependencies.len(), 1);
    assert_eq!(
        dependencies[0].resolution,
        FactStatus::Established(
            index
                .symbols_named("getId")
                .into_iter()
                .find(|symbol| symbol.id.package == "base" && symbol.id.parameters.is_empty())
                .unwrap()
                .id
                .clone()
        )
    );
}

#[test]
fn required_call_planning_does_not_cross_implicit_receiver_scopes() {
    let source = r#"package inherited
open class Root {
    open fun getId(): String {
        return "root"
    }
}
open class Middle : Root() {
    fun getId(clazz: Class<*>): String {
        return "overload"
    }
}
class Box {
    fun getId(): String {
        return "box"
    }
}
class Child : Middle() {
    fun read(box: Box): String {
        return box.run { getId() }
    }
    fun Box.readFromBox(): String {
        return getId()
    }
}
"#;
    let index = provider(&[("ImplicitReceivers.kt", source)]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let path = Path::new("ImplicitReceivers.kt");
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), path);
    plan_required_calls(source, &tree, path, &index, false, &mut plan);
    let dependencies = plan
        .dependencies
        .iter()
        .filter(|dependency| dependency.spelling == "getId")
        .collect::<Vec<_>>();
    assert_eq!(dependencies.len(), 2);
    assert!(
        dependencies
            .iter()
            .all(|dependency| dependency.resolution == FactStatus::Unknown),
        "implicit receiver calls must not bind to Child's farther inherited method: {dependencies:?}"
    );
}

#[test]
fn java_checked_throws_do_not_prove_an_inherited_noarg_call() {
    let java = r#"package inherited;
public class Root {
    public String getId() throws java.io.IOException {
        return "root";
    }
}
"#;
    let index = provider(&[
        ("Root.java", java),
        ("Child.kt", "package inherited\nclass Child : Root()\n"),
    ]);
    assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);
}

#[test]
fn java_generic_return_does_not_prove_an_inherited_noarg_call() {
    let java = r#"package inherited;
public class Root {
    public <T> T getId() {
        return null;
    }
}
"#;
    let index = provider(&[
        ("Root.java", java),
        ("Child.kt", "package inherited\nclass Child : Root()\n"),
    ]);
    assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);
}

#[test]
fn java_primitive_return_survives_a_required_generic_overload() {
    let java = r#"package inherited;
public class Root {
    public int getId() {
        return 7;
    }
}
"#;
    let middle = r#"package inherited
open class Middle : Root() {
    fun <T> getId(clazz: Class<T>): T {
        return TODO()
    }
}
"#;
    let child = "package inherited\nclass Child : Middle()\n";
    let index = provider(&[
        ("Root.java", java),
        ("Middle.kt", middle),
        ("Child.kt", child),
    ]);
    assert_eq!(
        inherited(&index, "Child", "getId"),
        FactStatus::Established(member(&index, "Root", "getId"))
    );
}

#[test]
fn java_platform_return_does_not_invent_a_nonnull_call_contract() {
    let index = provider(&[
        (
            "Root.java",
            "package inherited;\npublic class Root {\n    public String getId() {\n        return null;\n    }\n}\n",
        ),
        ("Child.kt", "package inherited\nclass Child : Root()\n"),
    ]);
    assert_eq!(inherited(&index, "Child", "getId"), FactStatus::Unknown);
}

#[test]
fn constructor_headers_cannot_bind_an_inherited_instance_method() {
    let parent = r#"package inherited
open class Root {
    open fun getId(): String = "root"
}
open class Middle(initial: String = "") : Root() {
    fun getId(clazz: Class<*>): String = "overload"
}
fun getId(): String = "factory"
"#;
    for header in [
        "class Child(initial: String = getId()) : Middle(initial)",
        "class Child : Middle(getId())",
    ] {
        let source = format!("{parent}{header}\n");
        let index = provider(&[("ConstructorScope.kt", &source)]);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(&source, None).unwrap();
        let path = Path::new("ConstructorScope.kt");
        let mut plan = notlin::translation_plan::analyze(&source, tree.root_node(), path);
        plan_required_calls(&source, &tree, path, &index, false, &mut plan);
        let calls = plan
            .dependencies
            .iter()
            .filter(|dependency| dependency.spelling == "getId")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1, "{source}\n{calls:?}");
        assert_eq!(calls[0].resolution, FactStatus::Unknown, "{source}");
    }
}
