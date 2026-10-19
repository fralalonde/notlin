use notlin::semantics::workspace_symbol;
use notlin::workspace::SourceIndex;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn conflict(files: &[(&str, &str)]) -> bool {
    let fixture = Fixture(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "getter-identity-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
    );
    fs::create_dir_all(&fixture.0).unwrap();
    for (name, source) in files {
        fs::write(fixture.0.join(name), source).unwrap();
    }
    let index = SourceIndex::discover(&fixture.0).unwrap();
    let provider = index
        .declarations()
        .find(|declaration| declaration.name == "Provider")
        .unwrap();
    let child = index
        .declarations()
        .find(|declaration| declaration.name == "Child")
        .unwrap();
    index
        .default_property_getter_conflict(
            provider,
            &HashSet::from([workspace_symbol(&index, child)]),
            std::slice::from_ref(&fixture.0),
        )
        .is_some()
}

const ROOT: &str = "package sample\ninterface Root<T> {\n fun getId(): T\n}\ninterface Other<T> {\n fun getId(): T\n}\n";

#[test]
fn nullable_substitution_is_idempotent_across_inheritance_paths() {
    assert!(!conflict(&[
        ("Root.kt", ROOT),
        (
            "Provider.kt",
            r#"package sample
interface Provider<T : String?> : Root<T?>, Other<String?> {
    override fun getId(): T? = TODO()
}


class Child : Provider<String?> {}
"#
        )
    ]));
}

#[test]
fn concrete_nested_arguments_match_through_multiple_generic_edges() {
    assert!(!conflict(&[
        ("Root.kt", ROOT),
        (
            "Provider.kt",
            r#"package sample
class Key {}
class Box<T> {}
interface Provider<T> : Root<Box<List<T>>>, Other<Box<List<T>>> {
    override fun getId(): Box<List<T>> = TODO()
}
interface Forward<U> : Provider<U> {}
class Child : Forward<Key> {}
"#
        )
    ]));
}

#[test]
fn same_named_types_from_different_packages_are_not_equal() {
    assert!(conflict(&[
        ("Root.kt", ROOT),
        ("Left.kt", "package left\nclass Key {}\n"),
        ("Right.kt", "package right\nclass Key {}\n"),
        (
            "Provider.kt",
            r#"package sample
interface Provider : Root<left.Key>, Other<right.Key> {
    override fun getId(): left.Key = TODO()
}
class Child : Provider {}
"#
        )
    ]));
}

#[test]
fn duplicate_qualified_declarations_do_not_establish_type_identity() {
    assert!(conflict(&[
        ("Root.kt", ROOT),
        ("First.kt", "package sample\nclass Key {}\n"),
        ("Second.kt", "package sample\nclass Key {}\n"),
        (
            "Provider.kt",
            r#"package sample
interface Provider : Root<Key>, Other<Key> {
    override fun getId(): Key = TODO()
}
class Child : Provider {}
"#
        )
    ]));
}

#[test]
fn unimported_globally_unique_type_does_not_establish_identity() {
    assert!(conflict(&[
        ("Root.kt", ROOT),
        ("Foreign.kt", "package foreign\nclass Key {}\n"),
        (
            "Provider.kt",
            r#"package sample
interface Provider : Root<Key>, Other<Key> {
    override fun getId(): Key = TODO()
}
class Child : Provider {}
"#
        )
    ]));
}

#[test]
fn raw_generic_edge_keeps_the_required_fact_unknown() {
    assert!(conflict(&[
        ("Root.kt", ROOT),
        (
            "Provider.kt",
            r#"package sample
class Key {}
interface Provider<T> : Root<T>, Other<T> {
    override fun getId(): T = TODO()
}
class Child : Provider {}
"#
        )
    ]));
}

fn java_erasure_fixture(provider: &str) -> bool {
    conflict(&[
        (
            "Root.java",
            "package sample; public interface Root extends java.io.Serializable { Base getId(); }\n",
        ),
        (
            "Base.kt",
            "package sample\nopen class Base {}\nclass Concrete : Base() {}\n",
        ),
        ("Provider.kt", provider),
    ])
}

#[test]
fn exact_java_erasure_accepts_default_method_with_matching_type_parameter_bound() {
    assert!(!java_erasure_fixture(
        "package sample\n\
         interface Provider<K : Base> : Root {\n\
             override fun getId(): K {\n\
                 return TODO()\n\
             }\n\
         }\n\
         class Child : Provider<Concrete> {}\n"
    ));
}

#[test]
fn exact_java_erasure_does_not_accept_a_kotlin_property() {
    assert!(java_erasure_fixture(
        "package sample\n\
         interface Provider<K : Base> : Root {\n\
             val id: K\n\
                 get() = TODO()\n\
         }\n\
         class Child : Provider<Concrete> {}\n"
    ));
}

#[test]
fn exact_java_erasure_requires_the_bound_itself_to_match() {
    assert!(java_erasure_fixture(
        "package sample\n\
         class Narrow : Base() {}\n\
         interface Provider<K : Narrow> : Root {\n\
             override fun getId(): K {\n\
                 return TODO()\n\
             }\n\
         }\n\
         class Child : Provider<Narrow> {}\n"
    ));
}

#[test]
fn exact_java_erasure_rejects_nullable_getter_return() {
    assert!(java_erasure_fixture(
        "package sample\n\
         interface Provider<K : Base> : Root {\n\
             override fun getId(): K? {\n\
                 return null\n\
             }\n\
         }\n\
         class Child : Provider<Base> {}\n"
    ));
}

#[test]
fn exact_java_erasure_rejects_multiple_and_where_bounds() {
    assert!(java_erasure_fixture(
        "package sample\n\
         interface Other {}\n\
         class Both : Base(), Other {}\n\
         interface Provider<K> : Root where K : Base, K : Other {\n\
             override fun getId(): K {\n\
                 return TODO()\n\
             }\n\
         }\n\
         class Child : Provider<Both> {}\n"
    ));
}

#[test]
fn exact_java_erasure_rejects_unresolved_bound() {
    assert!(conflict(&[
        (
            "Root.java",
            "package sample; public interface Root { Base getId(); }\n",
        ),
        (
            "Provider.kt",
            "package sample\n\
             interface Provider<K : MissingBound> : Root {\n\
                 override fun getId(): K {\n\
                     return TODO()\n\
                 }\n\
             }\n\
             class Child : Provider<MissingBound> {}\n",
        ),
    ]));
}

#[test]
fn method_local_java_type_parameter_is_not_a_same_named_class() {
    assert!(conflict(&[
        (
            "Root.java",
            "package sample; public interface Root { <Base extends Foreign> Base getId(); }\n"
        ),
        (
            "Base.kt",
            "package sample\nopen class Base {}\nclass Concrete : Base() {}\ninterface Foreign {}\n"
        ),
        (
            "Provider.kt",
            r#"package sample
interface Provider<K : Base> : Root {
    override fun getId(): K = TODO()
}
class Child : Provider<Concrete> {}
"#
        ),
    ]));
}

#[test]
fn default_getter_without_a_competing_accessor_needs_no_cross_owner_type_proof() {
    assert!(!conflict(&[(
        "Provider.kt",
        r#"package sample
import java.util.*
interface Root {
    fun getLookupKey(): String
}
interface Provider : Root {
    fun getId(): String = getLookupKey()
}
class Child : Provider {
    override fun getLookupKey(): String = "key"
}
"#
    )]));
}

#[test]
fn uuid_explicit_and_standard_star_imports_have_the_same_getter_identity() {
    assert!(!conflict(&[
        (
            "Root.kt",
            "package sample\nimport java.util.UUID\ninterface Root { fun getId(): UUID }\n"
        ),
        (
            "Provider.kt",
            "package sample\nimport java.util.*\ninterface Provider : Root { override fun getId(): UUID = UUID.randomUUID() }\nclass Child : Provider {}\n"
        ),
    ]));
}

#[test]
fn java_and_kotlin_string_names_have_the_same_getter_identity() {
    assert!(!conflict(&[
        (
            "Root.java",
            "package sample; public interface Root { java.lang.String getId(); }\n"
        ),
        (
            "Provider.kt",
            "package sample\nimport java.util.*\ninterface Provider : Root { override fun getId(): String = \"id\" }\nclass Child : Provider {}\n"
        ),
    ]));
}

#[test]
fn java_primitive_getters_match_their_kotlin_scalar_contracts() {
    for (java, kotlin, value) in [
        ("boolean", "Boolean", "true"),
        ("byte", "Byte", "1"),
        ("short", "Short", "1"),
        ("int", "Int", "1"),
        ("long", "Long", "1L"),
        ("float", "Float", "1.0f"),
        ("double", "Double", "1.0"),
        ("char", "Char", "'x'"),
        ("Object", "Any", "\"value\""),
    ] {
        let root = format!("package sample; public interface Root {{ {java} getId(); }}\n");
        let provider = format!(
            "package sample\ninterface Provider : Root {{ override fun getId(): {kotlin} = {value} }}\nclass Child : Provider {{}}\n"
        );
        assert!(
            !conflict(&[("Root.java", &root), ("Provider.kt", &provider)]),
            "{java} and {kotlin} should preserve the same getter contract"
        );
    }
}

#[test]
fn boxed_java_scalar_is_not_treated_as_a_primitive_getter() {
    assert!(conflict(&[
        (
            "Root.java",
            "package sample; public interface Root { java.lang.Integer getId(); }\n"
        ),
        (
            "Provider.kt",
            "package sample\ninterface Provider : Root { override fun getId(): Int = 1 }\nclass Child : Provider {}\n"
        ),
    ]));
}
