use clap::Parser;
use notlin::cli::Cli;
use notlin::java_ir::{parse_java, render, tidy_generated_java};
use std::path::Path;

#[test]
fn imports_are_sorted_unique_and_pruned_using_syntax_and_documentation() {
    let source = r#"package sample;

import java.util.stream.Stream;
import java.util.Set;
import java.util.List;
import java.util.List;
import java.util.*;
import java.util.*;
import static java.util.Collections.emptyList;
import static java.util.Collections.emptyList;
import java.time.Instant;


/** See {@link Instant}. */
class Sample {
    List<String> values = emptyList();
    String literal = "Set Stream";


}
"#;
    let mut unit = parse_java(source).unwrap();
    tidy_generated_java(&mut unit);
    let java = render(&unit);
    assert_eq!(java.matches("import java.util.List;").count(), 1, "{java}");
    assert_eq!(java.matches("import java.util.*;").count(), 1, "{java}");
    assert_eq!(
        java.matches("import static java.util.Collections.emptyList;")
            .count(),
        1,
        "{java}"
    );
    assert!(!java.contains("import java.util.Set;"), "{java}");
    assert!(!java.contains("import java.util.stream.Stream;"), "{java}");
    assert!(java.contains("import java.time.Instant;"), "{java}");
    assert!(java.contains("String literal = \"Set Stream\";"), "{java}");
    assert!(
        java.find("import java.time.Instant;").unwrap()
            < java.find("import java.util.List;").unwrap()
    );
    assert!(!java.contains("\n\n\n"), "{java}");
    assert!(!java.contains("\n\n}"), "{java}");
    tidy_generated_java(&mut unit);
    assert_eq!(render(&unit), java);
    parse_java(&java).unwrap();
}

#[test]
fn formatting_preserves_comments_and_text_block_contents() {
    let source = "// Header\npackage sample;\nimport z.Z; // Explain Z\nimport b.B;\n/* group */\nimport a.A;\nclass Sample {\n    String text = \"\"\"\n        List Stream\n\n\n        }\n        \"\"\";\n    // Keep this comment.\n\n\n}\n";
    let mut unit = parse_java(source).unwrap();
    tidy_generated_java(&mut unit);
    let java = render(&unit);
    assert!(java.starts_with("// Header\npackage sample;"), "{java}");
    assert!(java.contains("// Explain Z"), "{java}");
    assert!(java.contains("import z.Z; // Explain Z"), "{java}");
    assert!(java.contains("/* group */"), "{java}");
    assert!(java.contains("// Keep this comment."), "{java}");
    assert!(
        java.contains("\"\"\"\n        List Stream\n\n\n        }\n        \"\"\""),
        "{java}"
    );
    parse_java(&java).unwrap();
}

#[test]
#[ignore = "requires the accepted reference Java corpus manifest"]
fn accepted_reference_corpus_keeps_every_body_token_and_remains_valid_java() {
    use notlin::java_ir::{JavaElement, JavaSyntaxNode};
    fn body_tokens(node: &JavaSyntaxNode, tokens: &mut Vec<String>) {
        if node.category == notlin::java_ir::JavaCategory::Import {
            return;
        }
        for element in &node.children {
            match element {
                JavaElement::Token(token) if !token.text.is_empty() => {
                    tokens.push(token.text.clone())
                }
                JavaElement::Token(_) => {}
                JavaElement::Declaration(child)
                | JavaElement::Type(child)
                | JavaElement::Expression(child)
                | JavaElement::Statement(child)
                | JavaElement::Annotation(child)
                | JavaElement::Import(child)
                | JavaElement::Other(child) => body_tokens(child, tokens),
            }
        }
    }
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/idiomatic-java-20261102");
    let manifest = std::fs::read_to_string(artifacts.join("corpus-manifest.txt")).unwrap();
    let mut files = 0;
    let mut before_imports = 0;
    let mut after_imports = 0;
    for path in manifest.lines() {
        let source = std::fs::read_to_string(path).unwrap();
        assert!(source.starts_with("// NOTLIN:"));
        let mut unit = parse_java(&source).unwrap();
        let mut before = Vec::new();
        body_tokens(&unit.root, &mut before);
        before_imports += unit
            .root
            .children
            .iter()
            .filter(|element| matches!(element, JavaElement::Import(_)))
            .count();
        tidy_generated_java(&mut unit);
        after_imports += unit
            .root
            .children
            .iter()
            .filter(|element| matches!(element, JavaElement::Import(_)))
            .count();
        let java = render(&unit);
        let parsed = parse_java(&java).unwrap_or_else(|error| panic!("{path}: {error}"));
        let mut after = Vec::new();
        body_tokens(&parsed.root, &mut after);
        assert_eq!(before, after, "body tokens changed: {path}");
        tidy_generated_java(&mut unit);
        assert_eq!(render(&unit), java, "not idempotent: {path}");
        files += 1;
    }
    assert!(files > 4000);
    std::fs::write(artifacts.join("corpus-result.json"), format!("{{\"files\":{files},\"imports_before\":{before_imports},\"imports_after\":{after_imports},\"all_body_tokens_preserved\":true,\"all_java_parses\":true,\"idempotent\":true}}\n")).unwrap();
}

const MODEL: &str = r#"package sample
interface Named {
    val label: String
    fun absent(value: String?): Boolean = value == null
    fun present(value: String?): Boolean = null != value
    fun same(value: String?): Boolean = value == label
}
enum class Phase(@get:java.lang.Deprecated override val label: String, var rank: Int) : Named {
    READY("ready", 1), DONE("done", 2)
}
enum class Flag(val enabled: Boolean) { ON(true), OFF(false) }
enum class Ready(val isReady: Boolean) { YES(true), NO(false) }
enum class NullableFlag(val isReady: Boolean?) { YES(true), UNKNOWN(null) }
enum class WithBody(val label: String) { ONE("one"); val extra: Int = 3 }
class Bag(val values: List<String>)
class Probe {
    var calls: Int = 0
    fun next(): String? { calls = calls + 1; return null }
    fun absent(): Boolean = next() == null
    fun primitiveMissing(value: Int): Boolean = value == null
}
"#;

fn translate(source: &str, lombok: bool) -> Vec<(String, String)> {
    let mut args = vec!["notlin", "--annotations", "none"];
    if lombok {
        args.push("--lombok");
    }
    args.push("Model.kt");
    let cli = Cli::parse_from(args);
    let (files, errors, _, _) = notlin::transpiler::transpile(source, Path::new("Model.kt"), &cli);
    assert_eq!(errors, 0);
    files
}

#[test]
fn both_modes_emit_minimal_imports_direct_null_checks_and_appropriate_accessors() {
    for lombok in [false, true] {
        let files = translate(MODEL, lombok);
        let java = |name: &str| &files.iter().find(|(path, _)| path == name).unwrap().1;
        let named = java("Named.java");
        assert!(named.contains("value == null"), "{named}");
        assert!(named.contains("null != value"), "{named}");
        assert!(
            named.contains("Objects.equals(value, this.getLabel())"),
            "{named}"
        );
        assert!(named.contains("import java.util.Objects;"), "{named}");
        let bag = java("Bag.java");
        assert!(bag.contains("import java.util.List;"), "{bag}");
        let phase = java("Phase.java");
        if lombok {
            assert!(phase.contains("@Getter\npublic enum Phase"), "{phase}");
            assert!(!phase.contains("getLabel()"), "{phase}");
            assert!(
                phase.contains("onMethod_ = {@java.lang.Deprecated}"),
                "{phase}"
            );
            assert!(!phase.contains("setRank("), "{phase}");
            assert!(phase.contains("@Setter\n    private int rank;"), "{phase}");
        } else {
            assert!(phase.contains("getLabel()"), "{phase}");
            assert!(phase.contains("setRank("), "{phase}");
        }
        // Kotlin getEnabled() differs from Lombok isEnabled().
        let flag = java("Flag.java");
        assert!(flag.contains("getEnabled()"), "{flag}");
        assert!(!flag.contains("@Getter"), "{flag}");
        let ready = java("Ready.java");
        assert_eq!(ready.contains("@Getter"), lombok, "{ready}");
        let nullable_flag = java("NullableFlag.java");
        assert!(nullable_flag.contains("isReady()"), "{nullable_flag}");
        assert!(!nullable_flag.contains("@Getter"), "{nullable_flag}");
        let body = java("WithBody.java");
        assert!(body.contains("getLabel()"), "{body}");
        assert!(!body.contains("@Getter"), "{body}");
        for (_, source) in &files {
            assert!(!source.contains("import java.util.*;"), "{source}");
            assert!(
                !source.contains("import java.util.stream.Stream;"),
                "{source}"
            );
            assert!(!source.contains("\n\n\n"), "{source}");
            parse_java(source).unwrap();
        }
    }
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA and NOTLIN_LOMBOK"]
fn real_toolchain_preserves_accessors_annotations_null_checks_and_evaluation_count() {
    use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
    use std::fs;
    use std::path::PathBuf;
    let root = std::env::temp_dir().join(format!("notlin-idiomatic-{}", std::process::id()));
    assert!(!root.exists());
    fs::create_dir_all(&root).unwrap();
    let runner = r#"package sample;
public class Runner {
    public static void main(String[] args) throws Exception {
        Phase phase = Phase.READY;
        phase.setRank(7);
        System.out.println(phase.getLabel() + ":" + phase.getRank());
        System.out.println(phase.absent(null) + ":" + phase.absent("x"));
        System.out.println(phase.present(null) + ":" + phase.present("x"));
        System.out.println(phase.same(null) + ":" + phase.same("ready"));
        System.out.println(Phase.class.getMethod("getLabel").isAnnotationPresent(Deprecated.class));
        System.out.println(Flag.ON.getEnabled() + ":" + Ready.YES.isReady());
        System.out.println(NullableFlag.UNKNOWN.isReady() == null);
        System.out.println(WithBody.ONE.getLabel() + ":" + WithBody.ONE.getExtra());
        System.out.println(new Bag(java.util.List.of("a", "b")).getValues().size());
        Probe probe = new Probe();
        System.out.println(probe.absent() + ":" + probe.getCalls());
        System.out.println(probe.primitiveMissing(42));
    }
}
"#;
    for (mode, lombok) in [("original", false), ("plain", false), ("lombok", true)] {
        let directory = root.join(mode);
        fs::create_dir_all(&directory).unwrap();
        if mode == "original" {
            fs::write(directory.join("Model.kt"), MODEL).unwrap();
        } else {
            for (name, java) in translate(MODEL, lombok) {
                fs::write(directory.join(name), java).unwrap();
            }
        }
        fs::write(directory.join("Runner.java"), runner).unwrap();
        let mut cfg = JvmValidationConfig::new(
            std::env::var_os("NOTLIN_KOTLINC").unwrap(),
            std::env::var_os("NOTLIN_JAVAC").unwrap(),
            std::env::var_os("NOTLIN_JAVA").unwrap(),
            directory.join("classes"),
        );
        cfg.kotlinc_args.extend([
            "-jvm-target".into(),
            "17".into(),
            "-jvm-default=no-compatibility".into(),
        ]);
        cfg.javac_args.extend([
            "--release".into(),
            "17".into(),
            if lombok { "-proc:full" } else { "-proc:none" }.into(),
        ]);
        cfg.classpath.push(PathBuf::from(
            std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap(),
        ));
        cfg.classpath.push(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"),
        );
        if lombok {
            cfg.classpath
                .push(std::env::var_os("NOTLIN_LOMBOK").unwrap().into());
        }
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("kt") => cfg.kotlin_sources.push(path),
                Some("java") => cfg.java_sources.push(path),
                _ => {}
            }
        }
        cfg.run_main_class = Some("sample.Runner".into());
        let result = validate_jvm_sources(&cfg).unwrap_or_else(|error| panic!("{mode}: {error:?}"));
        let output = result.commands.last().unwrap().stdout.replace("\r\n", "\n");
        assert_eq!(
            output,
            "ready:7\ntrue:false\nfalse:true\nfalse:true\ntrue\ntrue:true\ntrue\none:3\n2\ntrue:1\nfalse\n",
            "{mode}"
        );
    }
    let resolved = root.canonicalize().unwrap();
    assert!(resolved.starts_with(std::env::temp_dir().canonicalize().unwrap()));
    assert_eq!(resolved.file_name(), root.file_name());
    fs::remove_dir_all(&resolved).unwrap();
}
