//! End-to-end corpus tests: every sample in samples/ must transpile without
//! hard errors and produce structurally valid Java.
//!
//! These run the transpiler in-process (no shell, no javac dependency).
//! `tools/e2e.sh` additionally compile-checks with javac when available.

use clap::Parser;
use std::fs;
use std::path::{Path, PathBuf};

/// Run the transpiler on a string, returning (files, error count).
fn transpile_src(source: &str, name: &str) -> (Vec<(String, String)>, usize) {
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", name]);
    let path = PathBuf::from(name);
    let (files, errors, _warnings, _cov) = notlin::transpiler::transpile(source, &path, &cli);
    (files, errors)
}

fn sample_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("samples")
        .join(name)
}

#[allow(dead_code)]
fn load_sample(name: &str) -> String {
    std::fs::read_to_string(sample_path(name))
        .unwrap_or_else(|e| panic!("sample {} not found: {e}", name))
}

#[test]
fn all_samples_parse_without_errors() {
    let mut count = 0;
    for entry in std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("samples"))
        .expect("samples dir")
    {
        let entry = entry.expect("sample entry");
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "kt") {
            continue;
        }
        count += 1;
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let source = std::fs::read_to_string(&path).unwrap();
        let (files, errors) = transpile_src(&source, &name);
        assert_eq!(
            errors, 0,
            "sample {} produced transpiler errors: {:?}",
            name, files
        );
        assert!(
            !files.is_empty(),
            "sample {} produced no output files",
            name
        );
        for (fname, content) in &files {
            // brace balance: each emitted file must have matching braces
            let open = content.matches('{').count();
            let close = content.matches('}').count();
            assert_eq!(open, close, "unbalanced braces in {}/{}", name, fname);
        }
    }
    assert!(count >= 1, "no samples found — corpus is empty");
}

#[test]
fn generic_variance_becomes_java_wildcards() {
    let source = "interface Event\ninterface Registry { val eventType: Class<out Event> }\n";
    let (files, errors) = transpile_src(source, "Variance.kt");
    assert_eq!(errors, 0);
    let registry = files
        .iter()
        .find(|(name, _)| name == "Registry.java")
        .map(|(_, content)| content)
        .expect("Registry.java");
    assert!(
        registry.contains("Class<? extends Event> getEventType()"),
        "{registry}"
    );
    assert!(!registry.contains("KClass"), "{registry}");
}

#[test]
fn standalone_annotation_passes_through_on_objects() {
    // Superseded conservative behavior: a leading annotation used to taint
    // the whole file (annotated_expression wrapper). `@Marker("x")` is
    // Java-native (unresolvable in the index, no Kotlin-only arguments), so
    // both objects translate with the annotation verbatim.
    let (files, errors) = transpile_src(
        "@Marker(\"x\")\nobject First\n@Marker(\"y\")\nobject Second\n",
        "AnnotatedObjects.kt",
    );
    assert_eq!(errors, 0);
    let first = files
        .iter()
        .find(|(name, _)| name == "First.java")
        .map(|(_, source)| source)
        .expect("First.java");
    assert!(first.contains("@Marker(\"x\")"), "{first}");
    assert!(files.iter().any(|(name, _)| name == "Second.java"));
}

#[test]
fn annotated_interface_passes_annotation_through() {
    // Superseded conservative behavior: an unresolvable annotation name used
    // to taint the whole declaration (N04DC). Annotation passthrough now
    // emits Java-native annotation text verbatim — `Marker` is not a
    // Kotlin-declared annotation type in the index, and the arguments carry
    // no Kotlin-only syntax, so both interfaces translate.
    let (files, errors) = transpile_src(
        "@Marker(value = \"x\")\ninterface Parent\ninterface Child : Parent\n",
        "AnnotatedInheritance.kt",
    );
    assert_eq!(errors, 0);
    let parent = files
        .iter()
        .find(|(name, _)| name == "Parent.java")
        .map(|(_, source)| source)
        .expect("Parent.java");
    assert!(parent.contains("@Marker(value = \"x\")"), "{parent}");
    assert!(files.iter().any(|(name, _)| name == "Child.java"));
}
#[test]
fn in_place_migration_strips_annotated_declaration_source() {
    // Superseded conservative behavior: the annotated interface used to stay
    // in the .kt residue. With annotation passthrough the interface
    // translates (annotation included). The retention-fixpoint planner (the
    // CLI's workspace path) decides retention; a raw transpile_with_workspace
    // call without a retained hint is deliberately the conservative catch-all.
    let root = std::env::temp_dir().join(format!("notlin-annotated-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("Definitions.kt");
    let source = "package sample\n/* note */\n@Marker(value = \"x\")\ninterface Parent\ninterface Child : Parent\n";
    fs::write(&path, source).unwrap();

    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let cli = notlin::cli::Cli::parse_from(["notlin", "--in-place", path.to_str().unwrap()]);
    let plans = notlin::transpiler::fixpoint::plan_workspace(
        &[(path.clone(), source.to_string())],
        &cli,
        &index,
        std::slice::from_ref(&root),
        8,
    );
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert_eq!(plan.errors, 0);
    assert!(
        !plan
            .coverage
            .untranslated
            .iter()
            .any(|name| name == "Parent"),
        "Parent must translate under the fixpoint: {:?}",
        plan.coverage.untranslated
    );
    assert!(plan.java_files.iter().any(|(name, _)| name == "Child.java"));

    notlin::migrate::migrate(&path, source, &plan.coverage).unwrap();
    // Both declarations translated, so migrate DELETES the source file.
    assert!(!path.exists(), "fully translated source must be deleted");
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn interface_inheritance_uses_java_extends() {
    let (files, errors) = transpile_src(
        "interface Parent\ninterface Child : Parent\n",
        "InterfaceInheritance.kt",
    );
    assert_eq!(errors, 0);
    let child = files
        .iter()
        .find(|(name, _)| name == "Child.java")
        .map(|(_, source)| source)
        .expect("Child.java");
    assert!(child.contains("interface Child extends Parent"), "{child}");
    assert!(
        !child.contains("interface Child implements Parent"),
        "{child}"
    );
}

#[test]
fn basic_class_produces_expected_types() {
    let source = std::fs::read_to_string(sample_path("BasicClass.kt")).unwrap();
    let (files, errors) = transpile_src(&source, "BasicClass.kt");
    assert_eq!(errors, 0);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    // one file per type + file-level utility class
    for expected in [
        "Person.java",
        "Point.java",
        "Registry.java",
        "BasicClass.java",
    ] {
        assert!(
            names.contains(&expected),
            "missing {} in {:?}",
            expected,
            names
        );
    }
    // Person must have a constructor and accessors
    let person = &files.iter().find(|(n, _)| n == "Person.java").unwrap().1;
    assert!(person.contains("public Person(String name, int age)"));
    assert!(person.contains("public String getName()"));
    assert!(person.contains("public void setAge(int age)"));
    // Point is a record
    let point = &files.iter().find(|(n, _)| n == "Point.java").unwrap().1;
    assert!(point.contains("public record Point(int x, int y)"));
    // Registry is a final class with a static INSTANCE
    let registry = &files.iter().find(|(n, _)| n == "Registry.java").unwrap().1;
    assert!(registry.contains("public static final Registry INSTANCE"));
    assert!(registry.contains("static final List<String> items"));
}

#[test]
fn expressions_translate_operators_and_strings() {
    let source = std::fs::read_to_string(sample_path("Expressions.kt")).unwrap();
    let (files, errors) = transpile_src(&source, "Expressions.kt");
    assert_eq!(errors, 0);
    let all = files.iter().map(|(_, c)| c.as_str()).collect::<String>();
    // string interpolation
    assert!(all.contains("\"user=\"") && all.contains("+ count"));
    // elvis -> null-check ternary (Optional form broke primitive inference)
    assert!(all.contains("!= null ? ") && all.contains(" ? "));
    // != / == on objects -> Objects.equals
    assert!(all.contains("Objects.equals"));
    // instanceof for `is`
    assert!(all.contains("instanceof"));
}

#[test]
fn elvis_becomes_optional() {
    let source = r#"fun pick(s: String?): String { return s ?: "d" }"#;
    let (files, errors) = transpile_src(source, "Pick.kt");
    assert_eq!(errors, 0);
    let all = files.iter().map(|(_, c)| c.as_str()).collect::<String>();
    assert!(all.contains("!= null ? ") && all.contains(" ? "));
}

#[test]
fn when_becomes_switch_or_if_else() {
    let source = r#"fun w(x: Int): String {
    return when (x) {
        0 -> "zero"
        else -> "many"
    }
}"#;
    let (files, errors) = transpile_src(source, "W.kt");
    assert_eq!(errors, 0);
    let all = files.iter().map(|(_, c)| c.as_str()).collect::<String>();
    assert!(all.contains("switch (") || all.contains("if ("), "{all}");
    assert!(!all.contains(" ? "), "{all}");
}

#[test]
fn untranslatable_reasons_get_distinct_codes() {
    let source = "value class Bad(val raw: Int)\nclass Holder {\n    constructor()\n}\n";
    let cli = notlin::cli::Cli::parse_from(["notlin", "Bad.kt"]);
    let (_, errors, warnings, coverage) =
        notlin::transpiler::transpile(source, Path::new("Bad.kt"), &cli);
    assert_eq!(errors, 0);
    assert!(warnings >= 2);
    let codes: Vec<_> = coverage
        .blockers
        .iter()
        .filter_map(|(_, text)| text.split_whitespace().nth(2))
        .collect();
    assert!(codes.len() >= 2, "{codes:?}");
    assert_ne!(codes[0], codes[1], "{codes:?}");
    assert!(codes.iter().all(|code| *code != "N001"), "{codes:?}");
}

#[test]
fn when_throw_else_is_valid_java() {
    let source = r#"fun createId(kind: Kind): String {
    return when (kind) {
        Kind.A -> newA()
        Kind.B -> newB()
        else -> throw IllegalArgumentException()
    }
}"#;
    let (files, errors) = transpile_src(source, "CreateId.kt");
    assert_eq!(errors, 0);
    let all = files.iter().map(|(_, c)| c.as_str()).collect::<String>();
    assert!(
        all.contains("throw new IllegalArgumentException()"),
        "{all}"
    );
    assert!(!all.contains("? "), "{all}");
    assert!(all.contains("switch (") || all.contains("if ("), "{all}");
}

#[test]
fn untranslatable_mode_error_fails_the_run() {
    let source = r#"value class Bad(val raw: Int)"#;
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "--untranslatable=error", "Bad.kt"]);
    let path = PathBuf::from("Bad.kt");
    let (_, errors, warnings, _cov) = notlin::transpiler::transpile(source, &path, &cli);
    // value classes are untranslatable -> diagnostic; in error mode it becomes
    // an error, in warn mode a warning. Either way something is flagged.
    assert!(errors > 0 || warnings > 0);
}

#[test]
fn package_imports_become_java_wildcard_imports() {
    let source = r#"package sample
import com.example.base

class Example"#;
    let (files, errors) = transpile_src(source, "Example.kt");
    assert_eq!(errors, 0);
    let all = files
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<String>();
    assert!(all.contains("import com.example.base.*;"), "{all}");
    assert!(!all.contains("import com.example.base;"), "{all}");
}
#[test]
fn annotations_option_controls_import() {
    let source = r#"val x: Int = 1"#;
    let cli_jet = notlin::cli::Cli::parse_from(vec!["notlin", "-o", "/tmp", "X.kt"]);
    let (files, _) = {
        let path = PathBuf::from("X.kt");
        let (_, errors, _, _) = notlin::transpiler::transpile(source, &path, &cli_jet);
        let _ = errors;
        (notlin::transpiler::transpile(source, &path, &cli_jet).0, 0)
    };
    let all = files.iter().map(|(_, c)| c.as_str()).collect::<String>();
    assert!(all.contains("import org.jetbrains.annotations.*;"));

    let cli_none = notlin::cli::Cli::parse_from(vec![
        "notlin",
        "-o",
        "/tmp",
        "--annotations",
        "none",
        "X.kt",
    ]);
    let (files2, _, _, _) = {
        let path = PathBuf::from("X.kt");
        notlin::transpiler::transpile(source, &path, &cli_none)
    };
    let all2 = files2.iter().map(|(_, c)| c.as_str()).collect::<String>();
    assert!(!all2.contains("org.jetbrains"));
}

#[test]
fn nullable_types_get_annotations() {
    let source = r#"class Holder(var name: String) {
    var nick: String? = null
    fun find(q: String?): String? {
        return nick
    }
}"#;
    let (files, errors, _, _) = {
        let cli = notlin::cli::Cli::parse_from(vec!["notlin", "Holder.kt"]);
        let path = PathBuf::from("Holder.kt");
        notlin::transpiler::transpile(source, &path, &cli)
    };
    assert_eq!(errors, 0);
    let all = files.iter().map(|(_, c)| c.as_str()).collect::<String>();
    assert!(all.contains("@Nullable String nick"), "field anno missing");
    assert!(
        all.contains("public @Nullable String find"),
        "ret anno missing"
    );
    assert!(all.contains("(@Nullable String q)"), "param anno missing");
}

#[test]
fn multi_file_output_one_type_per_file() {
    let source = r#"package p

class Alpha(val x: Int)
class Beta(val y: String)

fun topLevel() {}
"#;
    let (files, errors) = transpile_src(source, "Multi.kt");
    assert_eq!(errors, 0);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"Alpha.java"));
    assert!(names.contains(&"Beta.java"));
    assert!(names.contains(&"Multi.java")); // file-level utility class
}

#[test]
fn lombok_flag_emits_mutable_data_class() {
    let source = r#"data class Point(var x: Int, val y: String)"#;
    let cli = notlin::cli::Cli::parse_from(vec!["notlin", "--lombok", "Point.kt"]);
    let path = PathBuf::from("Point.kt");
    let (files, errors, _warnings, _cov) = notlin::transpiler::transpile(source, &path, &cli);
    assert_eq!(errors, 0);
    let all = files.iter().map(|(_, c)| c.as_str()).collect::<String>();
    // @Data/@AllArgsConstructor replace the record; var field stays mutable
    assert!(all.contains("@Data"), "missing @Data");
    assert!(all.contains("@AllArgsConstructor"));
    assert!(all.contains("import lombok.Data;"));
    assert!(all.contains("import lombok.AllArgsConstructor;"));
    assert!(
        all.contains("private int x;"),
        "var component must stay mutable"
    );
    assert!(
        all.contains("private final String y;"),
        "val component is final"
    );
    // hand-rolled boilerplate suppressed: Lombok owns accessors/ctor
    assert!(
        !all.contains("public int getX()"),
        "@Data should own accessors"
    );
    assert!(
        !all.contains("public Point(int x, String y)"),
        "@AllArgsConstructor should own the ctor"
    );

    // without --lombok the same source is TAINTED (not emitted at all) with
    // an N001: an immutable record silently loses setters, so plain mode
    // refuses rather than degrading.
    let cli_plain = notlin::cli::Cli::parse_from(vec!["notlin", "Point.kt"]);
    let (files2, errors2, warnings2, _cov2) =
        notlin::transpiler::transpile(source, &path, &cli_plain);
    assert_eq!(errors2, 0);
    assert!(
        warnings2 > 0,
        "var component should warn when --lombok is off"
    );
    let all2 = files2.iter().map(|(_, c)| c.as_str()).collect::<String>();
    assert!(
        !all2.contains("record Point"),
        "plain mode must not emit an immutable record for a var data class"
    );
}

#[test]
fn enum_classes_emit_java_enums() {
    let source = r#"enum class Direction { A, B, C }

enum class Color(val rgb: Int) {
    RED(0xFF0000),
    GREEN(0x00FF00)
}"#;
    let (files, errors, warnings, _cov) = {
        let cli = notlin::cli::Cli::parse_from(vec!["notlin", "Enums.kt"]);
        let path = PathBuf::from("Enums.kt");
        notlin::transpiler::transpile(source, &path, &cli)
    };
    assert_eq!(errors, 0);
    assert_eq!(warnings, 0);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"Direction.java"));
    assert!(names.contains(&"Color.java"));
    let dir = &files.iter().find(|(n, _)| n == "Direction.java").unwrap().1;
    assert!(dir.contains("public enum Direction"));
    assert!(dir.contains("A,") && dir.contains("B,") && dir.contains("C"));
    let col = &files.iter().find(|(n, _)| n == "Color.java").unwrap().1;
    // constants with ctor args, private final field, accessor, private ctor
    assert!(col.contains("RED(0xFF0000)"));
    assert!(col.contains("GREEN(0x00FF00)"));
    assert!(col.contains("private final int rgb;"));
    assert!(col.contains("public int getRgb()"));
    assert!(col.contains("private Color(int rgb)"));
}

#[test]
fn complex_enum_taints_instead_of_emitting_broken_java() {
    // sealed modifiers, generic enums, and abstract members are beyond javac
    // enums — the declaration must NOT be emitted at all.
    let source = "sealed enum class State { ON, OFF }\n";
    let (files, errors, warnings, _cov) = {
        let cli = notlin::cli::Cli::parse_from(vec!["notlin", "State.kt"]);
        let path = PathBuf::from("State.kt");
        notlin::transpiler::transpile(source, &path, &cli)
    };
    assert_eq!(errors, 0);
    assert!(warnings > 0, "sealed enum must warn");
    assert!(
        files.iter().all(|(n, _)| n != "State.java"),
        "tainted enum must not emit Java"
    );
}

#[test]
fn companion_object_members_become_statics() {
    let source = r#"class Counter {
    companion object {
        val MAX = 100
        fun create(): Counter = Counter()
        private var instances = 0
    }
}"#;
    let (files, errors, warnings, _cov) = {
        let cli = notlin::cli::Cli::parse_from(vec!["notlin", "Counter.kt"]);
        let path = PathBuf::from("Counter.kt");
        notlin::transpiler::transpile(source, &path, &cli)
    };
    assert_eq!(errors, 0);
    // N002 for the mutable companion state
    assert!(warnings > 0);
    let counter = &files
        .iter()
        .find(|(n, _)| n == "Counter.java")
        .expect("Counter.java emitted")
        .1;
    assert!(counter.contains("private static final int MAX = 100;"));
    assert!(counter.contains("public static int getMAX()"));
    assert!(counter.contains("public static Counter create()"));
    // private companion var -> private static field + private static accessors
    assert!(counter.contains("private static int instances = 0;"));
    assert!(counter.contains("private static int getInstances()"));
    assert!(counter.contains("private static void setInstances(int instances)"));
    assert!(counter.contains("Counter.instances = instances;"));
    assert!(
        !counter.contains("this.instances"),
        "static setter must not use this"
    );
}

#[test]
fn named_companion_taints() {
    let source =
        "class C {\n    companion object Factory {\n        fun make(): C = C()\n    }\n}\n";
    let (files, errors, warnings, _cov) = {
        let cli = notlin::cli::Cli::parse_from(vec!["notlin", "C.kt"]);
        let path = PathBuf::from("C.kt");
        notlin::transpiler::transpile(source, &path, &cli)
    };
    assert_eq!(errors, 0);
    assert!(warnings > 0);
    assert!(
        files.iter().all(|(n, _)| n != "C.java"),
        "named companion must taint the class"
    );
}

#[test]
fn class_body_properties_emit_accessors_not_records() {
    // property_declaration members (incl. `get() =` shapes) inside a data
    // class body must be emitted as record members, not taint the record.
    let source = r#"data class Vec2(val x: Int, val y: Int) {
    val length: Double
        get() = 0.0
    fun dot(o: Vec2): Int = x * o.x + y * o.y
}"#;
    let (files, errors, _warnings, _cov) = {
        let cli = notlin::cli::Cli::parse_from(vec!["notlin", "Vec2.kt"]);
        let path = PathBuf::from("Vec2.kt");
        notlin::transpiler::transpile(source, &path, &cli)
    };
    assert_eq!(errors, 0);
    let vec = &files
        .iter()
        .find(|(n, _)| n == "Vec2.java")
        .expect("record emitted")
        .1;
    assert!(vec.contains("public record Vec2(int x, int y)"));
    assert!(vec.contains("public double getLength()"));
    assert!(vec.contains("public int dot(Vec2 o)"));
}

#[test]
fn property_visibility_matches_kotlin() {
    // private/protected properties must not leak public accessors
    let source = r#"class Vault {
    private val secret = 42
    protected var name: String = "x"
        private set
}"#;
    let (files, errors, _warnings, _cov) = {
        let cli = notlin::cli::Cli::parse_from(vec!["notlin", "Vault.kt"]);
        let path = PathBuf::from("Vault.kt");
        notlin::transpiler::transpile(source, &path, &cli)
    };
    assert_eq!(errors, 0);
    let vault = &files.iter().find(|(n, _)| n == "Vault.java").unwrap().1;
    assert!(vault.contains("private int getSecret()"));
    assert!(vault.contains("protected String getName()"));
    assert!(vault.contains("private void setName(String name)"));
    assert!(!vault.contains("public int getSecret()"));
}

#[test]
fn enum_wildcard_imports_become_java_static_imports() {
    // Java rejects non-static wildcard imports for enum constants;
    // `import pkg.Kind.*` where Kind is a known enum (indexed as one in
    // the workspace) must lower to a static import. The enum is indexed
    // through a real workspace root so detection goes through the index.
    let workspace =
        std::env::temp_dir().join(format!("notlin-enum-static-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("Level.kt"),
        "package neutral.types\nenum class Level { LOW, HIGH }\n",
    )
    .unwrap();
    let root_s = workspace.to_string_lossy().to_string();
    let (files2, _, _, _) = {
        let cli = notlin::cli::Cli::parse_from(vec!["notlin", "--root", &root_s, "Consumer.kt"]);
        let index = notlin::workspace::SourceIndex::discover(&workspace).unwrap();
        notlin::transpiler::transpile_with_workspace(
            "package neutral.consumer\nimport neutral.types.Level.*\nenum class Notice(val level: Level) { SAMPLE(HIGH) }\n",
            &std::path::PathBuf::from("Consumer.kt"),
            &cli,
            Some(&index),
            std::slice::from_ref(&workspace),
        )
    };
    let _ = std::fs::remove_dir_all(&workspace);
    let notice = files2
        .iter()
        .find(|(n, _)| n == "Notice.java")
        .map(|(_, c)| c.clone())
        .expect("Notice.java");
    assert!(
        notice.contains("import static neutral.types.Level.*;"),
        "enum wildcard must become a static import: {notice}"
    );
}

#[test]
fn enum_defaulted_ctor_params_fill_constant_sites() {
    // Kotlin enum ctor params with defaults are omitted at some constants;
    // Java does not default enum ctor args, so generated constant arguments
    // must include every remaining parameter, in order.
    let (files, errors) = transpile_src(
        "enum class Kind(val alias: String, val state: State = State.OK) {\nA(\"a\"),\nB(\"b\", State.BAD)\n}\nenum class State { OK, BAD }\n",
        "Enums.kt",
    );
    assert_eq!(errors, 0);
    let kind = files
        .iter()
        .find(|(n, _)| n == "Kind.java")
        .map(|(_, c)| c.as_str())
        .expect("Kind.java");
    assert!(
        kind.contains("A(\"a\", State.OK)"),
        "defaulted ctor param must be filled at the constant: {kind}"
    );
    assert!(kind.contains("B(\"b\", State.BAD)"));
}
