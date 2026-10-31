//! Resolve a Kotlin selector's explicit import before applying a stdlib rule.
//! Absent imports and foreign imports are different facts; aliases retain the
//! canonical callable name, including quoted identifiers and commented imports.

use tree_sitter::Node;

pub(super) fn path(source: &str, import: Node<'_>) -> Option<String> {
    let path = import
        .named_children(&mut import.walk())
        .find(|node| node.kind() == "qualified_identifier")?;
    Some(
        path.named_children(&mut path.walk())
            .filter(|node| node.kind() == "identifier")
            .map(|node| {
                node.utf8_text(source.as_bytes())
                    .ok()
                    .map(|name| name.trim_matches('`'))
            })
            .collect::<Option<Vec<_>>>()?
            .join("."),
    )
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Binding {
    Absent,
    Standard(String),
    Foreign,
}

pub(super) fn binding(
    source: &str,
    mut context: Node<'_>,
    spelling: &str,
    package: &str,
) -> Binding {
    while let Some(parent) = context.parent() {
        context = parent;
    }
    let spelling = spelling.trim_matches('`');
    let mut result = Binding::Absent;
    for import in context
        .named_children(&mut context.walk())
        .filter(|node| node.kind() == "import")
    {
        let Some(path) = import
            .named_children(&mut import.walk())
            .find(|node| node.kind() == "qualified_identifier")
        else {
            continue;
        };
        let names = path
            .named_children(&mut path.walk())
            .filter(|node| node.kind() == "identifier")
            .filter_map(|node| node.utf8_text(source.as_bytes()).ok())
            .map(|name| name.trim_matches('`'))
            .collect::<Vec<_>>();
        let Some((name, prefix)) = names.split_last() else {
            continue;
        };
        let alias = import
            .named_children(&mut import.walk())
            .find(|node| node.kind() == "identifier")
            .and_then(|node| node.utf8_text(source.as_bytes()).ok())
            .map(|name| name.trim_matches('`'));
        if alias.unwrap_or(name) != spelling
            || import
                .children(&mut import.walk())
                .any(|node| node.kind() == "*")
        {
            continue;
        }
        let candidate = if prefix.join(".") == package {
            Binding::Standard((*name).to_owned())
        } else {
            Binding::Foreign
        };
        if result != Binding::Absent && result != candidate {
            return Binding::Foreign;
        }
        result = candidate;
    }
    result
}
