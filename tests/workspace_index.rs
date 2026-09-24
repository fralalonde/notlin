use clap::Parser;
use filetime::{FileTime, set_file_mtime};
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::{SourceIndex, SourceLanguage};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

fn symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(target, link);
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    match result {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::PermissionDenied => false,
        Err(error) => panic!("could not create test symlink: {error}"),
    }
}

fn symlink_dir(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_dir(target, link);
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    match result {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::PermissionDenied => false,
        Err(error) => panic!("could not create test symlink: {error}"),
    }
}

#[test]
fn workspace_discovery_ignores_unrelated_dangling_symlinks() {
    let root = std::env::temp_dir().join(format!("notlin-dangling-link-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Types.kt"), "package sample\nclass Types\n").unwrap();
    if !symlink_file(Path::new("missing-target"), &root.join("unrelated-link")) {
        fs::remove_dir_all(root).unwrap();
        return;
    }

    let index = SourceIndex::discover(&root).unwrap();

    assert_eq!(index.declarations().next().unwrap().name, "Types");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn cache_directory_symlink_is_not_followed() {
    let root = std::env::temp_dir().join(format!("notlin-cache-link-{}", std::process::id()));
    let outside = std::env::temp_dir().join(format!("notlin-cache-outside-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(root.join("Types.kt"), "package sample\nclass Types\n").unwrap();
    if !symlink_dir(&outside, &root.join(".notlin")) {
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
        return;
    }

    let (index, stats) = SourceIndex::discover_with_stats(&root).unwrap();

    assert_eq!(index.declarations().next().unwrap().name, "Types");
    assert!(!stats.cache_written);
    assert!(!outside.join("index-v1.bin").exists());
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn cache_temporary_symlink_is_not_followed() {
    let root = std::env::temp_dir().join(format!("notlin-cache-temp-link-{}", std::process::id()));
    let outside =
        std::env::temp_dir().join(format!("notlin-cache-temp-outside-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(root.join(".notlin")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(root.join("Types.kt"), "package sample\nclass Types\n").unwrap();
    let sentinel = outside.join("sentinel");
    fs::write(&sentinel, "untouched").unwrap();
    let temporary = root
        .join(".notlin")
        .join(format!("index-v1.bin.tmp-{}", std::process::id()));
    if !symlink_file(&sentinel, &temporary) {
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
        return;
    }

    let (_, stats) = SourceIndex::discover_with_stats(&root).unwrap();

    assert!(!stats.cache_written);
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "untouched");
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn cache_file_symlink_is_not_followed() {
    let root = std::env::temp_dir().join(format!("notlin-cache-file-link-{}", std::process::id()));
    let outside =
        std::env::temp_dir().join(format!("notlin-cache-file-outside-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(root.join(".notlin")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(root.join("Types.kt"), "package sample\nclass Types\n").unwrap();
    let sentinel = outside.join("sentinel");
    fs::write(&sentinel, "untouched").unwrap();
    if !symlink_file(&sentinel, &root.join(".notlin/index-v1.bin")) {
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
        return;
    }

    let (_, stats) = SourceIndex::discover_with_stats(&root).unwrap();

    assert!(!stats.cache_written);
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "untouched");
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn workspace_discovery_stops_at_symlink_cycles() {
    let root = std::env::temp_dir().join(format!("notlin-symlink-cycle-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("nested")).unwrap();
    fs::write(root.join("Types.kt"), "package sample\nclass Types\n").unwrap();
    if !symlink_dir(&root, &root.join("nested/back")) {
        fs::remove_dir_all(root).unwrap();
        return;
    }

    let index = SourceIndex::discover(&root).unwrap();

    assert_eq!(index.files.len(), 1);
    assert_eq!(index.declarations().next().unwrap().name, "Types");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn persistent_index_reuses_unchanged_sources() {
    let root = std::env::temp_dir().join(format!("notlin-index-cache-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Types.kt"), "package sample\nclass Types\n").unwrap();

    let (_, first) = SourceIndex::discover_with_stats(&root).unwrap();
    assert_eq!(first.parsed_files, 1);
    assert_eq!(first.reused_files, 0);
    assert!(root.join(".notlin/index-v1.bin").is_file());

    let (index, second) = SourceIndex::discover_with_stats(&root).unwrap();
    assert_eq!(second.parsed_files, 0);
    assert_eq!(second.reused_files, 1);
    assert_eq!(index.declarations().next().unwrap().name, "Types");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn persistent_index_detects_same_size_same_mtime_changes() {
    let root =
        std::env::temp_dir().join(format!("notlin-index-fingerprint-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let source = root.join("Types.kt");
    fs::write(&source, "package sample\nclass Before\n").unwrap();
    let original_mtime = FileTime::from_last_modification_time(&fs::metadata(&source).unwrap());
    SourceIndex::discover_with_stats(&root).unwrap();

    fs::write(&source, "package sample\nclass Afterx\n").unwrap();
    set_file_mtime(&source, original_mtime).unwrap();
    let (index, stats) = SourceIndex::discover_with_stats(&root).unwrap();

    assert_eq!(stats.parsed_files, 1);
    assert_eq!(stats.reused_files, 0);
    assert_eq!(index.declarations().next().unwrap().name, "Afterx");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn persistent_index_persists_metadata_only_updates_once() {
    let root = std::env::temp_dir().join(format!("notlin-index-touch-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let source = root.join("Types.kt");
    fs::write(&source, "package sample\nclass Types\n").unwrap();
    let original_mtime = FileTime::from_last_modification_time(&fs::metadata(&source).unwrap());
    SourceIndex::discover_with_stats(&root).unwrap();

    let touched_mtime = FileTime::from_unix_time(original_mtime.unix_seconds() + 2, 0);
    set_file_mtime(&source, touched_mtime).unwrap();
    let (_, second) = SourceIndex::discover_with_stats(&root).unwrap();
    let (_, third) = SourceIndex::discover_with_stats(&root).unwrap();

    assert_eq!(second.parsed_files, 0);
    assert_eq!(second.reused_files, 1);
    assert!(second.cache_written);
    assert!(!third.cache_written);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn oversized_index_cache_is_ignored_and_replaced() {
    let root = std::env::temp_dir().join(format!("notlin-index-oversized-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".notlin")).unwrap();
    fs::write(root.join("Types.kt"), "package sample\nclass Types\n").unwrap();
    let cache_path = root.join(".notlin/index-v1.bin");
    fs::File::create(&cache_path)
        .unwrap()
        .set_len(257 * 1024 * 1024)
        .unwrap();

    let (index, stats) = SourceIndex::discover_with_stats(&root).unwrap();

    assert_eq!(index.declarations().next().unwrap().name, "Types");
    assert_eq!(stats.parsed_files, 1);
    assert!(stats.cache_written);
    assert!(fs::metadata(&cache_path).unwrap().len() < 257 * 1024 * 1024);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn persistent_index_removes_deleted_sources() {
    let root = std::env::temp_dir().join(format!("notlin-index-delete-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let removed = root.join("Removed.kt");
    fs::write(&removed, "package sample\nclass Removed\n").unwrap();
    fs::write(root.join("Stable.kt"), "package sample\nclass Stable\n").unwrap();
    SourceIndex::discover_with_stats(&root).unwrap();

    fs::remove_file(&removed).unwrap();
    let (index, stats) = SourceIndex::discover_with_stats(&root).unwrap();

    assert_eq!(stats.parsed_files, 0);
    assert_eq!(stats.reused_files, 1);
    assert!(stats.cache_written);
    assert!(
        !index
            .declarations()
            .any(|declaration| declaration.name == "Removed")
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn persistent_index_reparses_only_changed_sources() {
    let root = std::env::temp_dir().join(format!("notlin-index-update-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let changed = root.join("Changed.kt");
    fs::write(&changed, "package sample\nclass Before\n").unwrap();
    fs::write(
        root.join("Stable.java"),
        "package sample; class Stable {}\n",
    )
    .unwrap();
    SourceIndex::discover_with_stats(&root).unwrap();

    fs::write(&changed, "package sample\nclass AfterChange\n").unwrap();
    let (index, stats) = SourceIndex::discover_with_stats(&root).unwrap();

    assert_eq!(stats.parsed_files, 1);
    assert_eq!(stats.reused_files, 1);
    assert!(
        index
            .declarations()
            .any(|declaration| declaration.name == "AfterChange")
    );
    assert!(
        !index
            .declarations()
            .any(|declaration| declaration.name == "Before")
    );

    fs::remove_dir_all(root).unwrap();
}

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
