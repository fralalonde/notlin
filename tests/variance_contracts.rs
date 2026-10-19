use clap::Parser;
use notlin::{cli::Cli, migration_pipeline::plan_workspace_migration, workspace::SourceIndex};
use std::fs;

fn plan_fixture(
    suffix: &str,
    child_extra: &str,
    selected_parent: bool,
) -> notlin::migration_pipeline::WorkspaceMigrationPlan {
    let root =
        std::env::temp_dir().join(format!("notlin-variance-{suffix}-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let definitions = "package neutral.variance\ninterface Base {}\nclass Derived : Base {}\n";
    let parent = "package neutral.variance\ninterface Parent {\n val items: List<Base>\n companion object {\n  suspend fun remainKotlin() {}\n }\n}\n";
    let child = format!(
        "package neutral.variance\nclass Child(override val items: List<Derived>) : Parent {{ {child_extra} }}\n"
    );
    let mut sources = Vec::new();
    for (name, source) in [
        ("Definitions.kt", definitions.to_owned()),
        ("Parent.kt", parent.to_owned()),
        ("Child.kt", child),
    ] {
        let path = root.join(name);
        fs::write(&path, &source).unwrap();
        if name != "Parent.kt" || selected_parent {
            sources.push((path, source));
        }
    }
    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", root.to_str().unwrap()]);
    let roots = sources
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    let plan = plan_workspace_migration(sources, &cli, &index, &roots, |_| {}).unwrap();
    for (path, original) in &plan.original_sources {
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            *original,
            "planning must never write originals"
        );
    }
    fs::remove_dir_all(root).unwrap();
    plan
}

#[test]
fn readonly_list_contract_repair_is_committed_with_accepted_child() {
    let plan = plan_fixture("accepted", "", true);
    let child = plan
        .generated_java
        .iter()
        .find(|output| output.name == "Child.java")
        .unwrap_or_else(|| panic!("child retained: {:#?}", plan.translation_plans));
    assert!(child.source.contains("List<Derived>"), "{}", child.source);
    let (parent_path, original_parent) = plan
        .original_sources
        .iter()
        .find(|(path, _)| path.ends_with("Parent.kt"))
        .unwrap();
    let parent = &plan.final_kotlin_sources[parent_path];
    assert!(
        parent.contains("List<@kotlin.jvm.JvmWildcard Base>"),
        "{parent}"
    );
    let aggregate = &plan.translation_plans[parent_path];
    assert_eq!(aggregate.variance_contracts.len(), 1);
    assert_eq!(
        aggregate.source_hash,
        *blake3::hash(original_parent.as_bytes()).as_bytes()
    );
    assert_eq!(
        aggregate.declarations[0].id.source_hash,
        aggregate.source_hash
    );
    assert_eq!(aggregate.variance_contracts[0].implementer, child.owners[0]);
    assert_eq!(
        aggregate.variance_contracts[0].edit.location.snapshot_hash,
        aggregate.source_hash
    );
}

#[test]
fn failed_child_does_not_leave_speculative_contract_annotations() {
    let plan = plan_fixture("rejected", "suspend fun remainKotlin() {}", true);
    assert!(
        !plan
            .generated_java
            .iter()
            .any(|output| output.name == "Child.java")
    );
    assert!(
        plan.final_kotlin_sources
            .values()
            .all(|source| !source.contains("@kotlin.jvm.JvmWildcard"))
    );
    assert!(
        plan.translation_plans
            .values()
            .all(|plan| plan.variance_contracts.is_empty())
    );
}

#[test]
fn supporting_parent_outside_selected_sources_is_never_repaired() {
    let plan = plan_fixture("outside", "", false);
    assert!(
        !plan
            .generated_java
            .iter()
            .any(|output| output.name == "Child.java")
    );
    assert!(
        plan.translation_plans
            .values()
            .all(|plan| plan.variance_contracts.is_empty())
    );
    assert!(
        plan.source_edits
            .iter()
            .all(|edit| !edit.location.file.ends_with("Parent.kt"))
    );
}

#[test]
fn a_later_java_parent_consumes_the_kotlin_variance_marker() {
    let source = "package neutral.variance\ninterface Base {}\ninterface Parent {\n val items: List<@kotlin.jvm.JvmWildcard Base>\n}\n";
    let cli = Cli::parse_from(["notlin", "--annotations", "none", "Definitions.kt"]);
    let (outputs, errors, _, _) =
        notlin::transpiler::transpile(source, std::path::Path::new("Definitions.kt"), &cli);
    assert_eq!(errors, 0);
    let java = &outputs
        .iter()
        .find(|(name, _)| name == "Parent.java")
        .expect("annotated parent translates later")
        .1;
    assert!(java.contains("List<? extends Base> getItems()"), "{java}");
    assert!(!java.contains("JvmWildcard"), "{java}");
    assert!(!java.contains("? extends ? extends"), "{java}");
}
