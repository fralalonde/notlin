//! Wildcard-imported declarations still emit a name-bound constant field.
//!
//! The plain capability proves a `const val … : String` slot from the source
//! tree alone. A wildcard import makes that spelling ambiguous to a Java
//! reader, so the source proof alone refuses. The bounded wildcard path adds
//! native declaration-type facts (exact descriptor + nullability) and binds
//! the emitted type name with a single-type import; without those facts the
//! constant keeps the Legacy (retained) shape.
use std::{fs, path::PathBuf, process::Command};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn root(label: &str) -> Scratch {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "notlin-field-proof-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&path).unwrap();
    Scratch(path)
}

/// The declaring file keeps a wildcard import *and* a consumer that must stay
/// Kotlin (`string_literal` / nullable `trim` are unproved operations), so the
/// field proof is the only thing that can release the constant.
const WILDCARD_STATE: &str = "package probe\nimport java.util.*\nconst val TOKEN: String = \"token\"\nclass Reader {\n    fun read(): String = \"[$TOKEN]\"\n    fun pad(v: String?): String? = v?.trim()\n}\n";

fn run_fixture(label: &str, mutation: &str, state: &str, enabled: bool) -> Scratch {
    let root = root(label);
    fs::write(root.0.join("State.kt"), state).unwrap();
    fs::write(
        root.0.join("Plain.kt"),
        "package probe\nclass Plain { fun identity(v: Int): Int = v }\n",
    )
    .unwrap();
    let helper = root.0.join("helper.py");
    let script = r#"#!/usr/bin/env python3
import sys,json,pathlib,os
base=pathlib.Path(__file__).parent
mode=os.environ['REFERENCE_MUTATION']
for line in sys.stdin:
 r=json.loads(line);q=r['query'];kind=next(iter(q));value=q[kind]
 with (base/'journal').open('a') as out:out.write(line)
 if kind=='ResolveCalls':
  file=pathlib.Path(value['file']);source=file.read_bytes();refs=[]
  if file.name=='State.kt':
   start=source.index(b'const val');end=source.index(b'\n',start);offset=source.index(b'$TOKEN')+1
   ref={'byte_offset':offset,'end_byte':offset+5,'text':'TOKEN','target_file':str(file.resolve()),'target_start_byte':start,'target_end_byte':end,'target_name':'TOKEN','target_package':'probe'}
   if mode!='old-helper':
    ref['target_jvm_descriptor']='Ljava/lang/String;'
    ref['target_nullable']=False
   if mode=='object-descriptor':ref['target_jvm_descriptor']='Ljava/lang/Object;'
   if mode=='nullable':ref['target_nullable']=True
   if mode=='contradictory-nullability':ref['target_nullable']=None
   refs=[ref]
  answer={'CallSites':{'file':str(file),'sites':[],'references':refs}}
 elif kind=='ClassMembers':answer={'Members':{'owner':value['owner'],'members':None}}
 else:answer={'Pong':{'version':1}}
 print(json.dumps({'version':1,'id':r['id'],'answer':answer}),flush=True)
"#;
    fs::write(&helper, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let jar = root.0.join("notlin-jvm-facts-full.jar");
    fs::write(&jar, "scripted helper fixture").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root.0)
        .args([
            "--annotations",
            "none",
            "--in-place",
            "--coverage-report",
            "coverage.json",
            "--jvm-facts",
            if enabled { "on" } else { "off" },
            ".",
        ])
        .env_remove("NOTLIN_JVM_FACTS_FULL_JAR")
        .env_remove("NOTLIN_JVM_FACTS_KOTLIN")
        .env_remove("NOTLIN_CLASSPATH")
        .env("NOTLIN_JAVA", &helper)
        .env("NOTLIN_JVM_FACTS_JAR", jar)
        .env("NOTLIN_KOTLIN_SOURCES", &root.0)
        .env("REFERENCE_MUTATION", mutation)
        .output()
        .unwrap();
    fs::write(root.0.join("diagnostics.log"), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{label}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    root
}

fn diagnostics(root: &Scratch) -> String {
    fs::read_to_string(root.0.join("diagnostics.log")).unwrap_or_default()
}

#[test]
fn wildcard_declaration_binds_the_emitted_field_type_with_an_exact_import() {
    let root = run_fixture("wildcard", "valid", WILDCARD_STATE, true);
    let java = fs::read_to_string(root.0.join("State.java")).unwrap_or_else(|_| {
        panic!(
            "a declaration proved by native descriptor facts must gain Java ownership: {}",
            diagnostics(&root)
        )
    });
    // The source spelling `String` is not provable under a Java wildcard import;
    // the emitted name has to be bound by a single-type import instead.
    assert!(
        java.contains("import java.lang.String;"),
        "the proved field type must be name-bound: {java}"
    );
    assert!(
        java.contains("public static final String TOKEN = \"token\";"),
        "the field keeps the proved, canonical Java type: {java}"
    );
    let retained = fs::read_to_string(root.0.join("State.kt")).unwrap();
    assert!(
        retained.contains("class Reader"),
        "the reference proof does not sign the consumer's operations"
    );
    assert!(
        retained.contains("import probe.State.TOKEN"),
        "same-package consumers need a nameable Java field: {retained}"
    );
    assert!(retained.contains("v?.trim()"));
    assert!(
        root.0.join("Plain.java").exists(),
        "the unrelated control declaration still translates"
    );
}

#[test]
fn the_plain_declaring_file_keeps_its_legacy_spelling_and_import_block() {
    // The new binding must not leak into the already-proved (no wildcard) path.
    let plain = WILDCARD_STATE.replace("import java.util.*\n", "");
    let root = run_fixture("plain-control", "valid", &plain, true);
    let java = fs::read_to_string(root.0.join("State.java"))
        .unwrap_or_else(|_| panic!("plain constant must translate: {}", diagnostics(&root)));
    assert!(
        !java.contains("import java.lang.String;"),
        "the new single-type import belongs to the wildcard proof only: {java}"
    );
}

#[test]
fn unproved_or_contradictory_declaration_facts_keep_the_legacy_floor() {
    // Old helper (no declaration facts), a wrong descriptor, a nullable
    // declaration type and a non-boolean nullability all fail closed: the
    // reference stays Kotlin exactly as it did before this slice.
    for mutation in [
        "old-helper",
        "object-descriptor",
        "nullable",
        "contradictory-nullability",
    ] {
        let root = run_fixture(mutation, mutation, WILDCARD_STATE, true);
        assert!(
            !root.0.join("State.java").exists(),
            "{mutation}: an unproved declaration type must not release the constant"
        );
        assert!(
            root.0.join("Plain.java").exists(),
            "{mutation}: the unrelated control still translates"
        );
    }
}

#[test]
fn a_name_that_java_can_capture_is_not_bound_by_import() {
    // `class String` in the same compilation unit shadows a single-type import
    // (javac rejects the field), so the proof must refuse before emission.
    let same_file_type = "package probe\nimport java.util.*\nclass String\nconst val TOKEN: String = \"token\"\nclass Reader {\n    fun read(): String = \"[$TOKEN]\"\n    fun pad(v: String?): String? = v?.trim()\n}\n";
    let root = run_fixture("same-file-type", "valid", same_file_type, true);
    assert!(
        !root.0.join("State.java").exists(),
        "a same-file `String` declaration captures the emitted name: {}",
        diagnostics(&root)
    );

    // A second single-type import with the same simple name would collide with
    // the binding import in generated Java.
    let colliding_import = "package probe\nimport java.util.*\nimport probe.String\nconst val TOKEN: String = \"token\"\nclass Reader {\n    fun read(): String = \"[$TOKEN]\"\n    fun pad(v: String?): String? = v?.trim()\n}\n";
    let root = run_fixture("colliding-import", "valid", colliding_import, true);
    assert!(
        !root.0.join("State.java").exists(),
        "a conflicting `String` import cannot be shadowed: {}",
        diagnostics(&root)
    );

    // A typealias named `String` would make the emitted spelling depend on
    // alias lowering, which is a different proof: refuse rather than guess.
    let aliased = "package probe\nimport java.util.*\ntypealias String = kotlin.String\nconst val TOKEN: String = \"token\"\nclass Reader {\n    fun read(): String = \"[$TOKEN]\"\n    fun pad(v: String?): String? = v?.trim()\n}\n";
    let root = run_fixture("aliased-type", "valid", aliased, true);
    assert!(
        !root.0.join("State.java").exists(),
        "a `String` typealias is not the builtin binding this proof means: {}",
        diagnostics(&root)
    );
}

#[test]
fn helper_off_runs_never_ask_for_declaration_facts() {
    let root = run_fixture("helper-off", "valid", WILDCARD_STATE, false);
    assert!(!root.0.join("State.java").exists());
    assert!(
        !root.0.join("journal").exists(),
        "helper off must not ask for binding facts"
    );
}

#[test]
fn old_shaped_property_references_still_decode_without_declaration_facts() {
    use notlin::facts::protocol::{Answer, decode_response};
    let line = r#"{"version":1,"id":3,"answer":{"CallSites":{"file":"/tmp/A.kt","sites":[],"references":[{"byte_offset":1,"end_byte":6,"text":"TOKEN","target_file":"/tmp/A.kt","target_start_byte":0,"target_end_byte":5,"target_name":"TOKEN","target_package":"probe"}]}}}"#;
    let response = decode_response(line).expect("old-shaped line still decodes");
    let Answer::CallSites { references, .. } = response.answer else {
        panic!("wrong answer variant");
    };
    let reference = &references.expect("references present")[0];
    assert_eq!(reference.target_jvm_descriptor, None);
    assert_eq!(reference.target_nullable, None);

    // And an absent proof serializes with its exact old shape: no new keys.
    let encoded = serde_json::to_string(reference).unwrap();
    assert!(
        !encoded.contains("target_jvm_descriptor") && !encoded.contains("target_nullable"),
        "absent declaration facts must not change the wire shape: {encoded}"
    );
}
