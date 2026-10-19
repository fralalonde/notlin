use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider};
use std::path::PathBuf;

fn public_getter(source: &str) -> bool {
    let provider = SyntaxSemanticProvider::new([(PathBuf::from("Contract.kt"), source.to_owned())]);
    let member = provider
        .symbols_named("read")
        .into_iter()
        .find(|symbol| symbol.id.kind == "function")
        .expect("fixture method");
    provider.is_public_instance_callable(&member.id)
}

#[test]
fn proven_jdk_reference_results_preserve_public_jvm_names() {
    for (imports, ty) in [
        ("import java.util.*", "UUID"),
        ("import java.util.UUID\nimport foreign.*", "UUID"),
        ("import java.math.BigDecimal", "BigDecimal"),
        ("import java.time.Instant", "Instant"),
        ("import java.util.Locale", "Locale"),
        ("", "java.util.UUID"),
        ("", "StringBuilder"),
    ] {
        assert!(
            public_getter(&format!(
                "package sample\n{imports}\ninterface Contract {{ fun read(): {ty} }}\n"
            )),
            "{imports}: {ty}"
        );
    }
}

#[test]
fn unknown_or_rebound_results_do_not_prove_public_jvm_names() {
    for source in [
        "import java.util.*\nimport foreign.*\ninterface Contract { fun read(): UUID }",
        "interface Contract { fun read(): UUID }",
        "import foreign.UUID\ninterface Contract { fun read(): UUID }",
        "import java.util.UUID as Id\ninterface Contract { fun read(): Id }",
        "typealias UUID = java.util.UUID\ninterface Contract { fun read(): UUID }",
        "@JvmInline value class UUID(val value: Long)\ninterface Contract { fun read(): UUID }",
        "import java.util.UUID\ninterface Contract { @JvmName(\"renamed\") fun read(): UUID }",
        "import java.util.UUID\nclass Contract { private fun read(): UUID = TODO() }",
        "import java.util.UUID\nclass Contract { internal fun read(): UUID = TODO() }",
    ] {
        assert!(!public_getter(source), "{source}");
    }
}
