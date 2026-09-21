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
pub fn map_type_name(kotlin_type: &str) -> &str {
    match kotlin_type {
        "Any" => "Object",
        "Unit" => "void",
        "Nothing" => "Void",
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
        "UByte" | "UShort" | "UInt" | "ULong" => "long",
        _ => kotlin_type,
    }
}

/// Root package of the nullability annotation set, for `import ...Nullable;`
pub fn nullable_import(set: AnnotationSet) -> Option<&'static str> {
    match set {
        AnnotationSet::Jetbrains => Some("org.jetbrains.annotations"),
        AnnotationSet::Jspecify => Some("org.jspecify.annotations"),
        AnnotationSet::None => None,
    }
}
