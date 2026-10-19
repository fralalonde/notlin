use clap::Parser;
use notlin::{cli::Cli, migration_pipeline::plan_workspace_migration, workspace::SourceIndex};
use std::{fs, path::PathBuf, process::Command};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(case: &str) -> Self {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "notlin-variance-safety-{case}-{}",
                std::process::id()
            ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let fixture = Self { root };
        fixture.write("Definitions.kt", DEFINITIONS);
        fixture.write("Parent.kt", PARENT);
        fixture.write("Child.kt", CHILD);
        fixture
    }

    fn write(&self, name: &str, text: &str) {
        fs::write(self.root.join(name), text).unwrap();
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn relative_path(&self, name: &str) -> PathBuf {
        self.path(name)
            .strip_prefix(std::env::current_dir().unwrap())
            .unwrap()
            .to_path_buf()
    }

    fn index(&self) -> SourceIndex {
        SourceIndex::discover(&self.root).unwrap()
    }

    fn original_sources(&self) -> Vec<(PathBuf, String)> {
        ["Definitions.kt", "Parent.kt", "Child.kt"]
            .into_iter()
            .map(|name| {
                (
                    self.path(name),
                    fs::read_to_string(self.path(name)).unwrap(),
                )
            })
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const DEFINITIONS: &str = r#"package neutral.variance

interface Base {
}

class Derived : Base {
}
"#;

const PARENT: &str = r#"package neutral.variance

interface Parent {
    val items: List<Base>

    companion object {
        suspend fun remainKotlin() {}
    }
}
"#;

const CHILD: &str = r#"package neutral.variance

class Child(override val items: List<Derived>) : Parent {
}
"#;

#[test]
fn relative_inputs_keep_variance_contract_metadata_anchored_to_original_snapshot() {
    let fixture = Fixture::new("relative");
    let index = fixture.index();
    let names = ["Definitions.kt", "Parent.kt", "Child.kt"];
    let sources = names
        .iter()
        .map(|name| {
            let path = fixture.relative_path(name);
            let text = fs::read_to_string(fixture.path(name)).unwrap();
            (path, text)
        })
        .collect::<Vec<_>>();
    let roots = sources
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    let cli = Cli::parse_from(["notlin", "--in-place"]);

    let plan = plan_workspace_migration(sources.clone(), &cli, &index, &roots, |_| {}).unwrap();
    let child = plan
        .generated_java
        .iter()
        .find(|output| output.name == "Child.java")
        .expect("the accepted child should be emitted");
    let (parent_path, original_parent) = sources
        .iter()
        .find(|(path, _)| path.ends_with("Parent.kt"))
        .unwrap();
    let source_hash = *blake3::hash(original_parent.as_bytes()).as_bytes();
    let aggregate = plan
        .translation_plans
        .get(parent_path)
        .expect("the parent ownership plan should be retained");

    assert!(plan.final_kotlin_sources[parent_path].contains("List<@kotlin.jvm.JvmWildcard Base>"));
    assert_eq!(aggregate.source_hash, source_hash);
    assert_eq!(aggregate.declarations[0].id.source_hash, source_hash);
    assert_eq!(aggregate.variance_contracts.len(), 1);
    let contract = &aggregate.variance_contracts[0];
    assert_eq!(contract.implementer, child.owners[0]);
    assert_eq!(contract.edit.location.snapshot_hash, source_hash);
    assert_eq!(
        contract.edit.location.file,
        fs::canonicalize(fixture.path("Parent.kt")).unwrap()
    );
    for (path, original) in &sources {
        assert_eq!(fs::read_to_string(path).unwrap(), *original);
    }
}

#[test]
fn output_only_mode_does_not_emit_child_requiring_unapplied_parent_contract() {
    let fixture = Fixture::new("output-only");
    let out = fixture.path("java-out");
    fs::create_dir_all(&out).unwrap();
    let originals = fixture.original_sources();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .arg("--root")
        .arg(&fixture.root)
        .arg("--out-dir")
        .arg(&out)
        .arg(fixture.path("Definitions.kt"))
        .arg(fixture.path("Parent.kt"))
        .arg(fixture.path("Child.kt"))
        .output()
        .expect("run output-only notlin");
    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !out.join("Child.java").exists(),
        "output-only mode cannot rely on a source contract annotation it will not apply"
    );
    for (path, original) in originals {
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }
}

#[test]
fn max_pass_budget_includes_retry_after_rejected_variance_contract() {
    let fixture = Fixture::new("max-pass");
    let parent = fixture.path("Parent.kt");
    let parent_text = fs::read_to_string(&parent).unwrap();
    fs::write(
        &parent,
        parent_text.replace(
            "suspend fun remainKotlin() {}",
            "suspend fun remainKotlin() {}\n        fun anotherKotlinOnlyMember() {}",
        ),
    )
    .unwrap();
    let child = fixture.path("Child.kt");
    let child_text = fs::read_to_string(&child).unwrap();
    fs::write(
        &child,
        child_text.replace(
            "class Child(override val items: List<Derived>) : Parent {",
            "class Child(override val items: List<Derived>) : Parent {\n    suspend fun remainKotlin() {}",
        ),
    )
    .unwrap();
    let originals = fixture.original_sources();
    let index = fixture.index();
    let sources = originals.clone();
    let roots = sources
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    let cli = Cli::parse_from(["notlin", "--in-place", "--max-pass", "2"]);

    let error = match plan_workspace_migration(sources, &cli, &index, &roots, |_| {}) {
        Ok(_) => panic!("rejected speculative contract retry must not exceed the pass budget"),
        Err(error) => error,
    };
    assert!(
        error.contains("did not converge within 2 planning passes"),
        "{error}"
    );
    for (path, original) in originals {
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }
}
