//! Retained property reads through a stdlib value-scope lambda.
//!
//! The parallel-interface repair promotes the *source* property `key` to an
//! explicit `getKey()` method on both branches of a diamond, even though one
//! branch keeps the property on its own Kotlin ancestor. A retained consumer
//! that reads the property through an implicit lambda parameter
//! (`info.let { it.key }`) therefore has to be repaired too; the binding of
//! `it` comes from the canonical stdlib `let`, not from spelling.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "notlin-scoped-lambda-input-{name}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, dir: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = self.0.join(dir);
        fs::create_dir_all(&root).unwrap();
        for (name, text) in files {
            fs::write(root.join(name), text).unwrap();
        }
        root
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The declarations of the measured RED, genericized: a property diamond whose
/// parent interfaces own `suspend` members, plus a Java `Key`.
const CORE: [(&str, &str); 5] = [
    (
        "Info.kt",
        "package bridgeprobe\n\ninterface Info : LookupBase {\n    val key: Key\n    suspend fun pendingInfo()\n}\n",
    ),
    (
        "DerivedInfo.kt",
        "package bridgeprobe\n\ninterface DerivedInfo : Info {\n    override val key: Key\n    suspend fun pendingDerived()\n}\n",
    ),
    (
        "LookupBase.kt",
        "package bridgeprobe\n\ninterface LookupBase {\n    val lookup: String\n    suspend fun pending()\n}\n",
    ),
    (
        "Keyed.kt",
        "package bridgeprobe\n\ninterface Keyed {\n    val key: Key\n}\n",
    ),
    (
        "Key.java",
        "package bridgeprobe; public final class Key { public final String value; public Key(String value){this.value=value;} }\n",
    ),
];

const RELAY: (&str, &str) = (
    "Relay.kt",
    "package bridgeprobe\n\nclass Relay(val info: DerivedInfo) : Keyed {\n    override val key: Key\n        get() = info.let { it.key }\n    suspend fun stay() {}\n}\n",
);

const COMBINED: (&str, &str) = (
    "Combined.kt",
    "package bridgeprobe\n\nabstract class Combined(override val key: Key, override val lookup: String) : DerivedInfo, Keyed {\n}\n",
);

fn c4() -> Vec<(&'static str, &'static str)> {
    CORE.iter()
        .chain([&RELAY, &COMBINED])
        .copied()
        .collect::<Vec<_>>()
}

fn migrate(root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin")
}

fn read(root: &Path, name: &str) -> String {
    fs::read_to_string(root.join(name)).unwrap_or_default()
}

fn snapshot(root: &Path) -> Vec<(String, String)> {
    let mut files = fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read_to_string(entry.path()).unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

/// The measured RED. `Keyed` translates to Java, so `key` becomes a JavaBean
/// getter on that branch while `Info`/`DerivedInfo` stay Kotlin and carry the
/// promoted `fun getKey()`. The retained consumer must call that method through
/// the value `let` binds to `it`.
#[test]
fn retained_parallel_interface_getter_read_uses_bound_lambda_input() {
    let scratch = Scratch::new("c4");
    let root = scratch.write("work", &c4());

    let migrated = migrate(&root);
    let stderr = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{stderr}");

    let keyed = read(&root, "Keyed.java");
    assert!(
        keyed.contains("getKey()"),
        "Keyed keeps its getter:\n{keyed}"
    );
    assert!(!root.join("Keyed.kt").exists());

    for retained in ["Info.kt", "DerivedInfo.kt"] {
        let text = read(&root, retained);
        assert!(
            text.contains("fun getKey("),
            "{retained} promotes its property to a method:\n{text}"
        );
        assert!(!text.contains("val key"), "{retained}:\n{text}");
    }

    let relay = read(&root, "Relay.kt");
    assert!(
        relay.contains("info.let { it.getKey() }"),
        "the retained read through `it` is repaired:\n{relay}"
    );
    assert!(!relay.contains("it.key"), "{relay}");
    assert!(relay.contains("fun getKey"), "{relay}");

    // A second production run over its own output is a fixed point.
    let before = snapshot(&root);
    let again = migrate(&root);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert_eq!(before, snapshot(&root), "second run must be byte identical");
}

/// The measured receiver ancestry ends in `java.io.Serializable`. That ancestor
/// resolves in the JDK, not in the workspace, and must not block the proof.
#[test]
fn serializable_receiver_ancestry_admits_the_same_repair() {
    let scratch = Scratch::new("serializable");
    let mut files = c4();
    files[2] = (
        "LookupBase.kt",
        "package bridgeprobe\nimport java.io.Serializable\n\ninterface LookupBase : Serializable {\n    val lookup: String\n    suspend fun pending()\n}\n",
    );
    let root = scratch.write("work", &files);

    let migrated = migrate(&root);
    let stderr = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{stderr}");
    assert!(root.join("Keyed.java").is_file(), "{stderr}");
    let relay = read(&root, "Relay.kt");
    assert!(
        relay.contains("info.let { it.getKey() }"),
        "a JDK-only ancestor is not a reason to decline:\n{relay}"
    );
    assert!(!relay.contains("it.key"), "{relay}");
}

/// Every implicit-`it` and untyped named parameter of a proven value-scope call
/// binds the call's receiver, at whatever nesting depth the lambda sits.
#[test]
fn nested_names_and_aliases_bind_the_value_scope_receiver() {
    let scratch = Scratch::new("nested");
    let consumer = (
        "Consumers.kt",
        r#"package bridgeprobe

class NamedRelay(val info: DerivedInfo) {
    fun viaName(): Key = info.let { value -> value.key }
    fun viaAlias(): DerivedInfo {
        val other: DerivedInfo = info
        return other.also { it.key }
    }
}

class NestedRelay(val info: DerivedInfo, val parcel: Parcel) {
    fun nested(): String = parcel.let { outer -> info.let { it.key }.value + outer.key }
}
"#,
    );
    let parcel = (
        "Parcel.kt",
        "package bridgeprobe\n\nclass Parcel {\n    val key: String\n        get() = \"parcel\"\n}\n",
    );
    let files = c4()
        .into_iter()
        .chain([consumer, parcel])
        .collect::<Vec<_>>();
    let root = scratch.write("work", &files);

    let migrated = migrate(&root);
    let stderr = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{stderr}");

    let text = read(&root, "Consumers.kt");
    assert!(
        text.contains("info.let { value -> value.getKey() }"),
        "an untyped named parameter binds the receiver:\n{text}"
    );
    assert!(
        text.contains("other.also { it.getKey() }"),
        "`also` binds the receiver too:\n{text}"
    );
    assert!(
        text.contains("info.let { it.getKey() }.value + outer.key"),
        "the inner `it` is the `info` receiver while the outer `it` stays `Parcel`:\n{text}"
    );
    assert!(
        text.contains("parcel.let { outer -> "),
        "the outer parameter is left alone:\n{text}"
    );
}

/// A `let` that is *not* the stdlib value-scope function binds a different
/// value. Repairing `it.key` against the receiver type would be wrong, so the
/// call is left exactly as written.
#[test]
fn foreign_value_scope_bindings_are_declined() {
    let scratch = Scratch::new("foreign");
    let member = (
        "MemberRelay.kt",
        r#"package bridgeprobe

class MemberRelay(val info: DerivedInfo) {
    fun label(): String = info.let { it.key }

    fun <R> DerivedInfo.let(block: (Parcel) -> R): R = block(Parcel())
}
"#,
    );
    let aliased = (
        "AliasRelay.kt",
        r#"package bridgeprobe

import helpers.customLet as let

class AliasRelay(val info: DerivedInfo) {
    fun label(): String = info.let { it.key }
}
"#,
    );
    let helpers = (
        "helpers.kt",
        r#"package helpers

import bridgeprobe.Parcel

fun <T> T.customLet(block: (Parcel) -> T): T = block(Parcel())
"#,
    );
    let parcel = (
        "Parcel.kt",
        "package bridgeprobe\n\nclass Parcel {\n    val key: String\n        get() = \"parcel\"\n}\n",
    );
    let files = c4()
        .into_iter()
        .chain([member, aliased, helpers, parcel])
        .collect::<Vec<_>>();
    let root = scratch.write("work", &files);

    let migrated = migrate(&root);
    let stderr = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{stderr}");

    for name in ["MemberRelay.kt", "AliasRelay.kt"] {
        let text = read(&root, name);
        assert!(
            text.contains("fun label(): String = info.let { it.key }"),
            "{name} is not the stdlib value binding; its read must stay:\n{text}"
        );
        assert!(!text.contains("getKey"), "{name}:\n{text}");
    }
}

/// Non-read positions and unproven bindings never gain a getter call.
#[test]
fn non_read_positions_and_unproven_bindings_are_declined() {
    let scratch = Scratch::new("positions");
    let consumer = (
        "Positions.kt",
        r#"package bridgeprobe

class Positions(val info: DerivedInfo) : Keyed {
    override val key: Key
        get() = info.let { it.key }

    fun reference(): KProperty0<Key> = info.let { it::key }

    fun throughUnknown(values: List<DerivedInfo>): Key = values.map { it.key }.first()
}
"#,
    );
    let files = c4().into_iter().chain([consumer]).collect::<Vec<_>>();
    let root = scratch.write("work", &files);

    let migrated = migrate(&root);
    let stderr = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{stderr}");

    let text = read(&root, "Positions.kt");
    assert!(text.contains("info.let { it.getKey() }"), "{text}");
    assert!(
        text.contains("info.let { it::key }"),
        "a property reference is not a read position:\n{text}"
    );
    assert!(
        text.contains("values.map { it.key }"),
        "`map` binds its element, not its receiver, so the read is unproven:\n{text}"
    );
}

/// The measured runtime context: the receiver's ancestry really includes
/// `java.io.Serializable`, the implementation of `DerivedInfo` is Java, and the
/// probe is compiled once against the ORIGINAL sources so the candidate cannot
/// fall back to original owners.
const RUNTIME_FILES: [(&str, &str); 7] = [
    (
        "LookupBase.kt",
        "package bridgeprobe\nimport java.io.Serializable\n\ninterface LookupBase : Serializable {\n    val lookup: String\n    suspend fun pending()\n}\n",
    ),
    CORE[0],
    CORE[1],
    CORE[3],
    CORE[4],
    RELAY,
    COMBINED,
];

const CALLERS: [(&str, &str); 2] = [
    (
        "InfoImpl.java",
        r#"package bridgeprobe;
public final class InfoImpl implements DerivedInfo {
 public final Key expected = new Key("payload"); public int reads=0;
 public Key getKey(){reads++; return expected;}
 public String getLookup(){return "lookup";}
 public Object pending(kotlin.coroutines.Continuation<? super kotlin.Unit> c){return kotlin.Unit.INSTANCE;}
 public Object pendingInfo(kotlin.coroutines.Continuation<? super kotlin.Unit> c){return kotlin.Unit.INSTANCE;}
 public Object pendingDerived(kotlin.coroutines.Continuation<? super kotlin.Unit> c){return kotlin.Unit.INSTANCE;}
}
"#,
    ),
    (
        "Probe.java",
        r#"package bridgeprobe;
public final class Probe {public static void main(String[] args) throws Exception {
 var root=java.nio.file.Path.of(Relay.class.getProtectionDomain().getCodeSource().getLocation().toURI()).toRealPath();
 if(!root.equals(java.nio.file.Path.of(args[0]).toRealPath()))throw new AssertionError("wrong Relay origin");
 InfoImpl info=new InfoImpl(); Key result=new Relay(info).getKey();
 if(result!=info.expected || info.reads!=1 || !result.value.equals("payload"))throw new AssertionError();
 System.out.println("value=payload; getter-calls=1; identity=true");
}}
"#,
    ),
];

struct Tools {
    javac: PathBuf,
    java: PathBuf,
    kotlinc: PathBuf,
    classpath: String,
    lombok_plugin: PathBuf,
    env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

/// A wildcard import of a foreign package binds every name that package
/// exports, and the grammar spells the wildcard as a `*` token beside the path
/// node — not as `.*` inside it. `let` is that package's function, so `it` is
/// not the receiver and the read stays exactly as written. Comments and tabs in
/// the header change nothing; a commented-out import is not an import node and
/// therefore binds nothing.
#[test]
fn foreign_wildcard_import_forms_bind_the_value_scope_name() {
    let scratch = Scratch::new("wildcard");
    let foreign = (
        "Scope.kt",
        "package foreignscope\n\nimport bridgeprobe.Parcel\n\nfun <T> T.let(block: (Parcel) -> T): T = block(Parcel())\n",
    );
    let parcel = (
        "Parcel.kt",
        "package bridgeprobe\n\nclass Parcel {\n    val key: String\n        get() = \"parcel\"\n}\n",
    );
    let consumers = [
        (
            "Wildcard.kt",
            "package bridgeprobe\n\nimport foreignscope.*\n\nclass WildcardRelay(val info: DerivedInfo) {\n    fun label(): String = info.let { it.key }\n}\n",
        ),
        (
            "Tabbed.kt",
            "package bridgeprobe\n\nimport\tforeignscope.*\n\nclass TabbedRelay(val info: DerivedInfo) {\n    fun label(): String = info.let { it.key }\n}\n",
        ),
        (
            "Commented.kt",
            "package bridgeprobe\n\nimport /* keep */ foreignscope /* scope */ .*\n\nclass CommentedRelay(val info: DerivedInfo) {\n    fun label(): String = info.let { it.key }\n}\n",
        ),
        (
            "Masked.kt",
            "package bridgeprobe\n\n/* import foreignscope.* */\n\nclass MaskedRelay(val info: DerivedInfo) {\n    fun label(): Key = info.let { it.key }\n}\n",
        ),
        (
            "Redundant.kt",
            "package bridgeprobe\n\nimport kotlin.collections.*\n\nclass RedundantRelay(val info: DerivedInfo) {\n    fun label(): Key = info.let { it.key }\n}\n",
        ),
    ];
    let files = c4()
        .into_iter()
        .chain([foreign, parcel])
        .chain(consumers)
        .collect::<Vec<_>>();
    let root = scratch.write("work", &files);

    let migrated = migrate(&root);
    let stderr = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{stderr}");

    for name in ["Wildcard.kt", "Tabbed.kt", "Commented.kt"] {
        let text = read(&root, name);
        assert!(
            text.contains("info.let { it.key }"),
            "{name}: a foreign wildcard binds `let`, so the read must stay:\n{text}"
        );
        assert!(!text.contains("getKey"), "{name}:\n{text}");
    }
    let masked = read(&root, "Masked.kt");
    assert!(
        masked.contains("info.let { it.getKey() }"),
        "a commented-out import is not an import node:\n{masked}"
    );
    let redundant = read(&root, "Redundant.kt");
    assert!(
        redundant.contains("info.let { it.getKey() }"),
        "a default-import wildcard widens nothing:\n{redundant}"
    );
}

/// An enclosing lambda decides what a bare identifier means. `with(…) { … }`
/// gives the block the argument as its implicit receiver, so `info` is
/// `Context.info` (an `Other`), not the relay's `DerivedInfo`; the read must
/// stay. The enclosing shape is declined because it is not a *proved* value
/// scope block, not because `with` is on a list — an arbitrary call taking a
/// receiver lambda is declined the same way.
#[test]
fn enclosing_receiver_lambdas_decline_the_value_scope_repair() {
    let scratch = Scratch::new("receiver-lambda");
    let boundary = (
        "Boundary.kt",
        r#"package bridgeprobe

fun <R> Context.transform(block: Context.() -> R): R = block()

class Boundary(val info: DerivedInfo) {
    fun shadowed(): Key = with(Context(Other(Key("alternate")))) { info.let { it.key } }
    fun unlisted(): Key = Context(Other(Key("alternate"))).transform { info.let { it.key } }
}
"#,
    );
    let context = (
        "Context.java",
        "package bridgeprobe; public final class Context {public final Other info; public Context(Other info){this.info=info;} }\n",
    );
    let other = (
        "Other.java",
        "package bridgeprobe; public final class Other {public final Key key; public Other(Key key){this.key=key;} }\n",
    );
    let files = c4()
        .into_iter()
        .chain([boundary, context, other])
        .collect::<Vec<_>>();
    let root = scratch.write("work", &files);

    let migrated = migrate(&root);
    let stderr = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{stderr}");

    let text = read(&root, "Boundary.kt");
    assert_eq!(
        text.matches("it.key").count(),
        2,
        "an unproved enclosing lambda leaves `info` and `it` unproved:\n{text}"
    );
    assert!(!text.contains("getKey"), "{text}");
    // The ordinary retained relay in the same run keeps its repair.
    let relay = read(&root, "Relay.kt");
    assert!(relay.contains("info.let { it.getKey() }"), "{relay}");
}

/// An ancestor outside the index is not an ancestor without members: a compiled
/// library base can own `let` itself, so that receiver's binding is unproved
/// while a sibling with the same property repair keeps it.
#[test]
fn unresolved_receiver_ancestry_declines_the_value_scope_repair() {
    let scratch = Scratch::new("external");
    let relay = (
        "ExternalRelay.kt",
        r#"package bridgeprobe

interface ExternalDerived : DerivedInfo, libscope.ScopeBase

class ProvenRelay(val info: DerivedInfo) {
    fun label(): Key = info.let { it.key }
}

class ExternalRelay(val info: ExternalDerived) {
    fun label(): Key = info.let { it.key }
}
"#,
    );
    let files = c4().into_iter().chain([relay]).collect::<Vec<_>>();
    let root = scratch.write("work", &files);

    let migrated = migrate(&root);
    let stderr = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{stderr}");

    let text = read(&root, "ExternalRelay.kt");
    assert_eq!(
        text.matches("it.getKey()").count(),
        1,
        "the proved receiver keeps the repair:\n{text}"
    );
    assert_eq!(
        text.matches("it.key").count(),
        1,
        "an unresolved ancestor could own `let`, so that read stays:\n{text}"
    );
}

/// A direct nested classifier of the receiver can own the selected name: the
/// inner `let(block: (Other) -> Key)` constructor binds its block parameter to
/// `Other`, whose `key` is a public Java field. Repairing `it.key` to a getter
/// would break the retained file, so the receiver's own nesting inventory has
/// to claim the name. The caller's package scan alone cannot see it.
#[test]
fn cross_package_inner_constructor_claims_the_value_scope_name() {
    let scratch = Scratch::new("inner-constructor");
    let files: [(&str, &str); 5] = [
        (
            "Key.java",
            "package bridgeprobe; public class Key { public final String value; public Key(String value){this.value=value;} }\n",
        ),
        (
            "Other.java",
            "package bridgeprobe; public final class Other {public final Key key; public Other(Key key){this.key=key;} }\n",
        ),
        (
            "Keyed.kt",
            "package bridgeprobe\ninterface Keyed {\n val key: Key\n}\n",
        ),
        (
            "WrappedInfo.kt",
            "package modelprobe\nimport bridgeprobe.Key\nimport bridgeprobe.Keyed\nimport bridgeprobe.Other\nclass WrappedInfo(override val key: Key) : Keyed {\n inner class let(block: (Other) -> Key) : Key(\"constructed\") {\n  init { block(Other(Key(\"alternate\"))) }\n }\n suspend fun pending() {}\n}\n",
        ),
        (
            "Relay.kt",
            "package bridgeprobe\nimport modelprobe.WrappedInfo\nclass ShadowRelay(val info: WrappedInfo) : Keyed {\n override val key: Key\n  get() = info.let { it.key }\n suspend fun pending() {}\n}\n",
        ),
    ];
    let root = scratch.write("work", &files);

    let migrated = migrate(&root);
    let stderr = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{stderr}");

    let keyed = read(&root, "Keyed.java");
    assert!(
        keyed.contains("getKey()"),
        "Keyed keeps its getter:\n{keyed}"
    );
    let wrapped = read(&root, "WrappedInfo.kt");
    assert!(
        wrapped.contains("fun getKey"),
        "WrappedInfo bridges the promoted getter:\n{wrapped}"
    );
    let relay = read(&root, "Relay.kt");
    assert!(root.join("Relay.kt").is_file(), "{stderr}");
    assert!(
        relay.contains("info.let { it.key }"),
        "the inner constructor binds `it` to Other, whose `key` is a field:\n{relay}"
    );
    assert!(!relay.contains("it.getKey()"), "{relay}");
}

/// An inner `let` owned by an unrelated class — even one in the consumer's own
/// package — claims nothing on the receiver. The genuine repair stays.
#[test]
fn unrelated_inner_classifiers_do_not_disable_the_proved_repair() {
    let scratch = Scratch::new("unrelated-inner");
    let noise = (
        "Noise.kt",
        "package bridgeprobe\n\nclass NoiseOwner {\n    inner class let(block: (Other) -> String) : String()\n}\n",
    );
    let other = (
        "Other.java",
        "package bridgeprobe; public final class Other {public final Key key; public Other(Key key){this.key=key;} }\n",
    );
    let files = c4().into_iter().chain([noise, other]).collect::<Vec<_>>();
    let root = scratch.write("work", &files);

    let migrated = migrate(&root);
    let stderr = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{stderr}");
    assert!(root.join("Keyed.java").is_file(), "{stderr}");

    let relay = read(&root, "Relay.kt");
    assert!(
        relay.contains("info.let { it.getKey() }"),
        "an unrelated owner's nested `let` must not disable the repair:\n{relay}"
    );
    assert!(!relay.contains("it.key"), "{relay}");
    let noise_text = read(&root, "Noise.kt");
    assert!(
        noise_text.contains("inner class let"),
        "the unrelated owner is untouched:\n{noise_text}"
    );
}

fn tools() -> Tools {
    let kotlinc = PathBuf::from(std::env::var_os("NOTLIN_KOTLINC").expect("NOTLIN_KOTLINC"));
    let kotlin_home = kotlinc
        .parent()
        .and_then(Path::parent)
        .expect("kotlinc lives in <kotlin>/bin")
        .to_path_buf();
    let javac = PathBuf::from(std::env::var_os("NOTLIN_JAVAC").expect("NOTLIN_JAVAC"));
    let java_home = javac
        .parent()
        .and_then(Path::parent)
        .expect("javac lives in <jdk>/bin")
        .to_path_buf();
    let stdlib = kotlin_home.join("lib/kotlin-stdlib.jar");
    let annotations = kotlin_home.join("lib/annotations-13.0.jar");
    let lombok = PathBuf::from(std::env::var_os("NOTLIN_LOMBOK").expect("NOTLIN_LOMBOK"));
    let classpath = std::env::join_paths([&lombok, &stdlib, &annotations])
        .unwrap()
        .into_string()
        .unwrap();
    let env = std::env::vars_os()
        .filter(|(key, _)| !key.to_string_lossy().starts_with("NOTLIN_"))
        .chain([(
            std::ffi::OsString::from("JAVA_HOME"),
            java_home.clone().into_os_string(),
        )])
        .collect();
    Tools {
        javac: java_home.join("bin/javac"),
        java: java_home.join("bin/java"),
        kotlinc: kotlin_home.join("bin/kotlinc"),
        lombok_plugin: kotlin_home.join("lib/lombok-compiler-plugin.jar"),
        classpath,
        env,
    }
}

impl Tools {
    fn run(&self, program: &Path, args: &[&std::ffi::OsStr], cwd: &Path) -> Output {
        Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap_or_else(|error| panic!("run {}: {error}", program.display()))
    }
}

fn kt_sources(root: &Path) -> Vec<std::ffi::OsString> {
    let mut files = fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
        .map(|path| path.into_os_string())
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn java_sources(root: &Path) -> Vec<std::ffi::OsString> {
    let mut files = fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "java")
        })
        .map(|path| path.into_os_string())
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn assert_ok(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The runtime oracle: an original-compiled Java probe drives the retained
/// getter before and after the migration, and the candidate classes must be the
/// only owner the probe resolves.
#[test]
#[ignore = "requires JVM tools, Kotlin stdlib, Lombok, and its Kotlin compiler plugin"]
fn real_toolchain_preserves_the_retained_getter_value() {
    let tools = tools();
    let scratch = Scratch::new("jvm");

    let original = scratch.write("original", &RUNTIME_FILES);
    let original_classes = scratch.0.join("original-classes");
    fs::create_dir_all(&original_classes).unwrap();
    let key = original.join("Key.java");
    assert_ok(
        &tools.run(
            &tools.javac,
            &[
                std::ffi::OsStr::new("-d"),
                original_classes.as_os_str(),
                key.as_os_str(),
            ],
            &original,
        ),
        "original Key.java compiles",
    );
    let mut kotlin: Vec<std::ffi::OsString> = vec![
        "-cp".into(),
        original_classes.clone().into_os_string(),
        "-d".into(),
        original_classes.clone().into_os_string(),
    ];
    kotlin.extend(kt_sources(&original));
    let refs: Vec<&std::ffi::OsStr> = kotlin.iter().map(|arg| arg.as_os_str()).collect();
    assert_ok(
        &tools.run(&tools.kotlinc, &refs, &original),
        "original Kotlin sources compile",
    );

    let callers = scratch.write("callers", &CALLERS);
    let caller_classes = scratch.0.join("caller-classes");
    fs::create_dir_all(&caller_classes).unwrap();
    let caller_cp = std::env::join_paths([original_classes.as_path()])
        .unwrap()
        .into_string()
        .unwrap()
        + &format!(":{}", tools.classpath);
    let mut javac_args: Vec<std::ffi::OsString> = vec![
        "-proc:none".into(),
        "-cp".into(),
        caller_cp.into(),
        "-d".into(),
        caller_classes.clone().into_os_string(),
    ];
    javac_args.extend(java_sources(&callers));
    let refs: Vec<&std::ffi::OsStr> = javac_args.iter().map(|arg| arg.as_os_str()).collect();
    assert_ok(
        &tools.run(&tools.javac, &refs, &callers),
        "the probe compiles against the ORIGINAL sources",
    );

    let probe = |classes: &Path| {
        let classpath = std::env::join_paths([caller_classes.as_path(), classes])
            .unwrap()
            .into_string()
            .unwrap()
            + &format!(":{}", tools.classpath);
        tools.run(
            &tools.java,
            &[
                std::ffi::OsStr::new("-cp"),
                std::ffi::OsStr::new(&classpath),
                std::ffi::OsStr::new("bridgeprobe.Probe"),
                classes.as_os_str(),
            ],
            classes,
        )
    };
    let before = probe(&original_classes);
    assert_ok(&before, "the original mixed tree runs");
    assert_eq!(
        String::from_utf8_lossy(&before.stdout).trim(),
        "value=payload; getter-calls=1; identity=true"
    );

    let migrated = scratch.write("migrated", &RUNTIME_FILES);
    let output = migrate(&migrated);
    assert_ok(&output, "the migration succeeds");
    let relay = read(&migrated, "Relay.kt");
    assert!(relay.contains("info.let { it.getKey() }"), "{relay}");
    assert!(migrated.join("Keyed.java").is_file());

    // Kotlin first, then javac: the real mixed-source stages.
    let candidate_classes = scratch.0.join("candidate-classes");
    fs::create_dir_all(&candidate_classes).unwrap();
    let mut kotlin: Vec<std::ffi::OsString> = vec![
        format!("-Xplugin={}", tools.lombok_plugin.display()).into(),
        "-cp".into(),
        tools.classpath.clone().into(),
        "-d".into(),
        candidate_classes.clone().into_os_string(),
    ];
    kotlin.extend(kt_sources(&migrated));
    kotlin.extend(java_sources(&migrated));
    let refs: Vec<&std::ffi::OsStr> = kotlin.iter().map(|arg| arg.as_os_str()).collect();
    assert_ok(
        &tools.run(&tools.kotlinc, &refs, &migrated),
        "the candidate mixed tree compiles with the Lombok plugin",
    );
    let mut javac_args: Vec<std::ffi::OsString> = vec![
        "-proc:full".into(),
        "-cp".into(),
        format!("{}:{}", candidate_classes.display(), tools.classpath).into(),
        "-d".into(),
        candidate_classes.clone().into_os_string(),
        "--release".into(),
        "17".into(),
    ];
    javac_args.extend(java_sources(&migrated));
    let refs: Vec<&std::ffi::OsStr> = javac_args.iter().map(|arg| arg.as_os_str()).collect();
    assert_ok(
        &tools.run(&tools.javac, &refs, &migrated),
        "javac accepts the mixed tree",
    );

    let after = probe(&candidate_classes);
    assert_ok(
        &after,
        "the original-compiled probe runs on candidate classes",
    );
    assert_eq!(
        String::from_utf8_lossy(&after.stdout).trim(),
        "value=payload; getter-calls=1; identity=true"
    );
}

/// The measured parent counterexample: `Context.let(block: Context.() -> Key)`
/// is a *member* of the lambda receiver's type, so it shadows the stdlib
/// extension and its block runs with `Context` as `this`. A bare `info` inside
/// that block is `Context.info` (a Java `Other` with a public `key` field), not
/// the relay's `DerivedInfo`. Repairing `it.key` against the relay's property
/// reads a getter that the retained file cannot compile.
#[test]
fn lambda_receiver_member_let_keeps_the_inner_value_binding() {
    let scratch = Scratch::new("member-let");
    let files = [
        (
            "Info.kt",
            "package bridgeprobe\n\ninterface Info : LookupBase {\n    val key: Key\n    suspend fun pendingInfo()\n}\n",
        ),
        (
            "DerivedInfo.kt",
            "package bridgeprobe\n\ninterface DerivedInfo : Info {\n    override val key: Key\n    suspend fun pendingDerived()\n}\n",
        ),
        (
            "LookupBase.kt",
            "package bridgeprobe\n\ninterface LookupBase {\n    val lookup: String\n    suspend fun pending()\n}\n",
        ),
        (
            "Keyed.kt",
            "package bridgeprobe\n\ninterface Keyed {\n    val key: Key\n}\n",
        ),
        (
            "Combined.kt",
            "package bridgeprobe\n\nabstract class Combined(override val key: Key, override val lookup: String) : DerivedInfo, Keyed {\n}\n",
        ),
        (
            "Context.kt",
            "package scopeprobe\nimport bridgeprobe.Other\nimport bridgeprobe.Key\nclass Context(val info: Other) {\n fun let(block: Context.() -> Key): Key = block(this)\n suspend fun stay() {}\n}\n",
        ),
        (
            "Key.java",
            "package bridgeprobe; public final class Key { public final String value; public Key(String value){this.value=value;} }\n",
        ),
        (
            "Other.java",
            "package bridgeprobe; public final class Other {public final Key key; public Other(Key key){this.key=key;} }\n",
        ),
        (
            "Relay.kt",
            "package bridgeprobe\nimport scopeprobe.Context\n\nclass Relay(val info: DerivedInfo, val context: Context) : Keyed {\n    override val key: Key\n        get() = context.let { info.let { it.key } }\n    suspend fun stay() {}\n}\n",
        ),
    ];
    let root = scratch.write("work", &files);

    let migrated = migrate(&root);
    let stderr = String::from_utf8_lossy(&migrated.stderr);
    assert!(migrated.status.success(), "{stderr}");

    // The retained interface side still promotes its property: this regression
    // is about the lambda receiver, not about weakening the repair.
    for retained in ["Info.kt", "DerivedInfo.kt"] {
        let text = read(&root, retained);
        assert!(text.contains("fun getKey("), "{retained}:\n{text}");
    }
    assert!(read(&root, "Keyed.java").contains("getKey()"));

    let relay = read(&root, "Relay.kt");
    assert!(
        relay.contains("context.let { info.let { it.key } }"),
        "`Context.let` is a member of the lambda receiver, so `info` is not the relay property:\n{relay}"
    );
    assert!(!relay.contains("it.getKey()"), "{relay}");
}

/// The shared declarations of the receiver-binding controls, including the
/// Kotlin `Context` whose `let`/`withScope` are *members* of the lambda
/// receiver's type.
fn receiver_binding_core() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "Info.kt",
            "package bridgeprobe\n\ninterface Info : LookupBase {\n    val key: Key\n    suspend fun pendingInfo()\n}\n",
        ),
        (
            "DerivedInfo.kt",
            "package bridgeprobe\n\ninterface DerivedInfo : Info {\n    override val key: Key\n    suspend fun pendingDerived()\n}\n",
        ),
        (
            "LookupBase.kt",
            "package bridgeprobe\n\ninterface LookupBase {\n    val lookup: String\n    suspend fun pending()\n}\n",
        ),
        (
            "Keyed.kt",
            "package bridgeprobe\n\ninterface Keyed {\n    val key: Key\n}\n",
        ),
        (
            "Combined.kt",
            "package bridgeprobe\n\nabstract class Combined(override val key: Key, override val lookup: String) : DerivedInfo, Keyed {\n}\n",
        ),
        (
            "Context.kt",
            "package scopeprobe\nimport bridgeprobe.Other\nimport bridgeprobe.Key\nclass Context(val info: Other) {\n fun let(block: Context.() -> Key): Key = block(this)\n fun withScope(block: Context.() -> Key): Key = block(this)\n suspend fun stay() {}\n}\n",
        ),
        (
            "Key.java",
            "package bridgeprobe; public final class Key { public final String value; public Key(String value){this.value=value;} }\n",
        ),
        (
            "Other.java",
            "package bridgeprobe; public final class Other {public final Key key; public Other(Key key){this.key=key;} }\n",
        ),
    ]
}

/// A bare `info` inside an extension function on `Context` resolves to the
/// extension receiver's member before the enclosing class's: the extension
/// receiver is an implicit receiver even without any lambda.
#[test]
fn extension_receiver_member_dominates_the_enclosing_class() {
    let scratch = Scratch::new("extension-receiver");
    let relay = (
        "Relay.kt",
        "package bridgeprobe\nimport scopeprobe.Context\nclass Relay(val info: DerivedInfo, val context: Context) : Keyed {\n override val key: Key get() = context.read()\n private fun Context.read(): Key = info.let { it.key }\n suspend fun stay() {}\n}\n",
    );
    let root = scratch.write(
        "work",
        &receiver_binding_core()
            .into_iter()
            .chain([relay])
            .collect::<Vec<_>>(),
    );

    let migrated = migrate(&root);
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    let text = read(&root, "Relay.kt");
    assert!(
        text.contains("private fun Context.read(): Key = info.let { it.key }"),
        "{text}"
    );
    assert!(!text.contains("it.getKey()"), "{text}");
}

/// An inner class inheriting a compiled `Holder` can own `info` itself. The
/// unresolved ancestor is not an absence, and the file-wide hint must not stand
/// in for the inherited member.
#[test]
fn unresolved_inherited_member_is_unknown_not_a_global_hint() {
    let scratch = Scratch::new("inherited-member");
    let relay = (
        "Relay.kt",
        "package bridgeprobe\nimport scopeprobe.Context\nimport layerprobe.Holder\nclass Relay(val info: DerivedInfo, val context: Context) : Keyed {\n override val key: Key get() = Inner(context.info).read()\n inner class Inner(other: Other) : Holder(other) {\n  fun read(): Key = info.let { it.key }\n }\n suspend fun stay() {}\n}\n",
    );
    let root = scratch.write(
        "work",
        &receiver_binding_core()
            .into_iter()
            .chain([relay])
            .collect::<Vec<_>>(),
    );

    let migrated = migrate(&root);
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    let text = read(&root, "Relay.kt");
    assert!(
        text.contains("fun read(): Key = info.let { it.key }"),
        "{text}"
    );
    assert!(!text.contains("it.getKey()"), "{text}");
}

/// A typed lexical local stays authoritative for a direct receiver read even
/// inside an opaque receiver scope: `selected.key` is repaired, while the call
/// namespace inside `withScope` stays unproved.
#[test]
fn typed_local_survives_an_opaque_receiver_scope() {
    let scratch = Scratch::new("typed-local");
    let relay = (
        "Relay.kt",
        "package bridgeprobe\nimport scopeprobe.Context\nclass Relay(val info: DerivedInfo, val context: Context) : Keyed {\n override val key: Key get() {\n  val selected: DerivedInfo = info\n  return context.withScope { selected.key }\n }\n suspend fun stay() {}\n}\n",
    );
    let root = scratch.write(
        "work",
        &receiver_binding_core()
            .into_iter()
            .chain([relay])
            .collect::<Vec<_>>(),
    );

    let migrated = migrate(&root);
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    let text = read(&root, "Relay.kt");
    assert!(
        text.contains("context.withScope { selected.getKey() }"),
        "{text}"
    );
    assert!(!text.contains("selected.key"), "{text}");
}

/// An anonymous object's own member owns `info` before the enclosing class.
#[test]
fn anonymous_object_member_is_not_the_enclosing_property() {
    let scratch = Scratch::new("anonymous-member");
    let relay = (
        "Relay.kt",
        "package bridgeprobe\nimport scopeprobe.Context\nclass Relay(val info: DerivedInfo, val context: Context) : Keyed {\n override val key: Key get() = object {\n  val info: Other = context.info\n  fun read(): Key = info.let { it.key }\n }.read()\n suspend fun stay() {}\n}\n",
    );
    let root = scratch.write(
        "work",
        &receiver_binding_core()
            .into_iter()
            .chain([relay])
            .collect::<Vec<_>>(),
    );

    let migrated = migrate(&root);
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    let text = read(&root, "Relay.kt");
    assert!(
        text.contains("fun read(): Key = info.let { it.key }"),
        "{text}"
    );
    assert!(!text.contains("it.getKey()"), "{text}");
}

/// A class that declares its own `sequenceOf` producer shadows the stdlib
/// spelling, so the element type of `for (entry in sequenceOf(info))` is a
/// legacy guess, not proof. The loop variable is still a lexical binder, which
/// is why the legacy direct-read repair keeps working, but the stdlib value
/// `let` must not promote that guess to a lambda parameter type: the real
/// element is the Java `Other`, whose `key` is a field and not a getter.
#[test]
fn shadowed_loop_element_hint_is_not_lambda_type_evidence() {
    let scratch = Scratch::new("shadowed-loop-producer");
    let relay = (
        "Relay.kt",
        "package bridgeprobe\nimport scopeprobe.Context\nclass Relay(val info: DerivedInfo, val context: Context) : Keyed {\n override val key: Key get() {\n  for (entry in sequenceOf(info)) { return entry.let { it.key } }\n  error(\"empty\")\n }\n private fun sequenceOf(value: DerivedInfo): Sequence<Other> = kotlin.sequences.sequenceOf(context.info)\n suspend fun stay() {}\n}\n",
    );
    let root = scratch.write(
        "work",
        &receiver_binding_core()
            .into_iter()
            .chain([relay])
            .collect::<Vec<_>>(),
    );

    let migrated = migrate(&root);
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    // The retained property side still promotes its property: this regression
    // is about the element hint, not about weakening the repair.
    for retained in ["Info.kt", "DerivedInfo.kt"] {
        let text = read(&root, retained);
        assert!(text.contains("fun getKey("), "{retained}:\n{text}");
    }
    let text = read(&root, "Relay.kt");
    assert!(
        text.contains("entry.let { it.key }"),
        "the element type is a legacy spelling guess, not proof of `it`'s type:\n{text}"
    );
    assert!(!text.contains("it.getKey()"), "{text}");
}

/// `info` here is neither a member of `Relay` nor a lexical binder: the only
/// answer is the file-wide table, and its entry comes from an unrelated sibling
/// constructor in the same file. That is a legacy hint, not binding or type
/// evidence, so the value `let` declines instead of typing `it` as the
/// sibling's `DerivedInfo` and repairing a getter the real `Other` lacks.
#[test]
fn file_wide_hint_from_sibling_constructor_is_not_type_evidence() {
    let scratch = Scratch::new("unbound-global-hint");
    let global = (
        "Global.kt",
        "package bridgeprobe\nval info: Other by lazy { Other(Key(\"alternate\")) }\n",
    );
    let relay = (
        "Relay.kt",
        "package bridgeprobe\nimport scopeprobe.Context\nclass Relay(val context: Context) : Keyed {\n override val key: Key get() = info.let { it.key }\n suspend fun stay() {}\n}\nclass Noise(val info: DerivedInfo) { suspend fun keep() {} }\n",
    );
    let root = scratch.write(
        "work",
        &receiver_binding_core()
            .into_iter()
            .chain([global, relay])
            .collect::<Vec<_>>(),
    );

    let migrated = migrate(&root);
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    for retained in ["Info.kt", "DerivedInfo.kt"] {
        let text = read(&root, retained);
        assert!(text.contains("fun getKey("), "{retained}:\n{text}");
    }
    let text = read(&root, "Relay.kt");
    assert!(
        text.contains("info.let { it.key }"),
        "a sibling constructor's property is a file-wide hint, not this binding's type:\n{text}"
    );
    assert!(!text.contains("it.getKey()"), "{text}");
}

/// The call namespace is decided by the whole environment, not by where the
/// receiver lookup happened to answer. `selected` is a canonical typed local,
/// so the receiver proof answers *before* walking out of `Context.read`, but
/// that extension receiver owns `fun DerivedInfo.let`, which takes `Other` and
/// shadows the stdlib value function. The callee is therefore unproved and the
/// read stays written, while the typed local keeps its direct-read repair.
#[test]
fn extension_receiver_callee_barrier_survives_a_typed_local() {
    let scratch = Scratch::new("extension-member-callee");
    let context = (
        "Context.kt",
        "package scopeprobe\nimport bridgeprobe.Other\nimport bridgeprobe.Key\nimport bridgeprobe.DerivedInfo\nclass Context(val info: Other) {\n fun DerivedInfo.let(block: (Other) -> Key): Key = block(info)\n suspend fun stay() {}\n}\n",
    );
    let relay = (
        "Relay.kt",
        "package bridgeprobe\nimport scopeprobe.Context\nclass Relay(val info: DerivedInfo, val context: Context) : Keyed {\n override val key: Key get() = context.read()\n private fun Context.read(): Key {\n  val selected: DerivedInfo = this@Relay.info\n  return selected.let { it.key }\n }\n suspend fun stay() {}\n}\n",
    );
    let files = receiver_binding_core()
        .into_iter()
        .filter(|(name, _)| *name != "Context.kt")
        .chain([context, relay])
        .collect::<Vec<_>>();
    let root = scratch.write("work", &files);

    let migrated = migrate(&root);
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    for retained in ["Info.kt", "DerivedInfo.kt"] {
        let text = read(&root, retained);
        assert!(text.contains("fun getKey("), "{retained}:\n{text}");
    }
    let text = read(&root, "Relay.kt");
    assert!(
        text.contains("return selected.let { it.key }"),
        "the extension receiver's member `let` is not the stdlib value function:\n{text}"
    );
    assert!(!text.contains("it.getKey()"), "{text}");
}
