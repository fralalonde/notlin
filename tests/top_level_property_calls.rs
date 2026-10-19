use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(sources: &[(&str, &str)]) -> Self {
        Self::selected(sources, &[])
    }

    fn selected(sources: &[(&str, &str)], selected: &[&str]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "notlin-top-level-property-cases-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        for (name, source) in sources {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
        command.args(["--root", root.to_str().unwrap(), "--in-place"]);
        if selected.is_empty() {
            command.arg(&root);
        } else {
            command.args(selected.iter().map(|file| root.join(file)));
        }
        let output = command.output().expect("run notlin");
        assert!(
            output.status.success(),
            "notlin failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self(root)
    }
}

#[test]
fn explicit_input_paths_use_the_accepted_rounds_indexed_snapshot_identity() {
    let fixture = Fixture::selected(
        &[
            (
                "State.kt",
                "package sample.state\nvar count = 0\nsuspend fun sameFile() { count++ }\n",
            ),
            (
                "Caller.kt",
                "package sample\nsuspend fun caller() { sample.state.count++ }\n",
            ),
        ],
        &["State.kt", "Caller.kt"],
    );
    assert!(fixture.0.join("State.java").is_file());
    assert!(
        fs::read_to_string(fixture.0.join("State.kt"))
            .unwrap()
            .contains("sample.state.State.count++")
    );
    assert!(
        fs::read_to_string(fixture.0.join("Caller.kt"))
            .unwrap()
            .contains("sample.state.State.count++")
    );
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn migrated_top_level_mutable_property_repairs_kotlin_qualified_reads_and_writes() {
    let fixture = Fixture::new(&[
        (
            "State.kt",
            "package sample.state\nvar calls = 0\nfun bump() { calls++ }\n",
        ),
        (
            "Runner.kt",
            "package sample\nsuspend fun retainedCaller() { sample.state.calls = 2; sample.state.calls++ }\n",
        ),
    ]);
    let java = fs::read_to_string(fixture.0.join("State.java")).expect("generated property facade");
    assert!(java.contains("public static int calls"), "{java}");
    let kotlin = fs::read_to_string(fixture.0.join("Runner.kt")).expect("retained caller");
    assert!(kotlin.contains("sample.state.State.calls = 2"), "{kotlin}");
    assert!(kotlin.contains("sample.state.State.calls++"), "{kotlin}");
}

#[test]
fn migrated_property_repairs_same_package_and_aliased_import_uses() {
    let fixture = Fixture::new(&[
        ("State.kt", "package sample.state\nvar count = 0\n"),
        (
            "Peer.kt",
            "package sample.state\nsuspend fun peer() { count++ }\n",
        ),
        (
            "Runner.kt",
            "package sample.runner\nimport sample.state.count as total\nsuspend fun runner() { total += 2 }\n",
        ),
    ]);
    let java = fs::read_to_string(fixture.0.join("State.java")).unwrap();
    assert!(java.contains("public static int count"), "{java}");
    let peer = fs::read_to_string(fixture.0.join("Peer.kt")).unwrap();
    assert!(peer.contains("sample.state.State.count++"), "{peer}");
    let runner = fs::read_to_string(fixture.0.join("Runner.kt")).unwrap();
    assert!(
        runner.contains("import sample.state.State.count as total"),
        "{runner}"
    );
    assert!(runner.contains("total += 2"), "{runner}");
}

#[test]
fn custom_accessors_and_initializer_groups_remain_kotlin_owned() {
    let fixture = Fixture::new(&[
        (
            "Custom.kt",
            "package sample.custom\nvar count = 0\n    get() = field + 1\n    set(value) { field = value }\n",
        ),
        (
            "Ordered.kt",
            "package sample.ordered\nval first = \"${System.getProperty(\"x\")}\"\nvar later = 0\n",
        ),
    ]);
    let custom = fs::read_to_string(fixture.0.join("Custom.kt")).unwrap();
    assert!(custom.contains("get() = field + 1"), "{custom}");
    assert!(!fixture.0.join("Custom.java").exists());
    let ordered = fs::read_to_string(fixture.0.join("Ordered.kt")).unwrap();
    assert!(ordered.contains("System.getProperty"), "{ordered}");
    assert!(ordered.contains("var later = 0"), "{ordered}");
    assert!(!fixture.0.join("Ordered.java").exists());
}

#[test]
fn callable_property_references_and_implicit_receivers_fail_closed() {
    let fixture = Fixture::new(&[
        (
            "Callable.kt",
            "package sample.callable\nvar count = 0\nfun reference() { val read = ::count }\n",
        ),
        (
            "Extension.kt",
            "package sample.extension\nvar count = 0\nfun Any?.bump() { count++ }\n",
        ),
        (
            "Lambda.kt",
            "package sample.lambda\nvar count = 0\nfun runIt() { Any().run { count++ } }\n",
        ),
    ]);
    for name in ["Callable", "Extension", "Lambda"] {
        let source = fs::read_to_string(fixture.0.join(format!("{name}.kt"))).unwrap();
        assert!(source.contains("var count = 0"), "{name}: {source}");
        assert!(!fixture.0.join(format!("{name}.java")).exists());
    }
}

#[test]
fn java_facade_wildcard_and_comment_jvmname_do_not_release_boolean_properties() {
    let fixture = Fixture::new(&[
        (
            "State.kt",
            "// @file:JvmName(\"Wrong\")\npackage sample.javause\nvar isReady = false\nvar calls = 0\n",
        ),
        (
            "JavaUse.java",
            "package sample.javause;\nimport static sample.javause.StateKt.*;\nfinal class JavaUse { boolean ready() { return isReady(); } int calls() { return getCalls(); } }\n",
        ),
    ]);
    let kotlin = fs::read_to_string(fixture.0.join("State.kt")).unwrap();
    assert!(kotlin.contains("var isReady = false"), "{kotlin}");
    assert!(kotlin.contains("var calls = 0"), "{kotlin}");
    assert!(!fixture.0.join("State.java").exists());
}

#[test]
fn ambiguous_wildcard_property_imports_retain_both_candidates() {
    let fixture = Fixture::new(&[
        ("First.kt", "package sample.first\nvar calls = 0\n"),
        ("Second.kt", "package sample.second\nvar calls = 0\n"),
        (
            "Use.kt",
            "package sample.use\nimport sample.first.*\nimport sample.second.*\nsuspend fun use() { calls++ }\n",
        ),
    ]);
    for name in ["First", "Second"] {
        let source = fs::read_to_string(fixture.0.join(format!("{name}.kt"))).unwrap();
        assert!(source.contains("var calls = 0"), "{name}: {source}");
        assert!(!fixture.0.join(format!("{name}.java")).exists());
    }
}

#[test]
fn effectful_file_initialization_moves_as_one_group_while_classes_remain_independent() {
    let fixture = Fixture::new(&[(
        "State.kt",
        "package sample\nval values: List<String> = listOf(\"first\")\nvar count = 0\nsuspend fun retained() {}\nfun ordinary(): Int = 1\nclass Independent(val value: String)\n",
    )]);
    let source = fs::read_to_string(fixture.0.join("State.kt")).unwrap();
    assert!(source.contains("val values:"), "{source}");
    assert!(source.contains("var count = 0"), "{source}");
    assert!(source.contains("fun ordinary()"), "{source}");
    assert!(
        source.contains("initialization order or timing"),
        "{source}"
    );
    assert!(!fixture.0.join("State.java").exists());
    assert!(fixture.0.join("Independent.java").is_file());
}
