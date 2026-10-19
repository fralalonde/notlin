use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider, TypeRef};
use std::path::PathBuf;

fn member<'a>(
    provider: &'a SyntaxSemanticProvider,
    name: &str,
) -> &'a notlin::semantics::SemanticSymbol {
    let matches = provider
        .symbols_named(name)
        .into_iter()
        .filter(|symbol| symbol.id.kind == "function")
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "expected one indexed function named {name}"
    );
    matches[0]
}

#[test]
fn explicit_nongeneric_member_return_is_available_only_with_stable_source_abi() {
    let file = PathBuf::from("src/Api.kt");
    let source = r#"
package sample
import java.util.Optional

class Item
class Api {
    fun explicit(): Optional<Item> = TODO()
    fun inferred() = explicit()
    fun <T> methodGeneric(): Optional<T> = TODO()
}
class GenericApi<T> {
    fun ownerGeneric(): Optional<T> = TODO()
}
class ExtensionApi {
    fun Item.extensionReturn(): Optional<Item> = TODO()
}
"#;
    let provider = SyntaxSemanticProvider::new([(file, source.to_owned())]);

    let explicit = member(&provider, "explicit");
    assert_eq!(
        provider.explicit_nongeneric_kotlin_return(&explicit.id),
        Some(TypeRef::parse("Optional<Item>"))
    );
    for name in [
        "inferred",
        "methodGeneric",
        "ownerGeneric",
        "extensionReturn",
    ] {
        let candidate = member(&provider, name);
        assert_eq!(
            provider.explicit_nongeneric_kotlin_return(&candidate.id),
            None,
            "{name} must not establish a reusable payload"
        );
    }
}

#[test]
fn reindexing_without_rebuilding_provider_invalidates_unique_source_fact() {
    let file = PathBuf::from("src/Api.kt");
    let source = "package sample\nimport java.util.Optional\nclass Item\nclass Api {\n fun explicit(): Optional<Item> = TODO()\n}\n";
    let mut provider = SyntaxSemanticProvider::new([(file.clone(), source.to_owned())]);
    let explicit = member(&provider, "explicit").id.clone();
    assert!(
        provider
            .explicit_nongeneric_kotlin_return(&explicit)
            .is_some()
    );

    // Re-indexing changed text without rebuilding the retained provider leaves
    // duplicate declaration identities, which cannot be used as a payload.
    provider.index_file(
        &file,
        "package sample\nimport java.util.Optional\nclass Item\nclass Api {\n fun explicit(): Optional<String> = TODO()\n}\n",
    );
    assert_eq!(provider.explicit_nongeneric_kotlin_return(&explicit), None);
}
