use notlin::getter_normalization::{discover, plan_java_callers, plan_layout};
use notlin::workspace::{SourceIndex, SourceOverlay};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);

fn empty_fixture(label: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!(
            "getter-layout-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    assert!(!root.exists());
    fs::create_dir_all(&root).unwrap();
    root
}

fn fixture(label: &str) -> PathBuf {
    let root = empty_fixture(label);
    for (name, text) in [
        (
            "Ids.kt",
            "package sample\nopen class Wide(val value:String)\nclass Narrow(value:String):Wide(value)\n",
        ),
        (
            "Root.kt",
            "package sample\ninterface ReadRoot {val id:Wide}\ninterface Root:ReadRoot {override var id:Wide}\n",
        ),
        (
            "Model.kt",
            "package sample\nimport jakarta.persistence.MappedSuperclass\n@MappedSuperclass open class Base(override var id:Wide):Root {fun getId(suffix:String):String=id.value+suffix}\nopen class Child(id:Wide):Base(id) {var calls:Int=0;fun getId():Narrow {calls++;return Narrow(id.value+\"/lookup\")}}\nclass Typed(override val id:Narrow):ReadRoot\n",
        ),
        ("Leaf.kt", "package sample\nclass Leaf(id:Wide):Child(id)\n"),
        (
            "Existing.java",
            "package sample;public class Existing implements Root {private Wide id;public Existing(Wide id){this.id=id;}public Wide getId(){return id;}public void setId(Wide value){id=value;}}\n",
        ),
        (
            "MappedSuperclass.java",
            "package jakarta.persistence;@java.lang.annotation.Target(java.lang.annotation.ElementType.TYPE) public @interface MappedSuperclass {}\n",
        ),
        (
            "Probe.java",
            r#"package sample;
public class Probe {
 public static String bean(Child child,boolean kotlin) throws Exception {
  com.fasterxml.jackson.databind.ObjectMapper mapper=new com.fasterxml.jackson.databind.ObjectMapper();
  if(kotlin)mapper.registerModule(new com.fasterxml.jackson.module.kotlin.KotlinModule.Builder().build());
  return mapper.valueToTree(child).get("id").get("value").asText();
 }
 public static String beanType() throws Exception {return Typed.class.getMethod("getId").getReturnType().getSimpleName();}
}
"#,
        ),
        (
            "Runner.kt",
            r#"package sample
fun main() {
 val child:Child=Leaf(Wide("stored"));val base:Base=child;val read:ReadRoot=child
 println(base.id.value+"|"+read.id.value+"|"+child.id.value+"|"+child.getId().value)
 base.id=Wide("set");println(base.id.value+"|"+read.id.value)
 println(Probe.bean(child,false));println(Probe.bean(child,true));println(child.calls)
 val existing:ReadRoot=Existing(Wide("java"));println(existing.id.value)
 val typed=Typed(Narrow("typed"));println(typed.id.value+"|"+Probe.beanType())
}
"#,
        ),
    ] {
        fs::write(root.join(name), text).unwrap();
    }
    root
}

fn normalize(root: &Path) {
    // These declaration-layout controls have no original Java getter calls.
    // The caller migration controls below compile and attribute original Java.
    let index = SourceIndex::discover(root).unwrap();
    let families = discover(&index);
    let family = families.iter().find(|f| f.root.name == "ReadRoot").unwrap();
    let plan = plan_layout(&index, family).unwrap();
    assert!(plan.caller_migration_required);
    assert_eq!(plan.bean_views.len(), 4);
    assert_eq!(plan.java_adapters.len(), 1);
    fs::write(
        root.join("layout-plan.json"),
        serde_json::to_vec_pretty(&plan).unwrap(),
    )
    .unwrap();
    for overlay in plan.overlays(&index).unwrap() {
        match overlay {
            SourceOverlay::Replace { path, source, .. } => fs::write(path, source).unwrap(),
            SourceOverlay::Delete { .. } => panic!("no deletions"),
        }
    }
}

fn normalize_attributed(root: &Path, all_open: bool) {
    // Compile the original Kotlin/Java inputs together before attributing Java
    // against those classes. The stage is outside the translated source root.
    let _original = native_into(root, all_open, false, "original-classes");
    let index = SourceIndex::discover(root).unwrap();
    let families = discover(&index);
    let family = families
        .iter()
        .find(|family| family.root.name == "ReadRoot")
        .unwrap();
    let mut classpath = vec![
        root.join("original-classes"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"),
    ];
    for name in [
        "NOTLIN_KOTLIN_STDLIB",
        "NOTLIN_KOTLIN_REFLECT",
        "NOTLIN_JACKSON_CORE",
        "NOTLIN_JACKSON_ANNOTATIONS",
        "NOTLIN_JACKSON_DATABIND",
        "NOTLIN_JACKSON_KOTLIN",
    ] {
        classpath.push(std::env::var_os(name).unwrap().into());
    }
    let context = notlin::java_bindings::JavaAnalysisContext {
        java: std::env::var_os("NOTLIN_JAVA").unwrap().into(),
        classpath,
        processors: Vec::new(),
        processor_path: Vec::new(),
        release: Some(17),
        enable_preview: false,
        roots: vec!["sample.ReadRoot".into()],
        additional_inputs: Vec::new(),
        timeout: std::time::Duration::from_secs(60),
    };
    let stage = root.with_file_name(format!(
        "{}-java-attribution",
        root.file_name().unwrap().to_string_lossy()
    ));
    let bindings = notlin::java_bindings::collect(
        &index,
        &index
            .java_files()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>(),
        &[family.original_getter.clone()],
        &context,
        &stage,
    )
    .unwrap();
    let calls = plan_java_callers(&index, family, &[bindings]).unwrap();
    assert!(calls.unresolved_calls.is_empty(), "{calls:#?}");
    assert_eq!(calls.edits.len(), 10, "{calls:#?}");
    let mut plan = plan_layout(&index, family).unwrap();
    plan.edits.extend(calls.edits);
    for overlay in plan.overlays(&index).unwrap() {
        match overlay {
            SourceOverlay::Replace { path, source, .. } => fs::write(path, source).unwrap(),
            SourceOverlay::Delete { .. } => panic!("layout cannot delete a source"),
        }
    }
}

#[test]
fn layout_plan_is_pure_and_rejects_a_changed_source_snapshot() {
    let root = fixture("snapshot");
    let original = fs::read_to_string(root.join("Root.kt")).unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let families = discover(&index);
    let family = families.iter().find(|f| f.root.name == "ReadRoot").unwrap();
    let plan = plan_layout(&index, family).unwrap();
    assert_eq!(fs::read_to_string(root.join("Root.kt")).unwrap(), original);
    let file = index
        .kotlin_files()
        .find(|file| file.path.ends_with("Root.kt"))
        .unwrap();
    let changed = index
        .with_overlays(&[SourceOverlay::Replace {
            path: file.path.clone(),
            source: original + "\n// changed snapshot\n",
            language: notlin::workspace::SourceLanguage::Kotlin,
        }])
        .unwrap();
    assert!(plan.overlays(&changed).unwrap_err().contains("snapshot"));
    normalize(&root);
    let model = fs::read_to_string(root.join("Model.kt")).unwrap();
    assert!(model.contains("override fun getId():Narrow"), "{model}");
    let existing = fs::read_to_string(root.join("Existing.java")).unwrap();
    assert!(
        existing.contains("public Wide getId(){return id;}"),
        "{existing}"
    );
    assert!(
        existing.contains("public Wide readId() { return getId(); }"),
        "{existing}"
    );
}

#[test]
fn layout_keeps_annotated_getter_headers_until_their_roles_are_established() {
    let root = fixture("getter-annotation");
    let source = "package sample\ninterface ReadRoot {val id:Wide\n @Deprecated(\"contract\") get()=Wide(\"root\")}\ninterface Root:ReadRoot {override var id:Wide}\n";
    fs::write(root.join("Root.kt"), source).unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let families = discover(&index);
    let family = families
        .iter()
        .find(|family| family.root.name == "ReadRoot")
        .unwrap();
    let error = plan_layout(&index, family).unwrap_err();
    assert!(error.contains("getter-header annotation"), "{error}");
    assert_eq!(fs::read_to_string(root.join("Root.kt")).unwrap(), source);
}

fn native(root: &Path, all_open: bool, lombok: bool) -> String {
    native_into(root, all_open, lombok, "classes")
}

fn native_into(root: &Path, all_open: bool, lombok: bool, output: &str) -> String {
    use notlin::jvm_validation::{JvmValidationConfig, validate_jvm_sources};
    let mut config = JvmValidationConfig::new(
        std::env::var_os("NOTLIN_KOTLINC").unwrap(),
        std::env::var_os("NOTLIN_JAVAC").unwrap(),
        std::env::var_os("NOTLIN_JAVA").unwrap(),
        root.join(output),
    );
    config
        .kotlinc_args
        .extend(["-jvm-target".into(), "17".into()]);
    if all_open {
        config.kotlinc_args.extend([
            format!(
                "-Xplugin={}",
                PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_ALLOPEN_PLUGIN").unwrap()).display()
            )
            .into(),
            "-P".into(),
            "plugin:org.jetbrains.kotlin.allopen:annotation=jakarta.persistence.MappedSuperclass"
                .into(),
        ]);
    }
    config.javac_args.extend([
        "--release".into(),
        "17".into(),
        if lombok { "-proc:full" } else { "-proc:none" }.into(),
    ]);
    for name in [
        "NOTLIN_KOTLIN_STDLIB",
        "NOTLIN_KOTLIN_REFLECT",
        "NOTLIN_JACKSON_CORE",
        "NOTLIN_JACKSON_ANNOTATIONS",
        "NOTLIN_JACKSON_DATABIND",
        "NOTLIN_JACKSON_KOTLIN",
    ] {
        config
            .classpath
            .push(std::env::var_os(name).unwrap().into());
    }
    config
        .classpath
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"));
    if lombok {
        config.kotlinc_args.push(
            format!(
                "-Xplugin={}",
                PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").unwrap()).display()
            )
            .into(),
        );
        config
            .classpath
            .push(std::env::var_os("NOTLIN_LOMBOK").unwrap().into());
    }
    for entry in fs::read_dir(root).unwrap() {
        let file = entry.unwrap().path();
        match file.extension().and_then(|s| s.to_str()) {
            Some("kt") => config.kotlin_sources.push(file),
            Some("java") => config.java_sources.push(file),
            _ => (),
        }
    }
    config.run_main_class = Some("sample.RunnerKt".into());
    let result =
        validate_jvm_sources(&config).unwrap_or_else(|error| panic!("{root:?}: {error:?}"));
    fs::write(
        root.join(format!("native-{output}.log")),
        format!("{result:#?}"),
    )
    .unwrap();
    result.commands.last().unwrap().stdout.replace("\r\n", "\n")
}

fn workspace_config(root: &Path) -> serde_json::Value {
    let mut classpath = Vec::new();
    for key in [
        "NOTLIN_KOTLIN_STDLIB",
        "NOTLIN_KOTLIN_REFLECT",
        "NOTLIN_JACKSON_CORE",
        "NOTLIN_JACKSON_ANNOTATIONS",
        "NOTLIN_JACKSON_DATABIND",
        "NOTLIN_JACKSON_KOTLIN",
        "NOTLIN_LOMBOK",
    ] {
        classpath.push(PathBuf::from(std::env::var_os(key).unwrap()));
    }
    classpath
        .push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/jetbrains-annotations.jar"));
    let index = SourceIndex::discover(root).unwrap();
    let config = serde_json::json!({
        "version":1,
        "kotlinc":PathBuf::from(std::env::var_os("NOTLIN_KOTLINC").unwrap()),
        "javac":PathBuf::from(std::env::var_os("NOTLIN_JAVAC").unwrap()),
        "java":PathBuf::from(std::env::var_os("NOTLIN_JAVA").unwrap()),
        "kotlin_sources":index.kotlin_files().map(|file|file.path.clone()).collect::<Vec<_>>(),
        "java_sources":index.java_files().map(|file|file.path.clone()).collect::<Vec<_>>(),
        "classpath":classpath,
        "kotlinc_args":["-jvm-target","17",format!("-Xplugin={}",PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_ALLOPEN_PLUGIN").unwrap()).display()),"-P","plugin:org.jetbrains.kotlin.allopen:annotation=jakarta.persistence.MappedSuperclass",format!("-Xplugin={}",PathBuf::from(std::env::var_os("NOTLIN_KOTLIN_LOMBOK_PLUGIN").unwrap()).display())],
        "javac_args":["--release","17","-proc:full"]
    });
    config
}

#[test]
#[ignore = "requires real Kotlin/JVM tools, all-open, Lombok and Jackson in NOTLIN_* variables"]
fn workspace_migration_compiles_original_bindings_and_applies_java_with_kotlin() {
    for (lombok, keep_base, keep_chain) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (true, true, false),
        (false, true, true),
        (true, true, true),
    ] {
        let root = fixture(&format!("workspace-{lombok}-{keep_base}-{keep_chain}"));
        if keep_base {
            let model = fs::read_to_string(root.join("Model.kt")).unwrap();
            fs::write(
                root.join("Model.kt"),
                model.replace(
                    ":Root {fun getId",
                    ":Root {suspend fun remainKotlin() {}\nfun getId",
                ),
            )
            .unwrap();
        }
        if keep_chain {
            let model = fs::read_to_string(root.join("Model.kt")).unwrap();
            fs::write(
                root.join("Model.kt"),
                model.replace(
                    "open class Child(id:Wide):Base(id) {",
                    "open class Child(id:Wide):Base(id) {suspend fun remainKotlinChild() {}\n",
                ),
            )
            .unwrap();
            fs::write(root.join("Leaf.kt"),"package sample\nclass Leaf(id:Wide):Child(id),Marker {suspend fun remainKotlinLeaf(){}}\n").unwrap();
        }
        let contracts = fs::read_to_string(root.join("Root.kt")).unwrap();
        fs::write(root.join("Root.kt"), contracts.replace("interface ReadRoot {val id:Wide}", "interface ReadRoot {val id:Wide;fun stored():String=id.value;fun isSame(other:ReadRoot):Boolean=id===other.id}")).unwrap();
        fs::write(
            root.join("Sibling.kt"),
            "package sample\ninterface Marker:ReadRoot\nclass Sibling(id:Wide):Base(id),Marker\n",
        )
        .unwrap();
        fs::write(
            root.join("Saved.kt"),
            "package sample\ndata class Saved constructor(@Id override val id:Wide):ReadRoot\n",
        )
        .unwrap();
        fs::write(root.join("Id.java"), "package sample;@java.lang.annotation.Target({java.lang.annotation.ElementType.FIELD,java.lang.annotation.ElementType.PARAMETER}) public @interface Id {}\n").unwrap();
        let source = fs::read_to_string(root.join("Probe.java")).unwrap();
        fs::write(root.join("Probe.java"), source.replace("public class Probe {", "public class Probe { public static String property(Base base){return base.getId().getValue();} public static String independent(Child child){return child.getId().getValue();}")).unwrap();
        let runner = fs::read_to_string(root.join("Runner.kt")).unwrap().replace(
            " println(base.id.value",
            " println(read.stored()+\"|\"+read.isSame(read))\n println(Sibling(Wide(\"sibling\")).stored())\n val leaf=Leaf(Wide(\"leaf\"));val leafRoot:ReadRoot=leaf;println(leafRoot.id.value+\"|\"+leaf.getId().value+\"|\"+leaf.calls)\n println(base.id.value",
        );
        fs::write(
            root.join("Runner.kt"),
            runner.replace(
                " val typed=",
                " println(Probe.property(base)+\"|\"+Probe.independent(child))\n println(Saved(Wide(\"saved\")).id.value)\n val typed=",
            ),
        )
        .unwrap();
        let expected = native_into(&root, true, false, "original-behavior");
        fs::write(
            root.join("validation.json"),
            serde_json::to_vec_pretty(&workspace_config(&root)).unwrap(),
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
        command.current_dir(&root).args([
            "--root",
            ".",
            "--in-place",
            "--jvm-facts",
            "off",
            "--annotations",
            "none",
            "--validation-config",
            "validation.json",
            "--coverage-report",
            "coverage.json",
        ]);
        if lombok {
            command.arg("--lombok");
        }
        let result = command.arg(".").output().unwrap();
        fs::write(root.join("workspace-translation.log"), &result.stderr).unwrap();
        assert!(
            result.status.success(),
            "{root:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let existing = fs::read_to_string(root.join("Existing.java")).unwrap();
        assert!(existing.contains("readId()"), "{existing}");
        let probe = fs::read_to_string(root.join("Probe.java")).unwrap();
        assert!(probe.contains("base.readId().getValue()"), "{probe}");
        assert!(probe.contains("child.getId().getValue()"), "{probe}");
        if keep_chain {
            assert!(
                root.join("Leaf.kt").is_file() && !root.join("Child.java").is_file(),
                "retained covariant chain was not exercised: {root:?}"
            );
        } else {
            assert!(
                root.join("Leaf.java").is_file(),
                "descendant retained: {root:?}"
            );
        }
        assert!(
            root.join("ReadRoot.java").is_file(),
            "property-owning interface retained: {root:?}"
        );
        assert!(
            root.join("Root.java").is_file(),
            "mutable property interface retained: {root:?}"
        );
        assert!(
            root.join("Sibling.java").is_file(),
            "inherited compatible class getter retained: {root:?}"
        );
        let coverage: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("coverage.json")).unwrap()).unwrap();
        assert_eq!(coverage["original"], 12);
        let actual = native_into(&root, true, lombok, "workspace-result");
        assert_eq!(actual, expected, "{root:?}");
    }
}

#[test]
fn unrelated_return_types_do_not_create_an_invalid_java_bean_override() {
    let root = fixture("unrelated-return");
    let source = fs::read_to_string(root.join("Model.kt")).unwrap();
    fs::write(
        root.join("Model.kt"),
        source.replace(
            "fun getId():Narrow {calls++;return Narrow(id.value+\"/lookup\")}",
            "fun getId():String {calls++;return id.value+\"/lookup\"}",
        ),
    )
    .unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let families = discover(&index);
    let family = families
        .iter()
        .find(|family| family.root.name == "ReadRoot")
        .unwrap();
    let error = plan_layout(&index, family).unwrap_err();
    assert!(error.contains("compatible bean return"), "{error}");
    assert!(
        !fs::read_to_string(root.join("Root.kt"))
            .unwrap()
            .contains("readId")
    );
}

#[test]
fn an_inherited_java_getter_is_not_assigned_a_role_from_an_empty_source_header() {
    let root = fixture("inherited-java-getter");
    fs::write(root.join("Existing.java"), "package sample;public class Existing extends Child {public Existing(Wide id){super(id);}}\n").unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let families = discover(&index);
    let family = families
        .iter()
        .find(|family| family.root.name == "ReadRoot")
        .unwrap();
    let error = plan_layout(&index, family).unwrap_err();
    assert!(error.contains("compiled declaration facts"), "{error}");
    assert!(
        !fs::read_to_string(root.join("Root.kt"))
            .unwrap()
            .contains("readId")
    );
}

#[test]
#[ignore = "requires real Kotlin/JVM tools, all-open, Lombok and Jackson in NOTLIN_* variables"]
fn strict_mode_cannot_apply_half_of_a_prepared_getter_family() {
    let root = fixture("workspace-strict");
    let model = fs::read_to_string(root.join("Model.kt")).unwrap();
    assert!(model.contains(":Root {fun getId"));
    fs::write(
        root.join("Model.kt"),
        model.replace(
            ":Root {fun getId",
            ":Root {suspend fun keep() {}\nfun getId",
        ),
    )
    .unwrap();
    let config = workspace_config(&root);
    fs::write(
        root.join("validation.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let originals = index
        .kotlin_files()
        .chain(index.java_files())
        .map(|file| (file.path.clone(), file.source_text().to_owned()))
        .collect::<Vec<_>>();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root)
        .args([
            "--root",
            ".",
            "--in-place",
            "--jvm-facts",
            "off",
            "--annotations",
            "none",
            "--validation-config",
            "validation.json",
            "--untranslatable",
            "error",
            ".",
        ])
        .output()
        .unwrap();
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    fs::write(root.join("strict-translation.log"), diagnostics.as_bytes()).unwrap();
    assert!(!output.status.success(), "{diagnostics}");
    assert!(
        diagnostics.contains("strict-mode source veto"),
        "{diagnostics}"
    );
    for (path, source) in originals {
        assert_eq!(fs::read_to_string(path).unwrap(), source);
    }
    assert!(!root.join("Root.java").exists());
    assert!(!root.join("Leaf.java").exists());
}

#[test]
#[ignore = "requires real Kotlin/JVM tools, all-open, Lombok and Jackson in NOTLIN_* variables"]
fn configuration_support_sources_are_bound_and_snapshot_checked_without_external_edits() {
    use clap::Parser;
    for related in [false, true] {
        let root = fixture(&format!("workspace-support-{related}"));
        let extra_dir = root.with_file_name(format!(
            "{}-context",
            root.file_name().unwrap().to_string_lossy()
        ));
        fs::create_dir(&extra_dir).unwrap();
        let extra = extra_dir.join("Context.java");
        fs::write(&extra, if related { "package sample;public class Context {public static Wide read(Base base){return base.getId();}}" } else { "package sample;public class Context {public static String getId(){return \"context\";}}" }).unwrap();
        let mut config = workspace_config(&root);
        config["java_sources"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(&extra).unwrap());
        let config_path = root.join("validation.json");
        fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        let cli = notlin::cli::Cli::parse_from([
            "notlin",
            "--root",
            root.to_str().unwrap(),
            "--in-place",
            "--jvm-facts",
            "off",
            "--annotations",
            "none",
            "--validation-config",
            config_path.to_str().unwrap(),
            root.to_str().unwrap(),
        ]);
        let sources = index
            .kotlin_files()
            .map(|file| (file.path.clone(), file.source_text().to_owned()))
            .collect();
        let plan = notlin::migration_pipeline::plan_workspace_migration(
            sources,
            &cli,
            &index,
            std::slice::from_ref(&root),
            |_| {},
        )
        .unwrap();
        assert!(
            plan.supporting_java
                .keys()
                .all(|path| !path.starts_with(&extra_dir))
        );
        if related {
            assert!(
                plan.java_bindings.is_empty(),
                "external property caller must retain the original family"
            );
            assert!(plan.supporting_java.is_empty());
        } else {
            assert!(!plan.java_bindings.is_empty());
            notlin::migration_pipeline::verify_original_snapshots(&plan).unwrap();
            fs::write(&extra, "package sample;public class Context {public static String getId(){return \"changed\";}}").unwrap();
            let error = notlin::migration_pipeline::verify_original_snapshots(&plan).unwrap_err();
            assert!(
                error.contains("inputs changed") || error.contains("source changed"),
                "{error}"
            );
        }
        assert!(
            root.join("Root.kt").exists(),
            "planning must not write Kotlin"
        );
        assert!(!root.join("Root.java").exists());
    }
}

fn convert(root: &Path, lombok: bool, inputs: &[&str]) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
    command.current_dir(root).args([
        "--root",
        ".",
        "--in-place",
        "--jvm-facts",
        "off",
        "--annotations",
        "none",
    ]);
    if lombok {
        command.arg("--lombok");
    }
    let output = command.args(inputs).output().unwrap();
    fs::write(root.join("translation.stdout"), &output.stdout).unwrap();
    fs::write(root.join("translation.stderr"), &output.stderr).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for input in inputs {
        if Path::new(input)
            .extension()
            .is_none_or(|extension| extension != "kt")
        {
            continue;
        }
        assert!(
            root.join(input).with_extension("java").exists(),
            "{input} retained: {root:?}"
        );
    }
}

#[test]
#[ignore = "requires real Kotlin/JVM tools, all-open, Lombok and Jackson in NOTLIN_* variables"]
fn an_open_bean_family_preserves_the_property_beside_retained_kotlin_methods() {
    for lombok in [false, true] {
        let root = empty_fixture(&format!("open-bean-family-{lombok}"));
        for (file, source) in [
            (
                "Enabled.kt",
                "package sample\ninterface IEnabledAware {val enabled:Any}\n",
            ),
            (
                "Middle.kt",
                "package sample\nopen class Middle(override val enabled:Any):IEnabledAware\n",
            ),
            (
                "Leaf.kt",
                "package sample\nopen class Leaf(enabled:Any):Middle(enabled){open fun getEnabled():String=enabled.toString();companion object Named}\n",
            ),
            (
                "Runner.kt",
                "package sample\nfun main(){val stored=StringBuilder(\"stored\");val leaf=Leaf(stored);val root:IEnabledAware=leaf;val middle:Middle=leaf;println(\"${root.enabled===stored}|${leaf.getEnabled()}|${middle.enabled===stored}\")}\n",
            ),
        ] {
            fs::write(root.join(file), source).unwrap();
        }
        let expected = native_into(&root, false, false, "original-open-bean");
        assert_eq!(expected.trim(), "true|stored|true");
        convert(&root, lombok, &["."]);
        assert!(root.join("IEnabledAware.java").is_file(), "{root:?}");
        assert!(
            root.join("Middle.kt").is_file(),
            "base must exercise retained Kotlin: {root:?}"
        );
        assert_eq!(
            native_into(&root, false, lombok, "translated-open-bean"),
            expected,
            "{root:?}"
        );
    }
}

#[test]
#[ignore = "requires real Kotlin/JVM tools, all-open, Lombok and Jackson in NOTLIN_* variables"]
fn bounded_property_getter_calls_preserve_a_generic_forwarding_interface() {
    for lombok in [false, true] {
        let root = fixture(&format!("bounded-property-getter-{lombok}"));
        fs::write(root.join("Contracts.kt"), "package sample\ninterface KeyType\nclass Key(val text:String):KeyType\ninterface HasKey<I:KeyType>{fun getKey():I}\ninterface Lookup<I:KeyType>:HasKey<I>\nclass Value(val stored:Key):Lookup<Key>{override fun getKey():Key=stored}\n").unwrap();
        fs::write(root.join("Forwarder.kt"), "package sample\ninterface Forwarder<T:Lookup<I>,I:KeyType>:Lookup<I>{val data:T;override fun getKey():I=data.getKey()}\n").unwrap();
        fs::write(
            root.join("ForwardValue.kt"),
            "package sample\nclass ForwardValue(override val data:Value):Forwarder<Value,Key>\n",
        )
        .unwrap();
        fs::write(
            root.join("Snapshot.kt"),
            "package sample\ndata class Snapshot(val value:Value)\n",
        )
        .unwrap();
        let runner = fs::read_to_string(root.join("Runner.kt")).unwrap();
        fs::write(
            root.join("Runner.kt"),
            runner.replace(
                " val typed=",
                " println(ForwardValue(Snapshot(Value(Key(\"forwarded\"))).value).getKey().text)\n val typed=",
            ),
        )
        .unwrap();
        let expected = native_into(&root, true, false, "original-forwarder");
        convert(
            &root,
            lombok,
            &["Forwarder.kt", "ForwardValue.kt", "Snapshot.kt"],
        );
        if lombok {
            let snapshot = fs::read_to_string(root.join("Snapshot.java")).unwrap();
            assert!(snapshot.contains("@lombok.Value"), "{snapshot}");
            assert!(!snapshot.contains("import lombok.Value;"), "{snapshot}");
        }
        assert_eq!(
            native_into(&root, true, lombok, "translated-forwarder"),
            expected,
            "{root:?}"
        );
    }
}

#[test]
#[ignore = "requires real Kotlin/JVM tools, all-open, Lombok and Jackson in NOTLIN_* variables"]
fn interface_conversion_preserves_generic_and_collection_property_reads() {
    for all_open in [false, true] {
        for mode in ["original", "plain", "lombok"] {
            let root = fixture(&format!("root-callers-{mode}-{all_open}"));
            let probe = fs::read_to_string(root.join("Probe.java")).unwrap();
            fs::write(root.join("Probe.java"), probe.replace("public class Probe {", r#"public class Probe {
 private Base stored;
 Base getStored(){return stored;}
 static <T extends ReadRoot> String bounded(T value){return value.getId().getValue();}
 static <T> T typed(Class<T> type,T value){return type.cast(value);}
 interface Getter<T,R> {R apply(T value);}
 interface Append {String apply(Base value,String suffix);}
 public static String calls(Child child) {
  Base base=child;ReadRoot root=child;Probe probe=new Probe();probe.stored=child;
  var made=new Base(new Wide("made"));
  Getter<Base,Wide> wide=Base::getId;
  java.util.function.Supplier<Wide> bound=base::getId;
  java.util.function.Function<Child,Narrow> narrow=Child::getId;
  Append overload=Base::getId;
  Object candidate=child;String pattern;
  if(candidate instanceof Base found)pattern=found.getId().getValue();else throw new AssertionError();
  String mapped=java.util.List.<Base>of(child).stream().map(value->value.getId().getValue()).findFirst().orElseThrow();
  return base.getId().getValue()+"|"+root.getId().getValue()+"|"+bounded(base)+"|"+made.getId().getValue()
   +"|"+probe.getStored().getId().getValue()+"|"+wide.apply(base).getValue()+"|"+bound.get().getValue()
   +"|"+child.getId().getValue()+"|"+narrow.apply(child).getValue()+"|"+overload.apply(base,"!")
   +"|"+typed(Base.class,child).getId().getValue()+"|"+pattern+"|"+mapped;
 }
"#)).unwrap();
            let runner = fs::read_to_string(root.join("Runner.kt")).unwrap();
            let runner = runner.replace(
                " val existing:ReadRoot=",
                " println(generic(child)+\"|\"+withWhere(child)+\"|\"+Holder(child).value()+\"|\"+mapped(listOf(child)))\n println(Probe.calls(child))\n val existing:ReadRoot=",
            );
            fs::write(
                root.join("Runner.kt"),
                runner
                    + r#"
fun <T:ReadRoot> generic(value:T):String=value.id.value
fun <T> withWhere(value:T):String where T:ReadRoot=value.id.value
class Holder<T:ReadRoot>(val current:T) {fun value():String=current.id.value}
fun <T:ReadRoot> mapped(values:List<T>):String=values.map {it.id.value}.joinToString(",")
"#,
            )
            .unwrap();
            if mode != "original" {
                normalize_attributed(&root, all_open);
                convert(&root, mode == "lombok", &["."]);
                assert!(root.join("Root.java").exists(), "root retained: {root:?}");
                assert!(root.join("Leaf.java").exists(), "leaf retained: {root:?}");
            }
            assert_eq!(
                native(&root, all_open, mode == "lombok"),
                "stored|stored|stored|stored/lookup\nset|set\nset/lookup\nset/lookup\n3\nset|set|set|set\nset|set|set|made|set|set|set|set/lookup|set/lookup|set!|set|set|set\njava\ntyped|Narrow\n",
                "{mode},all-open={all_open},root={root:?}"
            );
        }
    }
}

#[test]
#[ignore = "requires real Kotlin/JVM tools, Lombok and Jackson in NOTLIN_* variables"]
fn where_bounds_and_increment_statements_preserve_values_and_accessor_effects() {
    for mode in ["original", "plain", "lombok"] {
        let root = fixture(&format!("bounds-increments-{mode}"));
        fs::write(
            root.join("Counter.kt"),
            r#"package sample
open class BaseCode(val text:String)
interface Tag
class Code(text:String):BaseCode(text),Tag
class BoundBox<T>(val current:T) where T:Tag,T:BaseCode {fun label():String=current.text}
class Counter {
 var reads:Int=0
 var writes:Int=0
 private var stored:Int=0
 var value:Int
   get() {reads++;return stored}
   set(newValue) {writes++;stored=newValue}
 var tiny:Byte=127
 var lastRecorded:Int=0
 var delegated:Int=0
   set(incoming)=record(incoming)
 private fun record(incoming:Int) {lastRecorded=incoming}
 fun advance() {value++;++value;tiny++}
 fun locals():Int {var value=3;val before=value++;val after=++value;return before*100+after}
}
"#,
        )
        .unwrap();
        let runner = fs::read_to_string(root.join("Runner.kt")).unwrap();
        fs::write(root.join("Runner.kt"), runner.replace(" val existing:ReadRoot=", r#" val counter=Counter();counter.advance();counter.delegated=7
 println("${counter.reads}|${counter.writes}|${counter.value}|${counter.tiny}|${counter.locals()}|${BoundBox(Code("proof")).label()}|${counter.lastRecorded}|${counter.delegated}")
 val existing:ReadRoot="#)).unwrap();
        if mode != "original" {
            convert(&root, mode == "lombok", &["Counter.kt"]);
            let java = fs::read_to_string(root.join("BoundBox.java")).unwrap();
            assert!(java.contains("T extends BaseCode & Tag"), "{java}");
        }
        assert_eq!(
            native(&root, false, mode == "lombok"),
            "stored|stored|stored|stored/lookup\nset|set\nset/lookup\nset/lookup\n3\n3|2|2|-128|305|proof|7|0\njava\ntyped|Narrow\n",
            "{mode},root={root:?}"
        );
    }
}

#[test]
#[ignore = "requires real Kotlin/JVM tools, all-open, Lombok and Jackson in NOTLIN_* variables"]
fn automatic_layout_preserves_property_and_bean_views_with_a_real_java_leaf() {
    for all_open in [false, true] {
        for (mode, normalized, converted, lombok) in [
            ("original", false, false, false),
            ("layout", true, false, false),
            ("plain-leaf", true, true, false),
            ("lombok-leaf", true, true, true),
        ] {
            let root = fixture(&format!("{mode}-{all_open}"));
            if normalized {
                normalize(&root);
            }
            if converted {
                let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
                command.current_dir(&root).args([
                    "--root",
                    ".",
                    "--in-place",
                    "--jvm-facts",
                    "off",
                    "--annotations",
                    "none",
                ]);
                if lombok {
                    command.arg("--lombok");
                }
                let output = command.arg("Leaf.kt").output().unwrap();
                fs::write(root.join("translation.stdout"), &output.stdout).unwrap();
                fs::write(root.join("translation.stderr"), &output.stderr).unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(root.join("Leaf.java").exists(), "leaf retained: {root:?}");
            }
            assert_eq!(
                native(&root, all_open, lombok),
                "stored|stored|stored|stored/lookup\nset|set\nset/lookup\nset/lookup\n3\njava\ntyped|Narrow\n",
                "{mode},all-open={all_open}"
            );
        }
    }
}
