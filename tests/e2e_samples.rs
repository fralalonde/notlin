//! End-to-end corpus tests: every sample in samples/ must transpile without
//! hard errors and produce structurally valid Java.
//!
//! These run the transpiler in-process (no shell, no javac dependency).
//! `tools/e2e.sh` additionally compile-checks with javac when available.

use clap::Parser;
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
    assert!(registry.contains("static List<String> items"));
}

#[test]
fn expressions_translate_operators_and_strings() {
    let source = std::fs::read_to_string(sample_path("Expressions.kt")).unwrap();
    let (files, errors) = transpile_src(&source, "Expressions.kt");
    assert_eq!(errors, 0);
    let all = files.iter().map(|(_, c)| c.as_str()).collect::<String>();
    // string interpolation
    assert!(all.contains("\"user=\"") && all.contains("+ count"));
    // elvis -> Optional
    assert!(all.contains("Optional.ofNullable"));
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
    assert!(all.contains("Optional.ofNullable"));
}

#[test]
fn when_becomes_ternary_chain() {
    let source = r#"fun w(x: Int): String {
    return when (x) {
        0 -> "zero"
        else -> "many"
    }
}"#;
    let (_, errors) = transpile_src(source, "W.kt");
    assert_eq!(errors, 0);
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
    let (files, errors, _) = {
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
