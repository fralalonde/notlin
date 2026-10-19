use notlin::semantics::{FactStatus, SymbolId, SyntaxSemanticProvider, plan_required_calls};
use std::path::Path;

fn call_resolution(source: &str, spelling: &str) -> Option<FactStatus<SymbolId>> {
    let file = Path::new("PlatformConstructor.kt");
    let provider = SyntaxSemanticProvider::new([(file.to_path_buf(), source.to_owned())]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "invalid fixture: {source}");
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), file);
    plan_required_calls(source, &tree, file, &provider, false, &mut plan);
    plan.dependencies
        .into_iter()
        .find(|dependency| dependency.spelling == spelling)
        .map(|dependency| dependency.resolution)
}

fn assert_proven(source: &str, spelling: &str) {
    assert_eq!(
        call_resolution(source, spelling),
        None,
        "expected {spelling} constructor to be proven: {source}"
    );
}

fn assert_unproven(source: &str, spelling: &str) {
    assert!(
        call_resolution(source, spelling).is_some(),
        "expected {spelling} constructor to remain a required call: {source}"
    );
}

#[test]
fn big_decimal_constructors_require_an_exact_import_and_supported_argument_type() {
    for expression in [
        "BigDecimal(text)",
        "BigDecimal(doubleValue)",
        "BigDecimal(intValue)",
        "BigDecimal(longValue)",
        "BigDecimal(\"1.25\")",
        "BigDecimal(1.25)",
        "BigDecimal(1)",
        "BigDecimal(1L)",
        "java.math.BigDecimal(\"1.25\")",
        "java.math.BigDecimal(1.25)",
        "java.math.BigDecimal(1)",
        "java.math.BigDecimal(1L)",
    ] {
        let parameters = match expression {
            "BigDecimal(text)" => "text: String",
            "BigDecimal(doubleValue)" => "doubleValue: Double",
            "BigDecimal(intValue)" => "intValue: Int",
            "BigDecimal(longValue)" => "longValue: Long",
            _ => "",
        };
        let source =
            format!("import java.math.BigDecimal\nfun build({parameters}) = {expression}\n");
        assert_proven(&source, "BigDecimal");
    }
}

#[test]
fn string_builder_zero_argument_constructors_are_intrinsic_and_fully_qualified() {
    for source in [
        "fun build() = StringBuilder()",
        "import java.lang.StringBuilder\nfun build() = StringBuilder()",
        "fun build() = java.lang.StringBuilder()",
    ] {
        assert_proven(source, "StringBuilder");
    }
}

#[test]
fn unsupported_constructor_shapes_and_argument_types_remain_required() {
    for source in [
        "import java.math.BigDecimal\nfun build() = BigDecimal()",
        "import java.math.BigDecimal\nfun build() = BigDecimal(\"1\", \"2\")",
        "import java.math.BigDecimal\nfun build() = BigDecimal(value = \"1\")",
        "import java.math.BigDecimal\nfun build(values: Array<String>) = BigDecimal(*values)",
        "import java.math.BigDecimal\nfun build(value: Float) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun build(value: Number) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun build(value: Double?) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun build(value: Any) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun <T> build(value: T) = BigDecimal(value)",
        "import java.math.BigDecimal\nfun build() { val value = obtain(); BigDecimal(value) }",
        "import java.math.BigDecimal\nfun build() = BigDecimal(1.0f)",
        "fun build() = StringBuilder(\"text\")",
    ] {
        assert_unproven(
            source,
            if source.contains("StringBuilder") {
                "StringBuilder"
            } else {
                "BigDecimal"
            },
        );
    }
}

#[test]
fn source_declarations_and_values_named_like_constructors_keep_source_ownership() {
    let source_owned = [
        "import java.math.BigDecimal\nclass BigDecimal\nfun build() = BigDecimal(\"1\")",
        "typealias BigDecimal = String\nfun build() = BigDecimal(\"1\")",
        "import java.math.BigDecimal\nfun BigDecimal(value: String) = value\nfun build() = BigDecimal(\"1\")",
        "import java.math.BigDecimal\nfun build() { fun BigDecimal(value: String) = value; BigDecimal(\"1\") }",
        "import java.math.BigDecimal\nfun build(BigDecimal: (String) -> String) = BigDecimal(\"1\")",
        "import java.math.BigDecimal\nfun build() { val BigDecimal: (String) -> String = { it }; BigDecimal(\"1\") }",
        "import java.math.BigDecimal\nclass Factory(val BigDecimal: (String) -> String) {\n fun build() = BigDecimal(\"1\")\n}\n",
        "import java.math.BigDecimal\nopen class Factory {\n fun BigDecimal(value: String) = value\n}\nclass Child : Factory() {\n fun build() = BigDecimal(\"1\")\n}\n",
    ];
    for source in source_owned {
        let resolution = call_resolution(source, "BigDecimal");
        assert!(
            resolution.is_some(),
            "source declaration was bypassed: {source}"
        );
    }

    for source in [
        "class StringBuilder\nfun build() = StringBuilder()",
        "fun StringBuilder() = \"source\"\nfun build() = StringBuilder()",
        "fun build(StringBuilder: () -> String) = StringBuilder()",
        "fun build() { val StringBuilder: () -> String = { \"source\" }; StringBuilder() }",
    ] {
        assert_unproven(source, "StringBuilder");
        if source.contains("fun StringBuilder()") {
            assert!(
                matches!(
                    call_resolution(source, "StringBuilder"),
                    Some(FactStatus::Established(_))
                ),
                "a source factory should keep its established source dependency: {source}"
            );
        }
    }
}

#[test]
fn conflicting_aliased_duplicate_and_wildcard_imports_do_not_prove_simple_big_decimal() {
    for source in [
        "import java.math.BigDecimal\nimport custom.BigDecimal\nfun build() = BigDecimal(\"1\")",
        "import java.math.BigDecimal as Decimal\nfun build() = BigDecimal(\"1\")",
        "import java.math.BigDecimal as Decimal\nfun build() = Decimal(\"1\")",
        "import java.math.BigDecimal\nimport java.math.BigDecimal\nfun build() = BigDecimal(\"1\")",
        "import java.math.*\nfun build() = BigDecimal(\"1\")",
        "import java.math.*\nimport foreign.*\nfun build() = BigDecimal(\"1\")",
    ] {
        assert_unproven(
            source,
            if source.contains("= Decimal(") {
                "Decimal"
            } else {
                "BigDecimal"
            },
        );
    }
}

#[test]
fn explicit_constructor_imports_take_precedence_over_external_wildcards() {
    assert_proven(
        "import foreign.*\nimport java.math.BigDecimal\nfun build() = BigDecimal(\"1\")",
        "BigDecimal",
    );
    assert_proven("fun build() = java.math.BigDecimal(\"1\")", "BigDecimal");
    assert_unproven(
        "import java.math.BigDecimal\nfun String.unaryPlus(): Int = 1\nfun build() = BigDecimal(+\"text\")",
        "BigDecimal",
    );
}

#[test]
fn unchecked_exception_constructors_require_a_canonical_binding_and_applicable_overload() {
    for name in ["IllegalArgumentException", "IllegalStateException"] {
        for imports in [
            "",
            "import java.util.*\n",
            "import java.time.*\n",
            "import java.lang.*\n",
        ] {
            for value in ["", "\"message\""] {
                assert_proven(&format!("{imports}fun build() = {name}({value})"), name);
            }
        }
        assert_proven(
            &format!(
                "import foreign.*\nimport java.lang.{name}\nfun build() = {name}(\"message\")"
            ),
            name,
        );
        for source in [
            format!("import foreign.*\nfun build() = {name}(\"message\")"),
            format!("import custom.{name}\nfun build() = {name}()"),
            format!("fun build(value: Any) = {name}(value)"),
            format!("fun build(value: String?) = {name}(value)"),
            format!("fun build() = {name}(12)"),
            format!("class {name}\nfun build() = {name}()"),
            format!("fun build({name}: () -> String) = {name}()"),
        ] {
            assert_unproven(&source, name);
        }
    }
}

#[test]
fn primitive_parameter_type_names_must_not_be_shadowed() {
    for source in [
        "import java.math.BigDecimal\nclass Double\nfun build(value: Double) = BigDecimal(value)",
        "import java.math.BigDecimal\nimport custom.Double\nfun build(value: Double) = BigDecimal(value)",
        "import java.math.BigDecimal\ntypealias Double = Int\nfun build(value: Double) = BigDecimal(value)",
    ] {
        assert_unproven(source, "BigDecimal");
    }
}

#[test]
fn lambda_loop_and_catch_bindings_do_not_reuse_outer_parameter_types() {
    for source in [
        "import java.math.BigDecimal\nfun build(value: Double) { listOf(\"x\").forEach { value -> BigDecimal(value) } }",
        "import java.math.BigDecimal\nfun build(value: Double) { for (value in listOf(\"x\")) { BigDecimal(value) } }",
        "import java.math.BigDecimal\nfun build(value: Double) { try { error(\"x\") } catch (value: Exception) { BigDecimal(value) } }",
    ] {
        assert_unproven(source, "BigDecimal");
    }
}

#[test]
fn fully_qualified_constructor_calls_respect_a_shadowed_java_root() {
    for source in [
        "fun build(java: Int) = java.math.BigDecimal(\"1\")",
        "fun build(java: (String) -> String) = java.lang.StringBuilder()",
        "import custom.Root as java\nfun build() = java.math.BigDecimal(\"1\")",
        "import foreign.*\nfun build() = java.math.BigDecimal(\"1\")",
        "object java {\n object math {\n fun BigDecimal(value: String): String = value\n }\n}\nfun build() = java.math.BigDecimal(\"1\")\n",
    ] {
        assert_unproven(
            source,
            if source.contains("BigDecimal") {
                "BigDecimal"
            } else {
                "StringBuilder"
            },
        );
    }
}
