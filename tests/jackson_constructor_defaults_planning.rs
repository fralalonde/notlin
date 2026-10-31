use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider};
use notlin::translation_plan::PreparationBlockerKind;
use std::path::{Path, PathBuf};

fn provider(files: &[(&str, &str)]) -> SyntaxSemanticProvider {
    SyntaxSemanticProvider::new(
        files
            .iter()
            .map(|(path, source)| (PathBuf::from(path), (*source).to_owned()))
            .collect::<Vec<_>>(),
    )
}

fn assert_retained(provider: &SyntaxSemanticProvider, file: &str, class_name: &str) {
    let symbol = provider
        .symbols_in_file(Path::new(file))
        .into_iter()
        .find(|symbol| symbol.id.name == class_name && symbol.id.kind == "class")
        .unwrap_or_else(|| panic!("missing {class_name} symbol in {file}"));
    assert!(provider.has_jackson_default_constructor_semantics(&symbol.id));

    let source = provider.source_for_file(Path::new(file)).unwrap();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), Path::new(file));
    plan.plan_jackson_default_constructor_losses(Path::new(file), provider, false);
    assert!(
        plan.declarations.iter().any(|decision| {
            decision.candidate_owner == notlin::translation_plan::BackendOwner::Kotlin
                && decision.retention_reasons.iter().any(|reason| {
                    matches!(
                        reason,
                        notlin::translation_plan::RetentionReason::PreparationBlocker {
                            kind: PreparationBlockerKind::SemanticLoss,
                            ..
                        }
                    )
                })
        }),
        "plan did not retain {class_name}: {:#?}",
        plan.declarations
    );
}

#[test]
fn direct_and_aliased_jackson_annotations_make_defaults_observable() {
    let source = r#"
import com.fasterxml.jackson.annotation.JsonTypeName as JsonKind
@JsonKind("record")
data class Aliased(val id: Int = 1)
"#;
    let index = provider(&[("Aliased.kt", source)]);
    assert_retained(&index, "Aliased.kt", "Aliased");
}

#[test]
fn inherited_kotlin_and_java_class_contracts_make_defaults_observable() {
    let parent_kt = r#"
import com.fasterxml.jackson.annotation.JsonTypeInfo
@JsonTypeInfo(use = JsonTypeInfo.Id.NAME)
interface KContract
"#;
    let child_kt = "data class KotlinDto(val id: Int = 1) : KContract\n";
    let parent_java = r#"
package sample;
import com.fasterxml.jackson.annotation.JsonTypeInfo;
@JsonTypeInfo(use = JsonTypeInfo.Id.NAME)
public interface JContract {}
"#;
    let child_java_contract_kt =
        "package sample\ndata class JavaDto(val id: Int = 1) : JContract\n";
    let index = provider(&[
        ("KContract.kt", parent_kt),
        ("KotlinDto.kt", child_kt),
        ("JContract.java", parent_java),
        ("JavaDto.kt", child_java_contract_kt),
    ]);
    assert_retained(&index, "KotlinDto.kt", "KotlinDto");
    assert_retained(&index, "JavaDto.kt", "JavaDto");
}

#[test]
fn jackson_constructor_parameter_annotation_is_inherited_as_contract_evidence() {
    let parent = r#"
import com.fasterxml.jackson.annotation.JsonProperty
open class Base(@param:JsonProperty("id") val id: String)
"#;
    let child = "data class Dto(val name: String = \"x\") : Base(\"id\")\n";
    let index = provider(&[("Base.kt", parent), ("Dto.kt", child)]);
    assert_retained(&index, "Dto.kt", "Dto");
}

#[test]
fn wildcard_jackson_annotation_is_recognized_but_custom_same_name_is_not() {
    let wildcard = r#"
import com.fasterxml.jackson.annotation.*
@JsonTypeName("record")
data class WildcardDto(val id: Int = 1)
"#;
    let custom = r#"
annotation class JsonTypeName(val value: String)
@JsonTypeName("record")
data class CustomDto(val id: Int = 1)
"#;
    let unrelated = "data class PlainDto(val id: Int = 1)\n";
    let index = provider(&[
        ("WildcardDto.kt", wildcard),
        ("CustomDto.kt", custom),
        ("PlainDto.kt", unrelated),
    ]);
    assert_retained(&index, "WildcardDto.kt", "WildcardDto");
    for file in ["CustomDto.kt", "PlainDto.kt"] {
        let symbol = index
            .symbols_in_file(Path::new(file))
            .into_iter()
            .find(|symbol| symbol.id.kind == "class")
            .unwrap();
        assert!(!index.has_jackson_default_constructor_semantics(&symbol.id));
    }
}

#[test]
fn java_getter_annotations_survive_the_mixed_language_boundary() {
    let contract = "package sample; import com.fasterxml.jackson.annotation.JsonProperty; public interface Contract { @JsonProperty(\"id\") String getId(); }";
    let child = "package sample\nclass Mixed(val label: String = \"default\") : Contract { override fun getId(): String = label }\n";
    let index = provider(&[("Contract.java", contract), ("Mixed.kt", child)]);
    assert_retained(&index, "Mixed.kt", "Mixed");
}

#[test]
fn nested_defaulted_dto_keeps_its_enclosing_class_in_kotlin() {
    let source = "class Outer {\n @com.fasterxml.jackson.annotation.JsonTypeName(\"nested\")\n data class Nested(val id: Int = 1)\n}\n";
    let index = provider(&[("Outer.kt", source)]);
    assert_retained(&index, "Outer.kt", "Nested");
}

#[test]
fn comments_strings_and_unrelated_local_annotations_do_not_mark_the_outer_class() {
    let source = r#"
class Plain(val id: Int = 1) {
    // @com.fasterxml.jackson.annotation.JsonTypeName("comment")
    val text = "@com.fasterxml.jackson.annotation.JsonTypeName"
    fun unused() {
        @com.fasterxml.jackson.annotation.JsonTypeName("local")
        class Local
    }
}
"#;
    let index = provider(&[("Plain.kt", source)]);
    let symbol = index.symbols_named("Plain").into_iter().next().unwrap();
    assert!(!index.has_jackson_default_constructor_semantics(&symbol.id));
}

#[test]
fn explicit_approximation_mode_reports_the_loss_without_forcing_retention() {
    let source = "@com.fasterxml.jackson.annotation.JsonTypeName(\"dto\")\ndata class Dto(val id: Int = 1)\n";
    let index = provider(&[("Dto.kt", source)]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), Path::new("Dto.kt"));
    plan.plan_jackson_default_constructor_losses(Path::new("Dto.kt"), &index, true);
    assert!(
        plan.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "A001"
                && diagnostic.message.contains("Jackson")
                && diagnostic.message.contains("explicit null"))
    );
    assert!(
        plan.declarations
            .iter()
            .all(|decision| decision.retention_reasons.is_empty())
    );
}

#[test]
fn simple_default_factories_resolve_enums_and_preserve_custom_binding_boundaries() {
    let source = r#"
package sample
import com.fasterxml.jackson.annotation.JsonCreator
import com.fasterxml.jackson.annotation.JsonTypeName
import com.fasterxml.jackson.databind.annotation.JsonDeserialize as Deserialize
enum class Kind { DEFAULT, OTHER }
@JsonTypeName("simple")
data class Simple(val kind: Kind = Kind.DEFAULT, val enabled: Boolean = true, val note: String? = null)
@JsonTypeName("nullable")
data class Nullable(val note: String? = "fallback")
@JsonTypeName("generic")
data class Generic<T>(val value: T? = null)
@Deserialize(using = Custom::class)
interface CustomBound
data class Inherited(val enabled: Boolean = true) : CustomBound
@JsonTypeName("custom")
data class CustomCreator @JsonCreator constructor(val enabled: Boolean = true)
"#;
    let index = provider(&[("Defaults.kt", source)]);
    for (name, supported) in [
        ("Simple", true),
        ("Nullable", true),
        ("Generic", false),
        ("Inherited", false),
        ("CustomCreator", false),
    ] {
        let owner = &index.symbols_named(name)[0].id;
        assert!(
            index.has_jackson_default_constructor_semantics(owner),
            "{name}"
        );
        assert_eq!(
            index.has_jackson_default_factory(owner),
            supported,
            "{name}"
        );
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan =
        notlin::translation_plan::analyze(source, tree.root_node(), Path::new("Defaults.kt"));
    plan.plan_jackson_default_constructor_losses(Path::new("Defaults.kt"), &index, false);
    let simple = plan
        .declarations
        .iter()
        .find(|decision| decision.symbol_id.name == "Simple")
        .unwrap();
    assert!(simple.retention_reasons.is_empty(), "{simple:?}");
}

#[test]
fn inherited_ignore_unknown_uses_the_annotation_binding_not_its_spelling() {
    let contract = "package sample; import com.fasterxml.jackson.annotation.*; import java.util.*; @JsonIgnoreProperties(ignoreUnknown = true) public interface Contract {}";
    let fake =
        "package domain; public @interface JsonIgnoreProperties { boolean ignoreUnknown(); }";
    let custom = "package sample; import com.fasterxml.jackson.annotation.*; import domain.JsonIgnoreProperties; @JsonIgnoreProperties(ignoreUnknown = true) public interface Custom {}";
    let source = "package sample\nimport com.fasterxml.jackson.annotation.JsonTypeName\nimport com.fasterxml.jackson.annotation.JsonCreator\nimport com.fasterxml.jackson.databind.annotation.JsonDeserialize\n@JsonTypeName(\"real\") data class Real(val enabled: Boolean = true) : Contract\n@JsonTypeName(\"custom\") data class Other(val enabled: Boolean = true) : Custom\n";
    let index = provider(&[
        ("Contract.java", contract),
        ("Fake.java", fake),
        ("Custom.java", custom),
        ("Dtos.kt", source),
    ]);
    let real = &index.symbols_named("Real")[0].id;
    let other = &index.symbols_named("Other")[0].id;
    assert!(index.jackson_ignores_unknown_properties(real));
    assert!(!index.jackson_ignores_unknown_properties(other));
}
