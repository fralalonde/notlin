//! Fact-backed static-property imports preserve the original Kotlin consumers.
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
        "notlin-property-binding-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&path).unwrap();
    Scratch(path)
}
const SOURCE: &str = "package probe\nconst val TOKEN: String = \"token\"\nclass Reader {\n    fun read(): String = \"[$TOKEN]\"\n    fun pad(v: String?): String? = v?.trim()\n}\n";
fn run_fixture(contradiction: &str, enabled: bool) -> Scratch {
    let root = root(contradiction);
    fs::write(root.0.join("State.kt"), SOURCE).unwrap();
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
  if file.name=='State.kt' and mode!='absent':
   start=source.index(b'const val');end=source.index(b'\n',start);offset=source.index(b'$TOKEN')+1
   ref={'byte_offset':offset,'end_byte':offset+5,'text':'TOKEN','target_file':str(file.resolve()),'target_start_byte':start,'target_end_byte':end,'target_name':'TOKEN','target_package':'probe'}
   if mode=='wrong-target':ref['target_name']='OTHER'
   if mode=='wrong-span':ref['end_byte']+=1
   refs=[ref]
   if mode=='duplicate':refs.append(dict(ref))
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
        .env("REFERENCE_MUTATION", contradiction)
        .output()
        .unwrap();
    fs::write(root.0.join("diagnostics.log"), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    root
}
#[test]
fn exact_source_property_binding_retargets_imports_and_keeps_consumers_kotlin() {
    let root = run_fixture("valid", true);
    assert!(
        root.0.join("State.java").exists(),
        "a bound constant must gain Java ownership: {}",
        fs::read_to_string(root.0.join("diagnostics.log")).unwrap()
    );
    let retained = fs::read_to_string(root.0.join("State.kt")).unwrap();
    assert!(
        retained.contains("class Reader"),
        "the reference proof must not sign its consumer's operations"
    );
    assert!(
        retained.contains("import probe.State.TOKEN"),
        "same-package references need a nameable Java field: {retained}"
    );
    assert!(retained.contains("v?.trim()"));
    assert!(root.0.join("Plain.java").exists());
    assert!(
        fs::read_to_string(root.0.join("journal"))
            .unwrap()
            .contains("ResolveCalls")
    );
}
#[test]
fn missing_or_contradictory_source_property_bindings_keep_the_legacy_floor() {
    for mode in ["absent", "wrong-target", "wrong-span", "duplicate"] {
        let root = run_fixture(mode, true);
        assert!(
            !root.0.join("State.java").exists(),
            "{mode}: incomplete facts cannot release the constant"
        );
        assert!(root.0.join("Plain.java").exists());
    }
    let root = run_fixture("valid", false);
    assert!(!root.0.join("State.java").exists());
    assert!(
        !root.0.join("journal").exists(),
        "helper off must not ask for binding facts"
    );
}
