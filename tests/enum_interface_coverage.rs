use clap::Parser;
use std::path::Path;

fn translate(source: &str) -> String {
    let cli = notlin::cli::Cli::parse_from(["notlin", "--annotations", "none", "Model.kt"]);
    let (files, errors, _, coverage) =
        notlin::transpiler::transpile(source, Path::new("Model.kt"), &cli);
    assert_eq!(errors, 0);
    assert!(!files.is_empty(), "source stayed Kotlin: {coverage:?}");
    files.into_iter().map(|(_, java)| java).collect()
}

#[test]
fn empty_enum_emits_no_fabricated_constants() {
    let java = translate("package fixture\nenum class Empty\n");
    assert!(java.contains("enum Empty"), "{java}");
    assert!(!java.contains("INSTANCE"), "{java}");
    assert!(java.contains(';'), "{java}");
}

#[test]
fn empty_enum_inferred_strings_keep_their_java_identity() {
    let java = translate(
        "package fixture\nenum class String {;\n fun label() = \"zero\"\n val text get() = \"zero\"\n}\n",
    );
    assert!(java.contains("java.lang.String label()"), "{java}");
    assert!(java.contains("java.lang.String getText()"), "{java}");
}

#[test]
fn parameterized_enum_can_have_multiple_companion_groups() {
    let java = translate(
        "package fixture\nenum class Kind(val code: Int) {\n A(1), B(2);\n companion object {\n val FIRST: Set<Kind> = setOf(A)\n val BOTH: Collection<Kind> = setOf(B, A, B)\n }\n}\n",
    );
    assert!(java.contains("enum Kind"), "{java}");
    assert!(java.contains("java.util.Set<Kind> FIRST"), "{java}");
    assert!(java.contains("java.util.Collection<Kind> BOTH"), "{java}");
    assert!(java.contains("asList(B, A, B)"), "{java}");
    assert_eq!(java.matches("class Companion").count(), 1, "{java}");
}

#[test]
fn interface_default_properties_infer_their_getter_result() {
    let java = translate(
        "package fixture\ninterface Named { val label get() = \"ok\"; var enabled: Boolean }\n",
    );
    assert!(java.contains("default String getLabel()"), "{java}");
    assert!(java.contains("boolean getEnabled()"), "{java}");
    assert!(java.contains("void setEnabled(boolean value)"), "{java}");
}

#[test]
fn interface_companion_functions_become_static_methods() {
    let java = translate(
        "package fixture\ninterface Named {\n companion object {\n fun answer(): Int = 42\n fun <T> echo(value: T): T = value\n fun touch() {}\n }\n}\n",
    );
    assert!(java.contains("static int answer()"), "{java}");
    assert!(java.contains("return 42;"), "{java}");
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC, NOTLIN_JAVAC, NOTLIN_JAVA, and NOTLIN_KOTLIN_STDLIB"]
fn original_and_migrated_enum_interface_application_agree() {
    use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
    use std::{fs, path::PathBuf, process::Command};

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "notlin-enum-interface-native-{}",
            std::process::id()
        ));
    fs::create_dir_all(&root).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    let sources = [
        ("Empty.kt", "package fixture\nenum class Empty\n"),
        (
            "String.kt",
            "package fixture\nenum class String {;\n fun label() = \"zero\"\n val text get() = \"zero\"\n}\n",
        ),
        (
            "Kind.kt",
            "package fixture\nenum class Kind(val code: Int) {\n A(1), B(2);\n companion object {\n val FIRST: Set<Kind> = setOf(A)\n val BOTH: Collection<Kind> = setOf(B, A, B)\n }\n}\n",
        ),
        (
            "Named.kt",
            "package fixture\ninterface Named {\n val label get() = \"ok\"\n var enabled: Boolean\n companion object {\n fun answer(): Int = 42\n fun <T> echo(value: T): T = value\n fun touch() {}\n }\n}\n",
        ),
        (
            "Impl.kt",
            "package fixture\nclass Impl(override var enabled: Boolean) : Named\n",
        ),
    ];
    for (name, source) in sources {
        fs::write(root.join(name), source).unwrap();
    }
    fs::write(root.join("Driver.java"), r#"package fixture;
public class Driver {
    public static void main(java.lang.String[] args) throws Exception {
        if (Empty.values().length != 0 || String.values().length != 0) throw new AssertionError("constants");
        if (String.class.getDeclaredMethod("label").getReturnType() != java.lang.String.class
            || String.class.getDeclaredMethod("getText").getReturnType() != java.lang.String.class)
            throw new AssertionError("inferred String identity");
        if (!new java.util.ArrayList<>(Kind.Companion.getBOTH()).equals(java.util.List.of(Kind.B, Kind.A)))
            throw new AssertionError("group order or deduplication");
        if (Kind.Companion.getBOTH() != Kind.Companion.getBOTH()) throw new AssertionError("group identity");
        if (!Kind.Companion.getFIRST().equals(java.util.Set.of(Kind.A)) || Kind.A.getCode() != 1)
            throw new AssertionError("group or constructor");
        Named item = new Impl(false);
        item.setEnabled(true);
        if (!item.getEnabled() || !item.getLabel().equals("ok")) throw new AssertionError("accessors");
        if (Named.Companion.answer() != 42 || !Named.Companion.echo("echo").equals("echo"))
            throw new AssertionError("companion function");
        Named.Companion.touch();
        Object singleton = Named.Companion;
        if (singleton != Named.Companion) throw new AssertionError("companion identity");
        System.out.println("enums,groups,accessors,companions=ok");
    }
}
"#).unwrap();
    let configure = |output: &str| {
        let tool = |name| PathBuf::from(std::env::var_os(name).expect(name));
        let mut config = JvmValidationConfig::new(
            tool("NOTLIN_KOTLINC"),
            tool("NOTLIN_JAVAC"),
            tool("NOTLIN_JAVA"),
            root.join(output),
        );
        config.classpath.push(tool("NOTLIN_KOTLIN_STDLIB"));
        config.kotlinc_args.extend([
            "-jvm-target".into(),
            "17".into(),
            "-Xjvm-default=all".into(),
        ]);
        config.javac_args.extend(["--release".into(), "17".into()]);
        for entry in fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path();
            match path.extension().and_then(|ext| ext.to_str()) {
                Some("kt") => config.kotlin_sources.push(path),
                Some("java") => config.java_sources.push(path),
                _ => {}
            }
        }
        config.run_main_class = Some("fixture.Driver".into());
        config
    };
    eprintln!("native fixture: {}", root.display());
    eprintln!("compiling/running original");
    let original = validate_jvm_sources(&configure("original-classes"))
        .unwrap_or_else(|error| panic!("original: {error}"));
    eprintln!("migrating fixture");
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--jvm-facts=off", "--annotations", "none", "--in-place"])
        .current_dir(&root)
        .arg(".")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        std::string::String::from_utf8_lossy(&output.stderr)
    );
    for (name, _) in sources {
        assert!(
            root.join(name.replace(".kt", ".java")).exists(),
            "{name} retained: {}",
            std::string::String::from_utf8_lossy(&output.stderr)
        );
    }
    eprintln!("compiling/running migrated");
    let migrated = validate_jvm_sources(&configure("migrated-classes"))
        .unwrap_or_else(|error| panic!("migrated: {error}"));
    assert_eq!(
        original.commands.last().unwrap().stdout.trim(),
        "enums,groups,accessors,companions=ok"
    );
    assert_eq!(
        original.commands.last().unwrap().stdout,
        migrated.commands.last().unwrap().stdout
    );
}
