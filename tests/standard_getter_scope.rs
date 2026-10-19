use notlin::semantics::workspace_symbol;
use notlin::workspace::SourceIndex;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "standard-getter-scope-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        for (path, source) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        Self(root)
    }

    fn has_conflict(&self) -> bool {
        let index = SourceIndex::discover(&self.0).unwrap();
        let provider = index
            .declarations()
            .find(|declaration| declaration.name == "Provider")
            .expect("Provider declaration");
        let child = index
            .declarations()
            .find(|declaration| declaration.name == "Child")
            .expect("Child declaration");
        index
            .default_property_getter_conflict(
                provider,
                &HashSet::from([workspace_symbol(&index, child)]),
                std::slice::from_ref(&self.0),
            )
            .is_some()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn conflict(files: &[(&str, &str)]) -> bool {
    Fixture::new(files).has_conflict()
}

#[test]
fn standard_string_identity_survives_unrelated_wildcard_imports() {
    assert!(!conflict(&[
        (
            "left/Root.kt",
            r#"package left
import foreign.*
interface Root {
    fun getId(): String
}
"#,
        ),
        (
            "right/Other.kt",
            r#"package right
interface Other {
    fun getId(): String
}
"#,
        ),
        (
            "sample/Provider.kt",
            r#"package sample
import foreign.*
import left.Root
import right.Other
interface Provider : Root, Other {
    override fun getId(): String {
        return "ok"
    }
}
class Child : Provider {
}
"#,
        ),
    ]));
}

#[test]
fn java_util_uuid_identity_survives_unrelated_wildcard_imports() {
    assert!(!conflict(&[
        (
            "left/Root.kt",
            r#"package left
import java.util.*
import foreign.*
interface Root {
    fun getId(): UUID
}
"#,
        ),
        (
            "right/Other.kt",
            r#"package right
import java.util.UUID
interface Other {
    fun getId(): UUID
}
"#,
        ),
        (
            "sample/Provider.kt",
            r#"package sample
import java.util.*
import foreign.*
import left.Root
import right.Other
interface Provider : Root, Other {
    override fun getId(): UUID {
        return UUID.randomUUID()
    }
}
class Child : Provider {
}
"#,
        ),
    ]));
}

#[test]
fn unknown_custom_type_remains_a_conflict() {
    assert!(conflict(&[
        (
            "left/Root.kt",
            r#"package left
import foreign.*
interface Root {
    fun getId(): Token
}
"#,
        ),
        (
            "right/Other.kt",
            r#"package right
import foreign.*
interface Other {
    fun getId(): Token
}
"#,
        ),
        (
            "sample/Provider.kt",
            r#"package sample
import foreign.*
import left.Root
import right.Other
interface Provider : Root, Other {
    override fun getId(): Token {
        return TODO()
    }
}
class Child : Provider {
}
"#,
        ),
    ]));
}

#[test]
fn source_comparable_with_conflicting_getter_shadows_the_builtin_in_same_package() {
    assert!(conflict(&[(
        "sample/Contracts.kt",
        r#"package sample
interface Comparable<T> {
    fun getId(): Int
}
interface Provider {
    fun getId(): String {
        return "ok"
    }
}
class Child : Provider, Comparable<Child> {
}
"#,
    )]));
}

#[test]
fn source_comparable_with_conflicting_getter_shadows_the_builtin_through_wildcard_import() {
    assert!(conflict(&[
        (
            "foreign/Contracts.kt",
            r#"package foreign
interface Comparable<T> {
    fun getId(): Int
}
"#,
        ),
        (
            "sample/Contracts.kt",
            r#"package sample
import foreign.*
interface Provider {
    fun getId(): String {
        return "ok"
    }
}
class Child : Provider, Comparable<Child> {
}
"#,
        ),
    ]));
}

#[test]
fn standard_comparable_with_no_matching_getter_does_not_block_the_provider() {
    assert!(!conflict(&[(
        "sample/Contracts.kt",
        r#"package sample
interface Provider {
    fun getId(): String {
        return "ok"
    }
}
class Child : Provider, Comparable<Child> {
}
"#,
    )]));
}

#[test]
fn same_package_string_alias_is_not_treated_as_the_builtin() {
    assert!(conflict(&[(
        "sample/Contracts.kt",
        r#"package sample
class Token {
}
typealias String = Token
interface Root {
    fun getId(): String
}
interface Other {
    fun getId(): kotlin.String
}
interface Provider : Root, Other {
    override fun getId(): String {
        return Token()
    }
}
class Child : Provider {
}
"#,
    )]));
}

#[test]
fn wildcard_imported_string_alias_is_not_treated_as_the_builtin() {
    assert!(conflict(&[
        (
            "foreign/Types.kt",
            r#"package foreign
class Token {
}
typealias String = Token
"#,
        ),
        (
            "left/Root.kt",
            r#"package left
import foreign.*
interface Root {
    fun getId(): String
}
"#,
        ),
        (
            "right/Other.kt",
            r#"package right
interface Other {
    fun getId(): kotlin.String
}
"#,
        ),
        (
            "sample/Provider.kt",
            r#"package sample
import foreign.*
import left.Root
import right.Other
interface Provider : Root, Other {
    override fun getId(): kotlin.String {
        return "ok"
    }
}
class Child : Provider {
}
"#,
        ),
    ]));
}

#[test]
fn same_package_comparable_alias_is_not_treated_as_the_builtin() {
    assert!(conflict(&[(
        "sample/Contracts.kt",
        r#"package sample
interface CustomComparable<T> {
    fun getId(): Int
}
typealias Comparable<T> = CustomComparable<T>
interface Provider {
    fun getId(): String {
        return "ok"
    }
}
class Child : Provider, Comparable<Child> {
}
"#,
    )]));
}

#[test]
fn wildcard_imported_comparable_alias_is_not_treated_as_the_builtin() {
    assert!(conflict(&[
        (
            "foreign/Types.kt",
            r#"package foreign
interface CustomComparable<T> {
    fun getId(): Int
}
typealias Comparable<T> = CustomComparable<T>
"#,
        ),
        (
            "sample/Contracts.kt",
            r#"package sample
import foreign.*
interface Provider {
    fun getId(): String {
        return "ok"
    }
}
class Child : Provider, Comparable<Child> {
}
"#,
        ),
    ]));
}
