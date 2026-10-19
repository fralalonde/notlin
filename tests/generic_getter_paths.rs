use notlin::semantics::workspace_symbol;
use notlin::workspace::{DeclarationKind, SourceIndex};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(source: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "notlin-generic-getter-paths-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Contracts.kt"), source).unwrap();
        Self(root)
    }

    fn index(&self) -> SourceIndex {
        SourceIndex::discover(&self.0).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn retained_provider_conflict(source: &str) -> bool {
    let fixture = Fixture::new(source);
    let index = fixture.index();
    let provider = index
        .declarations()
        .find(|declaration| declaration.name == "Provider")
        .unwrap_or_else(|| {
            panic!(
                "Provider missing; parsed declarations: {:?}; source: {}",
                index
                    .declarations()
                    .map(|declaration| declaration.name.as_str())
                    .collect::<Vec<_>>(),
                fs::read_to_string(fixture.0.join("Contracts.kt")).unwrap()
            )
        });
    let child = index
        .declarations()
        .find(|declaration| declaration.name == "Child")
        .unwrap();
    assert_eq!(provider.kind, DeclarationKind::Interface);
    let retained = HashSet::from([workspace_symbol(&index, child)]);
    index
        .default_property_getter_conflict(provider, &retained, std::slice::from_ref(&fixture.0))
        .is_some()
}

#[test]
fn same_descendant_type_parameter_through_distinct_generic_paths_is_compatible() {
    assert!(!retained_provider_conflict(
        "package sample\n\
         interface Marker {}\n\
         interface Root<I : Marker> {\n\
             fun getId(): I {\n\
                 return TODO()\n\
             }\n\
         }\n\
         interface Other<J : Marker> {\n\
             fun getId(): J\n\
         }\n\
         interface Provider<X : Marker> : Root<X>, Other<X> {\n\
             override fun getId(): X {\n\
                 return TODO()\n\
             }\n\
         }\n\
         class Child<A : Marker> : Provider<A>\n"
    ));
}

#[test]
fn same_spelling_from_distinct_descendant_parameters_remains_a_conflict() {
    // The inherited declarations both spell their parameter `I`, but the two
    // paths substitute different Child parameters. Name equality is not proof.
    assert!(retained_provider_conflict(
        "package sample\n\
         interface Marker {}\n\
         interface Root<I : Marker> {\n\
             fun getId(): I {\n\
                 return TODO()\n\
             }\n\
         }\n\
         interface Other<I : Marker> {\n\
             fun getId(): I\n\
         }\n\
         interface Provider<X : Marker, Y : Marker> : Root<X>, Other<Y> {\n\
             override fun getId(): X {\n\
                 return TODO()\n\
             }\n\
         }\n\
         class Child<A : Marker, B : Marker> : Provider<A, B>\n"
    ));
}

#[test]
fn same_concrete_source_declaration_on_both_paths_is_compatible() {
    assert!(!retained_provider_conflict(
        "package sample\n\
         interface Marker {}\n\
         class ConcreteId : Marker\n\
         interface Root<I : Marker> {\n\
             fun getId(): I {\n\
                 return TODO()\n\
             }\n\
         }\n\
         interface Other<J : Marker> {\n\
             fun getId(): J\n\
         }\n\
         interface Provider : Root<ConcreteId>, Other<ConcreteId> {\n\
             override fun getId(): ConcreteId {\n\
                 return ConcreteId()\n\
             }\n\
         }\n\
         class Child : Provider\n"
    ));
}

#[test]
fn nested_generic_arguments_are_compared_recursively() {
    assert!(retained_provider_conflict(
        "package sample\n\
         interface Marker {}\n\
         interface Box<T : Marker> {}\n\
         interface Root<I : Marker> {\n\
             fun getId(): I {\n\
                 return TODO()\n\
             }\n\
         }\n\
         interface Other<J : Marker> {\n\
             fun getId(): J\n\
         }\n\
         interface Provider<X : Marker, Y : Marker> : Root<Box<X>>, Other<Box<Y>> {\n\
             override fun getId(): Box<X> {\n\
                 return TODO()\n\
             }\n\
         }\n\
         class Child<A : Marker, B : Marker> : Provider<A, B>\n"
    ));
}

#[test]
fn expanding_generic_inheritance_cycle_is_retained_without_unbounded_walk() {
    assert!(retained_provider_conflict(
        "package sample\n\
         interface Marker {}\n\
         interface Loop<T : Marker> : Loop<List<T>> {\n\
             fun getId(): T {\n\
                 return TODO()\n\
             }\n\
         }\n\
         interface Provider<X : Marker> : Loop<X> {\n\
             override fun getId(): X {\n\
                 return TODO()\n\
             }\n\
         }\n\
         class Child<A : Marker> : Provider<A>\n"
    ));
}

#[test]
fn nullable_mismatch_remains_a_conflict_even_when_parameter_identity_matches() {
    assert!(retained_provider_conflict(
        "package sample\n\
         interface Marker {}\n\
         interface Root<I : Marker> {\n\
             fun getId(): I {\n\
                 return TODO()\n\
             }\n\
         }\n\
         interface Other<J : Marker> {\n\
             fun getId(): J?\n\
         }\n\
         interface Provider<X : Marker> : Root<X>, Other<X> {\n\
             override fun getId(): X {\n\
                 return TODO()\n\
             }\n\
         }\n\
         class Child<A : Marker> : Provider<A>\n"
    ));
}
