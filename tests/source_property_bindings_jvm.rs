//! Real Kotlin/JVM differential for source-bound static property imports.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn tool(key: &str) -> PathBuf {
    std::env::var_os(key)
        .unwrap_or_else(|| panic!("{key} must be set"))
        .into()
}
fn checked(command: &mut Command) -> String {
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{command:?}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}
fn sources(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut kotlin = Vec::new();
    let mut java = Vec::new();
    for entry in fs::read_dir(root).unwrap() {
        let file = entry.unwrap().path();
        match file.extension().and_then(|s| s.to_str()) {
            Some("kt") => kotlin.push(file),
            Some("java") => java.push(file),
            _ => {}
        }
    }
    (kotlin, java)
}
fn compile(root: &Path, classes: &Path, kotlinc: &Path, javac: &Path, stdlib: &Path) {
    fs::create_dir_all(classes).unwrap();
    let (kotlin, java) = sources(root);
    if !kotlin.is_empty() {
        checked(
            Command::new(kotlinc)
                .args(&kotlin)
                .args(&java)
                .arg("-d")
                .arg(classes),
        );
    }
    if !java.is_empty() {
        checked(
            Command::new(javac)
                .arg("-cp")
                .arg(format!("{}:{}", classes.display(), stdlib.display()))
                .arg("-d")
                .arg(classes)
                .args(&java),
        );
    }
}

#[test]
#[ignore = "requires NOTLIN_KOTLINC/JAVAC/JAVA/KOTLIN_STDLIB, new full NOTLIN_JVM_FACTS_JAR and NOTLIN_ACCEPTED_CLI"]
fn source_bound_constants_preserve_retained_constructors_templates_and_unchanged_callers() {
    let kotlinc = tool("NOTLIN_KOTLINC");
    let javac = tool("NOTLIN_JAVAC");
    let java = tool("NOTLIN_JAVA");
    let stdlib = tool("NOTLIN_KOTLIN_STDLIB");
    let helper = tool("NOTLIN_JVM_FACTS_JAR");
    let accepted = tool("NOTLIN_ACCEPTED_CLI");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let scratch = Scratch(std::env::temp_dir().join(format!(
        "notlin-native-property-{}-{nonce}",
        std::process::id()
    )));
    fs::create_dir(&scratch.0).unwrap();
    let state = "package probe\n// π precedes the declarations and their source offsets.\nconst val TOKEN: String = \"token\";\n\nclass Failure : RuntimeException {\n    constructor(value: Int) : super(\"$TOKEN:$value\")\n    constructor(value: String) : super(value.replace(TOKEN, \"fixed\"))\n    override fun toString(): String = StringBuilder().append(TOKEN).toString()\n}\nclass Reader {\n    fun read(): String = \"$TOKEN\"\n    val value: String\n        get() = \"$TOKEN\"\n    fun trim(input: String?): String? = input?.trim()\n}\n";
    for (label,files) in [
        ("constructors",vec![("State.kt",state.to_owned())]),
        ("crlf",vec![("State.kt",state.replace('\n',"\r\n"))]),
        ("alias",vec![("State.kt","package probe\nconst val TOKEN: String = \"token\"\n".to_owned()),("Reader.kt","package probe\nimport probe.TOKEN as label\nclass Reader {\n    fun read(): String = \"$label\"\n    val value: String\n        get() = label\n    fun trim(input: String?): String? = input?.trim()\n}\n".to_owned()),("Failure.kt","package probe\nclass Failure : RuntimeException {\n    constructor(value: Int) : super(\"token:$value\")\n    constructor(value: String) : super(value.replace(\"token\", \"fixed\"))\n}\n".to_owned())]),
    ] {
        let base=scratch.0.join(label);let original=base.join("original");let classes=base.join("original-classes");let caller=base.join("caller");
        fs::create_dir_all(&original).unwrap();fs::create_dir_all(&caller).unwrap();
        for (name,source) in &files {fs::write(original.join(name),source).unwrap();}
        compile(&original,&classes,&kotlinc,&javac,&stdlib);
        let driver=base.join("Driver.java");
        fs::write(&driver,r#"package probe;
public class Driver { public static void main(String[] args) {
 System.out.println(new Reader().read()); System.out.println(new Reader().getValue());
 System.out.println(new Failure(7).getMessage()); System.out.println(new Failure("token").getMessage());
 for(String value:new String[]{null,"\u00a0 x \u00a0"}) System.out.println(new Reader().trim(value));
} }
"#).unwrap();
        checked(Command::new(&javac).arg("-cp").arg(&classes).arg("-d").arg(&caller).arg(&driver));
        let run=|classes:&Path| checked(Command::new(&java).arg("-cp").arg(format!("{}:{}:{}",caller.display(),classes.display(),stdlib.display())).arg("probe.Driver"));
        let expected=run(&classes);assert_eq!(expected,"token\ntoken\ntoken:7\nfixed\nnull\nx\n");
        for (version,binary) in [("accepted",accepted.clone()),("candidate",PathBuf::from(env!("CARGO_BIN_EXE_notlin")))] {
            let root=base.join(version);fs::create_dir_all(&root).unwrap();
            for (name,source) in &files {fs::write(root.join(name),source).unwrap();}
            let output=Command::new(&binary).current_dir(&root)
                .args(["--annotations","none","--jvm-facts","on","--in-place","--coverage-report","coverage.json","."])
                .env_remove("NOTLIN_JVM_FACTS_FULL_JAR").env_remove("NOTLIN_JVM_FACTS_KOTLIN").env_remove("NOTLIN_CLASSPATH")
                .env("NOTLIN_JVM_FACTS_JAR",&helper).env("NOTLIN_KOTLIN_SOURCES",&root).output().unwrap();
            assert!(output.status.success(),"{label}/{version}: {}",String::from_utf8_lossy(&output.stderr));
            let report=fs::read_to_string(root.join("coverage.json")).unwrap();
            assert_eq!(root.join("State.java").exists(),version=="candidate","{label}/{version}: {report}");
            if version == "candidate" { assert!(!root.join("Reader.java").exists(),"{label}/{version}: field binding cannot sign consumer operations: {report}"); }
            let mixed=base.join(format!("{version}-classes"));compile(&root,&mixed,&kotlinc,&javac,&stdlib);
            // The accepted binary is the ownership comparator, not the behavior
            // oracle: its already-emitted Reader.trim drops the null guard.
            // Compare candidate behavior to independently compiled Kotlin.
            if version == "candidate" { assert_eq!(run(&mixed),expected,"{label}/{version}: original-compiled caller"); }
            if version=="candidate" {
                let javap=javac.with_file_name("javap");
                let signature=checked(Command::new(&javap).args(["-p","-s","-classpath"]).arg(&mixed).arg("probe.State"));
                assert!(signature.contains("public static final java.lang.String TOKEN;"),"{signature}");
                assert!(signature.contains("descriptor: Ljava/lang/String;"),"{signature}");
                // The pre-migration Java compiler inlines Kotlin const fields;
                // inspect the generated field value independently as well.
                let inspector=base.join("Inspect.java");
                fs::write(&inspector,"package probe; public class Inspect { public static void main(String[] args) { System.out.println(State.TOKEN); } }").unwrap();
                checked(Command::new(&javac).arg("-cp").arg(&mixed).arg("-d").arg(&mixed).arg(inspector));
                let value=checked(Command::new(&java).arg("-cp").arg(&mixed).arg("probe.Inspect"));assert_eq!(value,"token\n");
            }
        }
    }
}

fn check_import_repair(label: &str, files: &[(&str, &str)], package: &str, prefix: Option<&str>) {
    let kotlinc = tool("NOTLIN_KOTLINC");
    let javac = tool("NOTLIN_JAVAC");
    let java = tool("NOTLIN_JAVA");
    let stdlib = tool("NOTLIN_KOTLIN_STDLIB");
    let helper = tool("NOTLIN_JVM_FACTS_JAR");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let scratch = Scratch(std::env::temp_dir().join(format!(
        "notlin-import-repair-{label}-{}-{nonce}",
        std::process::id()
    )));
    fs::create_dir(&scratch.0).unwrap();
    let original = scratch.0.join("original");
    let root = scratch.0.join("candidate");
    fs::create_dir(&original).unwrap();
    fs::create_dir(&root).unwrap();
    for (name, source) in files {
        fs::write(original.join(name), source).unwrap();
        fs::write(root.join(name), source).unwrap();
    }
    let original_classes = scratch.0.join("original-classes");
    compile(&original, &original_classes, &kotlinc, &javac, &stdlib);
    let callers = scratch.0.join("callers");
    fs::create_dir(&callers).unwrap();
    let driver = scratch.0.join("Driver.java");
    let package_header = if package.is_empty() {
        String::new()
    } else {
        format!("package {package};\n")
    };
    fs::write(&driver,format!("{package_header}public class Driver {{ public static void main(String[] args) {{ System.out.println(new Reader().read()); }} }}")).unwrap();
    checked(
        Command::new(&javac)
            .arg("-cp")
            .arg(&original_classes)
            .arg("-d")
            .arg(&callers)
            .arg(&driver),
    );
    let main_class = if package.is_empty() {
        "Driver".to_owned()
    } else {
        format!("{package}.Driver")
    };
    let run = |classes: &Path| {
        checked(
            Command::new(&java)
                .arg("-cp")
                .arg(format!(
                    "{}:{}:{}",
                    callers.display(),
                    classes.display(),
                    stdlib.display()
                ))
                .arg(&main_class),
        )
    };
    let expected = run(&original_classes);
    assert_eq!(expected, "xtoken\n");
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root)
        .args([
            "--annotations",
            "none",
            "--jvm-facts",
            "on",
            "--in-place",
            "--coverage-report",
            "coverage.json",
            ".",
        ])
        .env_remove("NOTLIN_JVM_FACTS_FULL_JAR")
        .env_remove("NOTLIN_JVM_FACTS_KOTLIN")
        .env_remove("NOTLIN_CLASSPATH")
        .env("NOTLIN_JVM_FACTS_JAR", &helper)
        .env("NOTLIN_KOTLIN_SOURCES", &root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{label}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        root.join("State.java").exists(),
        label != "wildcard-collision",
        "{label}: only a completely bound constant may translate"
    );
    if label == "wildcard-collision" {
        assert!(
            !root.join("Anchor.java").exists(),
            "retaining the target must not release its unsigned consumer"
        );
        assert!(root.join("Plain.java").exists());
    }
    assert!(
        !root.join("Reader.java").exists(),
        "{label}: a binding proof must not translate consumer operations"
    );
    let residual = fs::read_to_string(root.join("Reader.kt")).unwrap();
    if let Some(prefix) = prefix {
        assert!(
            residual.find(prefix).unwrap() < residual.find("import State.TOKEN").unwrap(),
            "{label}: misplaced import: {residual}"
        );
    }
    if label == "sibling" {
        assert!(
            root.join("Plain.java").exists(),
            "unrelated safe sibling must still translate"
        );
    }
    let classes = scratch.0.join("mixed-classes");
    compile(&root, &classes, &kotlinc, &javac, &stdlib);
    assert_eq!(run(&classes), expected, "{label}: original-compiled caller");
}

#[test]
#[ignore = "requires explicit real Kotlin/JVM tools and full helper"]
fn source_property_wildcard_import_does_not_capture_a_callable_binding() {
    check_import_repair(
        "wildcard-collision",
        &[
            (
                "State.kt",
                "package source\nimport java.util.*\nconst val TOKEN: String = \"token\"\nclass Anchor {\n fun text(): String = \"$TOKEN\"\n}\n",
            ),
            (
                "Reader.kt",
                "package probe\nimport source.*\nfun TOKEN(): String = \"\"\nclass Reader : RuntimeException {\n constructor() : super(TOKEN)\n fun read(): String = StringBuilder().append(\"x\").append(TOKEN).append(TOKEN()).toString()\n}\n",
            ),
            (
                "Plain.kt",
                "package probe\nclass Plain { fun identity(value: Int): Int = value }\n",
            ),
        ],
        "probe",
        None,
    );
}

#[test]
#[ignore = "requires explicit real Kotlin/JVM tools and full helper"]
fn source_property_wildcard_import_keeps_argument_consumers_kotlin() {
    check_import_repair(
        "wildcard-consumer",
        &[
            (
                "State.kt",
                "package source\nimport java.util.*\nconst val TOKEN: String = \"token\"\nclass Anchor {\n fun text(): String = \"$TOKEN\"\n}\n",
            ),
            (
                "Reader.kt",
                "package probe\nimport source.*\ninterface Contract {\n fun read(): String\n}\nclass Reader : Contract {\n override fun read(): String = StringBuilder().append(\"x\").append(TOKEN).toString()\n}\n",
            ),
        ],
        "probe",
        None,
    );
}

#[test]
#[ignore = "requires explicit real Kotlin/JVM tools and full helper"]
fn source_property_argument_reads_keep_inherited_consumers_kotlin() {
    check_import_repair(
        "argument",
        &[
            (
                "State.kt",
                "package probe\nimport java.util.*\nconst val TOKEN: String = \"token\"\nclass Anchor {\n fun text(): String = \"$TOKEN\"\n}\n",
            ),
            (
                "Reader.kt",
                "package probe\ninterface Contract {\n fun read(): String\n}\nclass Reader : Contract {\n override fun read(): String = StringBuilder().append(\"x\").append(TOKEN).toString()\n}\n",
            ),
        ],
        "probe",
        None,
    );
}

#[test]
#[ignore = "requires explicit real Kotlin/JVM tools and full helper"]
fn source_property_import_survives_a_translated_consumer_sibling() {
    check_import_repair(
        "sibling",
        &[
            (
                "State.kt",
                "package probe\nconst val TOKEN: String = \"token\"\n",
            ),
            (
                "Reader.kt",
                "package probe\nimport probe.TOKEN as label\nclass Reader {\n fun read(): String = \"x$label\"\n}\nclass Plain {\n fun identity(value: Int): Int = value\n}\n",
            ),
        ],
        "probe",
        None,
    );
}

#[test]
#[ignore = "requires explicit real Kotlin/JVM tools and full helper"]
fn source_property_import_follows_file_annotations_and_shebangs() {
    for (label, prefix) in [
        ("annotation", "@file:JvmName(\"Carrier\")\n"),
        ("shebang", "#!/usr/bin/env kotlin\n"),
    ] {
        let reader = format!("{prefix}class Reader {{\n fun read(): String = \"x$TOKEN\"\n}}\n");
        check_import_repair(
            label,
            &[
                ("State.kt", "const val TOKEN: String = \"token\"\n"),
                ("Reader.kt", &reader),
            ],
            "",
            Some(prefix.trim_end()),
        );
    }
}
