use notlin::semantics::{
    FactStatus, JavaPropertyRead, KotlinType, ReferenceReturnNullability, SemanticProvider,
    SymbolId, SyntaxSemanticProvider,
};
use std::path::PathBuf;

fn indexed(files: &[(&str, &str)]) -> SyntaxSemanticProvider {
    for (path, source) in files {
        let mut parser = tree_sitter::Parser::new();
        let language = if path.ends_with(".java") {
            tree_sitter_java::LANGUAGE.into()
        } else {
            tree_sitter_kotlin_ng::LANGUAGE.into()
        };
        parser
            .set_language(&language)
            .expect("fixture grammar loads");
        let tree = parser.parse(source, None).expect("fixture parses");
        assert!(
            !tree.root_node().has_error(),
            "fixture {path} contains a syntax error:\n{source}"
        );
    }
    SyntaxSemanticProvider::new(
        files
            .iter()
            .map(|(path, source)| (PathBuf::from(path), (*source).to_owned())),
    )
}

fn owner(provider: &SyntaxSemanticProvider) -> SymbolId {
    let owners = provider
        .symbols_named("Box")
        .into_iter()
        .filter(|symbol| symbol.id.kind == "class")
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(owners.len(), 1, "fixture has one Box class");
    owners[0].clone()
}

fn read(source: &str, property: &str) -> Option<JavaPropertyRead> {
    let provider = indexed(&[("Box.java", source)]);
    provider.java_property_read(&owner(&provider), property)
}

#[test]
fn public_reference_getters_preserve_platform_nullable_and_notnull_facts() {
    use ReferenceReturnNullability::{NonNull, Nullable, Platform};
    for (annotation, expected) in [
        ("", Platform),
        ("@org.jetbrains.annotations.Nullable", Nullable),
        ("@org.jetbrains.annotations.NotNull", NonNull),
    ] {
        let source = format!(
            "package sample;\npublic class Box {{ {annotation} public String getLabel() {{ return null; }} }}"
        );
        let fact = read(&source, "label").expect("public getter should be readable");
        assert_eq!(fact.nullability, expected, "{annotation}");
        assert_eq!(fact.result_type.nullable, expected != NonNull);
        assert!(!fact.jvm_primitive);
        assert_eq!(fact.target.name, "getLabel");
        assert_eq!(fact.location.file, PathBuf::from("Box.java"));
    }
}

#[test]
fn primitive_and_boxed_double_keep_distinct_jvm_contracts() {
    let primitive = read(
        "package sample; public class Box { public double getValue() { return 1.0; } }",
        "value",
    )
    .expect("primitive getter");
    assert!(primitive.jvm_primitive);
    assert_eq!(primitive.nullability, ReferenceReturnNullability::NonNull);
    assert_eq!(
        primitive.result_type.ty,
        KotlinType::Primitive("Double".into())
    );
    assert!(!primitive.result_type.nullable);

    for return_type in ["Double", "java.lang.Double"] {
        let source = format!(
            "package sample; public class Box {{ public {return_type} getValue() {{ return 1.0; }} }}"
        );
        let boxed = read(&source, "value").expect("boxed getter");
        assert!(!boxed.jvm_primitive, "{return_type}");
        assert_eq!(boxed.nullability, ReferenceReturnNullability::Platform);
        assert!(boxed.result_type.nullable, "{return_type}");
    }
}

#[test]
fn java_lang_double_import_is_accepted_but_type_aliases_and_shadows_are_not() {
    let imported = read(
        "package sample; import java.lang.Double; public class Box { public Double getValue() { return 1.0; } }",
        "value",
    );
    assert!(
        imported.is_some(),
        "the canonical explicit import is unambiguous"
    );

    let shadowed_type = indexed(&[
        (
            "Box.java",
            "package sample; public class Box { public Double getValue() { return null; } }",
        ),
        ("Double.java", "package sample; class Double {}"),
    ]);
    assert!(
        shadowed_type
            .java_property_read(&owner(&shadowed_type), "value")
            .is_none(),
        "a same-package Double declaration blocks implicit java.lang binding"
    );

    let imported_shadow = indexed(&[
        (
            "Box.java",
            "package sample; import other.Double; public class Box { public Double getValue() { return null; } }",
        ),
        ("Double.java", "package other; public class Double {}"),
    ]);
    assert!(
        imported_shadow
            .java_property_read(&owner(&imported_shadow), "value")
            .is_none(),
        "an explicit competing Double import is not java.lang.Double"
    );

    let aliased = indexed(&[
        (
            "Box.java",
            "package sample; public class Box { public Double getValue() { return null; } }",
        ),
        (
            "Alias.kt",
            "package sample\ntypealias Double = other.CustomDouble\n",
        ),
        ("CustomDouble.kt", "package other\nclass CustomDouble\n"),
    ]);
    assert!(
        aliased
            .java_property_read(&owner(&aliased), "value")
            .is_none(),
        "an indexed typealias collision must not authorize boxed-name binding"
    );
}

#[test]
fn generic_owner_or_method_return_contracts_are_rejected() {
    for source in [
        "package sample; public class Box<T> { public String getLabel() { return null; } }",
        "package sample; public class Box { public <T> T getLabel() { return null; } }",
    ] {
        assert!(read(source, "label").is_none(), "{source}");
    }
}

#[test]
fn unsafe_direct_getters_are_rejected_and_inherited_getters_keep_declaring_snapshots() {
    for source in [
        "package sample; public class Box { public String getLabel() throws java.io.IOException { return null; } }",
        "package sample; public class Box { public static String getLabel() { return null; } }",
        "package sample; public class Box { private String getLabel() { return null; } }",
        "package sample; public class Box { protected String getLabel() { return null; } }",
        "package sample; public class Box { public String label; public String getLabel() { return null; } }",
        "package sample; public class Box { public String getLabel() { return null; } public String getLabel() { return \"again\"; } }",
    ] {
        assert!(read(source, "label").is_none(), "{source}");
    }

    let inherited = indexed(&[
        (
            "Box.java",
            "package sample; public class Box extends Parent { }",
        ),
        (
            "Parent.java",
            "package sample; public class Parent { public String getLabel() { return null; } }",
        ),
    ]);
    let inherited_read = inherited
        .java_property_read(&owner(&inherited), "label")
        .expect("a public source-proven inherited getter is readable");
    assert_eq!(inherited_read.location.file, PathBuf::from("Parent.java"));
    assert_eq!(
        inherited_read.location.snapshot_hash,
        *blake3::hash(
            b"package sample; public class Parent { public String getLabel() { return null; } }"
        )
        .as_bytes()
    );
    assert_eq!(inherited_read.target.file, PathBuf::from("Parent.java"));
    assert!(
        inherited_read
            .dependencies
            .iter()
            .any(|id| id.name == "Box")
    );
    assert!(
        inherited_read
            .dependencies
            .iter()
            .any(|id| id.name == "Parent")
    );
}

#[test]
fn inherited_interface_diamonds_deduplicate_but_unrelated_getters_are_ambiguous() {
    let diamond = indexed(&[
        (
            "Box.java",
            "package sample; public abstract class Box implements Left, Right { }",
        ),
        (
            "Left.java",
            "package sample; public interface Left extends Root { }",
        ),
        (
            "Right.java",
            "package sample; public interface Right extends Root { }",
        ),
        (
            "Root.java",
            "package sample; public interface Root { String getLabel(); }",
        ),
    ]);
    let fact = diamond
        .java_property_read(&owner(&diamond), "label")
        .expect("one inherited declaration reached through a diamond is unique");
    assert_eq!(fact.location.file, PathBuf::from("Root.java"));
    assert_eq!(fact.target.file, PathBuf::from("Root.java"));

    let conflicting = indexed(&[
        (
            "Box.java",
            "package sample; public abstract class Box implements Left, Right { }",
        ),
        (
            "Left.java",
            "package sample; public interface Left { String getLabel(); }",
        ),
        (
            "Right.java",
            "package sample; public interface Right { String getLabel(); }",
        ),
    ]);
    assert!(
        conflicting
            .java_property_read(&owner(&conflicting), "label")
            .is_none(),
        "distinct unrelated contracts do not become unambiguous by spelling equality"
    );

    let conflicting_defaults = indexed(&[
        (
            "Box.java",
            "package sample; public class Box implements Left, Right { }",
        ),
        (
            "Left.java",
            "package sample; public interface Left { default String getLabel() { return null; } }",
        ),
        (
            "Right.java",
            "package sample; public interface Right { default String getLabel() { return null; } }",
        ),
    ]);
    assert!(
        conflicting_defaults
            .java_property_read(&owner(&conflicting_defaults), "label")
            .is_none(),
        "unrelated defaults remain ambiguous"
    );
}

#[test]
fn direct_covariant_overrides_win_and_overloads_do_not_hide_zero_argument_getters() {
    let provider = indexed(&[
        (
            "Box.java",
            "package sample; public class Box extends Child { @Override public String getLabel() { return null; } }",
        ),
        (
            "Child.java",
            "package sample; public class Child extends Parent { public String getLabel(int index) { return null; } }",
        ),
        (
            "Parent.java",
            "package sample; public class Parent { public Object getLabel() { return null; } public Object getLabel(String key) { return null; } }",
        ),
    ]);
    let fact = provider
        .java_property_read(&owner(&provider), "label")
        .expect("the direct covariant getter is selected");
    assert_eq!(fact.location.file, PathBuf::from("Box.java"));
    assert_eq!(fact.target.file, PathBuf::from("Box.java"));

    let inherited_overload = indexed(&[
        (
            "Box.java",
            "package sample; public class Box extends Child { }",
        ),
        (
            "Child.java",
            "package sample; public class Child extends Parent { public String getLabel(int index) { return null; } }",
        ),
        (
            "Parent.java",
            "package sample; public class Parent { public String getLabel() { return null; } }",
        ),
    ]);
    let fact = inherited_overload
        .java_property_read(&owner(&inherited_overload), "label")
        .expect("a distinct argument overload does not suppress a zero-argument getter");
    assert_eq!(fact.target.parameters.len(), 0);

    let inherited_covariant = indexed(&[
        (
            "Box.java",
            "package sample; public class Box extends Child { }",
        ),
        (
            "Child.java",
            "package sample; public class Child extends Parent { @Override public String getLabel() { return null; } }",
        ),
        (
            "Parent.java",
            "package sample; public class Parent { public Object getLabel() { return null; } }",
        ),
    ]);
    let fact = inherited_covariant
        .java_property_read(&owner(&inherited_covariant), "label")
        .expect("the most-specific inherited covariant getter is selected");
    assert_eq!(fact.location.file, PathBuf::from("Child.java"));
}

#[test]
fn inherited_unknown_or_incompatible_contracts_remain_unresolved() {
    for parent in [
        "public class Parent { public <T> T getLabel() { return null; } }",
        "public class Parent { public String getLabel() throws java.io.IOException { return null; } }",
        "public class Parent { @Custom public String getLabel() { return null; } }",
    ] {
        let provider = indexed(&[
            (
                "Box.java",
                "package sample; public class Box extends Parent { }",
            ),
            ("Parent.java", &format!("package sample; {parent}")),
        ]);
        assert!(
            provider
                .java_property_read(&owner(&provider), "label")
                .is_none(),
            "unsupported inherited declaration must block: {parent}"
        );
    }

    let incompatible_override = indexed(&[
        (
            "Box.java",
            "package sample; public class Box extends Parent { @Override @org.jetbrains.annotations.Nullable public String getLabel() { return null; } }",
        ),
        (
            "Parent.java",
            "package sample; public class Parent { @org.jetbrains.annotations.NotNull public Object getLabel() { return null; } }",
        ),
    ]);
    assert!(
        incompatible_override
            .java_property_read(&owner(&incompatible_override), "label")
            .is_none(),
        "a nullable override cannot weaken a nonnull inherited contract"
    );
}

#[test]
fn unproven_generated_overrides_do_not_fall_back_to_parent_contracts() {
    for declaration in [
        "@lombok.Getter public class Box extends Parent { private String label; }",
        "@lombok.Data public class Box extends Parent { private String label; }",
        "public class Box extends Parent { @lombok.Getter private String label; }",
        "@lombok.Getter @lombok.experimental.Accessors(prefix = \"m\") public class Box extends Parent { private String mLabel; }",
        "@lombok.Getter public class Box extends Parent { private boolean isLabel; }",
        "@UnknownProcessor public class Box extends Parent {}",
    ] {
        let provider = indexed(&[
            ("Box.java", &format!("package sample; {declaration}")),
            (
                "Parent.java",
                "package sample; public class Parent { public Object getLabel() { return null; } }",
            ),
        ]);
        assert!(
            provider
                .java_property_read(&owner(&provider), "label")
                .is_none(),
            "{declaration}"
        );
    }
    let provider = indexed(&[
        (
            "Box.java",
            "package sample; @lombok.Getter public class Box extends Parent { private boolean isEnabled; }",
        ),
        (
            "Parent.java",
            "package sample; public class Parent { public Boolean getIsEnabled() { return null; } }",
        ),
    ]);
    assert!(
        provider
            .java_property_read(&owner(&provider), "isEnabled")
            .is_none()
    );
}

#[test]
fn inherited_getter_types_and_nullability_resolve_in_the_declaring_file() {
    for (annotation, expected) in [
        ("", ReferenceReturnNullability::Platform),
        (
            "@org.jetbrains.annotations.Nullable",
            ReferenceReturnNullability::Nullable,
        ),
        (
            "@org.jetbrains.annotations.NotNull",
            ReferenceReturnNullability::NonNull,
        ),
    ] {
        let parent = format!(
            "package base;\nimport result.Result;\npublic class Parent {{ {annotation} public Result getLabel() {{ return null; }} }}"
        );
        let provider = indexed(&[
            (
                "Box.java",
                "package sample;\nimport base.Parent;\nimport unrelated.Result;\npublic class Box extends Parent {}",
            ),
            ("Parent.java", &parent),
            ("Result.java", "package result; public class Result {}"),
            (
                "unrelated/Result.java",
                "package unrelated; public class Result {}",
            ),
        ]);
        let fact = provider
            .java_property_read(&owner(&provider), "label")
            .unwrap_or_else(|| {
                panic!("inherited getter with {annotation:?} should resolve in Parent.java")
            });
        assert_eq!(fact.nullability, expected);
        assert!(
            matches!(fact.result_type.ty, KotlinType::Named { ref name, .. } if name == "result.Result")
        );
        assert!(
            fact.dependencies
                .iter()
                .any(|id| id.package == "result" && id.name == "Result")
        );
        assert!(!fact.dependencies.iter().any(|id| id.package == "unrelated"));
    }
}

#[test]
fn inherited_getters_reject_cycles_missing_parents_and_duplicate_owners() {
    for files in [
        vec![
            (
                "Box.java",
                "package sample; public class Box extends Parent {}",
            ),
            (
                "Parent.java",
                "package sample; public class Parent extends Box { public String getLabel() { return null; } }",
            ),
        ],
        vec![
            (
                "Box.java",
                "package sample; public class Box extends Parent {}",
            ),
            (
                "Parent.java",
                "package sample; public class Parent extends Missing { public String getLabel() { return null; } }",
            ),
        ],
        vec![
            (
                "Box.java",
                "package sample; public class Box extends Parent {}",
            ),
            (
                "Parent.java",
                "package sample; public class Parent { public String getLabel() { return null; } } public class Parent { public String getLabel() { return null; } }",
            ),
        ],
        vec![
            (
                "Box.java",
                "package sample; public class Box extends Parent<String> {}",
            ),
            (
                "Parent.java",
                "package sample; public class Parent<T> { public T getLabel() { return null; } }",
            ),
        ],
    ] {
        let provider = indexed(&files);
        assert!(
            provider
                .java_property_read(&owner(&provider), "label")
                .is_none()
        );
    }
}

#[test]
fn kotlin_property_ancestors_do_not_silently_disappear_from_java_hierarchy() {
    let provider = indexed(&[
        (
            "Box.java",
            "package sample; public class Box extends Parent { @Override public String getLabel() { return \"java\"; } }",
        ),
        (
            "Parent.kt",
            "package sample\nopen class Parent {\n    open val label: String = \"kotlin\"\n}\n",
        ),
    ]);
    assert!(
        provider
            .java_property_read(&owner(&provider), "label")
            .is_none(),
        "a Kotlin property synthesizes a JVM getter without a Java method symbol; its override contract is not yet modeled"
    );
}

#[test]
fn getter_dependencies_include_the_exact_owner_and_resolved_return_type() {
    let provider = indexed(&[
        (
            "Box.java",
            "package sample; public class Box { public Result getResult() { return null; } }",
        ),
        ("Result.java", "package sample; public class Result {}"),
    ]);
    let owner = owner(&provider);
    let fact = provider
        .java_property_read(&owner, "result")
        .expect("source return type resolves");
    assert!(fact.dependencies.contains(&owner));
    assert_eq!(fact.target.name, "getResult");
    assert!(matches!(fact.result_type.ty, KotlinType::Named { .. }));
    assert!(
        fact.dependencies
            .iter()
            .any(|dependency| dependency.name == "Result"),
        "the resolved return classifier is an explicit dependency"
    );
    assert!(matches!(
        provider.resolve_type_in_file(&fact.location.file, "sample", "Result"),
        FactStatus::Established(_)
    ));
}

#[test]
fn declared_getters_on_canonical_lombok_owners_keep_their_physical_contract() {
    for annotation in ["lombok.Data", "lombok.Value", "lombok.Getter"] {
        let source = format!(
            "package sample; @{annotation} public class Box {{ private String label; public String getLabel() {{ return label; }} }}"
        );
        let getter =
            read(&source, "label").expect("Lombok does not replace a declared public getter");
        assert_eq!(getter.target.name, "getLabel");
        assert_eq!(getter.method_name, "getLabel");
        assert_eq!(getter.nullability, ReferenceReturnNullability::Platform);
    }
    let imported = "package sample;\nimport lombok.Data;\n@Data public class Box { private String label; public String getLabel() { return label; } }";
    assert!(read(imported, "label").is_some());
    let shadow = "package sample;\nimport custom.Data;\n@Data public class Box { private String label; public String getLabel() { return label; } }";
    assert!(read(shadow, "label").is_none());
}

#[test]
fn annotation_defaults_owners_and_unknown_method_annotations_remain_unresolved() {
    let annotated_package = indexed(&[
        (
            "Box.java",
            "package sample; public class Box { public String getLabel() { return null; } }",
        ),
        (
            "package-info.java",
            "@org.jspecify.annotations.NullMarked package sample;",
        ),
    ]);
    assert!(
        annotated_package
            .java_property_read(&owner(&annotated_package), "label")
            .is_none(),
        "package nullness defaults are not inferred"
    );

    for source in [
        "package sample; @Custom public class Box { public String getLabel() { return null; } }",
        "package sample; public class Box { @Custom public String getLabel() { return null; } }",
    ] {
        assert!(read(source, "label").is_none(), "{source}");
    }

    let type_use = "package sample; public class Box { public java.lang.@org.jetbrains.annotations.Nullable String getLabel() { return null; } }";
    assert!(
        read(type_use, "label").is_none(),
        "return-type-use annotations need their own binding proof"
    );
}

#[test]
fn comment_imports_and_same_package_nullness_shadows_do_not_prove_annotations() {
    for (annotation, canonical) in [
        ("Nullable", "org.jetbrains.annotations.Nullable"),
        ("NotNull", "org.jetbrains.annotations.NotNull"),
    ] {
        let source = format!(
            "package sample; // import {canonical};\npublic class Box {{ @{annotation} public String getLabel() {{ return null; }} }}"
        );
        assert!(
            read(&source, "label").is_none(),
            "comment text is not an import: {annotation}"
        );
    }

    let same_package_shadow = indexed(&[
        (
            "Box.java",
            "package sample; public class Box { @Nullable public String getLabel() { return null; } }",
        ),
        ("Nullable.java", "package sample; @interface Nullable {}"),
    ]);
    assert!(
        same_package_shadow
            .java_property_read(&owner(&same_package_shadow), "label")
            .is_none(),
        "a same-package annotation with the same simple name is not JetBrains Nullable"
    );
}

#[test]
fn acronym_boolean_and_nested_platform_return_shapes_remain_unsupported() {
    let acronym = "package sample; public class Box { public String getURL() { return null; } }";
    assert!(read(acronym, "URL").is_none());

    let boolean = "package sample; public class Box { public boolean isActive() { return true; } }";
    assert!(
        read(boolean, "active").is_none(),
        "boolean isX synthetic properties are not in the current getX proof"
    );

    let generic = "package sample; import java.util.List; public class Box { public List<String> getLabels() { return null; } }";
    assert!(
        read(generic, "labels").is_none(),
        "platform nullability of nested generic return components is unresolved"
    );

    let array = "package sample; public class Box { public String[] getNames() { return null; } }";
    assert!(
        read(array, "names").is_none(),
        "platform nullability of array references and their elements is unresolved"
    );
}
