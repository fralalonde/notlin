use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::{SourceIndex, SourceLanguage};
use std::fs;
use std::path::PathBuf;

#[test]
fn discovers_kotlin_and_java_sources_recursively() {
    let root = std::env::temp_dir().join(format!("notlin-workspace-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("module/src/main/kotlin/sample")).unwrap();
    fs::create_dir_all(root.join("module/src/main/java/sample")).unwrap();
    fs::write(
        root.join("module/src/main/kotlin/sample/Types.kt"),
        "package sample\ninterface Contract\nclass Types : Contract\n",
    )
    .unwrap();
    fs::write(
        root.join("module/src/main/java/sample/Existing.java"),
        "package sample;\npublic class Existing {\n    private static int count;\n    public String read() { return \"x\"; }\n}\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    assert_eq!(index.kotlin_files().count(), 1);
    assert_eq!(index.java_files().count(), 1);
    assert_eq!(
        index.kotlin_files().next().unwrap().package.as_deref(),
        Some("sample")
    );
    assert_eq!(
        index.kotlin_files().next().unwrap().declarations[1].name,
        "Types"
    );
    assert_eq!(
        index.kotlin_files().next().unwrap().declarations[1].supertypes,
        vec!["Contract"]
    );
    let existing = index.java_files().next().unwrap();
    assert_eq!(existing.language, SourceLanguage::Java);
    assert!(
        existing.declarations[0]
            .members
            .iter()
            .any(|member| member.name == "count" && member.is_static)
    );
    assert!(
        existing.declarations[0]
            .members
            .iter()
            .any(|member| member.name == "read" && member.visibility.as_deref() == Some("public"))
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn resolves_imported_types_before_ambiguous_simple_names() {
    let root = std::env::temp_dir().join(format!("notlin-resolve-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Api.kt"), "package api\nclass Contract\n").unwrap();
    fs::write(root.join("Other.kt"), "package other\nclass Contract\n").unwrap();
    fs::write(
        root.join("Use.kt"),
        "package use\nimport api.Contract\nclass Use : Contract\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let use_file = index
        .kotlin_files()
        .find(|file| file.path.ends_with("Use.kt"))
        .unwrap();
    assert_eq!(use_file.imports, vec!["api.Contract"]);
    assert_eq!(
        index
            .resolve_type(use_file, "Contract")
            .unwrap()
            .package
            .as_deref(),
        Some("api")
    );
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn detects_residual_kotlin_subtypes_outside_translation_roots() {
    let root = std::env::temp_dir().join(format!("notlin-hierarchy-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("selected")).unwrap();
    fs::create_dir_all(root.join("residual")).unwrap();
    fs::write(
        root.join("selected/Base.kt"),
        "package sample\nopen class Base\n",
    )
    .unwrap();
    fs::write(
        root.join("residual/Child.kt"),
        "package sample\nclass Child : Base()\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let base = index
        .declarations()
        .find(|declaration| declaration.name == "Base")
        .unwrap();
    assert!(index.has_unselected_kotlin_subtype(base, &[root.join("selected")]));
    assert!(
        !index.has_unselected_kotlin_subtype(base, &[root.join("selected"), root.join("residual")])
    );
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn transpiler_retains_selected_base_with_residual_kotlin_subtype() {
    let root = std::env::temp_dir().join(format!("notlin-retention-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("selected")).unwrap();
    fs::create_dir_all(root.join("residual")).unwrap();
    let base_path = root.join("selected/Base.kt");
    fs::write(&base_path, "package sample\nopen class Base\n").unwrap();
    fs::write(
        root.join("residual/Child.kt"),
        "package sample\nclass Child : Base()\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", "selected/Base.kt"]);
    let source = fs::read_to_string(&base_path).unwrap();
    let (files, errors, warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &base_path,
        &cli,
        Some(&index),
        &[root.join("selected")],
    );
    assert_eq!(errors, 0);
    assert!(warnings > 0);
    assert!(files.is_empty());
    assert!(coverage.untranslated.iter().any(|name| name == "Base"));
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn detects_kotlin_subtypes_even_inside_translation_roots() {
    let root = std::env::temp_dir().join(format!("notlin-interface-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Contract.kt"),
        "package sample\ninterface Contract\n",
    )
    .unwrap();
    fs::write(
        root.join("Implementation.kt"),
        "package sample\nclass Implementation : Contract\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let contract = index
        .declarations()
        .find(|declaration| declaration.name == "Contract")
        .unwrap();
    assert!(index.has_kotlin_subtype(contract));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn detects_kotlin_subtype_among_multiple_supertypes() {
    let root =
        std::env::temp_dir().join(format!("notlin-multiple-supertypes-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Contracts.kt"),
        "package sample\ninterface First\ninterface Second\n",
    )
    .unwrap();
    fs::write(
        root.join("Implementation.kt"),
        "package sample\nclass Implementation : First, Second\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let second = index
        .declarations()
        .find(|declaration| declaration.name == "Second")
        .unwrap();
    assert!(index.has_kotlin_subtype(second));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn transpiler_retains_interface_with_kotlin_implementation() {
    let root =
        std::env::temp_dir().join(format!("notlin-interface-retention-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let contract_path = root.join("Contract.kt");
    fs::write(&contract_path, "package sample\ninterface Contract\n").unwrap();
    fs::write(
        root.join("Implementation.kt"),
        "package sample\nclass Implementation : Contract\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", contract_path.to_str().unwrap()]);
    let source = fs::read_to_string(&contract_path).unwrap();
    let (files, errors, _warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &contract_path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    assert!(files.is_empty());
    assert!(coverage.untranslated.iter().any(|name| name == "Contract"));
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn retention_matches_canonical_index_to_input_path() {
    let root = std::env::temp_dir().join(format!("notlin-canonical-path-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let contract_path = root.join("Contract.kt");
    fs::write(&contract_path, "package sample\ninterface Contract\n").unwrap();
    fs::write(
        root.join("Implementation.kt"),
        "package sample\nclass Implementation : Contract\n",
    )
    .unwrap();

    let canonical_root = fs::canonicalize(&root).unwrap();
    let index = SourceIndex::discover(&canonical_root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", contract_path.to_str().unwrap()]);
    let source = fs::read_to_string(&contract_path).unwrap();
    let (files, errors, _warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &contract_path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    assert!(
        files.is_empty(),
        "indexed={} input={}",
        index.files[0].path.display(),
        contract_path.display()
    );
    assert!(coverage.untranslated.iter().any(|name| name == "Contract"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn retains_top_level_property_referenced_by_kotlin_source() {
    let root = std::env::temp_dir().join(format!("notlin-top-level-caller-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let provider = root.join("Provider.kt");
    fs::write(&provider, "package sample\nconst val SEPARATOR = \":\"\n").unwrap();
    fs::write(
        root.join("Consumer.kt"),
        "package sample\nfun render(value: String) = value + SEPARATOR\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", provider.to_str().unwrap()]);
    let source = fs::read_to_string(&provider).unwrap();
    let (files, errors, warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &provider,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    assert!(warnings > 0);
    assert!(files.is_empty());
    assert!(coverage.untranslated.iter().any(|name| name == "SEPARATOR"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn retains_defaulted_constructor_used_by_kotlin_caller() {
    let root =
        std::env::temp_dir().join(format!("notlin-default-constructor-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let provider = root.join("Configuration.kt");
    fs::write(
        &provider,
        "package sample\ndata class Configuration(val values: List<String> = emptyList())\n",
    )
    .unwrap();
    fs::write(
        root.join("Consumer.kt"),
        "package sample\nfun create() = Configuration()\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", provider.to_str().unwrap()]);
    let source = fs::read_to_string(&provider).unwrap();
    let (files, errors, warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &provider,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    assert!(warnings > 0);
    assert!(files.is_empty());
    assert!(
        coverage
            .untranslated
            .iter()
            .any(|name| name == "Configuration")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn retains_non_null_property_override_of_nullable_kotlin_contract() {
    let root = std::env::temp_dir().join(format!(
        "notlin-nullability-contract-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("Definitions.kt");
    fs::write(
        &path,
        "package sample\nclass Value\ninterface Contract {\n    val item: Value?\n}\ndata class Implementation(override val item: Value) : Contract\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", path.to_str().unwrap()]);
    let source = fs::read_to_string(&path).unwrap();
    let (files, errors, warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    assert!(warnings > 0);
    assert!(!files.iter().any(|(name, _)| name == "Implementation.java"));
    assert!(
        coverage
            .untranslated
            .iter()
            .any(|name| name == "Implementation")
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn retains_property_smart_cast_boundary_used_by_kotlin() {
    let root = std::env::temp_dir().join(format!("notlin-smart-cast-{}", std::process::id()));
    let selected = root.join("selected");
    let residue = root.join("residue");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&selected).unwrap();
    fs::create_dir_all(&residue).unwrap();
    let selected_path = selected.join("Holder.kt");
    fs::write(
        &selected_path,
        "package sample\ninterface Value\nclass Detail(val text: String) : Value\ndata class Holder(val payload: Value)\n",
    )
    .unwrap();
    fs::write(
        residue.join("Consumer.kt"),
        "package sample\nfun render(holder: Holder): String {\n    if (holder.payload is Detail) return holder.payload.text\n    return \"\"\n}\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", selected_path.to_str().unwrap()]);
    let source = fs::read_to_string(&selected_path).unwrap();
    let (files, errors, warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &selected_path,
        &cli,
        Some(&index),
        std::slice::from_ref(&selected),
    );
    assert_eq!(errors, 0);
    assert!(warnings > 0);
    assert!(!files.iter().any(|(name, _)| name == "Holder.java"));
    assert!(coverage.untranslated.iter().any(|name| name == "Holder"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn discovery_reports_missing_workspace_root() {
    let error = SourceIndex::discover(&PathBuf::from("definitely-missing-workspace"))
        .expect_err("missing roots must not silently produce an empty index");
    assert!(error.contains("definitely-missing-workspace"));
}
