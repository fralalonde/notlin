use notlin::semantics::workspace_symbol;
use notlin::workspace::SourceIndex;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "default-getter-covariance-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
        for (relative, source) in files {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        Self(root)
    }

    fn conflicts(&self) -> bool {
        self.conflicts_for("Provider")
    }

    fn conflicts_for(&self, name: &str) -> bool {
        let index = SourceIndex::discover(&self.0).unwrap();
        let provider = index
            .declarations()
            .find(|declaration| declaration.name == name)
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

fn conflicts(source: &str) -> bool {
    Fixture::new(&[("Contracts.kt", source)]).conflicts()
}

#[test]
fn public_nonnull_source_return_covariant_with_inherited_getter() {
    assert!(!conflicts(
        "package sample\n\
         interface Wide {\n\
         }\n\
         class Narrow : Wide {\n\
         }\n\
         interface Root {\n\
             fun getValue(): Wide\n\
         }\n\
         interface Provider : Root {\n\
             override fun getValue(): Narrow {\n\
                 return Narrow()\n\
             }\n\
         }\n\
         class Child : Provider {\n\
         }\n"
    ));
}

#[test]
fn every_inherited_getter_branch_must_be_proven_compatible() {
    assert!(!conflicts(
        "package sample\n\
         interface Wide {\n\
         }\n\
         class Narrow : Wide {\n\
         }\n\
         interface NarrowRoot {\n\
             fun getValue(): Narrow\n\
         }\n\
         interface WideRoot {\n\
             fun getValue(): Wide\n\
         }\n\
         interface Provider : NarrowRoot, WideRoot {\n\
             override fun getValue(): Narrow {\n\
                 return Narrow()\n\
             }\n\
         }\n\
         class Child : Provider {\n\
         }\n"
    ));
}

#[test]
fn unrelated_return_types_remain_a_conflict() {
    assert!(conflicts(
        "package sample\n\
         interface Wide {\n\
         }\n\
         class Narrow {\n\
         }\n\
         interface Root {\n\
             fun getValue(): Wide\n\
         }\n\
         interface Provider : Root {\n\
             override fun getValue(): Narrow {\n\
                 return Narrow()\n\
             }\n\
         }\n\
         class Child : Provider {\n\
         }\n"
    ));

    assert!(conflicts(
        "package sample\n\
         interface Wide {\n\
         }\n\
         class Narrow : Wide {\n\
         }\n\
         interface Root {\n\
             fun <T> getValue(): Wide\n\
         }\n\
         interface Provider : Root {\n\
             override fun <T> getValue(): Narrow {\n\
                 return Narrow()\n\
             }\n\
         }\n\
         class Child : Provider {\n\
         }\n"
    ));
}

#[test]
fn aliases_and_generic_returns_remain_conservative() {
    assert!(conflicts(
        "package sample\n\
         interface Wide {\n\
         }\n\
         class Narrow : Wide {\n\
         }\n\
         typealias NarrowName = Narrow\n\
         interface Root {\n\
             fun getValue(): Wide\n\
         }\n\
         interface Provider : Root {\n\
             override fun getValue(): NarrowName {\n\
                 return Narrow()\n\
             }\n\
         }\n\
         class Child : Provider {\n\
         }\n"
    ));

    assert!(conflicts(
        "package sample\n\
         interface Wide {\n\
         }\n\
         class Narrow<T> : Wide {\n\
         }\n\
         interface Root {\n\
             fun getValue(): Wide\n\
         }\n\
         interface Provider : Root {\n\
             override fun getValue(): Narrow<String> {\n\
                 return Narrow()\n\
             }\n\
         }\n\
         class Child : Provider {\n\
         }\n"
    ));
}

#[test]
fn unique_source_wildcard_proves_covariance_but_ambiguous_imports_do_not() {
    let ambiguous = Fixture::new(&[
        (
            "left/Types.kt",
            "package left\n\
             interface Wide {\n\
             }\n\
             class Narrow : Wide {\n\
             }\n",
        ),
        (
            "right/Types.kt",
            "package right\n\
             class Narrow {\n\
             }\n",
        ),
        (
            "sample/Contracts.kt",
            "package sample\n\
             import left.*\n\
             import right.*\n\
             interface Root {\n\
                 fun getValue(): Wide\n\
             }\n\
             interface Provider : Root {\n\
                 override fun getValue(): Narrow {\n\
                     return Narrow()\n\
                 }\n\
             }\n\
             class Child : Provider {\n\
             }\n",
        ),
    ]);
    assert!(ambiguous.conflicts());

    let wildcard_only = Fixture::new(&[
        (
            "types/Types.kt",
            "package types\n\
             interface Wide {\n\
             }\n\
             class Narrow : Wide {\n\
             }\n",
        ),
        (
            "sample/Contracts.kt",
            "package sample\n\
             import types.*\n\
             interface Root {\n\
                 fun getValue(): Wide\n\
             }\n\
             interface Provider : Root {\n\
                 override fun getValue(): Narrow {\n\
                     return Narrow()\n\
                 }\n\
             }\n\
             class Child : Provider {\n\
             }\n",
        ),
    ]);
    assert!(!wildcard_only.conflicts());
}

#[test]
fn unknown_competing_getter_branch_still_blocks_translation() {
    assert!(conflicts(
        "package sample\n\
         interface Wide {\n\
         }\n\
         class Narrow : Wide {\n\
         }\n\
         interface Root {\n\
             fun getValue(): Wide\n\
         }\n\
         interface UnknownRoot {\n\
             fun getValue(): MissingType\n\
         }\n\
         interface Provider : Root, UnknownRoot {\n\
             override fun getValue(): Narrow {\n\
                 return Narrow()\n\
             }\n\
         }\n\
         class Child : Provider {\n\
         }\n"
    ));
}

#[test]
fn nullable_narrow_return_does_not_override_a_nonnull_kotlin_contract() {
    assert!(conflicts(
        "package sample\n\
         interface Wide {\n\
         }\n\
         class Narrow : Wide {\n\
         }\n\
         interface Root {\n\
             fun getValue(): Wide\n\
         }\n\
         interface Provider : Root {\n\
             override fun getValue(): Narrow? {\n\
                 return null\n\
             }\n\
         }\n\
         class Child : Provider {\n\
         }\n"
    ));
}

#[test]
fn owner_type_parameter_is_not_mistaken_for_same_named_source_class() {
    assert!(conflicts(
        "package sample\n\
         interface Wide {\n\
         }\n\
         class Narrow : Wide {\n\
         }\n\
         class Other {\n\
         }\n\
         interface Root {\n\
             fun getValue(): Wide\n\
         }\n\
         interface Provider<Narrow> : Root {\n\
             override fun getValue(): Narrow {\n\
                 return TODO()\n\
             }\n\
         }\n\
         class Child : Provider<Other> {\n\
         }\n"
    ));
}

#[test]
fn reference_default_getter_can_narrow_the_builtin_any_contract() {
    assert!(!conflicts(
        "package sample\nclass Narrow {}\ninterface Root {\n fun getValue(): Any\n}\ninterface Provider : Root {\n override fun getValue(): Narrow = Narrow()\n}\nclass Child : Provider {}\n"
    ));
}

#[test]
fn overridden_wider_default_is_checked_using_the_effective_implementation() {
    let fixture = Fixture::new(&[(
        "Contracts.kt",
        "package sample\nclass Narrow {}\ninterface Root {\n fun getValue(): Any = Narrow()\n}\ninterface NarrowRoot {\n fun getValue(): Narrow\n}\ninterface Provider : Root, NarrowRoot {\n override fun getValue(): Narrow = Narrow()\n}\nclass Child : Provider {}\n",
    )]);
    assert!(!fixture.conflicts_for("Root"));
    assert!(!fixture.conflicts());
}

#[test]
fn unproven_any_contracts_and_unboxed_value_getters_still_conflict() {
    for source in [
        "package sample\nclass Any {}\nclass Narrow {}\ninterface Root {\n fun getValue(): Any\n}\ninterface Provider : Root {\n override fun getValue(): Narrow = Narrow()\n}\nclass Child : Provider {}\n",
        "package sample\nimport external.*\nclass Narrow {}\ninterface Root {\n fun getValue(): Any\n}\ninterface Provider : Root {\n override fun getValue(): Narrow = Narrow()\n}\nclass Child : Provider {}\n",
        "package sample\n@JvmInline value class Narrow(val value: Int)\ninterface Root {\n fun getValue(): Any\n}\ninterface Provider : Root {\n override fun getValue(): Narrow = Narrow(1)\n}\nclass Child : Provider {}\n",
        "package sample\nclass Narrow {}\ninterface Root {\n fun getValue(): Any\n}\ninterface Provider : Root {\n override fun getValue(): Narrow? = null\n}\nclass Child : Provider {}\n",
    ] {
        assert!(conflicts(source), "{source}");
    }
}

fn bounded_provider_fixture(parameter: &str, extra: &str, arguments: &str) -> Fixture {
    Fixture::new(&[
        ("Wide.java", "package sample; public interface Wide {}"),
        (
            "Key.java",
            "package sample; public interface Key extends Wide {}",
        ),
        (
            "Concrete.java",
            "package sample; public final class Concrete implements Key {}",
        ),
        (
            "Root.java",
            "package sample; public interface Root { Wide getValue(); }",
        ),
        (
            "Contracts.kt",
            &format!(
                "package sample\n{extra}\ninterface Provider<{parameter}> : Root {{\n    val current: T\n    override fun getValue(): T = current\n}}\nclass Child : Provider<{arguments}> {{\n    override val current: Concrete = Concrete()\n    suspend fun retained() {{}}\n}}\n"
            ),
        ),
    ])
}

#[test]
fn bounded_generic_method_return_can_narrow_an_inherited_java_getter() {
    assert!(!bounded_provider_fixture("T : Key", "", "Concrete").conflicts());
}

#[test]
fn comparable_does_not_block_a_proven_generic_getter_bridge() {
    let fixture = bounded_provider_fixture("T : Key", "", "Concrete");
    let path = fixture.0.join("Contracts.kt");
    let source = fs::read_to_string(&path)
        .unwrap()
        .replace(
            "Provider<Concrete> {",
            "Provider<Concrete>, Comparable<Child> {",
        )
        .replace(
            "    suspend fun retained()",
            "    override fun compareTo(other: Child): Int = 0\n    suspend fun retained()",
        );
    fs::write(path, source).unwrap();
    assert!(!fixture.conflicts());
}

#[test]
fn unproven_generic_upper_bounds_still_retain_the_provider() {
    for (parameter, extra, arguments) in [
        ("T", "", "Concrete"),
        ("T : Key?", "", "Concrete"),
        ("T : Missing", "", "Concrete"),
        ("T : Bound", "typealias Bound = Key", "Concrete"),
        ("T : U, U : Key", "", "Concrete, Key"),
    ] {
        assert!(
            bounded_provider_fixture(parameter, extra, arguments).conflicts(),
            "{parameter}"
        );
    }
}
