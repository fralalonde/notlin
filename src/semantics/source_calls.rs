//! Syntax facts specific to invoking retained Kotlin source methods from Java.

/// A Kotlin Exceptions attribute needs a separate Java catch/throws plan. Until
/// that exists, even unchecked-only @Throws lists do not prove a plain call.
pub(super) fn declares_checked_throws(source: &str, node: tree_sitter::Node<'_>) -> bool {
    let mut pending = node
        .named_children(&mut node.walk())
        .filter(|child| child.kind() == "modifiers")
        .collect::<Vec<_>>();
    while let Some(modifier) = pending.pop() {
        if matches!(modifier.kind(), "annotation" | "annotation_entry") {
            let name = modifier
                .utf8_text(source.as_bytes())
                .unwrap_or("")
                .trim()
                .trim_start_matches('@')
                .split(|character: char| character == '(' || character.is_whitespace())
                .next()
                .unwrap_or("");
            if name.rsplit('.').next() == Some("Throws")
                || super::kotlin_import_lines(source).any(|import| {
                    import.split_once(" as ").is_some_and(|(path, alias)| {
                        alias.trim() == name && path.trim().rsplit('.').next() == Some("Throws")
                    })
                })
            {
                return true;
            }
            continue;
        }
        pending.extend(modifier.named_children(&mut modifier.walk()));
    }
    false
}

/// Classify only explicit reference return declarations. Java's unannotated
/// reference return is platform-nullable even when its parsed TypeRef carries
/// `nullable: false`.
pub(super) fn reference_return_nullability(
    source: &str,
    node: tree_sitter::Node<'_>,
    ty: &super::FactStatus<super::TypeRef>,
) -> Option<super::ReferenceReturnNullability> {
    use super::{FactStatus, KotlinType, ReferenceReturnNullability};

    if !matches!(node.kind(), "function_declaration" | "method_declaration")
        || super::own_type_parameters(node).is_some()
        || super::has_type_constraints(node)
    {
        return None;
    }
    let FactStatus::Established(ty) = ty else {
        return None;
    };
    let KotlinType::Named { name, arguments } = &ty.ty else {
        return None;
    };
    if name.is_empty() || name.starts_with("T:") || !arguments.is_empty() {
        return None;
    }

    if node.kind() == "method_declaration" {
        let modifiers = node
            .named_children(&mut node.walk())
            .find(|child| child.kind() == "modifiers");
        let has_modifier = |keyword: &str| {
            modifiers.is_some_and(|modifiers| {
                modifiers
                    .children(&mut modifiers.walk())
                    .any(|child| child.kind() == keyword)
            })
        };
        if has_modifier("static")
            || has_modifier("private")
            || has_modifier("protected")
            || !(has_modifier("public")
                || node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "interface_body"))
        {
            return None;
        }
        if super::declaration_type_node(node)
            .and_then(|ty| ty.utf8_text(source.as_bytes()).ok())
            .is_none_or(|ty| ty.contains('@'))
        {
            // Qualified/type-use annotations need their own enhancement
            // analysis; do not silently classify an ignored @NotNull as
            // a platform return and change its behavior on null.
            return None;
        }
        if java_method_declares_throws(source, node) {
            return None;
        }
        if java_enclosing_type_has_annotations(node) {
            // Type-level nullness defaults can affect every unannotated
            // method. We do not interpret defaults, so retain instead.
            return None;
        }
        return java_method_return_nullability(source, node);
    }

    if declares_checked_throws(source, node) {
        return None;
    }
    Some(if ty.nullable {
        ReferenceReturnNullability::Nullable
    } else {
        ReferenceReturnNullability::NonNull
    })
}

/// Package-info annotations can establish a package-wide nullness default.
/// This deliberately rejects any package annotation rather than guessing
/// which annotation libraries or defaults are active.
pub(super) fn annotated_java_package(source: &str) -> Option<String> {
    // package-info files are rare. Treat any annotation marker as uncertain,
    // including annotations sharing a line with the package declaration.
    if !source.contains('@') {
        return None;
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let package = tree
        .root_node()
        .named_children(&mut tree.root_node().walk())
        .find(|node| node.kind() == "package_declaration")?;
    let name = package
        .named_children(&mut package.walk())
        .find(|node| matches!(node.kind(), "identifier" | "scoped_identifier"))?;
    Some(
        name.utf8_text(source.as_bytes())
            .ok()?
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect(),
    )
}

pub(super) fn java_enclosing_type_has_annotations(mut node: tree_sitter::Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "class_declaration"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
        ) {
            let mut cursor = parent.walk();
            if parent.named_children(&mut cursor).any(|child| {
                child.kind() == "modifiers"
                    && child.named_children(&mut child.walk()).any(|modifier| {
                        matches!(modifier.kind(), "annotation" | "marker_annotation")
                    })
            }) {
                return true;
            }
        }
        node = parent;
    }
    false
}

pub(super) fn java_method_declares_throws(source: &str, node: tree_sitter::Node<'_>) -> bool {
    let end = node
        .child_by_field_name("body")
        .map_or(node.end_byte(), |body| body.start_byte());
    source.get(node.start_byte()..end).is_none_or(|header| {
        header
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .any(|word| word == "throws")
    })
}

pub(super) fn java_method_return_nullability(
    source: &str,
    node: tree_sitter::Node<'_>,
) -> Option<super::ReferenceReturnNullability> {
    use super::ReferenceReturnNullability;

    let modifiers = node
        .named_children(&mut node.walk())
        .find(|child| child.kind() == "modifiers");
    let Some(modifiers) = modifiers else {
        return Some(ReferenceReturnNullability::Platform);
    };
    let mut result = ReferenceReturnNullability::Platform;
    let mut has_nullness = false;
    let mut cursor = modifiers.walk();
    for annotation in modifiers
        .named_children(&mut cursor)
        .filter(|child| matches!(child.kind(), "annotation" | "marker_annotation"))
    {
        let name = annotation
            .child_by_field_name("name")
            .or_else(|| {
                annotation
                    .named_children(&mut annotation.walk())
                    .find(|child| matches!(child.kind(), "identifier" | "scoped_identifier"))
            })
            .and_then(|name| name.utf8_text(source.as_bytes()).ok())?;
        match java_nullness_annotation(source, name.trim())? {
            JavaNullnessAnnotation::Override => {}
            JavaNullnessAnnotation::NonNull if !has_nullness => {
                result = ReferenceReturnNullability::NonNull;
                has_nullness = true;
            }
            JavaNullnessAnnotation::Nullable if !has_nullness => {
                result = ReferenceReturnNullability::Nullable;
                has_nullness = true;
            }
            JavaNullnessAnnotation::NonNull | JavaNullnessAnnotation::Nullable => return None,
        }
    }
    Some(result)
}

#[derive(Clone, Copy)]
enum JavaNullnessAnnotation {
    Override,
    NonNull,
    Nullable,
}

fn java_nullness_annotation(source: &str, spelling: &str) -> Option<JavaNullnessAnnotation> {
    const NOT_NULL: &str = "org.jetbrains.annotations.NotNull";
    const NULLABLE: &str = "org.jetbrains.annotations.Nullable";
    match spelling {
        NOT_NULL => return Some(JavaNullnessAnnotation::NonNull),
        NULLABLE => return Some(JavaNullnessAnnotation::Nullable),
        "java.lang.Override" => return Some(JavaNullnessAnnotation::Override),
        "Override" => {
            let conflicting_import = source
                .lines()
                .filter_map(|line| line.trim().strip_prefix("import "))
                .map(|import| import.trim_end_matches(';').trim())
                .any(|import| {
                    import.rsplit('.').next() == Some("Override") && import != "java.lang.Override"
                });
            return (!conflicting_import).then_some(JavaNullnessAnnotation::Override);
        }
        _ => {}
    }
    let canonical = match spelling {
        "NotNull" => NOT_NULL,
        "Nullable" => NULLABLE,
        _ => return None,
    };
    let matching_imports = source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("import "))
        .map(|import| import.trim_end_matches(';').trim())
        .filter(|import| import.rsplit('.').next() == Some(spelling))
        .collect::<Vec<_>>();
    matches!(matching_imports.as_slice(), [import] if *import == canonical).then_some(
        if spelling == "NotNull" {
            JavaNullnessAnnotation::NonNull
        } else {
            JavaNullnessAnnotation::Nullable
        },
    )
}
