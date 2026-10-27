use notlin::semantics::SyntaxSemanticProvider;
use notlin::translation_plan::TranslationPlan;
use std::path::{Path, PathBuf};

fn provider(files: &[(&str, &str)]) -> SyntaxSemanticProvider {
    for (path, source) in files {
        let language = match Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some("kt") => tree_sitter_kotlin_ng::LANGUAGE.into(),
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

fn plan(source: &str, file: &str, provider: &SyntaxSemanticProvider) -> TranslationPlan {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let path = Path::new(file);
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), path);
    notlin::semantics::plan_syntax_lowerings(source, &tree, path, provider, &mut plan);
    plan
}

fn assert_no_todos(source: &str, file: &str) {
    let index = provider(&[(file, source)]);
    let plan = plan(source, file, &index);
    assert!(
        plan.todo_lowerings.is_empty(),
        "unsafe TODO lowering was planned for {file}: {:#?}",
        plan.todo_lowerings
    );
}

fn assert_planned_locations(source: &str, file: &str, expected: &[&str]) {
    let index = provider(&[(file, source)]);
    let plan = plan(source, file, &index);
    assert_eq!(plan.todo_lowerings.len(), expected.len());
    for (lowering, expected_text) in plan.todo_lowerings.iter().zip(expected) {
        assert_eq!(lowering.location.file, Path::new(file));
        assert_eq!(
            &source[lowering.location.start_byte..lowering.location.end_byte],
            *expected_text
        );
        assert_eq!(
            lowering.location.snapshot_hash,
            *blake3::hash(source.as_bytes()).as_bytes()
        );
    }
}

#[test]
fn exact_unqualified_stdlib_todo_calls_are_planned_in_typed_function_returns() {
    let file = "TodoReturns.kt";
    let source = r#"package todoplan
fun missing(): String = TODO()
fun missingWithReason(): String = TODO("later")
fun missingBlock(): String {
    return TODO()
}
"#;
    assert_planned_locations(source, file, &["TODO()", "TODO(\"later\")", "TODO()"]);
}

#[test]
fn known_jdk_wildcards_do_not_shadow_kotlin_todo() {
    let file = "TodoJdkWildcards.kt";
    let source = r#"package todoplan
import java.util.*
import java.lang.*
fun missing(): String = TODO()
"#;
    assert_planned_locations(source, file, &["TODO()"]);
}

#[test]
fn standalone_last_todo_in_typed_function_body_is_planned() {
    let file = "TodoStatement.kt";
    let source = "package todoplan\ninterface Api {\n    fun missing(): String {\n        TODO()\n    }\n}\n";
    assert_planned_locations(source, file, &["TODO()"]);
}

#[test]
fn typed_property_getter_may_return_a_direct_todo_call() {
    let file = "TodoGetter.kt";
    let source = r#"package todoplan
class Holder {
    val missing: String
        get() = TODO("not implemented")
}
"#;
    assert_planned_locations(source, file, &["TODO(\"not implemented\")"]);
}

#[test]
fn unknown_wildcard_import_keeps_todo_unresolved() {
    let source = r#"package todoplan
import example.extensions.*
fun missing(): String = TODO()
"#;
    assert_no_todos(source, "TodoUnknownWildcard.kt");
}

#[test]
fn explicit_and_aliased_custom_todo_imports_are_not_rewritten_as_stdlib() {
    let custom = r#"package custom
fun TODO(): String = "custom"
"#;
    let explicit = r#"package consumer
import custom.TODO
fun value(): String = TODO()
"#;
    let index = provider(&[("Custom.kt", custom), ("Explicit.kt", explicit)]);
    assert!(
        plan(explicit, "Explicit.kt", &index)
            .todo_lowerings
            .is_empty()
    );

    let aliased = r#"package consumer
import custom.TODO as TODO
fun value(): String = TODO()
"#;
    let alias_index = provider(&[("Custom.kt", custom), ("Aliased.kt", aliased)]);
    assert!(
        plan(aliased, "Aliased.kt", &alias_index)
            .todo_lowerings
            .is_empty()
    );
}

#[test]
fn local_member_companion_and_same_package_todo_declarations_shadow_stdlib() {
    let local = r#"package todoplan
fun value(): String {
    fun TODO(): String = "local"
    return TODO()
}
"#;
    assert_no_todos(local, "TodoLocalShadow.kt");

    let member = r#"package todoplan
class Holder {
    fun TODO(): String = "member"
    fun value(): String = TODO()
}
"#;
    assert_no_todos(member, "TodoMemberShadow.kt");

    let companion = r#"package todoplan
class Holder {
    companion object {
        fun TODO(): String = "companion"
    }
    fun value(): String = TODO()
}
"#;
    assert_no_todos(companion, "TodoCompanionShadow.kt");

    let same_package = r#"package todoplan
fun TODO(): String = "package"
fun value(): String = TODO()
"#;
    assert_no_todos(same_package, "TodoSamePackageShadow.kt");
}

#[test]
fn inherited_or_ambiguous_todo_members_are_not_treated_as_stdlib() {
    let inherited = r#"package todoplan
open class Parent {
    fun TODO(): String = "parent"
}
class Child : Parent() {
    fun value(): String = TODO()
}
"#;
    assert_no_todos(inherited, "TodoInheritedShadow.kt");

    let ambiguous = r#"package todoplan
interface First {
    fun TODO(): String
}
interface Second {
    fun TODO(): String
}
class Child : First, Second {
    fun value(): String = TODO()
}
"#;
    assert_no_todos(ambiguous, "TodoAmbiguousShadow.kt");
}

#[test]
fn lambda_extension_nested_and_initializer_todo_calls_are_not_planned() {
    let nested_lambda = r#"package todoplan
fun <T> use(block: () -> T): T = block()
fun value(): String = use {
    TODO()
}
"#;
    assert_no_todos(nested_lambda, "TodoNestedLambda.kt");

    let extension = r#"package todoplan
class Holder
fun Holder.value(): String = TODO()
"#;
    assert_no_todos(extension, "TodoExtension.kt");

    let nested_argument = r#"package todoplan
fun wrap(value: String): String = value
fun value(): String = wrap(TODO())
"#;
    assert_no_todos(nested_argument, "TodoNestedArgument.kt");

    let initializer = r#"package todoplan
val value: String = TODO()
"#;
    assert_no_todos(initializer, "TodoInitializer.kt");
}

#[test]
fn unsupported_todo_argument_shapes_are_not_planned() {
    let source = r#"package todoplan
fun value(): String = TODO("first", "second")
fun named(): String = TODO(message = "custom")
fun spread(values: Array<String>): String = TODO(*values)
fun typed(): String = TODO<String>()
"#;
    assert_no_todos(source, "TodoUnsupportedShape.kt");
}

#[test]
fn stale_provider_cannot_plan_todo_in_a_changed_source_snapshot() {
    let file = "TodoStale.kt";
    let source = r#"package todoplan
fun value(): String = TODO()
"#;
    let index = provider(&[(file, source)]);
    let changed = source.replace("TODO()", "TODO(\"changed\")");
    let changed_plan = plan(&changed, file, &index);
    assert!(
        changed_plan.todo_lowerings.is_empty(),
        "a provider indexed from an older source snapshot must not authorize TODO lowering"
    );
}

#[test]
fn bottom_return_metadata_is_not_replaced_by_a_java_void_contract() {
    assert_no_todos(
        "package todoplan\nfun missing(): Nothing = TODO()\n",
        "BottomFunction.kt",
    );
    assert_no_todos(
        "package todoplan\nclass Holder {\n val missing: Nothing\n  get() = TODO()\n}\n",
        "BottomProperty.kt",
    );
}

#[test]
fn inherited_java_callable_fields_and_unknown_generated_fields_block_stdlib_todo() {
    let caller = "package todoplan\nimport base.Parent\nclass Child : Parent {\n fun missing(): String = TODO()\n}\n";
    for (parent, accepted) in [
        ("package base; public interface Parent {}", true),
        (
            "package base; public interface Parent { kotlin.jvm.functions.Function0<String> TODO = () -> \"custom\"; }",
            false,
        ),
        ("package base; @Generated public interface Parent {}", false),
        (
            "package base; public interface Parent { static Object getTODO() { return null; } }",
            false,
        ),
    ] {
        let index = provider(&[("Parent.java", parent), ("Child.kt", caller)]);
        assert_eq!(
            !plan(caller, "Child.kt", &index).todo_lowerings.is_empty(),
            accepted,
            "{parent}"
        );
    }
}
