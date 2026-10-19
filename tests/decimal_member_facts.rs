use notlin::semantics::{SyntaxSemanticProvider, plan_required_calls};
use std::path::Path;

fn unresolved(source: &str, name: &str) -> bool {
    let file = Path::new("Decimal.kt");
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
        .iter()
        .any(|dependency| dependency.spelling == name)
}

#[test]
fn decimal_methods_require_applicable_jdk_types_and_preserve_chained_classifiers() {
    for import in ["import java.math.BigDecimal", "import java.math.*"] {
        let source = format!(
            "package sample\n{import}\nfun compute(a: BigDecimal, b: BigDecimal): String = a.add(b).subtract(BigDecimal.ONE).multiply(b).remainder(b).abs().negate().stripTrailingZeros().toPlainString()\n"
        );
        for name in [
            "add",
            "subtract",
            "multiply",
            "remainder",
            "abs",
            "negate",
            "stripTrailingZeros",
            "toPlainString",
        ] {
            assert!(
                !unresolved(&source, name),
                "{name} should be proven for {import}"
            );
        }
    }
    let source = "package sample\nfun compare(a: java.math.BigDecimal, b: java.math.BigDecimal): Int = a.compareTo(b)\n";
    assert!(!unresolved(source, "compareTo"));
}

#[test]
fn decimal_property_types_use_the_proven_declaring_context() {
    let source = "package sample\nimport java.math.BigDecimal\nclass Amount(val value: BigDecimal) {\n fun total(other: Amount): BigDecimal = this.value.add(other.value)\n}\n";
    assert!(!unresolved(source, "add"));
}

#[test]
fn nullable_unknown_or_shadowed_decimal_types_do_not_establish_jdk_members() {
    for source in [
        "import java.math.BigDecimal\nfun run(a: BigDecimal?, b: BigDecimal) = a.add(b)",
        "import java.math.BigDecimal\nfun run(a: BigDecimal, b: BigDecimal?) = a.add(b)",
        "import java.math.BigDecimal\nfun run(a: BigDecimal) = a.add(\"1\")",
        "import java.math.BigDecimal\nfun run(a: BigDecimal, b: BigDecimal) = a.add()",
        "import java.math.BigDecimal\nfun run(a: BigDecimal, b: BigDecimal) = a.add(b, b)",
        "import java.math.BigDecimal\nfun run(a: BigDecimal, b: BigDecimal) { val b = \"1\"; a.add(b) }",
        "import java.math.BigDecimal\nfun <BigDecimal> run(a: BigDecimal, b: BigDecimal) = a.add(b)",
        "import java.math.*\nimport foreign.*\nfun run(a: BigDecimal, b: BigDecimal) = a.add(b)",
        "import java.math.BigDecimal\nclass BigDecimal\nfun run(a: BigDecimal, b: BigDecimal) = a.add(b)",
        "typealias BigDecimal = String\nfun run(a: BigDecimal, b: BigDecimal) = a.add(b)",
        "import java.math.BigDecimal\nfun BigDecimal.add(other: String): BigDecimal = this\nfun run(a: BigDecimal) = a.add(\"1\")",
        "import java.math.BigDecimal\nfun run(a: BigDecimal, values: List<String>) { for (a in values) { a.add(BigDecimal.ONE) } }",
    ] {
        assert!(
            unresolved(source, "add"),
            "must remain unresolved: {source}"
        );
    }
}

#[test]
fn arbitrary_constants_and_unproven_constructor_results_do_not_gain_decimal_types() {
    for expression in ["BigDecimal.unknown", "BigDecimal()", "BigDecimal(1.0f)"] {
        let source =
            format!("import java.math.BigDecimal\nfun run(b: BigDecimal) = {expression}.add(b)\n");
        assert!(unresolved(&source, "add"), "{expression}");
    }
}

#[test]
fn proven_constructor_results_supply_decimal_receiver_facts() {
    for expression in [
        "BigDecimal(\"1\")",
        "BigDecimal(1.0)",
        "BigDecimal(1)",
        "BigDecimal(1L)",
    ] {
        let source =
            format!("import java.math.BigDecimal\nfun run(b: BigDecimal) = {expression}.add(b)\n");
        assert!(!unresolved(&source, "BigDecimal"), "{expression}");
        assert!(!unresolved(&source, "add"), "{expression}");
    }
}

#[test]
fn immutable_local_decimal_facts_respect_blocks_and_binding_scopes() {
    for binding in [
        "val amount = BigDecimal(\"1\")",
        "val amount: BigDecimal = BigDecimal(\"1\")",
    ] {
        let source = format!(
            "import java.math.BigDecimal\nfun build() {{\n {binding}\n amount.add(BigDecimal.ONE)\n}}\n"
        );
        assert!(!unresolved(&source, "add"), "{source}");
    }
    for body in [
        "var amount = BigDecimal(\"1\"); amount.add(BigDecimal.ONE)",
        "val amount: BigDecimal? = BigDecimal(\"1\"); amount.add(BigDecimal.ONE)",
        "val amount: Any = BigDecimal(\"1\"); amount.add(BigDecimal.ONE)",
        "val amount = BigDecimal(\"1\"); run { amount.add(BigDecimal.ONE) }",
        "val amount = BigDecimal(\"1\"); run { val amount = \"x\"; amount.add(BigDecimal.ONE) }",
        "val amount = BigDecimal(\"1\"); for (amount in values) amount.add(BigDecimal.ONE)",
        "val amount = BigDecimal(\"1\"); when (val amount = \"x\") { else -> amount.add(BigDecimal.ONE) }",
    ] {
        let source = format!(
            "import java.math.BigDecimal\nfun build(values: List<String>) {{\n {body}\n}}\n"
        );
        assert!(unresolved(&source, "add"), "{source}");
    }
}

#[test]
fn unqualified_properties_require_the_current_dispatch_owner_and_no_local_binding() {
    let source = "package sample\nimport java.math.BigDecimal\nclass Amount(val value: BigDecimal) {\n fun total(other: BigDecimal): BigDecimal = value.add(other)\n}\n";
    assert!(!unresolved(source, "add"));
    for body in [
        "val value = \"1\"; value.add(other)",
        "listOf(\"1\").forEach { value -> value.add(other) }",
        "for (value in listOf(\"1\")) { value.add(other) }",
    ] {
        let source = format!(
            "package sample\nimport java.math.BigDecimal\nclass Amount(val value: BigDecimal) {{\n fun total(other: BigDecimal) {{\n {body}\n }}\n }}\n"
        );
        assert!(unresolved(&source, "add"), "{body}");
    }
}
