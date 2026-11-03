use notlin::java_bindings::{JavaAnalysisContext, collect};
use notlin::workspace::SourceIndex;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn fixture(source: &str) -> (PathBuf, SourceIndex) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "javac-bindings-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    assert!(!root.exists());
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Use.java"), source).unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    (root, index)
}

fn context() -> JavaAnalysisContext {
    JavaAnalysisContext {
        java: std::env::var_os("NOTLIN_JAVA").expect("NOTLIN_JAVA").into(),
        classpath: Vec::new(),
        processors: Vec::new(),
        processor_path: Vec::new(),
        release: Some(17),
        enable_preview: false,
        roots: Vec::new(),
        additional_inputs: Vec::new(),
        timeout: Duration::from_secs(45),
    }
}

#[test]
#[ignore = "requires a JDK in NOTLIN_JAVA"]
fn javac_selects_getters_through_factories_callbacks_patterns_and_custom_sams() {
    let source = r#"package sample;
import java.util.List;
import java.util.Optional;
class Wide {}
class Narrow extends Wide {}
interface Root {Wide getId();}
class Base implements Root {
 public Wide getId(){return null;}
 public String getId(String suffix){return suffix;}
}
class Lookup extends Base {public Narrow getId(){return null;}}
class Other {public String getId(){return "other";}}
class Factory {static <T> T make(Class<T> type){return null;}}
interface Getter<T,R> {R invoke(T value);}
interface Append {String invoke(Base value,String suffix);}
class Use {
 Wide read(Object candidate,List<Base> values,Optional<Base> optional) {
  if(candidate instanceof Base found && found.getId()!=null) return found.getId();
  var made=Factory.make(Base.class);made.getId();
  values.stream().map(value->value.getId()).toList();
  optional.map(value->value.getId());
  Getter<Base,Wide> getter=Base::getId;
  Getter<Lookup,Narrow> narrow=Lookup::getId;
  Append append=Base::getId;
  return getter.invoke(made);
 }
 String shadow(Other Base){return Base.getId();}
 Wide escaped(Base base){String text="café 🦀";return base.\u0067etId();}
}
"#;
    let (root, index) = fixture(source);
    let file = index.java_files().next().unwrap();
    let mut context = context();
    context.roots.push("sample.Root".into());
    let bindings = collect(
        &index,
        &[file.path.clone()],
        &["getId".into()],
        &context,
        &root.join("analysis"),
    )
    .unwrap();
    let calls = bindings.calls(file).unwrap().unwrap();
    assert_eq!(
        fs::read_to_string(root.join("Use.java")).unwrap(),
        source,
        "attribution must not edit source files"
    );
    assert_eq!(calls.len(), 10, "{calls:#?}");
    assert_eq!(
        calls
            .iter()
            .filter(
                |call| call.owner_binary == "sample.Base" && call.descriptor == "()Lsample/Wide;"
            )
            .count(),
        7
    );
    assert!(calls.iter().any(|call| call.owner_binary == "sample.Lookup"
        && call.descriptor == "()Lsample/Narrow;"
        && call.reference));
    assert!(calls.iter().any(
        |call| call.descriptor == "(Ljava/lang/String;)Ljava/lang/String;"
            && call.parameter_count == 1
            && call.reference
    ));
    assert!(calls.iter().any(|call| call.owner_binary == "sample.Other"));
    assert!(
        calls
            .iter()
            .filter(|call| call.owner_binary == "sample.Base")
            .all(|call| call.related_roots == ["sample.Root"])
    );
    assert!(
        calls
            .iter()
            .filter(|call| call.owner_binary == "sample.Other")
            .all(|call| call.related_roots.is_empty())
    );
    assert!(
        calls
            .iter()
            .any(|call| &source[call.start_byte..call.end_byte] == r"\u0067etId")
    );
    bindings.verify_inputs().unwrap();
}

#[test]
#[ignore = "requires a JDK in NOTLIN_JAVA"]
fn source_strings_are_immutable_and_a_new_snapshot_cannot_reuse_old_bindings() {
    let original = "class Use {String getId(){return \"old\";} String read(){return getId();}}";
    let (root, index) = fixture(original);
    let file = index.java_files().next().unwrap();
    fs::write(
        root.join("Use.java"),
        "this is not the source supplied to javac",
    )
    .unwrap();
    let bindings = collect(
        &index,
        &[file.path.clone()],
        &["getId".into()],
        &context(),
        &root.join("analysis"),
    )
    .unwrap();
    assert_eq!(bindings.calls(file).unwrap().unwrap().len(), 1);
    let current = SourceIndex::discover(&root).unwrap();
    let changed = current
        .java_files()
        .find(|file| file.path.ends_with("Use.java"))
        .unwrap();
    assert!(
        bindings
            .calls(changed)
            .unwrap_err()
            .contains("source changed")
    );
}

#[test]
#[ignore = "requires a JDK in NOTLIN_JAVA"]
fn any_original_compilation_error_discards_the_entire_attribution_batch() {
    let (root, index) = fixture(
        "class Use {String getId(){return \"ok\";} Object read(){getId();return missing.getId();}}",
    );
    let file = index.java_files().next().unwrap();
    let error = collect(
        &index,
        &[file.path.clone()],
        &["getId".into()],
        &context(),
        &root.join("analysis"),
    )
    .unwrap_err();
    assert!(error.contains("did not attribute"), "{error}");
}

#[test]
#[ignore = "requires a JDK in NOTLIN_JAVA"]
fn changing_a_classpath_entry_invalidates_the_binding_context() {
    let (root, index) =
        fixture("class Use {String getId(){return \"ok\";} String read(){return getId();}}");
    let classes = root.join("classes");
    fs::create_dir(&classes).unwrap();
    let mut context = context();
    context.classpath.push(classes.clone());
    let file = index.java_files().next().unwrap();
    let bindings = collect(
        &index,
        &[file.path.clone()],
        &["getId".into()],
        &context,
        &root.join("analysis"),
    )
    .unwrap();
    fs::write(classes.join("new-resource"), "changed").unwrap();
    assert!(
        bindings
            .verify_inputs()
            .unwrap_err()
            .contains("inputs changed")
    );
}

#[test]
#[ignore = "requires a JDK in NOTLIN_JAVA"]
fn loop_catch_and_lambda_parameters_shadow_fields_while_outer_captures_keep_their_type() {
    let (root, index) = fixture(
        r#"package sample;
import java.util.List;
interface Root {String getId();}
class Base implements Root {public String getId(){return "base";}}
class Other {String getId(){return "other";}}
class OtherException extends RuntimeException {String getId(){return "error";}}
class Use {
 Base value;
 void read(List<Other> values){
  for(Other value:values){value.getId();}
  for(Other value=new Other();value!=null;){value.getId();break;}
  try {throw new OtherException();}catch(OtherException value){value.getId();}
  java.util.function.Function<Other,String> typed=(Other value)->value.getId();
  java.util.function.Function<Other,String> inferred=value->value.getId();
  java.util.function.Supplier<String> captured=()->value.getId();
 }
}
"#,
    );
    let file = index.java_files().next().unwrap();
    let mut context = context();
    context.roots.push("sample.Root".into());
    let bindings = collect(
        &index,
        &[file.path.clone()],
        &["getId".into()],
        &context,
        &root.join("analysis"),
    )
    .unwrap();
    let calls = bindings.calls(file).unwrap().unwrap();
    assert_eq!(calls.len(), 6);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.owner_binary == "sample.Other")
            .count(),
        4
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.owner_binary == "sample.OtherException")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(
                |call| call.related_roots == ["sample.Root"] && call.owner_binary == "sample.Base"
            )
            .count(),
        1
    );
}

#[test]
#[ignore = "requires a JDK in NOTLIN_JAVA"]
fn a_missing_hierarchy_root_only_proves_a_family_irrelevant_when_the_getter_has_no_calls() {
    for (source, expected) in [
        ("class Use {int read(){return 1;}}", true),
        (
            "class Use {String getId(){return \"id\";} String read(){return getId();}}",
            false,
        ),
    ] {
        let (root, index) = fixture(source);
        let file = index.java_files().next().unwrap();
        let mut context = context();
        context.roots.push("absent.Root".into());
        let bindings = collect(
            &index,
            &[file.path.clone()],
            &["getId".into()],
            &context,
            &root.join("analysis"),
        )
        .unwrap();
        assert_eq!(bindings.covers_family("absent.Root", "getId"), expected);
        assert!(!bindings.covers_family("absent.Root", "getOther"));
    }
}

#[test]
#[ignore = "requires a JDK in NOTLIN_JAVA"]
fn newly_created_processor_configuration_invalidates_an_existing_receipt() {
    let (root, index) =
        fixture("class Use {String getId(){return \"ok\";} String read(){return getId();}}");
    let file = index.java_files().next().unwrap();
    let config = root.join("lombok.config");
    let mut context = context();
    context.additional_inputs.push(config.clone());
    let bindings = collect(
        &index,
        &[file.path.clone()],
        &["getId".into()],
        &context,
        &root.join("analysis"),
    )
    .unwrap();
    fs::write(config, "lombok.accessors.fluent = true").unwrap();
    assert!(
        bindings
            .verify_inputs()
            .unwrap_err()
            .contains("inputs changed")
    );
}
