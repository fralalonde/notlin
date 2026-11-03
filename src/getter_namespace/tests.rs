use super::*;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "notlin-getter-namespace-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        for (name, source) in files {
            fs::write(root.join(name), source).unwrap();
        }
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const BASE: &str =
    "package sample\ninterface Root { var id:Any }\nopen class Base(override var id:Any):Root\n";
const CHILD: &str = "package sample\nclass Child:Base(\"wide\") { fun getId():String=\"narrow\"\nfun getId(type:Class<*>):String=\"overload\"\nsuspend fun keep() {} }\n";

fn inputs(index: &SourceIndex) -> Vec<(PathBuf, String)> {
    index
        .kotlin_files()
        .map(|file| (file.path.clone(), file.source_text().to_owned()))
        .collect()
}

#[test]
fn descendants_detect_original_and_split_getters_through_scoped_ancestry() {
    for child in [
        "package sample\nopen class Child:Base(\"wide\") { fun getId():String=id.toString() }\n",
        "package sample\nopen class Child:Base(\"wide\") { @kotlin.jvm.JvmName(\"getId\") fun notlinGetId():String=getId().toString() }\n",
    ] {
        let fixture = Fixture::new(&[
            ("Base.kt", BASE),
            ("Child.kt", child),
            (
                "Leaf.kt",
                "package leaf\nimport sample.Child as Parent\nopen class Middle:Parent()\nclass Leaf:Middle()\nclass Unrelated\n",
            ),
        ]);
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let owner = |file: &str, name: &str| {
            index
                .source_file(&fixture.0.join(file))
                .unwrap()
                .declarations
                .iter()
                .find(|owner| owner.name == name)
                .unwrap()
        };
        for name in ["Middle", "Leaf"] {
            assert!(
                inherits_independent_getters(&index, owner("Leaf.kt", name)),
                "{child}: {name}"
            );
        }
        assert!(!inherits_independent_getters(
            &index,
            owner("Leaf.kt", "Unrelated")
        ));
        assert!(!inherits_independent_getters(
            &index,
            owner("Child.kt", "Child")
        ));
    }
}
fn all_retained(index: &SourceIndex) -> HashSet<SymbolId> {
    index
        .kotlin_files()
        .flat_map(|file| {
            let tree = crate::transpiler::parse_tree(file.source_text());
            crate::translation_plan::analyze(file.source_text(), tree.root_node(), &file.path)
                .declarations
                .into_iter()
                .map(|owner| owner.symbol_id)
        })
        .collect()
}
fn rewritten(plan: &NameSplits, index: &SourceIndex, path: &std::path::Path) -> String {
    let file = index.source_file(path).unwrap();
    match plan
        .proposal(index, &file.path, file.source_text(), &Default::default())
        .unwrap()
    {
        crate::migrate::MigrationProposal::Rewrite(source) => source,
        crate::migrate::MigrationProposal::Untouched => file.source_text().to_owned(),
        crate::migrate::MigrationProposal::Delete => panic!("deleted retained source"),
    }
}

#[test]
fn bound_calls_change_while_property_reads_and_argument_overloads_stay_distinct() {
    let fixture = Fixture::new(&[
        ("Base.kt", BASE),
        ("Child.kt", CHILD),
        (
            "Caller.kt",
            "package sample\nfun read(child:Child, base:Base):String=child.getId()+base.id.toString()+child.getId(String::class.java)\n",
        ),
    ]);
    let index = SourceIndex::discover(&fixture.0).unwrap();
    let result = plan(&index, &inputs(&index), &all_retained(&index)).unwrap();
    let child = rewritten(&result, &index, &fixture.0.join("Child.kt"));
    let caller = rewritten(&result, &index, &fixture.0.join("Caller.kt"));
    assert!(child.contains("@kotlin.jvm.JvmName(\"getId\")"), "{child}");
    assert!(child.contains("fun notlinGetId()"), "{child}");
    assert!(child.contains("fun getId(type:Class<*>)"), "{child}");
    assert!(
        caller.contains("child.notlinGetId()+base.id.toString()+child.getId(String::class.java)"),
        "{caller}"
    );
    assert!(result.report.bridges.is_empty());
    assert_eq!(result.report.repairs.len(), 1);
}

#[test]
fn java_owned_callers_keep_the_existing_physical_getter_name() {
    let fixture = Fixture::new(&[
        ("Base.kt", BASE),
        ("Child.kt", CHILD),
        (
            "Caller.kt",
            "package sample\nfun read(child:Child):String=child.getId()\n",
        ),
    ]);
    let index = SourceIndex::discover(&fixture.0).unwrap();
    let retained = all_retained(&index)
        .into_iter()
        .filter(|id| id.name != "read")
        .collect();
    let result = plan(&index, &inputs(&index), &retained).unwrap();
    assert_eq!(
        rewritten(&result, &index, &fixture.0.join("Caller.kt")),
        fs::read_to_string(fixture.0.join("Caller.kt")).unwrap()
    );
    assert!(rewritten(&result, &index, &fixture.0.join("Child.kt")).contains("fun notlinGetId()"));
}

#[test]
fn namespace_edits_compose_with_removing_declarations_on_both_sides() {
    let source = format!(
        "package sample\nclass Before\n{}{}class After\n",
        BASE.strip_prefix("package sample\n").unwrap(),
        CHILD.strip_prefix("package sample\n").unwrap()
    );
    let fixture = Fixture::new(&[("Case.kt", &source)]);
    let index = SourceIndex::discover(&fixture.0).unwrap();
    let retained = all_retained(&index)
        .into_iter()
        .filter(|id| !matches!(id.name.as_str(), "Before" | "After"))
        .collect();
    let result = plan(&index, &inputs(&index), &retained).unwrap();
    let tree = crate::transpiler::parse_tree(&source);
    let spans = tree
        .root_node()
        .named_children(&mut tree.root_node().walk())
        .filter(|node| {
            node.child_by_field_name("name")
                .and_then(|name| name.utf8_text(source.as_bytes()).ok())
                .is_some_and(|name| matches!(name, "Before" | "After"))
        })
        .map(|node| (node.start_byte(), node.end_byte()))
        .collect();
    let coverage = crate::diagnostics::FileCoverage {
        translated: vec!["Before".into(), "After".into()],
        untranslated: vec!["Child".into()],
        translated_spans: spans,
        ..Default::default()
    };
    let crate::migrate::MigrationProposal::Rewrite(output) = result
        .proposal(&index, &fixture.0.join("Case.kt"), &source, &coverage)
        .unwrap()
    else {
        panic!("expected retained source");
    };
    assert!(!output.contains("class Before"), "{output}");
    assert!(!output.contains("class After"), "{output}");
    assert!(output.contains("fun notlinGetId()"), "{output}");
    assert!(
        !crate::transpiler::parse_tree(&output)
            .root_node()
            .has_error(),
        "{output}"
    );
}

#[test]
fn unresolved_calls_refuse_a_namespace_rewrite_before_any_source_is_changed() {
    let fixture = Fixture::new(&[
        ("Base.kt", BASE),
        ("Child.kt", CHILD),
        (
            "Caller.kt",
            "package sample\nfun read(value:Unknown)=value.getId()\n",
        ),
    ]);
    let index = SourceIndex::discover(&fixture.0).unwrap();
    let before = inputs(&index);
    let error = plan(&index, &before, &all_retained(&index)).err().unwrap();
    assert!(error.contains("cannot establish `getId`"), "{error}");
    assert_eq!(before, inputs(&index));
}

#[test]
fn positional_repairs_require_the_exact_planned_source_snapshot() {
    let fixture = Fixture::new(&[("Base.kt", BASE), ("Child.kt", CHILD)]);
    let index = SourceIndex::discover(&fixture.0).unwrap();
    let result = plan(&index, &inputs(&index), &all_retained(&index)).unwrap();
    let changed = CHILD.replace("narrow", "change");
    assert_eq!(changed.len(), CHILD.len());
    let error = result
        .proposal(
            &index,
            &fixture.0.join("Child.kt"),
            &changed,
            &Default::default(),
        )
        .unwrap_err();
    assert!(error.contains("source changed after planning"), "{error}");
}

#[test]
fn scoped_diagnostic_merges_existing_suppressions_without_repeating_the_annotation() {
    for (imports, annotation) in [
        ("", "@kotlin.Suppress(\"unused\")"),
        ("", "@Suppress(\"unused\",)"),
        ("", "@kotlin.Suppress(names = [\"unused\",])"),
        ("", "@kotlin.Suppress(names = [])"),
        ("", "@kotlin.Suppress(names = arrayOf(\"unused\"))"),
        ("import kotlin.Suppress as Quiet\n", "@Quiet()"),
        ("", "@kotlin.Suppress"),
        ("", "@kotlin.Suppress(\"INAPPLICABLE_JVM_NAME\")"),
    ] {
        let child = CHILD
            .replacen("package sample\n", &format!("package sample\n{imports}"), 1)
            .replacen("fun getId()", &format!("{annotation}\nfun getId()"), 1);
        let fixture = Fixture::new(&[("Base.kt", BASE), ("Child.kt", &child)]);
        let index = SourceIndex::discover(&fixture.0).unwrap();
        let result = plan(&index, &inputs(&index), &all_retained(&index)).unwrap();
        let changed = rewritten(&result, &index, &fixture.0.join("Child.kt"));
        assert_eq!(
            changed.matches("\"INAPPLICABLE_JVM_NAME\"").count(),
            1,
            "{changed}"
        );
        let suppression_count = changed.matches("@kotlin.Suppress").count()
            + changed.matches("@Suppress").count()
            + changed.matches("@Quiet").count();
        assert_eq!(suppression_count, 1, "{changed}");
        assert!(changed.contains("notlinGetId"), "{changed}");
    }
}
