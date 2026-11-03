//! Shared type-name and annotation helpers.

use crate::jvm_primitive::JvmPrimitive;
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AnnotationSet {
    Jetbrains,
    Jspecify,
    #[default]
    None,
}

impl AnnotationSet {
    #[allow(dead_code)]
    pub fn nullable(&self) -> Option<&'static str> {
        match self {
            AnnotationSet::Jetbrains => Some("@org.jetbrains.annotations.Nullable"),
            AnnotationSet::Jspecify => Some("@org.jspecify.annotations.Nullable"),
            AnnotationSet::None => None,
        }
    }
}

const NULLABLE_PREFIXES: [&str; 3] = [
    "@Nullable ",
    "@org.jetbrains.annotations.Nullable ",
    "@org.jspecify.annotations.Nullable ",
];

pub(crate) fn split_nullable_prefix(java_type: &str) -> (&str, &str) {
    NULLABLE_PREFIXES
        .iter()
        .find_map(|prefix| java_type.strip_prefix(prefix).map(|ty| (*prefix, ty)))
        .unwrap_or(("", java_type))
}

pub(crate) fn strip_nullable_prefix(java_type: &str) -> &str {
    split_nullable_prefix(java_type).1
}

pub(crate) fn erase_nullable_annotations(java_type: &str) -> String {
    NULLABLE_PREFIXES
        .iter()
        .fold(java_type.to_owned(), |ty, prefix| ty.replace(prefix, ""))
}

/// Nullability markers describe declarations. Older annotation jars do not
/// permit them in Java cast expressions, even when the declaration is nullable.
pub(crate) fn cast_type(java_type: &str) -> String {
    erase_nullable_annotations(java_type)
        .replace("@NotNull ", "")
        .replace("@NonNull ", "")
}

/// Map a Kotlin type name to its Java equivalent where they differ.
///
/// Kotlin's `Mutable*` collections are the same JVM types as the read-only
/// ones (`MutableList<T>` and `List<T>` both erase to `java.util.List`), so
/// their Java form is the java.util INTERFACE. Emitting the implementation
/// instead (`ArrayList`, `HashMap`, `HashSet`) changed the signature a Java
/// override has to reproduce — a Kotlin interface member declared
/// `fun addAll(items: MutableList<T>)` is `List` in the descriptor, and no
/// Java method taking `ArrayList` overrides it — and leaked a concrete type
/// into every emitted API. `MutableCollection`/`MutableIterator` had no entry
/// at all and reached Java as Kotlin names, which does not compile.
pub fn map_type_name(kotlin_type: &str) -> &str {
    let builtin = kotlin_type.strip_prefix("kotlin.").unwrap_or(kotlin_type);
    if let Some(primitive) = JvmPrimitive::kotlin(builtin) {
        return primitive.java;
    }
    if let Some(primitive) = builtin.strip_suffix("Array").and_then(JvmPrimitive::kotlin) {
        return primitive.array;
    }
    match kotlin_type {
        "Any" => "Object",
        "kotlin.Any" => "java.lang.Object",
        "kotlin.String" => "java.lang.String",
        "kotlin.CharSequence" => "java.lang.CharSequence",
        "kotlin.Enum" => "java.lang.Enum",
        "kotlin.Number" => "java.lang.Number",
        "kotlin.Comparable" => "java.lang.Comparable",
        // Kotlin KClass surfaces as java.lang.Class through the JVM
        // interop boundary — a Java `KClass` reference never resolves.
        "KClass" => "Class",
        "MutableList" => "List",
        "MutableMap" => "Map",
        "MutableSet" => "Set",
        "MutableCollection" => "Collection",
        "MutableIterable" => "Iterable",
        "MutableIterator" => "Iterator",
        "MutableListIterator" => "ListIterator",
        "MutableMap.MutableEntry" => "Map.Entry",
        "Nothing" => "Void",
        "kotlin.Nothing" => "java.lang.Void",
        // Unit is a reference type when it is stored or passed as a value.
        // Function declarations map an explicit Unit result to Java `void`
        // in the return-type context instead.
        "Unit" => "kotlin.Unit",
        "Array" | "kotlin.Array" => "__NOTLIN_ARRAY__",
        "UByte" | "UShort" | "UInt" | "ULong" => "long",
        // stdlib container types have no JDK twin; `to` emits a
        // SimpleImmutableEntry, so declared Pair<..> types rewrite to it
        // (member reads map .first/.second -> getKey/getValue).
        "Pair" => "java.util.AbstractMap.SimpleImmutableEntry",
        "Triple" => "__NOTLIN_TRIPLE__",
        _ => kotlin_type,
    }
}

/// Boxed names for generic type arguments: Java generics cannot hold
/// primitives, so `List<Int>` must be `List<Integer>`.
pub fn boxed_name(java_primitive: &str) -> Option<&'static str> {
    JvmPrimitive::java(java_primitive).map(|ty| ty.boxed)
}

/// A Java type usable in a reference-only context such as a cast or generic.
pub(crate) fn boxed_java_type(java_type: &str) -> String {
    boxed_name(java_type).unwrap_or(java_type).to_owned()
}

/// Marker emitted for `() -> R` function types: the type itself can't map to
/// Java source (no SAM syntax for arbitrary shapes without arity analysis).
/// Callers must recognize this string and flag the declaration untranslatable.
pub const FUNCTION_TYPE_PLACEHOLDER: &str = "\u{0}NOTLIN_FUNCTION_TYPE";

/// Root package of the nullability annotation set, for `import ...Nullable;`
pub fn nullable_import(set: AnnotationSet) -> Option<&'static str> {
    match set {
        AnnotationSet::Jetbrains => Some("org.jetbrains.annotations"),
        AnnotationSet::Jspecify => Some("org.jspecify.annotations"),
        AnnotationSet::None => None,
    }
}

/// Fully qualified non-null return annotation paired with the selected
/// nullability dialect. Fully qualified names avoid depending on generated
/// imports, while `None` preserves the explicit `--annotations none` contract.
pub fn non_null_annotation(set: AnnotationSet) -> Option<&'static str> {
    match set {
        AnnotationSet::Jetbrains => Some("@org.jetbrains.annotations.NotNull"),
        AnnotationSet::Jspecify => Some("@org.jspecify.annotations.NonNull"),
        AnnotationSet::None => None,
    }
}

/// JSpecify annotates the outer return type, including qualified names and
/// arrays. A declaration annotation before a qualified package name is illegal.
pub(crate) fn non_null_type(java_type: &str, set: AnnotationSet) -> String {
    let Some(annotation) = non_null_annotation(set) else {
        return java_type.to_owned();
    };
    if set != AnnotationSet::Jspecify {
        return format!("{annotation} {java_type}");
    }
    let (position, array) = outer_type_annotation_position(java_type);
    if array {
        format!(
            "{} {annotation} {}",
            java_type[..position].trim_end(),
            &java_type[position..]
        )
    } else {
        format!(
            "{}{annotation} {}",
            &java_type[..position],
            &java_type[position..]
        )
    }
}

fn outer_type_annotation_position(java_type: &str) -> (usize, bool) {
    let mut depth = 0;
    let mut last_dot = None;
    for (position, character) in java_type.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => depth -= 1,
            '.' if depth == 0 => last_dot = Some(position),
            '[' if depth == 0 => {
                return (position, true);
            }
            _ => {}
        }
    }
    (last_dot.map_or(0, |dot| dot + 1), false)
}

/// Nullable types are kept in prefix form during planning. Place JSpecify's
/// type-use annotation on the outer type only after all type rewrites finish.
pub(crate) fn place_jspecify_nullable_annotations(body: &str) -> String {
    const NAME: &str = "org.jspecify.annotations.Nullable";
    if !body.contains(NAME) {
        return body.to_owned();
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .expect("Java grammar");
    let Some(tree) = parser.parse(body, None) else {
        return body.to_owned();
    };
    let mut pending = vec![tree.root_node()];
    let mut edits = Vec::new();
    while let Some(node) = pending.pop() {
        if node.kind() == "marker_annotation"
            && node
                .child_by_field_name("name")
                .is_some_and(|name| name.utf8_text(body.as_bytes()).ok() == Some(NAME))
            && let Some(parent) = node.parent()
        {
            let ty = match parent.kind() {
                "modifiers" => parent
                    .parent()
                    .and_then(|decl| decl.child_by_field_name("type")),
                "annotated_type" => parent
                    .named_children(&mut parent.walk())
                    .find(|child| !matches!(child.kind(), "annotation" | "marker_annotation")),
                _ => None,
            };
            if let Some(ty) = ty {
                let text = &body[ty.byte_range()];
                let (position, array) = outer_type_annotation_position(text);
                if position > 0 {
                    let at = ty.start_byte() + position;
                    edits.push((node.start_byte(), node.end_byte(), ""));
                    edits.push((
                        at,
                        at,
                        if array {
                            " @org.jspecify.annotations.Nullable "
                        } else {
                            "@org.jspecify.annotations.Nullable "
                        },
                    ));
                }
            }
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    edits.sort_unstable();
    let mut result = body.to_owned();
    for (start, end, replacement) in edits.into_iter().rev() {
        result.replace_range(start..end, replacement);
    }
    result
}

/// Shorten annotation syntax only, never string literals or user type names.
/// A conflicting NonNull binding keeps the qualified spelling instead.
pub(crate) fn import_jspecify_non_null(body: &str, imports: &[String]) -> (String, bool) {
    import_generated_annotation(body, imports, "org.jspecify.annotations.NonNull")
}

pub(crate) fn import_nullable_annotation(
    body: &str,
    imports: &[String],
    set: AnnotationSet,
) -> (String, bool) {
    let Some(package) = nullable_import(set) else {
        return (body.to_owned(), false);
    };
    import_generated_annotation(body, imports, &format!("{package}.Nullable"))
}

/// Generated annotations carry their identity until the final import decision.
/// An existing annotation or ordinary type with the same short name keeps its
/// own binding. Only annotation-name nodes are shortened; literals stay data.
pub(crate) fn import_generated_annotation(
    body: &str,
    imports: &[String],
    name: &str,
) -> (String, bool) {
    let simple = name.rsplit('.').next().unwrap();
    if !body.contains(name)
        || imports
            .iter()
            .any(|import| import.rsplit('.').next() == Some(simple) && import != name)
    {
        return (body.to_owned(), false);
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_java::LANGUAGE.into())
        .expect("Java grammar");
    let Some(tree) = parser.parse(body, None) else {
        return (body.to_owned(), false);
    };
    let imported = imports.iter().any(|import| import == name);
    let mut pending = vec![tree.root_node()];
    let mut spans = Vec::new();
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "annotation" | "marker_annotation")
            && let Some(annotation_name) = node.child_by_field_name("name")
            && annotation_name.utf8_text(body.as_bytes()).ok() == Some(name)
        {
            spans.push((annotation_name.start_byte(), annotation_name.end_byte()));
        }
        if matches!(node.kind(), "identifier" | "type_identifier")
            && node.utf8_text(body.as_bytes()).ok() == Some(simple)
            && node.parent().is_some_and(|parent| {
                !(matches!(
                    parent.kind(),
                    "scoped_identifier" | "scoped_type_identifier"
                ) && parent.child_by_field_name("name") == Some(node))
                    && !(imported && matches!(parent.kind(), "annotation" | "marker_annotation"))
            })
        {
            return (body.to_owned(), false);
        }
        pending.extend(node.named_children(&mut node.walk()));
    }
    let used = !spans.is_empty();
    spans.sort_unstable();
    let mut result = body.to_owned();
    for (start, end) in spans.into_iter().rev() {
        result.replace_range(start..end, simple);
    }
    (result, used)
}

/// Box Kotlin primitive abbreviations inside a `type_arguments` text blob
/// (`<Op, Int>` -> `<Op, Integer>`). Splits on `<`, `,`, `>` and maps the
/// standalone primitive names.
pub fn box_primitive_generics(targs: &str) -> String {
    let mut out = String::new();
    let mut tok = String::new();
    for ch in targs.chars() {
        if ch == ',' || ch == '<' || ch == '>' {
            if !tok.is_empty() {
                let t = tok.trim();
                out.push_str(JvmPrimitive::kotlin(t).map_or(t, |ty| ty.boxed));
                tok.clear();
            }
            out.push(ch);
        } else {
            tok.push(ch);
        }
    }
    if !tok.is_empty() {
        let t = tok.trim();
        out.push_str(JvmPrimitive::kotlin(t).map_or(t, |ty| ty.boxed));
    }
    out
}

/// Preserve Kotlin declaration-site covariance when a translated Java
/// interface exposes a read-only collection as a return type. The concrete
/// implementation may then return `List<Derived>` for an interface contract of
/// `List<? extends Base>`, matching Kotlin's `List<out T>` relationship.
pub fn covariant_interface_return(java_type: &str) -> String {
    let (prefix, ty) = split_nullable_prefix(java_type);
    let Some(open) = ty.find('<') else {
        return java_type.to_string();
    };
    if !ty.ends_with('>') {
        return java_type.to_string();
    }
    let base = &ty[..open];
    let arguments = &ty[open + 1..ty.len() - 1];
    let parts = split_top_level_type_arguments(arguments);
    let covariant_argument = |argument: &str| {
        let argument = argument.trim();
        if argument.starts_with('?') {
            argument.to_owned()
        } else {
            format!("? extends {argument}")
        }
    };
    let rendered = match (base, parts.as_slice()) {
        ("List" | "Set" | "Collection" | "Iterable" | "Iterator" | "Sequence", [item]) => {
            format!("{base}<{}>", covariant_argument(item))
        }
        ("Map", [key, value]) => format!("Map<{}, {}>", key.trim(), covariant_argument(value)),
        _ => return java_type.to_string(),
    };
    format!("{prefix}{rendered}")
}

/// Kotlin read-only collections are declaration-site covariant. Java callers
/// need an equivalent wildcard when such a collection appears as a parameter;
/// otherwise `List<Derived>` cannot be passed to Kotlin's `List<Base>` API.
pub fn covariant_readonly_parameter(kotlin_type: &str, java_type: &str) -> String {
    let base = kotlin_type
        .trim()
        .trim_end_matches('?')
        .split('<')
        .next()
        .unwrap_or_default()
        .trim();
    if matches!(
        base,
        "List" | "Set" | "Collection" | "Iterable" | "Iterator" | "Sequence" | "Map"
    ) {
        covariant_interface_return(java_type)
    } else {
        java_type.to_string()
    }
}

fn split_top_level_type_arguments(arguments: &str) -> Vec<&str> {
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut parts = Vec::new();
    for (index, ch) in arguments.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&arguments[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&arguments[start..]);
    parts
}

pub(crate) fn kotlin_type_text(ty: &crate::semantics::KotlinType, nullable: bool) -> String {
    use crate::semantics::{KotlinType, Variance};
    let core = match ty {
        KotlinType::Primitive(name)
        | KotlinType::TypeParameter(name)
        | KotlinType::Unknown(name) => name.clone(),
        KotlinType::Named { name, arguments } => {
            let name = match name.as_str() {
                "kotlin.collections.List" | "kotlin.collections.MutableList" => "java.util.List",
                "kotlin.collections.Set" | "kotlin.collections.MutableSet" => "java.util.Set",
                "kotlin.collections.Map" | "kotlin.collections.MutableMap" => "java.util.Map",
                "kotlin.collections.Collection" | "kotlin.collections.MutableCollection" => {
                    "java.util.Collection"
                }
                _ => name,
            };
            if arguments.is_empty() {
                name.to_owned()
            } else {
                let args = arguments
                    .iter()
                    .map(|arg| {
                        let Some(ty) = &arg.ty else {
                            return "*".to_string();
                        };
                        let variance = match arg.variance {
                            Variance::Invariant => "",
                            Variance::In => "in ",
                            Variance::Out => "out ",
                        };
                        format!("{variance}{}", kotlin_type_text(ty, false))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{name}<{args}>")
            }
        }
        KotlinType::Array(inner) => format!("Array<{}>", kotlin_type_text(inner, false)),
        KotlinType::Nullable(inner) => return kotlin_type_text(inner, true),
        KotlinType::Function { .. } => "Object".to_string(),
    };
    format!("{core}{}", if nullable { "?" } else { "" })
}

pub(crate) fn type_ref_to_java(
    ty: &crate::semantics::TypeRef,
    annotations: AnnotationSet,
    returns: bool,
) -> Option<String> {
    let source = format!("val __type: {}", kotlin_type_text(&ty.ty, ty.nullable));
    let tree = crate::transpiler::parse_tree(&source);
    let declaration = tree.root_node().named_child(0)?;
    let variable = crate::transpiler::kt::child(declaration, "variable_declaration")?;
    let node = variable
        .named_children(&mut variable.walk())
        .find(|node| matches!(node.kind(), "user_type" | "nullable_type"))?;
    Some(if returns {
        crate::transpiler::kt::java_return_type(node, &source, annotations)
    } else {
        crate::transpiler::kt::java_type_ann(node, &source, annotations)
    })
}
