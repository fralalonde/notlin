use clap::Parser;
use filetime::{FileTime, set_file_mtime};
use notlin::cli::Cli;
use notlin::transpiler;
use notlin::workspace::{MemberConflictClass, SourceIndex, SourceLanguage, SourceOverlay};
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
fn relative_overlay_replaces_the_canonical_source_snapshot() {
    use notlin::semantics::{FactStatus, SemanticProvider, SyntaxSemanticProvider};
    let root = PathBuf::from(format!("target/overlay-identity-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("Symbols.kt");
    fs::write(&path, "package overlay\nclass Original\n").unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let updated = index
        .with_overlays(&[SourceOverlay::Replace {
            path: path.clone(),
            language: SourceLanguage::Kotlin,
            source: "package overlay\nclass Replacement\n".into(),
        }])
        .unwrap();
    assert_eq!(updated.kotlin_files().count(), 1);
    let provider = SyntaxSemanticProvider::new(
        updated
            .files
            .iter()
            .map(|file| (file.path.clone(), file.source_text().to_owned())),
    );
    assert_eq!(provider.resolve("Original"), FactStatus::Unknown);
    assert!(matches!(
        provider.resolve("Replacement"),
        FactStatus::Established(_)
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn workspace_selection_detects_when_every_kotlin_file_is_selected() {
    let root = std::env::temp_dir().join(format!("notlin-all-selected-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("first")).unwrap();
    fs::create_dir_all(root.join("second")).unwrap();
    fs::write(root.join("first/One.kt"), "package sample\nclass One\n").unwrap();
    fs::write(root.join("second/Two.kt"), "package sample\nclass Two\n").unwrap();

    let index = SourceIndex::discover(&root).unwrap();

    assert!(index.all_kotlin_selected(std::slice::from_ref(&root)));
    assert!(!index.all_kotlin_selected(&[root.join("first")]));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn retained_supertype_mismatch_reports_conflicting_member_names() {
    let root = std::env::temp_dir().join(format!("notlin-member-mismatch-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("contract.kt"),
        "package neutral.mismatch\ninterface Contract { val entries: List<String> }\n",
    )
    .unwrap();
    fs::write(
        root.join("implementation.kt"),
        "package neutral.mismatch\nclass Implementation(override val entries: List<Int>) : Contract\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let implementation = index
        .declarations()
        .find(|declaration| declaration.name == "Implementation")
        .expect("implementation indexed");

    let classifications =
        index.retained_supertype_member_conflicts(&implementation.supertypes, "Implementation");
    assert_eq!(classifications.len(), 1);
    assert_eq!(classifications[0].member_name, "entries");
    assert_eq!(
        classifications[0].classification,
        MemberConflictClass::InvariantGenericConflict
    );
    assert_eq!(
        index.retained_supertype_member_mismatches(&implementation.supertypes, "Implementation"),
        vec!["entries"]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn retained_supertype_members_have_structured_compatibility_classes() {
    let root = std::env::temp_dir().join(format!(
        "notlin-member-classifications-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    for (file, source) in [
        (
            "exact.kt",
            "package neutral.classification\ninterface ExactContract { val exact: String }\nclass ExactImplementation(override val exact: String) : ExactContract\n",
        ),
        (
            "covariant.kt",
            "package neutral.classification\nopen class Base\nclass Child : Base()\ninterface CovariantContract { val covariant: Base }\nclass CovariantImplementation(override val covariant: Child) : CovariantContract\n",
        ),
        (
            "generic.kt",
            "package neutral.classification\ninterface GenericContract<T> { val generic: T }\nclass GenericImplementation(override val generic: String) : GenericContract<String>\n",
        ),
        (
            "unrelated.kt",
            "package neutral.classification\nclass Left\nclass Right\ninterface UnrelatedContract { val unrelated: Left }\nclass UnrelatedImplementation(override val unrelated: Right) : UnrelatedContract\n",
        ),
        (
            "unknown.kt",
            "package neutral.classification\ninterface UnknownContract { val unknown: ExternalBase }\nclass UnknownImplementation(override val unknown: ExternalChild) : UnknownContract\n",
        ),
    ] {
        fs::write(root.join(file), source).unwrap();
    }

    let index = SourceIndex::discover(&root).unwrap();
    let classification = |class_name: &str| {
        let declaration = index
            .declarations()
            .find(|declaration| declaration.name == class_name)
            .unwrap_or_else(|| {
                panic!(
                    "{class_name} was not indexed; declarations: {:?}",
                    index
                        .declarations()
                        .map(|declaration| declaration.name.as_str())
                        .collect::<Vec<_>>()
                )
            });
        index
            .retained_supertype_member_conflicts(&declaration.supertypes, class_name)
            .into_iter()
            .next()
            .unwrap()
            .classification
    };

    assert_eq!(
        classification("ExactImplementation"),
        MemberConflictClass::Exact
    );
    assert_eq!(
        classification("CovariantImplementation"),
        MemberConflictClass::JavaCovariantReturn
    );
    let covariant = index
        .declarations()
        .find(|declaration| declaration.name == "CovariantImplementation")
        .unwrap();
    assert!(
        index
            .retained_supertype_member_mismatches(&covariant.supertypes, "CovariantImplementation")
            .is_empty(),
        "a Java-compatible covariant return must not become an N5258 root"
    );
    assert_eq!(
        classification("GenericImplementation"),
        MemberConflictClass::Exact
    );
    assert_eq!(
        classification("UnrelatedImplementation"),
        MemberConflictClass::UnrelatedReturnTypes
    );
    assert_eq!(
        classification("UnknownImplementation"),
        MemberConflictClass::UnknownType
    );
    fs::remove_dir_all(root).unwrap();
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
fn memberless_java_child_forwards_retained_kotlin_supertype() {
    let root =
        std::env::temp_dir().join(format!("notlin-retained-supertype-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("selected")).unwrap();
    fs::create_dir_all(root.join("residual")).unwrap();
    let child_path = root.join("selected/Leaf.kt");
    fs::write(&child_path, "package sample\ninterface Leaf : Base\n").unwrap();
    fs::write(
        root.join("residual/Base.kt"),
        "package sample\ninterface Base { fun consume(values: MutableIterable<Long>) }\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", child_path.to_str().unwrap()]);
    let source = fs::read_to_string(&child_path).unwrap();
    let retained = index
        .declarations()
        .filter(|d| d.name == "Base")
        .map(|d| notlin::semantics::workspace_symbol(&index, d))
        .collect();
    let (files, errors, warnings, coverage) = transpiler::transpile_with_workspace_hint(
        &source,
        &child_path,
        &cli,
        notlin::transpiler::WorkspaceScope {
            index: Some(&index),
            roots: &[root.join("selected")],
            retained_hint: Some(&retained),
            ..Default::default()
        },
        true,
    );
    assert_eq!(errors, 0);
    let leaf_java = files
        .iter()
        .find(|(name, _)| name.ends_with("Leaf.java"))
        .map(|(_, contents)| contents);
    assert!(
        leaf_java.is_some_and(|contents| contents.contains("interface Leaf extends Base")),
        "a memberless Java child should forward the retained Kotlin ABI: {files:?}"
    );
    assert_eq!(warnings, 0);
    assert!(!coverage.untranslated.iter().any(|name| name == "Leaf"));
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
    // The OLD conservative rule is restored (the selection-scope experiment
    // in subtype_scope_retention.rs proved over-reaching): an interface
    // retains whenever ANY Kotlin subtype exists, because a subtype retained
    // by an unrelated rule (annotation, enum entries ABI, KClass) cannot
    // implement a translated-away supertype. Ordered retention (decide
    // subtypes before supertypes) is the eventual unlock.
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
    let (files, errors, warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &contract_path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    assert!(warnings > 0);
    assert!(files.is_empty());
    assert!(coverage.untranslated.iter().any(|name| name == "Contract"));
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn retention_matches_canonical_index_to_input_path() {
    // Superseded conservative rule (see the sibling selected-implementation
    // test): with the subtype inside the selection, the interface now
    // translates together with its implementor. This test keeps its original
    // purpose — index path canonicalization — by asserting the retention
    // diagnostic STILL fires when the subtype lives outside the selection.
    let root = std::env::temp_dir().join(format!("notlin-canonical-path-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("selected")).unwrap();
    fs::create_dir_all(root.join("residual")).unwrap();
    let contract_path = root.join("selected").join("Contract.kt");
    fs::write(&contract_path, "package sample\ninterface Contract\n").unwrap();
    fs::write(
        root.join("residual").join("Implementation.kt"),
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
        std::slice::from_ref(&root.join("selected")),
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
    // A default on a parameter that is FOLLOWED by one without a default is
    // the only shape Java cannot express: trailing defaults become delegating
    // overloads, a middle default cannot be skipped by a positional caller.
    fs::write(
        &provider,
        "package sample\ndata class Configuration(val values: List<String> = emptyList(), val name: String)\n",
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
fn translates_trailing_default_constructor_used_by_kotlin_caller() {
    let root = std::env::temp_dir().join(format!("notlin-trailing-default-{}", std::process::id()));
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
    let (files, errors, _warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &provider,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    let java = files
        .iter()
        .find(|(name, _)| name == "Configuration.java")
        .map(|(_, text)| text.clone())
        .unwrap_or_else(|| {
            panic!(
                "a TRAILING default is expressible in Java as delegating overloads, so the class must translate; untranslated: {:?}",
                coverage.untranslated
            )
        });
    assert!(
        java.contains("public Configuration()"),
        "the omitted trailing argument must be exposed as a no-arg overload so Kotlin callers and Java call sites resolve:\n{java}"
    );
    assert!(
        java.contains("this("),
        "the overload must delegate to the full constructor with the declared default:\n{java}"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn translates_java_covariant_non_null_override_of_nullable_kotlin_contract() {
    let root = std::env::temp_dir().join(format!(
        "notlin-nullability-contract-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("Definitions.kt");
    fs::write(
        &path,
        "package sample\ninterface Base\nclass Child : Base\ninterface Contract {\n    val item: Base?\n}\ndata class Implementation(override val item: Child) : Contract\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", path.to_str().unwrap()]);
    let source = fs::read_to_string(&path).unwrap();
    let (files, errors, _warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &path,
        &cli,
        Some(&index),
        std::slice::from_ref(&root),
    );
    assert_eq!(errors, 0);
    let java = files
        .iter()
        .find(|(name, _)| name == "Implementation.java")
        .map(|(_, text)| text)
        .unwrap_or_else(|| {
            panic!(
                "a Java-return-substitutable nullable-to-non-null override must translate; untranslated: {:?}",
                coverage.untranslated
            )
        });
    assert!(
        java.contains("Child getItem()"),
        "the covariant getter must preserve the narrower return type:\n{java}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn translates_property_smart_cast_owner_repaired_by_the_rewrite_pass() {
    // Retained Kotlin that smart-casts a property no longer has to keep the
    // property's OWNER in Kotlin: the rewrite pass binds the read into a local
    // first, so the owner's translation costs the caller nothing.
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
    let (files, errors, _warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &selected_path,
        &cli,
        Some(&index),
        std::slice::from_ref(&selected),
    );
    assert_eq!(errors, 0);
    assert!(files.iter().any(|(name, _)| name == "Holder.java"));
    assert!(
        !coverage.untranslated.iter().any(|name| name == "Holder"),
        "the smart-cast boundary no longer retains the property owner: {:?}",
        coverage.untranslated
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn retains_property_smart_cast_the_rewrite_pass_cannot_reach() {
    // The receiver is a chain the pass cannot resolve to an owner, so nothing
    // proves the rewrite would fire: the property owner stays Kotlin rather than
    // becoming Java under a caller that would then fail to compile.
    let root =
        std::env::temp_dir().join(format!("notlin-smart-cast-unreach-{}", std::process::id()));
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
        "package sample\nfun render(holder: Holder, items: List<Holder>): String {\n    for (item in items) {\n        if (item.payload is Detail) return item.payload.text\n    }\n    return holder.payload.toString()\n}\n",
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
fn unrelated_smart_cast_with_same_property_name_does_not_retain_owner() {
    let root = std::env::temp_dir().join(format!(
        "notlin-smart-cast-unrelated-owner-{}",
        std::process::id()
    ));
    let selected = root.join("selected");
    let residue = root.join("residue");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&selected).unwrap();
    fs::create_dir_all(&residue).unwrap();
    let selected_path = selected.join("Rule.kt");
    fs::write(
        &selected_path,
        "package sample\ninterface Kind\ndata class Rule(val type: Kind)\n",
    )
    .unwrap();
    fs::write(
        residue.join("Consumer.kt"),
        "package sample\ninterface Value\nclass Detail(val text: String) : Value\ndata class Other(val type: Value)\nfun render(rule: Rule, other: Other): String {\n    if (other.type is Detail) return other.type.text\n    return rule.toString()\n}\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let cli = Cli::parse_from(["notlin", "--in-place", selected_path.to_str().unwrap()]);
    let source = fs::read_to_string(&selected_path).unwrap();
    let (files, errors, _warnings, coverage) = transpiler::transpile_with_workspace(
        &source,
        &selected_path,
        &cli,
        Some(&index),
        std::slice::from_ref(&selected),
    );
    assert_eq!(errors, 0);
    assert!(files.iter().any(|(name, _)| name == "Rule.java"));
    assert!(!coverage.untranslated.iter().any(|name| name == "Rule"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn discovery_reports_missing_workspace_root() {
    let error = SourceIndex::discover(&PathBuf::from("definitely-missing-workspace"))
        .expect_err("missing roots must not silently produce an empty index");
    assert!(error.contains("definitely-missing-workspace"));
}

#[test]
fn declarations_named_preserves_simple_name_collisions() {
    let root = std::env::temp_dir().join(format!("notlin-name-collision-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("left")).unwrap();
    fs::create_dir_all(root.join("right")).unwrap();
    fs::write(
        root.join("left/Contract.kt"),
        "package left\ninterface Contract\n",
    )
    .unwrap();
    fs::write(
        root.join("right/Contract.kt"),
        "package right\ninterface Contract\n",
    )
    .unwrap();

    let index = SourceIndex::discover(&root).unwrap();
    let packages: Vec<_> = index
        .declarations_named("Contract")
        .filter_map(|declaration| declaration.package.as_deref())
        .collect();

    assert_eq!(packages, ["left", "right"]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn virtual_overlay_rebuilds_without_changing_original() {
    let root = std::env::temp_dir().join(format!("notlin-overlay-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let root = fs::canonicalize(&root).unwrap();
    let base = root.join("Base.kt");
    let child = root.join("Child.kt");
    let caller = root.join("Caller.kt");
    fs::write(
        &base,
        "package sample
interface Base
",
    )
    .unwrap();
    fs::write(
        &child,
        "package sample
class Child : Base
",
    )
    .unwrap();
    fs::write(
        &caller,
        "package sample
fun make() = Old(1)
",
    )
    .unwrap();
    let original = SourceIndex::discover(&root).unwrap();
    let overlay = original
        .with_overlays(&[
            SourceOverlay::Replace {
                path: base.clone(),
                language: SourceLanguage::Kotlin,
                source: "package sample
interface Residual
"
                .into(),
            },
            SourceOverlay::Delete {
                path: child.clone(),
            },
            SourceOverlay::Replace {
                path: caller.clone(),
                language: SourceLanguage::Kotlin,
                source: "package sample
data class Fresh(val value: Int)
fun make() = Fresh(1)
"
                .into(),
            },
            SourceOverlay::Replace {
                path: root.join("Generated.java"),
                language: SourceLanguage::Java,
                source: "package sample; public class Generated {}".into(),
            },
        ])
        .unwrap();

    assert!(original.declarations_named("Base").next().is_some());
    assert!(overlay.declarations_named("Base").next().is_none());
    assert!(overlay.declarations_named("Residual").next().is_some());
    assert!(overlay.source_file(&child).is_none());
    assert!(overlay.declarations_named("Generated").next().is_some());
    let fresh = overlay.declarations_named("Fresh").next().unwrap();
    assert!(overlay.ctor_omission_evidence(fresh).patterns.is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn inherited_member_conflicts_use_each_declarations_package() {
    let root = std::env::temp_dir().join(format!("notlin-scoped-conflicts-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    for (file, source) in [
        (
            "left.kt",
            "package left\ninterface Contract { val type: String }\nclass Implementation(override val type: String) : Contract\n",
        ),
        (
            "right.kt",
            "package right\nimport left.*\ninterface Contract { val type: Int }\nclass Implementation(override val type: Int) : Contract\n",
        ),
        (
            "qualified.kt",
            "package use\ninterface Generic<T> { val value: T }\nclass Qualified(override val value: String) : use.Generic<String>\n",
        ),
    ] {
        fs::write(root.join(file), source).unwrap();
    }
    let index = SourceIndex::discover(&root).unwrap();
    for file in ["left.kt", "right.kt", "qualified.kt"] {
        let path = root.join(file);
        let source = index.source_file(&path).unwrap();
        let implementation = source.declarations.last().unwrap();
        let conflicts = index.retained_supertype_member_conflicts_in_file(
            &path,
            &implementation.supertypes,
            &implementation.name,
        );
        assert_eq!(conflicts.len(), 1, "{file}: {conflicts:?}");
        assert!(
            index
                .retained_supertype_member_mismatches_in_file(
                    &path,
                    &implementation.supertypes,
                    &implementation.name,
                )
                .is_empty(),
            "{file}: {conflicts:?}"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn explicit_generic_projections_allow_proven_covariant_returns() {
    let root =
        std::env::temp_dir().join(format!("notlin-projected-returns-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let source = r#"package projections
interface Base {}
interface Child : Base {}
interface Upper { val type: Class<out Base> }
class Narrow(override val type: Class<out Child>) : Upper {}
class Concrete(override val type: Class<Child>) : Upper {}
interface Star { val type: Class<*> }
class StarImpl(override val type: Class<Child>) : Star {}
interface Invariant { val type: Class<Base> }
class Invalid(override val type: Class<Child>) : Invariant {}
interface Unknown { val type: Class<out ExternalBase> }
class Unproven(override val type: Class<out ExternalChild>) : Unknown {}
interface Lower { val type: Box<in Child> }
class Wider(override val type: Box<Base>) : Lower {}
"#;
    for (i, declaration) in source.lines().skip(1).enumerate() {
        fs::write(
            root.join(format!("type{i}.kt")),
            format!("package projections\n{declaration}\n"),
        )
        .unwrap();
    }
    let index = SourceIndex::discover(&root).unwrap();
    for (name, allowed) in [
        ("Narrow", true),
        ("Concrete", true),
        ("StarImpl", true),
        ("Invalid", false),
        ("Unproven", false),
        ("Wider", true),
    ] {
        let declaration = index.declarations_named(name).next().unwrap_or_else(|| {
            panic!(
                "missing {name}: {:?}",
                index.declarations().map(|d| &d.name).collect::<Vec<_>>()
            )
        });
        let conflicts = index.retained_supertype_member_conflicts(&declaration.supertypes, name);
        assert_eq!(conflicts.len(), 1, "{name}");
        assert_eq!(
            index
                .retained_supertype_member_mismatches(&declaration.supertypes, name)
                .is_empty(),
            allowed,
            "{name}: {conflicts:?}"
        );
        if allowed {
            assert_eq!(
                conflicts[0].classification,
                MemberConflictClass::JavaCovariantReturn
            );
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn java_interface_ancestry_survives_translation_overlays() {
    let root = std::env::temp_dir().join(format!(
        "notlin-java-interface-ancestry-{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("Base.java"),
        "package ancestry; public interface Base {}\n",
    )
    .unwrap();
    fs::write(
        root.join("Middle.java"),
        "package ancestry; public interface Middle extends Base {}\n",
    )
    .unwrap();
    fs::write(root.join("types.kt"), "package ancestry\ninterface Leaf : Middle\ninterface Contract { val item: Base }\nclass Implementation(override val item: Leaf) : Contract\n").unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    assert_eq!(
        index
            .declarations_named("Middle")
            .next()
            .unwrap()
            .supertypes,
        ["Base"]
    );
    let implementation = index.declarations_named("Implementation").next().unwrap();
    assert!(
        index
            .retained_supertype_member_mismatches(&implementation.supertypes, "Implementation")
            .is_empty()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn enum_default_obligations_distinguish_single_defaults_and_bridges() {
    let root = std::env::temp_dir().join(format!(
        "notlin-enum-default-obligations-{}",
        std::process::id()
    ));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("defaults.kt"),
        r#"package defaults
interface Default {
    val alias: String
        get() = "default"
}
interface Other {
    val alias: String
        get() = "other"
}
interface Abstract : Default {
    override val alias: String
}
enum class Simple : Default { ONE; }
enum class Conflict : Default, Other { ONE; }
enum class Narrowed : Abstract { ONE; }
enum class Explicit : Default, Other {
    ONE;
    override val alias: String
        get() = "explicit"
}
interface CategoryDefault {
    val category: String
        get() = "category"
}
interface CategoryAbstract {
    val category: String
}
enum class Bridged : CategoryDefault, CategoryAbstract { ONE; }
"#,
    )
    .unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let path = root.join("defaults.kt");
    assert!(index.enum_default_obligations(&path, "Simple").is_empty());
    assert!(index.enum_default_obligations(&path, "Explicit").is_empty());
    assert_eq!(
        index.enum_default_obligations(&path, "Conflict"),
        ["getAlias"]
    );
    assert_eq!(
        index.enum_default_obligations(&path, "Narrowed"),
        ["getAlias"]
    );
    assert_eq!(
        index.enum_default_property_bridges(&path, "Bridged"),
        [notlin::workspace::EnumDefaultBridge {
            method_name: "getCategory".to_string(),
            return_type: "String".to_string(),
            provider: "CategoryDefault".to_string(),
        }]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn enum_default_bridge_preserves_first_direct_interface_branch() {
    let root =
        std::env::temp_dir().join(format!("notlin-enum-default-branch-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("defaults.kt"),
        r#"package defaults
interface Root {
    val baseAlias: String
        get() = "root"
}
interface Equipment : Root {
    override val baseAlias: String
        get() = "equipment"
}
interface Workstation : Equipment
interface Process : Root {
    override val baseAlias: String
        get() = super.baseAlias + "/process"
}
enum class WorkstationProcess : Workstation, Process { INSTANCE; }
"#,
    )
    .unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let path = root.join("defaults.kt");
    assert_eq!(
        index.enum_default_property_bridges(&path, "WorkstationProcess"),
        [notlin::workspace::EnumDefaultBridge {
            method_name: "getBaseAlias".to_string(),
            return_type: "String".to_string(),
            provider: "Workstation".to_string(),
        }]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn companion_constants_do_not_create_inherited_return_conflicts() {
    let root = std::env::temp_dir().join(format!("notlin-static-conflicts-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("types.kt"), "package statics\ninterface Parent {\n companion object {\n val EMPTY: String = \"\"\n }\n}\nclass Child : Parent {\n companion object {\n val EMPTY: Int = 0\n }\n}\n").unwrap();
    let index = SourceIndex::discover(&root).unwrap();
    let child = index.declarations_named("Child").next().unwrap();
    assert!(
        index
            .retained_supertype_member_mismatches(&child.supertypes, "Child")
            .is_empty()
    );
    fs::remove_dir_all(root).unwrap();
}
