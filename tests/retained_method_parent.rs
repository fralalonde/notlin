use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler::{self, WorkspaceScope};
use notlin::workspace::SourceIndex;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn probe(child: &str, parent: &str, java: &[(&str, &str)]) -> (Vec<(String, String)>, Vec<String>) {
    let root = std::env::temp_dir().join(format!(
        "notlin-retained-method-parent-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    let _scratch = Scratch(root.clone());
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("selected")).unwrap();
    fs::create_dir_all(root.join("residual")).unwrap();
    let child_path = root.join("selected/Forwarded.kt");
    fs::write(&child_path, child).unwrap();
    fs::write(root.join("residual/Contract.kt"), parent).unwrap();
    for (name, source) in java {
        let path = root.join("residual").join(name);
        fs::write(path, source).unwrap();
    }

    let index = SourceIndex::discover(&root).unwrap();
    assert!(
        index
            .declarations()
            .any(|declaration| declaration.name == "Forwarded")
    );
    assert!(
        index
            .declarations()
            .any(|declaration| declaration.name == "Contract")
    );
    let semantic_provider = notlin::semantics::SyntaxSemanticProvider::new(
        index
            .files
            .iter()
            .map(|file| (file.path.clone(), file.source_text().to_owned())),
    );
    let retained = index
        .declarations()
        .filter(|declaration| declaration.name != "Forwarded")
        .map(|declaration| notlin::semantics::workspace_symbol(&index, declaration))
        .collect();
    let cli = Cli::parse_from(["notlin", "--in-place", "selected/Forwarded.kt"]);
    let source = fs::read_to_string(&child_path).unwrap();
    let (outputs, errors, _, coverage) = transpiler::transpile_with_workspace_hint(
        &source,
        &child_path,
        &cli,
        WorkspaceScope {
            index: Some(&index),
            roots: &[root.join("selected")],
            retained_hint: Some(&retained),
            semantic_provider: Some(&semantic_provider),
            ..Default::default()
        },
        true,
    );
    assert_eq!(errors, 0, "{}", coverage.untranslated.join(", "));
    let untranslated = coverage.untranslated;
    (outputs, untranslated)
}

fn forwards(outputs: &[(String, String)]) -> bool {
    outputs.iter().any(|(name, source)| {
        name.ends_with("Forwarded.java") && source.contains("interface Forwarded")
    })
}

#[test]
fn generic_method_parent_with_retained_companion_forwards_unchanged_contract() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded<T> : Contract<T> { fun label(): String }\n",
        "package sample.forward\ninterface Contract<T> {\n fun payload(): T\n fun key(): String\n companion object { suspend fun remainKotlin() {} }\n}\n",
        &[],
    );
    assert!(forwards(&outputs), "{outputs:?}; retained={untranslated:?}");
    let java = outputs
        .iter()
        .find(|(name, _)| name.ends_with("Forwarded.java"))
        .unwrap()
        .1
        .as_str();
    assert!(java.contains("extends Contract<T>"), "{java}");
}

#[test]
fn parent_generic_override_of_java_method_remains_forwardable() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded<T : BaseKey> : Contract<T> { fun label(): String }\n",
        "package sample.forward\ninterface Contract<T : BaseKey> : BaseKey {\n override fun id(): T\n fun payload(): T\n companion object { suspend fun remainKotlin() {} }\n}\n",
        &[(
            "BaseKey.java",
            "package sample.forward; public interface BaseKey { Object id(); }",
        )],
    );
    assert!(forwards(&outputs), "{outputs:?}; retained={untranslated:?}");
}

#[test]
fn default_method_parent_needs_a_jvm_default_bridge_plan() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded : Contract { fun label(): String }\n",
        "package sample.forward\ninterface Contract {\n fun key(): String\n fun labelText(): String = key()\n companion object { suspend fun remainKotlin() {} }\n}\n",
        &[],
    );
    assert!(
        !forwards(&outputs),
        "{outputs:?}; retained={untranslated:?}"
    );
}

#[test]
fn property_parent_does_not_use_method_only_forwarding() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded : Contract { fun label(): String }\n",
        "package sample.forward\ninterface Contract { val payload: String\n companion object { suspend fun remainKotlin() {} }\n}\n",
        &[],
    );
    assert!(
        !forwards(&outputs),
        "{outputs:?}; retained={untranslated:?}"
    );
}

#[test]
fn jvm_renamed_parent_method_does_not_use_forwarding_proof() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded : Contract { fun label(): String }\n",
        "package sample.forward\ninterface Contract { @JvmName(\"renamed\") fun key(): String\n companion object { suspend fun remainKotlin() {} }\n}\n",
        &[],
    );
    assert!(
        !forwards(&outputs),
        "{outputs:?}; retained={untranslated:?}"
    );
}

#[test]
fn unresolved_parent_ancestry_does_not_use_forwarding_proof() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded : Contract { fun label(): String }\n",
        "package sample.forward\ninterface Contract : MissingParent { fun key(): String\n companion object { suspend fun remainKotlin() {} }\n}\n",
        &[],
    );
    assert!(
        !forwards(&outputs),
        "{outputs:?}; retained={untranslated:?}"
    );
}

#[test]
fn child_override_does_not_use_forwarding_proof() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded : Contract { override fun key(): String }\n",
        "package sample.forward\ninterface Contract { fun key(): String\n companion object { suspend fun remainKotlin() {} }\n}\n",
        &[],
    );
    assert!(
        !forwards(&outputs),
        "{outputs:?}; retained={untranslated:?}"
    );
}

#[test]
fn qualified_parent_is_not_rebound_to_a_same_named_local_contract() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded<T> : other.Contract<T> {\n fun label(): String\n}\n",
        "package sample.forward\ninterface Contract {\n val payload: String\n}\n",
        &[(
            "Other.kt",
            "package other\ninterface Contract<T> {\n fun payload(): T\n companion object {\n  suspend fun remainKotlin() {}\n }\n}\n",
        )],
    );
    assert!(forwards(&outputs), "{outputs:?}; retained={untranslated:?}");
}

#[test]
fn qualified_default_body_parent_does_not_borrow_a_local_abstract_contract_proof() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded<T> : other.Contract<T> {\n fun label(): String\n}\n",
        "package sample.forward\ninterface Contract<T> {\n fun payload(): T\n}\n",
        &[(
            "Other.kt",
            "package other\ninterface Contract<T> {\n fun payload(): T\n fun key(): String = \"kept\"\n companion object {\n  suspend fun remainKotlin() {}\n }\n}\n",
        )],
    );
    assert!(
        !forwards(&outputs),
        "{outputs:?}; retained={untranslated:?}"
    );
}

#[test]
fn sealed_parent_needs_an_explicit_permitted_subclass_emission_plan() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded : Contract {\n fun label(): String\n}\n",
        "package sample.forward\nsealed interface Contract {\n fun key(): String\n companion object {\n  suspend fun remainKotlin() {}\n }\n}\n",
        &[],
    );
    assert!(
        !forwards(&outputs),
        "{outputs:?}; retained={untranslated:?}"
    );
}

#[test]
fn java_ancestor_keeps_parameterized_methods_generic_returns_and_static_fields() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded<T> : Contract<T> {\n fun label(): String\n}\n",
        "package sample.forward\ninterface Contract<T> : Platform<T> {\n override fun payload(): T\n companion object {\n  suspend fun remainKotlin() {}\n }\n}\n",
        &[(
            "Platform.java",
            "package sample.forward;\npublic interface Platform<T> {\n String KIND = \"platform\";\n T payload();\n default boolean matches(T value) { return java.util.Objects.equals(payload(), value); }\n default java.util.List<T> payloads() { return java.util.Collections.singletonList(payload()); }\n}\n",
        )],
    );
    assert!(forwards(&outputs), "{outputs:?}; retained={untranslated:?}");
}

#[test]
fn kotlin_parameterized_method_contract_does_not_borrow_java_ancestor_proof() {
    let (outputs, untranslated) = probe(
        "package sample.forward\ninterface Forwarded<T> : Contract<T> {\n fun label(): String\n}\n",
        "package sample.forward\ninterface Contract<T> {\n fun matches(value: T): Boolean\n companion object {\n  suspend fun remainKotlin() {}\n }\n}\n",
        &[],
    );
    assert!(
        !forwards(&outputs),
        "{outputs:?}; retained={untranslated:?}"
    );
}
