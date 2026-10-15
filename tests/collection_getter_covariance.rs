use notlin::workspace::SourceIndex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{collections::HashSet, fs};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

fn safe(source: &str, parent_selected: bool, retain_parent: bool) -> (bool, Vec<String>) {
    let root = std::env::temp_dir().join(format!(
        "notlin-getter-covariance-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Fixture.kt"), source).unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let roots = parent_selected
        .then(|| root.clone())
        .into_iter()
        .collect::<Vec<_>>();
    let file = root.join("Fixture.kt");
    let declarations = index.declarations().collect::<Vec<_>>();
    let implementation = declarations
        .iter()
        .find(|d| d.name == "ChildApi")
        .copied()
        .unwrap_or_else(|| {
            panic!(
                "ChildApi missing; indexed declarations: {:?}",
                declarations.iter().map(|d| &d.name).collect::<Vec<_>>()
            )
        });
    let parent = declarations
        .iter()
        .find(|d| d.name == "ParentApi")
        .copied()
        .unwrap_or_else(|| {
            panic!(
                "ParentApi missing; indexed declarations: {:?}",
                declarations.iter().map(|d| &d.name).collect::<Vec<_>>()
            )
        });
    let retained = if retain_parent {
        HashSet::from([notlin::semantics::workspace_symbol(&index, parent)])
    } else {
        HashSet::new()
    };
    let conflicts = index.retained_supertype_member_conflicts_in_file(
        &file,
        &implementation.supertypes,
        "ChildApi",
    );
    let result = conflicts
        .iter()
        .map(|c| {
            (
                c.member_name.clone(),
                c.kind,
                index.selected_covariant_property_conflict_is_safe(&file, c, &roots, &retained),
            )
        })
        .map(|(name, kind, safe)| format!("{name}:{kind:?}:{safe}"))
        .collect();
    let any_safe = conflicts
        .iter()
        .any(|c| index.selected_covariant_property_conflict_is_safe(&file, c, &roots, &retained));
    fs::remove_dir_all(root).unwrap();
    (any_safe, result)
}

fn source_getter_eligible(source: &str, owner_name: &str) -> bool {
    let root = std::env::temp_dir().join(format!(
        "notlin-getter-source-proof-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Fixture.kt"), source).unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let declarations = index.declarations().collect::<Vec<_>>();
    let owner = declarations
        .iter()
        .find(|d| d.name == owner_name)
        .copied()
        .unwrap_or_else(|| {
            panic!(
                "{owner_name} missing; indexed declarations: {:?}",
                declarations.iter().map(|d| &d.name).collect::<Vec<_>>()
            )
        });
    let eligible = index.readonly_interface_getter_is_covariant_source(
        index.source_file(&root.join("Fixture.kt")).unwrap(),
        owner,
        "getItems",
    );
    fs::remove_dir_all(root).unwrap();
    eligible
}

#[test]
fn selected_interface_getter_with_readonly_collection_covariance_is_safe() {
    let (is_safe, _) = safe(
        "package sample\nopen class Base {}\nclass Child : Base() {}\ninterface ParentApi { fun getItems(): List<Base>\n}\ninterface ChildApi : ParentApi { override fun getItems(): List<Child>\n}\n",
        true,
        false,
    );
    assert!(is_safe);
}

#[test]
fn parameterized_getters_and_mutable_property_setters_remain_blocked() {
    let (parameterized, _) = safe(
        "package sample\nopen class Base {}\nclass Child : Base() {}\ninterface ParentApi { fun getItems(index: Int): List<Base>\n}\ninterface ChildApi : ParentApi { override fun getItems(index: Int): List<Child>\n}\n",
        true,
        false,
    );
    assert!(!parameterized);
    let (setter, _) = safe(
        "package sample\nopen class Base {}\nclass Child : Base() {}\ninterface ParentApi { fun getItems(): List<Base>; fun setItems(items: List<Base>) }\ninterface ChildApi : ParentApi { override fun getItems(): List<Child>\n}\n",
        true,
        false,
    );
    assert!(
        !setter,
        "a matching inherited setter makes the accessor mutable"
    );
}

#[test]
fn unselected_parent_and_custom_list_shadow_remain_blocked() {
    let source = "package sample\nopen class Base {}\nclass Child : Base() {}\ninterface ParentApi { fun getItems(): List<Base>\n}\ninterface ChildApi : ParentApi { override fun getItems(): List<Child>\n}\n";
    assert!(!safe(source, false, false).0);
    assert!(!safe(source, true, true).0);
    let shadow = "package sample\nopen class Base {}\nclass Child : Base() {}\nclass List<T> {}\ninterface ParentApi { fun getItems(): List<Base>\n}\ninterface ChildApi : ParentApi { override fun getItems(): List<Child>\n}\n";
    assert!(!safe(shadow, true, false).0);
}

#[test]
fn mutable_collection_and_generic_source_returns_do_not_get_covariance() {
    let mutable = "package sample\nopen class Base {}\ninterface ParentApi { fun getItems(): MutableList<Base>\n}\ninterface ChildApi : ParentApi { override fun getItems(): MutableList<Base>\n}\n";
    assert!(!source_getter_eligible(mutable, "ParentApi"));
    let generic = "package sample\ninterface ParentApi<T> { fun getItems(): T\n}\ninterface ChildApi : ParentApi<List<String>> { override fun getItems(): List<String>\n}\n";
    assert!(!source_getter_eligible(generic, "ParentApi"));
}

#[test]
fn java_parent_getter_contract_is_never_widened() {
    let root = std::env::temp_dir().join(format!(
        "notlin-java-getter-parent-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Fixture.kt"),
        "package sample\nopen class Base {}\nclass Child : Base() {}\ninterface ChildApi : JavaApi { override fun getItems(): List<Child>\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("JavaApi.java"),
        "package sample; public interface JavaApi { java.util.List<Object> getItems(); }\n",
    )
    .unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let file = root.join("Fixture.kt");
    let source_file = index.source_file(&file).unwrap();
    let child_api = index.declarations().find(|d| d.name == "ChildApi").unwrap();
    assert!(!index.readonly_interface_getter_is_covariant_source(
        source_file,
        child_api,
        "getItems",
    ));
    fs::remove_dir_all(root).unwrap();
}
