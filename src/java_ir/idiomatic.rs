//! Presentation cleanup for generated compilation units. Parsing and rendering
//! themselves remain lossless; this pass changes only imports and whitespace.

use super::{JavaCompilationUnit, JavaElement, JavaSyntaxNode, JavaTokenClass};
use std::collections::{BTreeSet, HashSet};

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct Import {
    is_static: bool,
    path: String,
}

fn import(node: &JavaSyntaxNode) -> Option<Import> {
    let mut tokens = Vec::new();
    super::collect_import_tokens(node, &mut tokens);
    let mut result = Import {
        is_static: false,
        path: String::new(),
    };
    for token in tokens {
        match token.text.as_str() {
            "" | "import" | ";" => {}
            "static" => result.is_static = true,
            "." | "*" => result.path.push_str(&token.text),
            _ if token.class == JavaTokenClass::Identifier => result.path.push_str(&token.text),
            _ => return None,
        }
    }
    (!result.path.is_empty()).then_some(result)
}

/// Apply to complete, accepted generated units, after helper injection. Neither
/// comments nor literals are rewritten. Wildcard imports retain their binding
/// behavior; exact imports are pruned only when their name has no syntax use.
pub fn tidy_generated_java(unit: &mut JavaCompilationUnit) {
    let mut used = HashSet::new();
    super::collect_identifiers(&unit.root, false, &mut used);
    collect_documentation_names(&unit.root, &mut used);
    let mut seen = BTreeSet::new();
    let trailing_comments = (0..unit.root.children.len())
        .map(|index| has_trailing_comment(&unit.root.children, index))
        .collect::<Vec<_>>();
    let mut index = 0;
    unit.root.children.retain(|element| {
        let attached_comment = trailing_comments[index];
        index += 1;
        let JavaElement::Import(node) = element else {
            return true;
        };
        if attached_comment || super::has_comment(node) {
            return true;
        }
        let Some(import) = import(node) else {
            return true;
        };
        let name = import.path.rsplit('.').next().unwrap_or("");
        (name == "*" || used.contains(name)) && seen.insert(import)
    });

    // A comment attached to or between imports is a sorting boundary. Moving
    // imports across it could detach a license, explanation, or suppression.
    let mut start = 0;
    while start < unit.root.children.len() {
        if sortable_import(&unit.root.children[start]).is_none()
            || has_trailing_comment(&unit.root.children, start)
        {
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < unit.root.children.len()
            && sortable_import(&unit.root.children[end]).is_some()
            && !has_trailing_comment(&unit.root.children, end)
        {
            end += 1;
        }
        unit.root.children[start..end]
            .sort_by_cached_key(|element| sortable_import(element).unwrap());
        let mut previous_static = None;
        for index in start..end {
            let key = sortable_import(&unit.root.children[index]).unwrap();
            let gap = if previous_static.is_some_and(|previous| previous != key.is_static)
                || (index == start && index > 0 && !is_comment(&unit.root.children[index - 1]))
            {
                "\n\n"
            } else if index == 0 {
                ""
            } else {
                "\n"
            };
            let JavaElement::Import(node) = &mut unit.root.children[index] else {
                unreachable!()
            };
            set_leading_trivia(node, gap);
            previous_static = Some(key.is_static);
        }
        start = end;
    }
    let mut pending = String::new();
    let mut previous = String::new();
    tidy_trivia(&mut unit.root, &mut pending, &mut previous);
    unit.root.trailing_trivia = if pending.chars().all(char::is_whitespace) {
        "\n".into()
    } else {
        pending
    };
}

fn sortable_import(element: &JavaElement) -> Option<Import> {
    match element {
        JavaElement::Import(node) if !super::has_comment(node) => import(node),
        _ => None,
    }
}

fn is_comment(element: &JavaElement) -> bool {
    matches!(element, JavaElement::Token(token) if token.class == JavaTokenClass::Comment)
}

fn has_trailing_comment(elements: &[JavaElement], index: usize) -> bool {
    matches!(elements.get(index + 1), Some(JavaElement::Token(token))
        if token.class == JavaTokenClass::Comment && !token.leading_trivia.contains('\n'))
}

fn set_leading_trivia(node: &mut JavaSyntaxNode, trivia: &str) -> bool {
    for element in &mut node.children {
        if let JavaElement::Token(token) = element {
            if token.text.is_empty() {
                token.leading_trivia.clear();
                continue;
            }
            token.leading_trivia = trivia.into();
            return true;
        }
        if let Some(child) = element.node_mut()
            && set_leading_trivia(child, trivia)
        {
            return true;
        }
    }
    false
}

fn collect_documentation_names(node: &JavaSyntaxNode, names: &mut HashSet<String>) {
    for element in &node.children {
        match element {
            JavaElement::Token(token)
                if token.class == JavaTokenClass::Comment && token.text.starts_with("/**") =>
            {
                names.extend(
                    token
                        .text
                        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '$')
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned),
                );
            }
            JavaElement::Token(_) => {}
            JavaElement::Declaration(child)
            | JavaElement::Type(child)
            | JavaElement::Expression(child)
            | JavaElement::Statement(child)
            | JavaElement::Annotation(child)
            | JavaElement::Import(child)
            | JavaElement::Other(child) => collect_documentation_names(child, names),
        }
    }
}

fn tidy_trivia(node: &mut JavaSyntaxNode, pending: &mut String, previous: &mut String) {
    for element in &mut node.children {
        if let JavaElement::Token(token) = element {
            pending.push_str(&std::mem::take(&mut token.leading_trivia));
            if token.text.is_empty() {
                continue;
            }
            let max_lines = if token.text == "}" || previous == "{" {
                1
            } else {
                2
            };
            if pending.chars().all(char::is_whitespace)
                && pending.bytes().filter(|b| *b == b'\n').count() > max_lines
            {
                let indentation = pending.rsplit('\n').next().unwrap_or("").to_owned();
                *pending = "\n".repeat(max_lines) + &indentation;
            }
            token.leading_trivia = std::mem::take(pending);
            previous.clone_from(&token.text);
        } else if let Some(child) = element.node_mut() {
            tidy_trivia(child, pending, previous);
        }
    }
    pending.push_str(&std::mem::take(&mut node.trailing_trivia));
}
