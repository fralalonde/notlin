//! Distinguish Kotlin KClass values from their explicit Java Class projection.

use super::{FactStatus, SemanticProvider};
use std::path::Path;

/// An annotation class literal denotes a reference classifier, not a runtime
/// KClass value. Source classifiers use the normal scoped resolver. An
/// explicit import also establishes the spelling of a dependency classifier
/// (including nested classes) without requiring its sources in the workspace.
pub(crate) fn annotation_reference_class(
    source: &str,
    file: &Path,
    site: tree_sitter::Node<'_>,
    spelling: &str,
    provider: Option<&dyn SemanticProvider>,
) -> Option<String> {
    let root = spelling.split('.').next()?;
    let package = super::package_name(source);
    if root.is_empty()
        || super::is_proven_local_shadow(site, root, source)
        || super::is_proven_constructor_property_shadow(site, root, source)
        || super::declaration_type_parameters(source, site)
            .iter()
            .any(|name| name == root)
        || provider.is_some_and(|provider| {
            provider.has_type_alias_in_package(&package, root)
                || provider
                    .symbols_named(root)
                    .iter()
                    .any(|symbol| symbol.id.file == file && !super::is_type_symbol(&symbol.id))
        })
    {
        return None;
    }
    let imports = super::kotlin_import_lines(source)
        .filter_map(|import| {
            let (path, alias) = import
                .split_once(" as ")
                .map_or((import, None), |(path, alias)| {
                    (path.trim(), Some(alias.trim()))
                });
            (alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path)) == root)
                .then_some((path, alias))
        })
        .collect::<Vec<_>>();
    if imports.len() > 1 {
        return None;
    }
    if let Some((path, alias)) = imports.first() {
        let (namespace, name) = path.rsplit_once('.')?;
        if provider.is_some_and(|provider| provider.has_type_alias_in_package(namespace, name)) {
            return None;
        }
        return Some(if alias.is_some() {
            format!("{path}{}", &spelling[root.len()..])
        } else {
            spelling.to_owned()
        });
    }
    let provider = provider?;
    match provider.resolve_type_in_file(file, &package, spelling) {
        FactStatus::Established(id) if super::is_type_symbol(&id) => Some(spelling.to_owned()),
        _ => None,
    }
}

pub(crate) fn established_default_companion_call(
    source: &str,
    file: &Path,
    call: tree_sitter::Node<'_>,
    navigation: tree_sitter::Node<'_>,
    provider: &dyn SemanticProvider,
) -> bool {
    let Some(member) = navigation.named_children(&mut navigation.walk()).last() else {
        return false;
    };
    let Ok(name) = member.utf8_text(source.as_bytes()) else {
        return false;
    };
    matches!(
        super::resolve_required_call(source, file, call, member, name, provider),
        FactStatus::Established(target) if provider.has_default_companion_jvm_abi(&target)
    )
}

pub(crate) fn contains_unlowerable_class_literal(
    source: &str,
    file: &Path,
    node: tree_sitter::Node<'_>,
    provider: Option<&dyn SemanticProvider>,
) -> bool {
    contains_kotlin_class_literal(source, node, &|receiver| {
        let Some(provider) = provider else {
            return false;
        };
        let spelling = receiver.utf8_text(source.as_bytes()).unwrap_or("").trim();
        let root = spelling.split('.').next().unwrap_or("");
        let package = super::package_name(source);
        if spelling.is_empty()
            || !spelling
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.'))
            || super::is_proven_local_shadow(receiver, root, source)
            || super::is_proven_constructor_property_shadow(receiver, root, source)
            || super::declaration_type_parameters(source, receiver)
                .iter()
                .any(|name| name == root)
            || provider.has_type_alias_in_package(&package, root)
            || provider
                .symbols_named(root)
                .iter()
                .any(|symbol| symbol.id.file == file && !super::is_type_symbol(&symbol.id))
            || !provider.symbols_named("java").is_empty()
            || super::kotlin_import_lines(source).any(|import| {
                let (path, visible) = import
                    .split_once(" as ")
                    .map(|(path, alias)| (path.trim(), alias.trim()))
                    .unwrap_or_else(|| (import, import.rsplit('.').next().unwrap_or(import)));
                (visible == "java" && path != "kotlin.jvm.java")
                    || (import.contains(" as ") && visible == root)
            })
        {
            return false;
        }
        // Only an indexed, top-level reference classifier has an established
        // name for this lowering. Primitive, instance, generic, alias, nested
        // and unknown external literals need additional lowering evidence.
        matches!(
            provider.resolve_type_in_file(file, &package, spelling),
            FactStatus::Established(id) if super::is_type_symbol(&id) && id.owner_path.is_empty()
        )
    })
}

fn unparenthesized(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    while node.kind() == "parenthesized_expression" {
        let Some(inner) = node
            .named_children(&mut node.walk())
            .find(|child| !child.is_extra())
        else {
            break;
        };
        node = inner;
    }
    node
}

fn class_literal_receiver<'tree>(
    node: tree_sitter::Node<'tree>,
    source: &str,
) -> Option<tree_sitter::Node<'tree>> {
    if !matches!(node.kind(), "navigation_expression" | "callable_reference") {
        return None;
    }
    let children = node
        .children(&mut node.walk())
        .filter(|child| !child.is_extra())
        .collect::<Vec<_>>();
    match children.as_slice() {
        [receiver, separator, member]
            if separator.kind() == "::"
                && member.utf8_text(source.as_bytes()).ok() == Some("class") =>
        {
            Some(*receiver)
        }
        _ => None,
    }
}

/// A bare class literal cannot be passed to retained Kotlin as a Java Class.
/// This is a syntax distinction, not proof of a callable's formal parameter
/// type or of the Java interop extension's resolution. Those remain separate
/// required semantic facts. In particular, no text inside a string or comment
/// is treated as a class literal.
fn contains_kotlin_class_literal<'tree>(
    source: &str,
    node: tree_sitter::Node<'tree>,
    can_lower_projection: &impl Fn(tree_sitter::Node<'tree>) -> bool,
) -> bool {
    if node.kind() == "navigation_expression" {
        let children = node
            .children(&mut node.walk())
            .filter(|child| !child.is_extra())
            .collect::<Vec<_>>();
        if let [receiver, separator, member] = children.as_slice()
            && separator.kind() == "."
            && member.utf8_text(source.as_bytes()).ok() == Some("java")
            && let Some(class_receiver) = class_literal_receiver(unparenthesized(*receiver), source)
        {
            // Only the projected literal is consumed. An expression receiver
            // might itself contain another, unconverted KClass argument.
            return !can_lower_projection(class_receiver)
                || contains_kotlin_class_literal(source, class_receiver, can_lower_projection);
        }
    }
    class_literal_receiver(node, source).is_some()
        || node
            .named_children(&mut node.walk())
            .any(|child| contains_kotlin_class_literal(source, child, can_lower_projection))
}

#[cfg(test)]
mod tests {
    use super::contains_kotlin_class_literal;

    #[test]
    fn companion_abi_facts_require_an_ordinary_holder_name_and_parameter_shapes() {
        for (holder, declaration, expected) in [
            (
                "companion object",
                "fun choose(type: Class<*>, value: Any?): String = \"ok\"",
                true,
            ),
            (
                "companion object Named",
                "fun choose(type: Class<*>): String = \"ok\"",
                false,
            ),
            (
                "companion object",
                "@kotlin.jvm.JvmName(\"renamed\") fun choose(type: Class<*>): String = \"ok\"",
                false,
            ),
            (
                "companion object",
                "@Renamed(\"renamed\") fun choose(type: Class<*>): String = \"ok\"",
                false,
            ),
            (
                "companion object",
                "private fun choose(type: Class<*>): String = \"ok\"",
                false,
            ),
            (
                "companion object",
                "fun <T> choose(type: Class<T>): String = \"ok\"",
                false,
            ),
            (
                "companion object",
                "fun choose(type: Class<*>, value: Token): String = \"ok\"",
                false,
            ),
            (
                "companion object",
                "fun choose(type: Class<*>, value: Alias): String = \"ok\"",
                false,
            ),
            (
                "companion object",
                "fun choose(type: Class<*>, value: java.io.File): String = \"ok\"",
                false,
            ),
        ] {
            let source = format!(
                "import kotlin.jvm.JvmName as Renamed\n@JvmInline value class Token(val value: Int)\ntypealias Alias = Token\nclass Factory {{\n {holder} {{\n  {declaration}\n }}\n}}\n"
            );
            let file = std::path::Path::new("Factory.kt");
            let provider = crate::semantics::SyntaxSemanticProvider::new([(
                file.to_path_buf(),
                source.clone(),
            )]);
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(&source, None).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            let methods = super::SemanticProvider::symbols_named(&provider, "choose");
            assert_eq!(methods.len(), 1, "{source}");
            assert_eq!(
                super::SemanticProvider::has_default_companion_jvm_abi(&provider, &methods[0].id),
                expected,
                "{source}"
            );
        }
    }

    #[test]
    fn projections_require_an_unshadowed_indexed_reference_classifier() {
        for (source, unsupported) in [
            (
                "enum class Choice { FIRST }\nfun make() = Factory.of(Choice::class.java)",
                false,
            ),
            (
                "enum class Choice { FIRST }\nfun make() = Factory.of((Choice::class).java)",
                false,
            ),
            ("fun make() = Factory.of(Int::class.java)", true),
            ("fun make(value: Any) = Factory.of(value::class.java)", true),
            (
                "enum class Choice { FIRST }\nfun make(Choice: Any) = Factory.of(Choice::class.java)",
                true,
            ),
            (
                "enum class Choice { FIRST }\ninline fun <reified Choice> make() = Factory.of(Choice::class.java)",
                true,
            ),
            (
                "enum class Choice { FIRST }\ntypealias Alias = Choice\nfun make() = Factory.of(Alias::class.java)",
                true,
            ),
            (
                "enum class Choice { FIRST }\nval kotlin.reflect.KClass<*>.java: kotlin.reflect.KClass<*> get() = this\nfun make() = Factory.of(Choice::class.java)",
                true,
            ),
            (
                "import custom.java\nenum class Choice { FIRST }\nfun make() = Factory.of(Choice::class.java)",
                true,
            ),
            (
                "import custom.project as java\nenum class Choice { FIRST }\nfun make() = Factory.of(Choice::class.java)",
                true,
            ),
            (
                "enum class Choice { FIRST }\nfun make() = Factory.of { Choice::class }",
                true,
            ),
        ] {
            let file = std::path::Path::new("Reference.kt");
            let provider = crate::semantics::SyntaxSemanticProvider::new([(
                file.to_path_buf(),
                source.to_owned(),
            )]);
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(source, None).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            assert_eq!(
                super::contains_unlowerable_class_literal(
                    source,
                    file,
                    tree.root_node(),
                    Some(&provider)
                ),
                unsupported,
                "{source}"
            );
        }
    }

    #[test]
    fn comments_do_not_count_as_callable_arguments() {
        for (expression, arity) in [
            ("Factory.of(/* argument */ Choice::class.java)", 1),
            ("Factory.of(Choice::class.java, /* default */ null)", 2),
            ("Factory.of(/* empty */)", 0),
        ] {
            let source = format!("fun probe() = {expression}\n");
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(&source, None).unwrap();
            assert!(!tree.root_node().has_error());
            let mut pending = vec![tree.root_node()];
            let call = loop {
                let node = pending.pop().unwrap();
                if node.kind() == "call_expression" {
                    break node;
                }
                pending.extend(node.named_children(&mut node.walk()));
            };
            assert_eq!(super::super::call_argument_count(call), arity, "{source}");
        }
    }

    #[test]
    fn java_projection_consumes_only_its_own_class_literal() {
        for (expression, expected) in [
            ("Choice::class.java", false),
            ("(Choice::class).java", false),
            ("((Choice::class.java))", false),
            ("Factory.of(Choice::class.java, Other::class.java)", false),
            ("Factory.of(Choice::class.java, Other::class)", true),
            ("Choice::class", true),
            ("Choice /* comment */ ::class", true),
            ("Choice::class /* comment */ .java", false),
            ("(/* comment */ Choice::class).java", false),
            ("Choice::class.javaObjectType", true),
            ("factory(Choice::class)::class.java", true),
            ("factory(Choice::class.java)::class.java", false),
            ("\"::class\"", false),
            ("\"${Choice::class}\"", true),
            ("\"${Choice::class.java}\"", false),
            ("Factory.of(/* Choice::class */ Choice::class.java)", false),
        ] {
            let source = format!("fun probe() = {expression}\n");
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
                .unwrap();
            let tree = parser.parse(&source, None).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            assert_eq!(
                contains_kotlin_class_literal(&source, tree.root_node(), &|_| true),
                expected,
                "{expression}"
            );
        }
    }
}
