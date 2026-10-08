use std::fs;
use std::path::Path;
use std::process::Command;

/// A bodyless Kotlin secondary constructor that delegates to the primary
/// constructor has a direct Java representation: an overload using `this`.
#[test]
fn bodyless_secondary_constructor_delegates_to_primary() {
    let root = Path::new("tests/tmp_scratch_secondary_ctor");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Token.kt"),
        "package neutral.secondary\nclass Token(val id: String) {\n    constructor() : this(\"fallback\")\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Token.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let java = fs::read_to_string(root.join("Token.java")).unwrap_or_default();

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        !root.join("Token.kt").exists(),
        "supported secondary constructor retained:\n{stderr}"
    );
    assert!(
        java.contains("public Token()") && java.contains("this(\"fallback\");"),
        "secondary constructor must emit a Java this-delegating overload:\n{java}"
    );
    assert!(
        !stderr.contains("secondary constructors not yet supported"),
        "supported constructor emitted a stale diagnostic:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn lombok_secondary_constructor_keeps_its_delegated_primary_constructor() {
    let root = Path::new("tests/tmp_scratch_secondary_ctor_lombok");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Token.kt"),
        "package neutral.secondary\nclass Token(val id: String) {\n    constructor() : this(\"fallback\")\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("Token.kt"))
        .output()
        .expect("run notlin");
    assert!(output.status.success(), "notlin failed: {output:?}");
    let java = fs::read_to_string(root.join("Token.java")).unwrap();
    assert!(
        java.contains("public Token(String id)") && java.contains("this.id = id;"),
        "a translated secondary constructor cannot rely on Lombok annotation processing:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn bodyless_secondary_constructor_delegates_to_superclass() {
    let root = Path::new("tests/tmp_scratch_secondary_ctor_super");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Child.kt"),
        "package neutral.secondary\n\
         open class Base(val id: String)\n\
         class Child : Base {\n\
         \x20   constructor() : super(\"fallback\")\n\
         }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Child.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let java = fs::read_to_string(root.join("Child.java")).unwrap_or_default();

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        !root.join("Child.kt").exists(),
        "supported super-delegating constructor retained:\n{stderr}"
    );
    assert!(
        java.contains("public Child()") && java.contains("super(\"fallback\");"),
        "secondary constructor must emit a Java super-delegating overload:\n{java}"
    );
    assert!(
        !stderr.contains("bodyless direct `this(...)` delegation"),
        "supported constructor emitted a stale diagnostic:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn secondary_constructor_with_simple_body_translates_after_delegation() {
    let root = Path::new("tests/tmp_scratch_secondary_ctor_body");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Token.kt"),
        "package neutral.secondary\nclass Token(val id: String) {\n    var label: String = \"\"\n    constructor(incoming: String) : this(\"fallback\") {\n        this.label = incoming\n    }\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Token.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let java = fs::read_to_string(root.join("Token.java")).unwrap_or_default();

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        !root.join("Token.kt").exists(),
        "simple constructor body retained:\n{stderr}"
    );
    assert!(
        java.contains("public Token(String incoming)")
            && java.contains("this(\"fallback\");")
            && java.contains("this.setLabel(incoming);"),
        "secondary constructor body must follow its Java delegation:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn named_secondary_delegation_lowers_supported_expressions_and_primary_defaults() {
    let root = Path::new("tests/tmp_scratch_secondary_ctor_named_defaults");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Token.kt"),
        "package neutral.secondary\n\
         import java.time.Instant\n\
         import java.time.OffsetDateTime\n\
         interface Source {\n\
         \x20   val id: String\n\
         \x20   val deadlines: Map<String, OffsetDateTime>\n\
         \x20   val priorities: Map<String, Int>\n\
         }\n\
         class Token(\n\
         \x20   val id: String,\n\
         \x20   val source: Source,\n\
         \x20   val created: Instant = Instant.now(),\n\
         \x20   val deadline: Instant,\n\
         \x20   val priority: Int = 0\n\
         ) {\n\
         \x20   constructor(source: Source) : this(\n\
         \x20       id = source.id,\n\
         \x20       source = source,\n\
         \x20       deadline = source.deadlines.getOrDefault(\"end\", OffsetDateTime.now()).toInstant(),\n\
         \x20       priority = source.priorities.getOrDefault(\"priority\", 1)\n\
         \x20   )\n\
         }\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args([
            "--root",
            root.to_str().unwrap(),
            "--in-place",
            "--allow-approximations",
        ])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");
    let java = fs::read_to_string(root.join("Token.java")).unwrap_or_default();
    assert!(
        java.contains("public Token(Source source)")
            && java.contains(
                "this(source.getId(), source, Instant.now(), source.getDeadlines().getOrDefault(\"end\", OffsetDateTime.now()).toInstant(), source.getPriorities().getOrDefault(\"priority\", 1));"
            ),
        "named arguments must be reordered and omitted primary defaults filled in the delegated Java constructor:\n{java}\n{stderr}"
    );
    assert!(
        !root.join("Token.kt").exists(),
        "supported delegated constructor retained:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn secondary_constructor_with_complex_delegation_argument_stays_kotlin() {
    let root = Path::new("tests/tmp_scratch_secondary_ctor_complex");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Token.kt"),
        "package neutral.secondary\nclass Token(val id: String) {\n    constructor(parts: Array<String>) : this(parts.joinToString())\n}\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root.join("Token.kt"))
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(output.status.success(), "notlin failed:\n{stderr}");
    assert!(
        root.join("Token.kt").exists(),
        "complex delegated argument was emitted as Java:\n{stderr}"
    );
    let kotlin = fs::read_to_string(root.join("Token.kt")).unwrap_or_default();
    assert!(
        (kotlin.contains("NOTLIN NE5A7: secondary constructor is not a direct `this(...)` or `super(...)` delegation with an empty or simple assignment-only body")
            || kotlin.contains("NOTLIN S001: required symbol `joinToString` could not be resolved"))
            && kotlin.contains("constructor(parts: Array<String>)"),
        "missing conservative diagnostic for the unsupported delegated expression:\n{kotlin}\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn default_null_delegation_selects_primary_reference_overload() {
    let root = Path::new("tests/tmp_scratch_null_ctor_overload");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("Command.kt"), "package neutral.secondary\nclass Context(val reason: String)\nclass Command(val id: String, val context: Context? = null) {\n constructor(id: String, reason: String): this(id, Context(reason))\n}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("Command.kt"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let java = fs::read_to_string(root.join("Command.java")).unwrap();
    assert!(java.contains("this(id, (Context) null);"), "{java}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn explicit_null_secondary_delegation_selects_primary_reference_overload() {
    let root = Path::new("tests/tmp_scratch_explicit_null_ctor");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("Command.kt"), "package neutral.secondary\nclass Context(val reason: String)\nclass Command(val id: String, val context: Context?) {\n constructor(id: String, reason: String): this(id, Context(reason))\n constructor(id: String): this(id, null)\n}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.join("Command.kt"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let java = fs::read_to_string(root.join("Command.java")).unwrap();
    assert!(java.contains("this(id, (Context) null);"), "{java}");
    let _ = fs::remove_dir_all(root);
}
