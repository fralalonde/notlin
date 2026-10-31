use notlin::semantics::{FactStatus, SemanticProvider, SyntaxSemanticProvider};
use std::path::PathBuf;

fn provider(files: &[(&str, &str)]) -> SyntaxSemanticProvider {
    SyntaxSemanticProvider::new(
        files
            .iter()
            .map(|(path, source)| (PathBuf::from(path), source.to_string()))
            .collect::<Vec<_>>(),
    )
}

#[test]
fn serialization_metadata_preserves_java_field_absence_and_declared_getters() {
    let source = r#"package sample;
import com.fasterxml.jackson.annotation.JsonTypeInfo;
import com.fasterxml.jackson.annotation.JsonIgnore;
@JsonTypeInfo(use = JsonTypeInfo.Id.NAME)
public interface Parent {
    @JsonIgnore int getCount();
}

"#;
    let index = provider(&[("Parent.java", source)]);
    let parent = index.symbols_named("Parent")[0].id.clone();
    assert!(matches!(
        index.java_field_named(&parent, "TODO"),
        FactStatus::Established(false)
    ));
    assert!(index.java_property_read(&parent, "count").is_some());
}

#[test]
fn metadata_annotated_java_ancestor_does_not_hide_stdlib_todo() {
    let child = "package sample\nclass Child : Parent {\n fun missing(): String = TODO()\n}\n";
    let parent = "package sample;\nimport com.fasterxml.jackson.annotation.JsonTypeInfo;\n@JsonTypeInfo(use = JsonTypeInfo.Id.NAME)\npublic interface Parent {}\n";
    let index = provider(&[("Parent.java", parent), ("Child.kt", child)]);
    let file = PathBuf::from("Child.kt");
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(child, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(child, tree.root_node(), &file);
    notlin::semantics::plan_syntax_lowerings(child, &tree, &file, &index, &mut plan);
    assert_eq!(plan.todo_lowerings.len(), 1);
}

#[test]
fn an_inherited_annotation_type_is_not_mistaken_for_the_imported_library_type() {
    let base = "package base;\npublic interface Base { @interface JsonTypeInfo {} }\n";
    let child = "package sample;\nimport base.Base;\nimport com.fasterxml.jackson.annotation.JsonTypeInfo;\npublic interface Child extends Base { @JsonTypeInfo int getCount(); }\n";
    let index = provider(&[("Base.java", base), ("Child.java", child)]);
    let annotation = index.symbols_named("JsonTypeInfo")[0];
    assert_eq!(annotation.id.kind, "annotation");
    assert_eq!(annotation.id.owner_path, ["Base"]);
    let child = index.symbols_named("Child")[0].id.clone();
    assert!(index.java_property_read(&child, "count").is_none());
}

#[test]
fn known_metadata_stars_can_coexist_with_source_packages_without_a_shadow() {
    let source = "package sample;\nimport com.fasterxml.jackson.annotation.*;\nimport domain.*;\n@JsonTypeInfo(use = JsonTypeInfo.Id.NAME)\npublic interface Parent { @JsonIgnore int getCount(); }\n";
    let index = provider(&[
        ("Parent.java", source),
        ("Other.java", "package domain; public interface Other {}"),
    ]);
    let parent = index.symbols_named("Parent")[0].id.clone();
    assert!(matches!(
        index.java_field_named(&parent, "TODO"),
        FactStatus::Established(false)
    ));
    assert!(index.java_property_read(&parent, "count").is_some());
    for other in [
        None,
        Some("package domain; public @interface JsonTypeInfo {}"),
        Some("package domain; public @interface JsonIgnore {}"),
    ] {
        let files = if let Some(other) = other {
            vec![("Parent.java", source), ("Other.java", other)]
        } else {
            vec![("Parent.java", source)]
        };
        let index = provider(&files);
        let parent = index.symbols_named("Parent")[0].id.clone();
        assert!(matches!(
            index.java_field_named(&parent, "TODO"),
            FactStatus::Unknown
        ));
        assert!(index.java_property_read(&parent, "count").is_none());
    }
}

#[test]
fn serialization_metadata_keeps_reference_nullness_and_unknown_effects_separate() {
    use notlin::semantics::ReferenceReturnNullability::{NonNull, Nullable, Platform};
    for (annotation, expected) in [
        ("", Platform),
        ("@org.jetbrains.annotations.NotNull", NonNull),
        ("@org.jetbrains.annotations.Nullable", Nullable),
    ] {
        let source = format!(
            "package sample;\nimport com.fasterxml.jackson.annotation.JsonProperty;\npublic interface Parent {{ @JsonProperty {annotation} String getLabel(); }}"
        );
        let index = provider(&[("Parent.java", &source)]);
        let parent = &index.symbols_named("Parent")[0].id;
        let read = index
            .java_property_read(parent, "label")
            .expect("bound getter");
        assert_eq!(read.nullability, expected);
        assert_eq!(read.result_type.nullable, expected != NonNull);
    }
    for annotation in ["@GenerateAccessors", "@NullMarked", "@JsonProperty"] {
        let source = format!(
            "package sample;\nimport custom.*;\n{annotation} public interface Parent {{ int getCount(); }}"
        );
        let index = provider(&[("Parent.java", &source)]);
        let parent = &index.symbols_named("Parent")[0].id;
        assert!(index.java_property_read(parent, "count").is_none());
        assert!(matches!(
            index.java_field_named(parent, "TODO"),
            FactStatus::Unknown
        ));
    }
}
