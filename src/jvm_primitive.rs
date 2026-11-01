//! Spellings of the eight JVM primitives after the caller has established binding.
//!
//! These lookups do not resolve source names. Keep import, alias, visibility and
//! shadowing proofs at the call site; in particular, boxed references do not gain
//! primitive nullability or descriptors merely because they share a Kotlin type.

#[derive(Debug, Clone, Copy)]
pub(crate) struct JvmPrimitive {
    pub kotlin: &'static str,
    pub kotlin_qualified: &'static str,
    pub java: &'static str,
    pub boxed: &'static str,
    pub boxed_qualified: &'static str,
    pub descriptor: &'static str,
    pub getter_descriptor: &'static str,
    pub array: &'static str,
}

const PRIMITIVES: [JvmPrimitive; 8] = [
    JvmPrimitive {
        kotlin: "Boolean",
        kotlin_qualified: "kotlin.Boolean",
        java: "boolean",
        boxed: "Boolean",
        boxed_qualified: "java.lang.Boolean",
        descriptor: "Z",
        getter_descriptor: "()Z",
        array: "boolean[]",
    },
    JvmPrimitive {
        kotlin: "Byte",
        kotlin_qualified: "kotlin.Byte",
        java: "byte",
        boxed: "Byte",
        boxed_qualified: "java.lang.Byte",
        descriptor: "B",
        getter_descriptor: "()B",
        array: "byte[]",
    },
    JvmPrimitive {
        kotlin: "Short",
        kotlin_qualified: "kotlin.Short",
        java: "short",
        boxed: "Short",
        boxed_qualified: "java.lang.Short",
        descriptor: "S",
        getter_descriptor: "()S",
        array: "short[]",
    },
    JvmPrimitive {
        kotlin: "Int",
        kotlin_qualified: "kotlin.Int",
        java: "int",
        boxed: "Integer",
        boxed_qualified: "java.lang.Integer",
        descriptor: "I",
        getter_descriptor: "()I",
        array: "int[]",
    },
    JvmPrimitive {
        kotlin: "Long",
        kotlin_qualified: "kotlin.Long",
        java: "long",
        boxed: "Long",
        boxed_qualified: "java.lang.Long",
        descriptor: "J",
        getter_descriptor: "()J",
        array: "long[]",
    },
    JvmPrimitive {
        kotlin: "Char",
        kotlin_qualified: "kotlin.Char",
        java: "char",
        boxed: "Character",
        boxed_qualified: "java.lang.Character",
        descriptor: "C",
        getter_descriptor: "()C",
        array: "char[]",
    },
    JvmPrimitive {
        kotlin: "Float",
        kotlin_qualified: "kotlin.Float",
        java: "float",
        boxed: "Float",
        boxed_qualified: "java.lang.Float",
        descriptor: "F",
        getter_descriptor: "()F",
        array: "float[]",
    },
    JvmPrimitive {
        kotlin: "Double",
        kotlin_qualified: "kotlin.Double",
        java: "double",
        boxed: "Double",
        boxed_qualified: "java.lang.Double",
        descriptor: "D",
        getter_descriptor: "()D",
        array: "double[]",
    },
];

impl JvmPrimitive {
    /// Bare Kotlin builtin spelling, without making a source binding claim.
    pub fn kotlin(name: &str) -> Option<&'static Self> {
        PRIMITIVES.iter().find(|ty| ty.kotlin == name)
    }

    /// A physical Java primitive; deliberately excludes wrappers and `void`.
    pub fn java(name: &str) -> Option<&'static Self> {
        PRIMITIVES.iter().find(|ty| ty.java == name)
    }

    /// Simple Java wrapper spelling, still subject to the caller's binding proof.
    pub fn boxed(name: &str) -> Option<&'static Self> {
        PRIMITIVES.iter().find(|ty| ty.boxed == name)
    }

    pub fn qualified_boxed(name: &str) -> Option<&'static Self> {
        Self::boxed(name.strip_prefix("java.lang.")?)
    }
}
