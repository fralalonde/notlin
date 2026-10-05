//! Java-native declaration annotations pass through verbatim; annotations
//! with Kotlin-only constructs or Kotlin-declared annotation types taint the
//! declaration into Kotlin residue (conservative, N04DC).
//!
//! Java-native = the annotation's name resolves to a PRE-EXISTING Java type
//! in the workspace index (third-party annotations like a JSON mapping or
//! bean-validation constraint live on Java types), or the workspace index is
//! absent/unresolvable and the annotation carries no Kotlin-only argument.
//! Kotlin-only = `::class` references, string templates, `[]` array args,
//! lambda/trailing-lambda arguments.

use std::fs;
use std::path::Path;
use std::process::Command;

fn run(root: &Path, input: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place", "--lombok"])
        .arg(input.to_str().unwrap())
        .output()
        .expect("run notlin");
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// An annotated declaration surrounded by other types must survive an
/// in-place pass even when its annotation has a Kotlin class literal.
#[test]
fn annotated_interface_between_types_is_not_lost_on_migration() {
    let root = Path::new("tests/tmp_scratch_anno_multi");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("Types.kt"),
        r#"package neutral.mapping

interface Plain {
    val code: String
}

@TypeInfo(
    use = TypeInfo.Id.NAME,
    include = TypeInfo.As.PROPERTY,
    property = "kind")
@SubTypes(
    SubTypes.Type(value = Leaf::class),
    SubTypes.Type(value = Other::class)
)
interface Tagged {
    val code: String
    val fallback: String
        get() = "none"
}

class Leaf
class Other
class Holder(val tag: Tagged)
"#,
    )
    .unwrap();
    let stderr = run(root, &root.join("Types.kt"));
    let java = root.join("Tagged.java").exists();
    let kotlin_source = fs::read_to_string(root.join("Types.kt")).unwrap_or_default();
    let kotlin = kotlin_source.contains("interface Tagged");
    assert!(java || kotlin, "annotated interface disappeared: {stderr}");
    if kotlin {
        assert!(
            kotlin_source.contains("@TypeInfo("),
            "annotation must stay with retained interface: {kotlin_source}"
        );
    } else {
        let tagged_java = fs::read_to_string(root.join("Tagged.java")).unwrap();
        assert!(
            tagged_java.contains("@TypeInfo(") && tagged_java.contains("@SubTypes("),
            "annotation must migrate with translated interface: {tagged_java}"
        );
    }
    let leaf_java = fs::read_to_string(root.join("Leaf.java")).unwrap_or_default();
    assert!(!leaf_java.contains("@TypeInfo"), "{leaf_java}");
    assert!(!leaf_java.contains("@SubTypes"), "{leaf_java}");
    let _ = fs::remove_dir_all(root);
}

/// A single-property string annotation on a data class with no workspace
/// index (single-file probe): no Kotlin-only argument, so it passes through
/// verbatim and the class translates.
#[test]
fn java_native_annotation_passes_through_without_index() {
    let root = Path::new("tests/tmp_scratch_anno_pass");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("anno.kt"),
        "package neutral.anno\n\
         \n\
         @com.example.Mapping(\"delta\")\n\
         data class Plain(val delta: String)\n",
    )
    .unwrap();
    let stderr = run(root, &root.join("anno.kt"));
    let java = fs::read_to_string(root.join("Plain.java")).unwrap_or_default();
    assert!(
        java.contains("@com.example.Mapping(\"delta\")"),
        "Java-native annotation must pass through verbatim, got:\n{java}"
    );
    assert!(
        !stderr.contains("N04DC"),
        "no annotation retention expected:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn untranslated_kotlin_annotation_type_is_java_visible() {
    let root = Path::new("tests/tmp_scratch_anno_kt");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("mine.kt"),
        "package neutral.annokt\n\
         \n\
         annotation class Mine(val tag: String)\n",
    )
    .unwrap();
    fs::write(
        root.join("use.kt"),
        "package neutral.annokt\n\
         \n\
         @Mine(\"t\")\n\
         data class UsesMine(val gamma: String)\n",
    )
    .unwrap();
    let stderr = run(root, &root.join("use.kt"));
    let java = fs::read_to_string(root.join("UsesMine.java")).unwrap_or_default();
    let kotlin = fs::read_to_string(root.join("use.kt")).unwrap_or_default();
    assert!(java.contains("@Mine(\"t\")"), "{java}\n{kotlin}\n{stderr}");
    let _ = fs::remove_dir_all(root);
}

#[test]
fn named_class_literal_without_spaces_lowers() {
    let root = Path::new("tests/tmp_scratch_anno_named_class");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("types.kt"),
        "package neutral.named\nclass Key\n@com.example.RepositoryID(id=Key::class)\nclass Repository\n",
    )
    .unwrap();
    let stderr = run(root, &root.join("types.kt"));
    let java = fs::read_to_string(root.join("Repository.java")).unwrap_or_default();
    assert!(
        java.contains("@com.example.RepositoryID(id=Key.class)"),
        "{java}\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

/// Kotlin-only annotation arguments still taint, because Java cannot express
/// them: an unresolvable `::class` reference, a string template, and a lambda.
/// (An `[]` array argument used to be listed here — it is NOT Kotlin-only:
/// Java spells it `{a, b}` and notlin lowers it, so the class translates with
/// the annotation intact. See tests/annotation_preserved.rs.)
#[test]
fn kotlin_only_annotation_arguments_taint() {
    for (name, arg) in [
        ("kclass", "using = com.example.Hand::class"),
        ("template", "\"v = ${'$'}{value}\""),
        ("lambda", "mapper = { it.toString() }"),
    ] {
        let root = Path::new("tests/tmp_scratch_anno_arg");
        let _ = fs::remove_dir_all(root);
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join("anno.kt"),
            format!(
                "package neutral.annoarg\n\
                 \n\
                 @com.example.Mapping({arg})\n\
                 data class Arg_{name}(val value: String)\n"
            ),
        )
        .unwrap();
        let stderr = run(root, &root.join("anno.kt"));
        let java = fs::read_to_string(root.join(format!("Arg_{name}.java"))).unwrap_or_default();
        let kotlin = fs::read_to_string(root.join("anno.kt")).unwrap_or_default();
        assert!(
            java.is_empty(),
            "Kotlin-only annotation argument must taint ({name}), got:\n{java}"
        );
        assert!(
            kotlin.contains("NOTLIN N04DC: declaration annotation is retained in Kotlin")
                && kotlin.contains(&format!("data class Arg_{name}")),
            "expected retained declaration and in-source annotation blocker ({name}):\n{kotlin}\n{stderr}"
        );
        let _ = fs::remove_dir_all(root);
    }
}

/// A use-site target (`@get:JsonProperty(...)`) passes through with the
/// target stripped for class-level use: on a getter-declared property the
/// Java side annotates the getter method, which notlin does not emit for
/// primary-constructor properties — the annotation still must not taint.
#[test]
fn use_site_target_annotation_does_not_taint() {
    let root = Path::new("tests/tmp_scratch_anno_site");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("anno.kt"),
        "package neutral.annosite\n\
         \n\
         @get:com.example.Mapping(\"alpha\")\n\
         data class Dto(val alpha: String)\n",
    )
    .unwrap();
    let stderr = run(root, &root.join("anno.kt"));
    let java = fs::read_to_string(root.join("Dto.java")).unwrap_or_default();
    assert!(
        !java.is_empty(),
        "use-site annotation must not taint the declaration, got:\n{java}"
    );
    assert!(
        !stderr.contains("N04DC"),
        "no annotation retention expected:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

/// The same annotation name repeated on one declaration (Kotlin allows
/// repeated annotations) cannot be expressed in plain Java without
/// `@Repeatable` — taint the declaration conservatively.
#[test]
fn repeated_same_name_annotation_taints() {
    let root = Path::new("tests/tmp_scratch_anno_repeat");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("anno.kt"),
        "package neutral.annorepeat\n\
         \n\
         @com.example.Mapping(\"a\")\n\
         @com.example.Mapping(\"b\")\n\
         data class Twice(val value: String)\n",
    )
    .unwrap();
    let stderr = run(root, &root.join("anno.kt"));
    let java = fs::read_to_string(root.join("Twice.java")).unwrap_or_default();
    let kotlin = fs::read_to_string(root.join("anno.kt")).unwrap_or_default();
    assert!(
        java.is_empty(),
        "repeated same-name annotation must taint the declaration, got:\n{java}"
    );
    assert!(
        kotlin.contains("NOTLIN NB03C: annotation com.example.Mapping is repeated")
            && kotlin.contains("data class Twice"),
        "expected retained declaration and in-source repeated-annotation blocker:\n{kotlin}\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}

/// A Kotlin annotation declaration becomes a Java annotation interface, and
/// declarations using it can migrate in the same batch.
#[test]
fn kotlin_annotation_declaration_and_use_translate_together() {
    let root = Path::new("tests/tmp_scratch_annotation_declaration");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("tag.kt"),
        "package neutral.annotationdecl\n\
         \n\
         annotation class Tag(val value: String, val enabled: Boolean = true)\n",
    )
    .unwrap();
    let stderr = run(root, &root.join("tag.kt"));
    let tag = fs::read_to_string(root.join("Tag.java")).unwrap_or_default();
    assert!(
        tag.contains("public @interface Tag"),
        "missing Java annotation type:\n{tag}"
    );
    assert!(
        tag.contains("String value();"),
        "missing annotation element:\n{tag}"
    );
    assert!(
        tag.contains("boolean enabled() default true;"),
        "missing Java default:\n{tag}"
    );
    assert!(
        !stderr.contains("N04DC"),
        "annotation declaration must not taint:\n{stderr}"
    );
    let _ = fs::remove_dir_all(root);
}
