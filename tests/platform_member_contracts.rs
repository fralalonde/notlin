use notlin::semantics::{SyntaxSemanticProvider, plan_required_calls};
use std::path::Path;

fn unresolved(source: &str) -> Vec<String> {
    let file = Path::new("Contracts.kt");
    let provider = SyntaxSemanticProvider::new([(file.to_path_buf(), source.to_owned())]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    assert!(!tree.root_node().has_error(), "{source}");
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), file);
    plan_required_calls(source, &tree, file, &provider, false, &mut plan);
    plan.dependencies
        .into_iter()
        .map(|dependency| dependency.spelling)
        .collect()
}

#[test]
fn signatures_compose_across_constructors_members_and_factories() {
    for source in [
        "fun text(n: Int): String = StringBuilder().append(n).append(\"!\").toString()",
        "import java.math.BigDecimal\nfun text(n: BigDecimal): String = StringBuilder().append(n.add(BigDecimal.ONE).toPlainString()).toString()",
        "import java.util.UUID\nfun text(): String = UUID.randomUUID().toString()",
        "import java.util.Locale\nfun locale(): Locale = Locale.getDefault()",
        "import java.time.Instant\nfun instant(): Instant = Instant.now()",
    ] {
        assert!(
            unresolved(source).is_empty(),
            "{source}: {:?}",
            unresolved(source)
        );
    }
}

#[test]
fn arity_alone_cannot_prove_an_overload_or_a_chain_result() {
    for argument in ["value", "null", "value = 1", "*values", "1, 2"] {
        let source = format!(
            "fun text(value: Any, values: Array<String>): String = StringBuilder().append({argument}).toString()"
        );
        assert!(
            unresolved(&source).iter().any(|name| name == "append"),
            "{source}"
        );
    }
    for source in [
        "import java.math.BigDecimal\nfun text(n: BigDecimal): String = n.add(value = n).toPlainString()",
        "import java.math.BigDecimal\nfun text(n: BigDecimal): String = n.add(n) { n }.toPlainString()",
        "fun text(builder: StringBuilder?): String = builder.append(1).toString()",
    ] {
        assert!(!unresolved(source).is_empty(), "{source}");
    }
}

#[test]
fn instance_values_cannot_authorize_static_dispatch() {
    for (imports, parameters, expression, name) in [
        (
            "import java.util.UUID",
            "value: UUID",
            "value.randomUUID()",
            "randomUUID",
        ),
        (
            "import java.util.Locale",
            "value: Locale",
            "value.getDefault()",
            "getDefault",
        ),
        (
            "import java.time.Instant",
            "value: Instant",
            "value.now()",
            "now",
        ),
        (
            "import java.util.UUID",
            "UUID: Any",
            "UUID.randomUUID()",
            "randomUUID",
        ),
    ] {
        let source = format!("{imports}\nfun run({parameters}) = {expression}");
        assert!(
            unresolved(&source)
                .iter()
                .any(|unresolved| unresolved == name),
            "{source}"
        );
    }
}
