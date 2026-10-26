//! Binding facts share the content-addressed call-site batch, including absence.
use notlin::facts::{classpath::source_hash, runtime::FactsRuntime};
use std::{fs, path::PathBuf, process::Command};
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn property_bindings_share_one_batch_and_reject_stale_or_contradictory_text() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = Scratch(std::env::temp_dir().join(format!(
        "notlin-property-runtime-{}-{nonce}",
        std::process::id()
    )));
    fs::create_dir(&root.0).unwrap();
    let file = root.0.join("Input.kt");
    fs::write(&file, "TOKEN\n").unwrap();
    let helper = root.0.join("helper.py");
    fs::write(&helper,r#"import sys,json,pathlib
for line in sys.stdin:
 r=json.loads(line);q=r['query'];kind=next(iter(q));v=q[kind]
 with pathlib.Path(__file__).with_suffix('.journal').open('a') as f:f.write(line)
 print(json.dumps({'version':1,'id':r['id'],'answer': {'CallSites':{'file':v['file'],'sites':[],'references':[{'byte_offset':0,'end_byte':5,'text':'TOKEN','target_file':v['file'],'target_start_byte':0,'target_end_byte':5,'target_name':'TOKEN','target_package':'probe'}]}}}),flush=True)
"#).unwrap();
    let mut command = Command::new("python3");
    command.arg(&helper);
    let runtime = FactsRuntime::with_command(command).unwrap();
    assert!(
        runtime
            .source_property_reference_at(&file, 0, &source_hash("different"))
            .is_none()
    );
    let proof = runtime
        .source_property_reference_at(&file, 0, &source_hash("TOKEN\n"))
        .unwrap();
    assert_eq!(proof.target_name, "TOKEN");
    assert!(
        runtime
            .source_property_reference_at(&file, 1, &source_hash("TOKEN\n"))
            .is_none()
    );
    assert_eq!(
        fs::read_to_string(helper.with_extension("journal"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    fs::write(&file, "OTHER\n").unwrap();
    assert!(
        runtime
            .source_property_reference_at(&file, 0, &source_hash("TOKEN\n"))
            .is_none()
    );
    assert!(
        runtime
            .source_property_reference_at(&file, 0, &source_hash("OTHER\n"))
            .is_none(),
        "helper text must match the exact source span"
    );
    assert_eq!(
        fs::read_to_string(helper.with_extension("journal"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}
