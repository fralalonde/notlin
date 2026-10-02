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
