use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "notlin-jspecify-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn translate(root: &Path, source: &str, lombok: bool) -> Vec<PathBuf> {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("Model.kt"), source).unwrap();
    let mut command = Command::new(
        std::env::var_os("NOTLIN_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_notlin").into()),
    );
    command.current_dir(root).arg("--in-place");
    if lombok {
        command.arg("--lombok");
    }
    let result = command.arg(".").output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !root.join("Model.kt").exists(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    fs::read_dir(root)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension().is_some_and(|ext| ext == "java")).then_some(path)
        })
        .collect()
}

const RETURNS: &str = "package sample\nimport org.jspecify.annotations.NonNull\ninterface Base {fun getText():String\nfun getId():java.util.UUID\nfun getValues():Array<String>}\ninterface Child:Base {override fun getText():String\noverride fun getId():java.util.UUID\noverride fun getValues():Array<String>}\n";
const COLLISION: &str = "package sample\nclass NonNull\ninterface Base {fun getToken():NonNull}\ninterface Child:Base {override fun getToken():NonNull}\n";
const LITERAL: &str = "package sample\ninterface Base {fun getText():String}\ninterface Child:Base {override fun getText():String=\"@org.jspecify.annotations.NonNull\"}\n";
const LOMBOK: &str = "package sample\ninterface Base {val text:String?}\ndata class Child(override val text:String=\"value\"):Base\n";
const NULLABLE: &str = "package sample\nclass NullableTypes(val id:java.util.UUID?, val values:Array<String>?) {fun echo(input:java.util.UUID?):java.util.UUID?=input}\n";
const NULLABLE_COLLISION: &str = "package sample\nclass Nullable\ninterface Base {fun token():Nullable?}\ninterface Child:Base {override fun token():Nullable?}\n";
const FOREIGN_NULLABLE: &str = r#"package sample
IMPORT_MARKER
interface Repository {
    @SOURCE_NULLABLE
    fun find(key:String):java.util.Optional<String>
    fun echo(value:java.util.UUID?):java.util.UUID?=value
    fun array(value:Array<String>?):Array<String>?=value
    fun literal():String="@org.jspecify.annotations.Nullable"
}
data class Bean(val name:String)
"#;

fn foreign_case(root: &Path, package: &str, style: &str) -> String {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("Nullable.java"), format!("package {package}; @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME) @java.lang.annotation.Target({{java.lang.annotation.ElementType.METHOD,java.lang.annotation.ElementType.PARAMETER,java.lang.annotation.ElementType.FIELD}}) public @interface Nullable {{}}\n")).unwrap();
    let (import, annotation) = match style {
        "alias" => (format!("import {package}.Nullable as Maybe"), "Maybe"),
        "star" => (format!("import {package}.*"), "Nullable"),
        _ => (format!("import {package}.Nullable"), "Nullable"),
    };
    FOREIGN_NULLABLE
        .replace("IMPORT_MARKER", &import)
        .replace("SOURCE_NULLABLE", annotation)
}

#[test]
fn nullable_imports_preserve_explicit_aliased_and_star_library_annotations() {
    let scratch = Scratch::new();
    for (package, style) in [
        ("org.springframework.lang", "explicit"),
        ("other", "alias"),
        ("thirdparty", "star"),
    ] {
        let root = scratch.0.join(style);
        let source = foreign_case(&root, package, style);
        translate(&root, &source, false);
        let java = fs::read_to_string(root.join("Repository.java")).unwrap();
        if style == "alias" {
            assert!(java.contains("@other.Nullable"), "{java}");
            assert!(
                java.contains("import org.jspecify.annotations.Nullable;"),
                "{java}"
            );
            assert!(java.contains("java.util.@Nullable UUID"), "{java}");
            assert!(java.contains("String @Nullable []"), "{java}");
        } else {
            assert!(
                !java.contains("import org.jspecify.annotations.Nullable;"),
                "{java}"
            );
            assert!(
                java.contains("java.util.@org.jspecify.annotations.Nullable UUID"),
                "{java}"
            );
            assert!(
                java.contains("String @org.jspecify.annotations.Nullable []"),
                "{java}"
            );
        }
        assert!(
            java.contains("\"@org.jspecify.annotations.Nullable\""),
            "{java}"
        );
    }
    let root = scratch.0.join("source-only");
    foreign_case(&root, "org.springframework.lang", "explicit");
    translate(
        &root,
        "package sample\nimport org.springframework.lang.Nullable\ninterface Repository { @Nullable fun find(key:String):java.util.Optional<String> }\n",
        false,
    );
    let java = fs::read_to_string(root.join("Repository.java")).unwrap();
    assert!(
        java.contains("import org.springframework.lang.Nullable;"),
        "{java}"
    );
    assert!(
        !java.contains("import org.jspecify.annotations.Nullable;"),
        "{java}"
    );
    let root = scratch.0.join("type-name");
    translate(&root, NULLABLE_COLLISION, false);
    let java = fs::read_to_string(root.join("Child.java")).unwrap();
    assert!(
        !java.contains("import org.jspecify.annotations.Nullable;"),
        "{java}"
    );
    assert!(
        java.contains("@org.jspecify.annotations.Nullable Nullable"),
        "{java}"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, JSpecify, and Lombok jars in NOTLIN_* variables"]
fn real_toolchain_preserves_foreign_nullable_markers_values_and_lombok_processing() {
    use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
    let scratch = Scratch::new();
    for (package, style) in [
        ("org.springframework.lang", "explicit"),
        ("other", "alias"),
        ("thirdparty", "star"),
    ] {
        let mut expected = None;
        for (label, translated, lombok) in [
            ("original", false, false),
            ("plain", true, false),
            ("lombok", true, true),
        ] {
            let root = scratch.0.join(style).join(label);
            let source = foreign_case(&root, package, style);
            fs::write(root.join("Model.kt"), &source).unwrap();
            fs::write(root.join("Runner.java"), format!(r#"package sample;
public class Runner {{ public static void main(String[] args) throws Exception {{
Repository repo = new Repository() {{ public java.util.Optional<String> find(String key) {{ return java.util.Optional.of(key); }} }};
System.out.println(Repository.class.getMethod("find", String.class).isAnnotationPresent({package}.Nullable.class));
System.out.println(repo.echo(null));
System.out.println(repo.echo(new java.util.UUID(0L,1L)));
System.out.println(repo.array(null)==null);
System.out.println(repo.literal());
System.out.println(new Bean("bean").getName());
}} }}
"#)).unwrap();
            if translated {
                translate(&root, &source, lombok);
            }
            let mut cfg = JvmValidationConfig::new(
                std::env::var_os("NOTLIN_KOTLINC").unwrap(),
                std::env::var_os("NOTLIN_JAVAC").unwrap(),
                std::env::var_os("NOTLIN_JAVA").unwrap(),
                root.join("classes"),
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
            cfg.classpath.extend([
                PathBuf::from(std::env::var_os("NOTLIN_JSPECIFY").unwrap()),
                PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap()),
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"),
            ]);
            if lombok {
                cfg.classpath
                    .push(std::env::var_os("NOTLIN_LOMBOK").unwrap().into());
            }
            for file in fs::read_dir(&root).unwrap() {
                let path = file.unwrap().path();
                match path.extension().and_then(|ext| ext.to_str()) {
                    Some("kt") => cfg.kotlin_sources.push(path),
                    Some("java") => cfg.java_sources.push(path),
                    _ => {}
                }
            }
            cfg.run_main_class = Some("sample.Runner".into());
            let result = validate_jvm_sources(&cfg)
                .unwrap_or_else(|error| panic!("{style}/{label}: {error:?}"));
            let output = result.commands.last().unwrap().stdout.replace("\r\n", "\n");
            assert_eq!(
                output,
                "true\nnull\n00000000-0000-0000-0000-000000000001\ntrue\n@org.jspecify.annotations.Nullable\nbean\n",
                "{style}/{label}"
            );
            if let Some(expected) = &expected {
                assert_eq!(&output, expected, "{style}/{label}");
            } else {
                expected = Some(output);
            }
        }
    }
}

#[test]
fn nullable_qualified_types_and_arrays_annotate_the_outer_type() {
    let scratch = Scratch::new();
    translate(&scratch.0, NULLABLE, false);
    let java = fs::read_to_string(scratch.0.join("NullableTypes.java")).unwrap();
    assert!(!java.contains("@Nullable java.util"), "{java}");
    assert!(!java.contains("@Nullable String[]"), "{java}");
    assert!(java.contains("java.util.@Nullable UUID"), "{java}");
    assert!(java.contains("String @Nullable []"), "{java}");
}

#[test]
fn default_jspecify_annotations_use_imports_and_legal_type_use_positions() {
    let scratch = Scratch::new();
    translate(&scratch.0, RETURNS, false);
    let java = fs::read_to_string(scratch.0.join("Child.java")).unwrap();
    assert_eq!(
        java.matches("import org.jspecify.annotations.NonNull;")
            .count(),
        1,
        "{java}"
    );
    assert!(!java.contains("org.jetbrains.annotations"), "{java}");
    assert!(java.contains("@NonNull String getText()"), "{java}");
    assert!(java.contains("java.util.@NonNull UUID getId()"), "{java}");
    assert!(java.contains("String @NonNull [] getValues()"), "{java}");
}

#[test]
fn annotation_import_does_not_shadow_a_source_type_or_rewrite_a_literal() {
    let scratch = Scratch::new();
    let collision = scratch.0.join("collision");
    translate(&collision, COLLISION, false);
    let java = fs::read_to_string(collision.join("Child.java")).unwrap();
    assert!(
        !java.contains("import org.jspecify.annotations.NonNull;"),
        "{java}"
    );
    assert!(
        java.contains("@org.jspecify.annotations.NonNull NonNull"),
        "{java}"
    );
    let literal = scratch.0.join("literal");
    translate(&literal, LITERAL, false);
    let java = fs::read_to_string(literal.join("Child.java")).unwrap();
    assert!(
        java.contains("\"@org.jspecify.annotations.NonNull\""),
        "{java}"
    );
    assert_eq!(
        java.matches("import org.jspecify.annotations.NonNull;")
            .count(),
        1,
        "{java}"
    );
}

#[test]
fn lombok_null_checks_do_not_steal_the_jspecify_import() {
    let scratch = Scratch::new();
    translate(&scratch.0, LOMBOK, true);
    let java = fs::read_to_string(scratch.0.join("Child.java")).unwrap();
    assert!(java.contains("@lombok.NonNull"), "{java}");
    assert!(!java.contains("import lombok.NonNull;"), "{java}");
    assert!(java.contains("@NonNull String getText()"), "{java}");
    assert_eq!(
        java.matches("import org.jspecify.annotations.NonNull;")
            .count(),
        1,
        "{java}"
    );
}

#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, JSpecify, and Lombok jars in NOTLIN_* variables"]
fn real_toolchain_compiles_jspecify_returns_and_lombok_without_import_conflicts() {
    let scratch = Scratch::new();
    let jar = PathBuf::from(std::env::var_os("NOTLIN_JSPECIFY").expect("NOTLIN_JSPECIFY"));
    for (label, source, lombok) in [
        ("returns", RETURNS, false),
        ("collision", COLLISION, false),
        ("literal", LITERAL, false),
        ("lombok", LOMBOK, true),
        ("nullable", NULLABLE, false),
        ("nullable-type-name", NULLABLE_COLLISION, false),
    ] {
        let root = scratch.0.join(label);
        let java = translate(&root, source, lombok);
        let mut cp = vec![jar.clone()];
        if lombok {
            cp.push(std::env::var_os("NOTLIN_LOMBOK").unwrap().into());
        }
        let result = Command::new(std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"))
            .args([
                "--release",
                "21",
                if lombok { "-proc:full" } else { "-proc:none" },
            ])
            .arg("-classpath")
            .arg(std::env::join_paths(&cp).unwrap())
            .arg("-d")
            .arg(root.join("classes"))
            .args(java)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        if lombok {
            let caller = root.join("Caller.kt");
            fs::write(
                &caller,
                "package sample\nsuspend fun read(child:Child):Int=child.text.length\n",
            )
            .unwrap();
            cp.push(root.join("classes"));
            cp.push(std::env::var_os("NOTLIN_KOTLIN_STDLIB").unwrap().into());
            let result = Command::new(std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"))
                .args(["-jvm-target", "21", "-classpath"])
                .arg(std::env::join_paths(cp).unwrap())
                .arg("-d")
                .arg(root.join("kotlin-classes"))
                .arg(caller)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "retained Kotlin must see the narrowed non-null getter: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
}
