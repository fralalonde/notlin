//! Native differential for a wildcard-proved constant field type.
//!
//! The Rust-level regression pins the CLI shape; this one proves the *Java*
//! half of the shared proof with real compilers: the emitted field type is
//! name-bound, so a same-package class called `String` — the capture a plain
//! source spelling would suffer — still leaves `java.lang.String` in the field
//! descriptor, and the unchanged original caller keeps running.
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

const STATE: &str = "package probe\nimport java.util.*\nconst val TOKEN: String = \"token\"\nclass Reader {\n    fun read(): String = \"[$TOKEN]\"\n    fun pad(v: String?): String? = v?.trim()\n}\n";

#[test]
#[ignore = "requires NOTLIN_KOTLINC/NOTLIN_JAVAC/NOTLIN_JAVA/NOTLIN_KOTLIN_STDLIB and the real full NOTLIN_JVM_FACTS_JAR"]
fn wildcard_field_type_stays_canonical_when_a_local_string_class_exists() {
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
        "notlin-native-field-type-{}-{nonce}",
        std::process::id()
    )));
    let original = scratch.0.join("original");
    let classes = scratch.0.join("original-classes");
    let caller = scratch.0.join("caller");
    let mixed = scratch.0.join("mixed-classes");
    let migrated = scratch.0.join("migrated");
    for dir in [&original, &caller, &migrated] {
        fs::create_dir_all(dir).unwrap();
    }
    let caller_file = caller.join("Driver.java");
    fs::write(
        &caller_file,
        "package probe;\npublic class Driver { public static void main(String[] args) {\n System.out.println(new Reader().read());\n System.out.println(new Reader().pad(null));\n} }\n",
    )
    .unwrap();

    // The original is compiled first: the caller below is the unchanged one.
    fs::write(original.join("State.kt"), STATE).unwrap();
    compile(&original, &classes, &kotlinc, &javac, &stdlib);
    checked(
        Command::new(&javac)
            .arg("-cp")
            .arg(&classes)
            .arg("-d")
            .arg(&caller)
            .arg(&caller_file),
    );
    let expected = {
        let output = checked(
            Command::new(&java)
                .arg("-cp")
                .arg(format!(
                    "{}:{}:{}",
                    caller.display(),
                    classes.display(),
                    stdlib.display()
                ))
                .arg("probe.Driver"),
        );
        assert_eq!(output, "[token]\nnull\n");
        output
    };

    fs::write(migrated.join("State.kt"), STATE).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&migrated)
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
        .env("NOTLIN_KOTLIN_SOURCES", &migrated)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let emitted = fs::read_to_string(migrated.join("State.java"))
        .expect("the proved constant gains Java ownership");
    assert!(
        emitted.contains("import java.lang.String;")
            && emitted.contains("public static final String TOKEN = \"token\";"),
        "{emitted}"
    );
    assert!(
        !migrated.join("Reader.java").exists(),
        "the field proof does not sign the consumer's operations"
    );

    // The compiled mixed tree gets the unchanged compiled caller.
    compile(&migrated, &mixed, &kotlinc, &javac, &stdlib);
    let was = checked(
        Command::new(&java)
            .arg("-cp")
            .arg(format!(
                "{}:{}:{}",
                caller.display(),
                mixed.display(),
                stdlib.display()
            ))
            .arg("probe.Driver"),
    );
    assert_eq!(was, expected, "the original caller is unchanged");
    let signature = checked(
        Command::new(javac.with_file_name("javap"))
            .args(["-p", "-s", "-classpath"])
            .arg(&mixed)
            .arg("probe.State"),
    );
    assert!(
        signature.contains("descriptor: Ljava/lang/String;"),
        "{signature}"
    );

    // Capture probe: a same-package `probe.String` is exactly what the plain
    // source spelling would have resolved to. The bound import must win, both
    // for javac and for the field's reflected descriptor.
    let decoy = scratch.0.join("decoy");
    let decoy_classes = scratch.0.join("decoy-classes");
    fs::create_dir_all(&decoy).unwrap();
    for entry in fs::read_dir(&migrated).unwrap() {
        let file = entry.unwrap().path();
        if file.extension().and_then(|s| s.to_str()) == Some("java") {
            fs::copy(&file, decoy.join(file.file_name().unwrap())).unwrap();
        }
    }
    fs::write(
        decoy.join("String.java"),
        "package probe;\npublic class String {}\n",
    )
    .unwrap();
    compile(&decoy, &decoy_classes, &kotlinc, &javac, &stdlib);
    let signature = checked(
        Command::new(javac.with_file_name("javap"))
            .args(["-p", "-s", "-classpath"])
            .arg(&decoy_classes)
            .arg("probe.State"),
    );
    assert!(
        signature.contains("descriptor: Ljava/lang/String;"),
        "a same-package `String` class must not capture the bound field type: {signature}"
    );
}
