//! Kotlin's `kotlin-jpa` plugin (the `no-arg` plugin with the JPA preset) adds
//! a zero-argument constructor to every @Entity/@Embeddable/@MappedSuperclass
//! class, so the ORM can instantiate it by reflection. Nothing in the source
//! spells that constructor out and the switch lives in the BUILD FILE, so the
//! Java side has to synthesize it from the annotation.
//!
//! The shape reproduced here was measured on the reference module's own
//! bytecode (`javap -p -c` of the kotlinc-2.3.21 + plugin.jpa entities):
//!
//!     public AuthenticatedConnectorRefEntity();
//!       Code: aload_0
//!             invokespecial .../NameLookupEntity."<init>":()V
//!             return
//!
//! so the constructor is `public`, assigns NO initializer values (its fields
//! come out at JVM defaults even where the Kotlin source initializes them), and
//! forwards to a zero-argument `super()`. Java cannot clear a field implicitly
//! and does not have a synthetic constructor, so the equivalent is an explicit
//! constructor that assigns every emitted instance field its type default —
//! which is also what makes it COMPILE: a body of `{}` beside `final` fields is
//! javac's "variable might not have been initialized".
//!
//! The negative cases carry the same weight as the positive ones: a class
//! without the annotation, a class that already declares a zero-argument
//! constructor, and a class whose every parameter has a default (Kotlin has its
//! own no-argument constructor for that one, and it delegates to the primary
//! constructor — so its initializers DO run and the field-clearing form would be
//! the wrong reproduction).

use std::fs;
use std::path::{Path, PathBuf};

const FIXTURE: &str = r#"package neutral.jpa

annotation class Entity
annotation class Embeddable
annotation class MappedSuperclass

@MappedSuperclass
abstract class Base(val id: String)

@Entity
class Item(
    val name: String,
    val count: Int,
    val ratio: Double,
    var enabled: Boolean
) : Base(name) {
    var items: MutableList<String> = mutableListOf()
    var attempts: Int = 0
    val computed: String get() = "x"
}

@Embeddable
class Embed(val x: String, val y: Long)

@Entity
class Ready(val a: String) {
    constructor() : this("x")
}

@Entity
class Defaulted(val a: String = "x", val b: Int = 1)

@Entity
class Simple(val name: String, var count: Int)

@Entity
class SimpleBody(val name: String) {
    var note: String = "n"
}

class Plain(val a: String)

@Entity
data class Key(val tenant: String, val code: Long)
"#;

fn run(name: &str, extra: &[&str]) -> (PathBuf, String) {
    let root = PathBuf::from(format!("tests/tmp_scratch_jpa_no_arg_{name}"));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("fixture.kt"), FIXTURE).unwrap();

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    command
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .args(extra)
        .arg(root.join("fixture.kt").to_str().unwrap());
    let out = command.output().expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(out.status.success(), "notlin failed:\n{stderr}");
    (root, stderr)
}

fn java_of(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join(format!("{name}.java")))
        .unwrap_or_else(|e| panic!("read {name}.java: {e}"))
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// The `--lombok`-off shape: ordinary final classes and records.
#[test]
fn the_jpa_no_arg_constructor_clears_every_field() {
    let (root, _) = run("plain", &[]);

    let item = java_of(&root, "Item");
    assert!(
        item.contains("public Item() {"),
        "@Entity needs the plugin's zero-argument constructor:\n{item}"
    );
    // `: Base(name)` forwards constructor arguments, and the plugin's
    // constructor cannot repeat them — it calls the SUPERCLASS's zero-argument
    // constructor, which this same rule gives Base.
    assert!(
        item.contains("super();"),
        "the synthesized constructor forwards to a zero-argument super:\n{item}"
    );
    for assignment in [
        "this.name = null;",
        "this.count = 0;",
        "this.ratio = 0.0;",
        "this.enabled = false;",
        // Body properties are cleared too: the plugin runs no initializers, so
        // a `val items = mutableListOf()` is null after this construction.
        "this.items = null;",
        "this.attempts = 0;",
    ] {
        assert!(
            item.contains(assignment),
            "missing JVM default assignment `{assignment}`:\n{item}"
        );
    }
    // `val computed: String get() = "x"` emits no field at all; assigning it
    // would not compile.
    assert!(
        !item.contains("this.computed"),
        "a property that emits no backing field must not be assigned:\n{item}"
    );

    let base = java_of(&root, "Base");
    assert!(
        base.contains("public Base() {") && base.contains("this.id = null;"),
        "@MappedSuperclass needs the same constructor (an @Entity subclass calls \
         super() on it):\n{base}"
    );
    assert!(
        !base.contains("super();"),
        "no explicit super call is needed when the class extends nothing:\n{base}"
    );

    let embed = java_of(&root, "Embed");
    assert!(
        embed.contains("public Embed() {")
            && embed.contains("this.x = null;")
            && embed.contains("this.y = 0L;"),
        "@Embeddable needs the same constructor, with primitive defaults:\n{embed}"
    );

    // Negative: nothing else grows a constructor.
    let plain = java_of(&root, "Plain");
    assert!(
        !plain.contains("public Plain()"),
        "only the JPA annotations trigger this constructor:\n{plain}"
    );

    // Negative: an explicit `constructor()` already satisfies JPA, and kotlinc
    // emits exactly one zero-argument constructor for that class.
    let ready = java_of(&root, "Ready");
    assert_eq!(
        occurrences(&ready, "public Ready()"),
        1,
        "a declared zero-argument constructor must not be duplicated:\n{ready}"
    );
    assert!(
        !ready.contains("this.a = null;"),
        "the declared constructor delegates to the primary constructor:\n{ready}"
    );

    // All parameters defaulted: Kotlin has a no-argument constructor of its own
    // and it delegates, so the initializers run. Reproduce THAT one.
    let defaulted = java_of(&root, "Defaulted");
    assert_eq!(
        occurrences(&defaulted, "public Defaulted()"),
        1,
        "kotlinc emits one zero-argument constructor here, not two:\n{defaulted}"
    );
    assert!(
        defaulted.contains("this(\"x\", 1);"),
        "the existing delegating constructor is the faithful one:\n{defaulted}"
    );

    // A record's components are final, so the constructor delegates to the
    // canonical one with each component's type default.
    let key = java_of(&root, "Key");
    assert!(
        key.contains("public record Key(") && key.contains("public Key() {"),
        "an @Entity data class needs the constructor too:\n{key}"
    );
    assert!(
        key.contains("this(null, 0L);"),
        "the record form delegates to the canonical constructor:\n{key}"
    );

    let _ = fs::remove_dir_all(&root);
}

/// The `--lombok` shape: `@Data`/`@Value` classes with explicit fields.
#[test]
fn the_lombok_forms_get_the_same_constructor() {
    let (root, _) = run("lombok", &["--lombok"]);

    let item = java_of(&root, "Item");
    assert!(item.contains("@Data"), "expected the Lombok form:\n{item}");
    assert!(
        item.contains("public Item() {")
            && item.contains("this.name = null;")
            && item.contains("this.items = null;"),
        "@Data does not generate a zero-argument constructor, so this one is \
         required:\n{item}"
    );

    let key = java_of(&root, "Key");
    assert!(
        key.contains("@Value"),
        "expected the Lombok value form:\n{key}"
    );
    assert!(
        key.contains("public Key() {")
            && key.contains("this.tenant = null;")
            && key.contains("this.code = 0L;"),
        "@Value implies an ALL-ARGS constructor only:\n{key}"
    );

    let ready = java_of(&root, "Ready");
    assert_eq!(
        occurrences(&ready, "public Ready()"),
        1,
        "still exactly one zero-argument constructor:\n{ready}"
    );

    let plain = java_of(&root, "Plain");
    assert!(
        !plain.contains("public Plain()"),
        "only the JPA annotations trigger this constructor:\n{plain}"
    );

    // Lombok generates NO constructor once the class declares one, so the
    // all-args constructor Kotlin callers use cannot be left to @Data /
    // @AllArgsConstructor — it has to be written beside the no-arg one, or the
    // class loses it entirely.
    let simple = java_of(&root, "Simple");
    assert!(
        simple.contains("public Simple(String name, int count)"),
        "the all-args constructor must be explicit next to the no-arg one:\n{simple}"
    );
    assert!(
        simple.contains("public Simple() {") && simple.contains("this.name = null;"),
        "and the no-arg one still gets its defaults:\n{simple}"
    );

    let body = java_of(&root, "SimpleBody");
    assert!(
        body.contains("public SimpleBody(String name)")
            && body.contains("public SimpleBody() {")
            && body.contains("this.note = null;"),
        "a body field is not part of the Kotlin constructor arity:\n{body}"
    );

    let _ = fs::remove_dir_all(&root);
}
