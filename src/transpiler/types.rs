//! Shared type-name and annotation helpers.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationSet {
    Jetbrains,
    Jspecify,
    None,
}

impl AnnotationSet {
    #[allow(dead_code)]
    pub fn nullable(&self) -> Option<&'static str> {
        match self {
            AnnotationSet::Jetbrains => Some("@Nullable"),
            AnnotationSet::Jspecify => Some("@Nullable"),
            AnnotationSet::None => None,
        }
    }
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
    match kotlin_type {
        "Any" => "Object",
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
        "Nothing" => "Void",
        "Unit" => "void",
        "Int" => "int",
        "Long" => "long",
        "Short" => "short",
        "Byte" => "byte",
        "Double" => "double",
        "Float" => "float",
        "Boolean" => "boolean",
        "Char" => "char",
        "IntArray" => "int[]",
        "LongArray" => "long[]",
        "ShortArray" => "short[]",
        "ByteArray" => "byte[]",
        "DoubleArray" => "double[]",
        "FloatArray" => "float[]",
        "BooleanArray" => "boolean[]",
        "CharArray" => "char[]",
        "Array" => "__NOTLIN_ARRAY__",
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
    match java_primitive {
        "int" => Some("Integer"),
        "long" => Some("Long"),
        "short" => Some("Short"),
        "byte" => Some("Byte"),
        "double" => Some("Double"),
        "float" => Some("Float"),
        "boolean" => Some("Boolean"),
        "char" => Some("Character"),
        _ => None,
    }
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
                out.push_str(match t {
                    "Int" => "Integer",
                    "Long" => "Long",
                    "Short" => "Short",
                    "Byte" => "Byte",
                    "Double" => "Double",
                    "Float" => "Float",
                    "Boolean" => "Boolean",
                    "Char" => "Character",
                    _ => t,
                });
                tok.clear();
            }
            out.push(ch);
        } else {
            tok.push(ch);
        }
    }
    if !tok.is_empty() {
        let t = tok.trim();
        out.push_str(match t {
            "Int" => "Integer",
            "Long" => "Long",
            "Short" => "Short",
            "Byte" => "Byte",
            "Double" => "Double",
            "Float" => "Float",
            "Boolean" => "Boolean",
            "Char" => "Character",
            _ => t,
        });
    }
    out
}

/// Preserve Kotlin declaration-site covariance when a translated Java
/// interface exposes a read-only collection as a return type. The concrete
/// implementation may then return `List<Derived>` for an interface contract of
/// `List<? extends Base>`, matching Kotlin's `List<out T>` relationship.
pub fn covariant_interface_return(java_type: &str) -> String {
    let (prefix, ty) = java_type
        .strip_prefix("@Nullable ")
        .map_or(("", java_type), |ty| ("@Nullable ", ty));
    let Some(open) = ty.find('<') else {
        return java_type.to_string();
    };
    if !ty.ends_with('>') {
        return java_type.to_string();
    }
    let base = &ty[..open];
    let arguments = &ty[open + 1..ty.len() - 1];
    let parts = split_top_level_type_arguments(arguments);
    let rendered = match (base, parts.as_slice()) {
        ("List" | "Set" | "Collection" | "Iterable" | "Iterator" | "Sequence", [item]) => {
            format!("{base}<? extends {}>", item.trim())
        }
        ("Map", [key, value]) => format!("Map<{}, ? extends {}>", key.trim(), value.trim()),
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
