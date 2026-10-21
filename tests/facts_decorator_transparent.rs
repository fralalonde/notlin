//! The decorator must be invisible in Phase 0: with no helper, or with a
//! helper that answers nothing, every trait method returns exactly what the
//! wrapped syntax provider returns.

use notlin::cli::JvmFactsMode;
use notlin::facts::client::FactsClient;
use notlin::facts::decorator::FactsProvider;
use notlin::facts::protocol::Query;
use notlin::semantics::{FactStatus, SemanticProvider, SyntaxSemanticProvider};
use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCE: &str = r#"
package sample

class Foo {
    fun greet(): String = "hi"
    fun count(x: Int): Int = x
}

class Bar : Foo()

typealias Alias = Foo
"#;

const FILE: &str = "sample/Sample.kt";

fn inner() -> SyntaxSemanticProvider {
    SyntaxSemanticProvider::new([(PathBuf::from(FILE), SOURCE.to_string())])
}

fn foo_symbol(provider: &SyntaxSemanticProvider) -> notlin::semantics::SymbolId {
    match provider.resolve("Foo") {
        FactStatus::Established(id) => id,
        other => panic!("fixture must resolve Foo uniquely, got {other:?}"),
    }
}

fn assert_transparent(provider: &FactsProvider) {
    let inner = inner();
    let path = Path::new(FILE);
    let foo = foo_symbol(&inner);

    assert_eq!(provider.resolve("Foo"), inner.resolve("Foo"));
    assert_eq!(provider.resolve("Missing"), inner.resolve("Missing"));
    assert_eq!(provider.symbols(), inner.symbols());
    assert_eq!(
        provider.symbols_in_file(path).len(),
        inner.symbols_in_file(path).len()
    );
    assert_eq!(
        provider.symbols_named("greet").len(),
        inner.symbols_named("greet").len()
    );
    assert_eq!(provider.type_aliases(), inner.type_aliases());
    assert_eq!(
        provider.type_alias_definition("sample", "Alias"),
        inner.type_alias_definition("sample", "Alias")
    );
    assert_eq!(
        provider.has_type_alias_in_package("sample", "Alias"),
        inner.has_type_alias_in_package("sample", "Alias")
    );
    assert_eq!(provider.source_for_file(path), inner.source_for_file(path));
    assert_eq!(
        provider.builtin_jvm_type_descriptor(path, "String"),
        inner.builtin_jvm_type_descriptor(path, "String")
    );
    assert_eq!(
        provider.returned_receiver_capability(&foo),
        inner.returned_receiver_capability(&foo)
    );
    assert_eq!(
        provider.is_public_instance_callable(&foo),
        inner.is_public_instance_callable(&foo)
    );
    assert_eq!(
        provider.has_plain_instance_property_getter(&foo),
        inner.has_plain_instance_property_getter(&foo)
    );
    assert_eq!(
        provider.equivalent_property_getter(&foo),
        inner.equivalent_property_getter(&foo)
    );
    assert_eq!(
        provider.java_property_read(&foo, "greet"),
        inner.java_property_read(&foo, "greet")
    );
    assert_eq!(provider.enum_entries(&foo), inner.enum_entries(&foo));
    assert_eq!(
        provider.java_field_named(&foo, "x"),
        inner.java_field_named(&foo, "x")
    );
    assert_eq!(
        provider.is_ordinary_jvm_reference(&foo),
        inner.is_ordinary_jvm_reference(&foo)
    );
    assert_eq!(
        provider.inherited_property_access(&foo, "greet"),
        inner.inherited_property_access(&foo, "greet")
    );
    assert_eq!(
        provider.has_stable_noarg_jvm_signature(&foo),
        inner.has_stable_noarg_jvm_signature(&foo)
    );
    assert_eq!(
        provider.explicit_nongeneric_kotlin_return(&foo),
        inner.explicit_nongeneric_kotlin_return(&foo)
    );
    assert_eq!(
        provider.reference_return_nullability(&foo),
        inner.reference_return_nullability(&foo)
    );
    assert_eq!(
        provider.has_plain_java_package_nullability("sample"),
        inner.has_plain_java_package_nullability("sample")
    );
    assert_eq!(
        provider.is_nonsealed_interface(&foo),
        inner.is_nonsealed_interface(&foo)
    );
    assert_eq!(
        provider.inherited_members(&foo, "greet").len(),
        inner.inherited_members(&foo, "greet").len()
    );
    assert_eq!(
        provider.resolve_inherited_noarg_call(&foo, "greet"),
        inner.resolve_inherited_noarg_call(&foo, "greet")
    );
    assert_eq!(
        provider.has_complete_inheritance(&foo),
        inner.has_complete_inheritance(&foo)
    );
    assert_eq!(
        provider.instance_contract_parents(&foo),
        inner.instance_contract_parents(&foo)
    );
    assert_eq!(
        provider.resolve_type_in_file(path, "sample", "Foo"),
        inner.resolve_type_in_file(path, "sample", "Foo")
    );
    assert_eq!(
        provider.map_alias_type_arguments(path, "sample", "Alias"),
        inner.map_alias_type_arguments(path, "sample", "Alias")
    );
    assert_eq!(
        provider.companion_members(&foo, "greet").len(),
        inner.companion_members(&foo, "greet").len()
    );
    assert_eq!(
        provider.has_default_companion_jvm_abi(&foo),
        inner.has_default_companion_jvm_abi(&foo)
    );
    assert_eq!(
        provider.java_property_getters(&foo, "greet").len(),
        inner.java_property_getters(&foo, "greet").len()
    );
    assert_eq!(
        provider.is_java_static_member(&foo),
        inner.is_java_static_member(&foo)
    );
    assert_eq!(
        provider.is_java_callable_member(&foo),
        inner.is_java_callable_member(&foo)
    );
    assert_eq!(
        provider.kotlin_constructor_metadata_dependency(&foo),
        inner.kotlin_constructor_metadata_dependency(&foo)
    );
    assert_eq!(
        provider.has_jackson_default_constructor_semantics(&foo),
        inner.has_jackson_default_constructor_semantics(&foo)
    );
}

#[test]
fn without_a_helper_every_answer_comes_from_the_inner_provider() {
    let provider = FactsProvider::transparent(inner());
    assert!(!provider.has_helper());
    assert!(provider.query(Query::Ping).is_none());
    assert_transparent(&provider);
}

#[test]
fn off_policy_never_starts_a_helper() {
    let provider =
        FactsProvider::from_policy(inner(), JvmFactsMode::Off).expect("off is infallible");
    assert!(!provider.has_helper());
    assert_transparent(&provider);
}

#[test]
fn a_silent_helper_changes_no_answer() {
    let mut command = Command::new("sh");
    command.arg("-c").arg("exit 0");
    let client = FactsClient::start_with_command(command).expect("fake helper starts");
    let provider = FactsProvider::with_client(inner(), client);
    assert!(provider.has_helper());
    // No upgraded answers exist yet: a helper that answers nothing yields
    // Unknown-equivalent results, which are the inner provider's answers.
    assert!(provider.query(Query::Ping).is_none());
    assert_transparent(&provider);
}

#[test]
fn on_without_a_jar_fails_before_writing_anything() {
    let root = std::env::temp_dir().join(format!("notlin-jvm-facts-on-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("Sample.kt");
    let original = "package sample\nclass Sample\n";
    std::fs::write(&source, original).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .current_dir(&root)
        .env_remove("NOTLIN_JVM_FACTS_JAR")
        .args(["--jvm-facts=on", "--in-place", "."])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("--jvm-facts=on requires NOTLIN_JVM_FACTS_JAR"),
        "{stderr}"
    );
    assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
    assert!(!root.join("Sample.java").exists());
    std::fs::remove_dir_all(root).unwrap();
}
