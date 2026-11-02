use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "import-cache-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, source).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let owned = self.0.canonicalize().unwrap();
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .canonicalize()
            .unwrap();
        assert!(owned.starts_with(target));
        fs::remove_dir_all(&self.0).unwrap();
    }
}

// The pre-cache reader from c22931c is the differential and timing control.
fn uncached_binding(file: &SourceFile, written: &str) -> WrittenImport {
    let (root, rest) = written
        .split_once('.')
        .map_or((written, None), |(root, rest)| (root, Some(rest)));
    let root = root.trim_matches('`');
    if root.is_empty() {
        return WrittenImport::NotApplicable;
    }
    let source = file.source_text();
    let tree = crate::transpiler::parse_tree(source);
    let mut nodes = vec![tree.root_node()];
    let mut target = None;
    while let Some(node) = nodes.pop() {
        if node.kind() != "import" {
            nodes.extend(node.named_children(&mut node.walk()));
            continue;
        }
        let Some(header) = crate::semantics::java_member_calls::import_header(source, node) else {
            if node
                .utf8_text(source.as_bytes())
                .is_ok_and(|text| text.contains(root))
            {
                return WrittenImport::Unknown;
            }
            continue;
        };
        if header.wildcard {
            continue;
        }
        let binds = match header.alias.as_deref() {
            Some(alias) => alias == root,
            None => header.path.rsplit('.').next() == Some(root),
        };
        if !binds {
            continue;
        }
        let qualified = match rest {
            Some(rest) => format!("{}.{rest}", header.path),
            None => header.path,
        };
        if target.as_ref().is_some_and(|prior| prior != &qualified) {
            return WrittenImport::Unknown;
        }
        target = Some(qualified);
    }
    target.map_or(WrittenImport::NotApplicable, WrittenImport::Bound)
}

#[test]
fn classifier_bindings_follow_changed_aliases_and_stars_without_reusing_a_stale_snapshot() {
    let fixture = Fixture::new();
    fixture.write(
        "Alpha.kt",
        "package alpha\nopen class First\nopen class Second\n",
    );
    fixture.write("Beta.kt", "package beta\nopen class Second\n");
    let path = fixture.write("Use.kt", "package consumer\nimport\talpha.First as Pick\nimport beta.*\nimport alpha.First as Same\nimport alpha.Second as Same\nclass Model: Pick()\n");
    let index = SourceIndex::discover(&fixture.0).unwrap();
    let file = index.source_file(&path).unwrap();
    for _ in 0..3 {
        assert_eq!(
            canonical_type_binding(&index, file, "Pick")
                .unwrap()
                .qualified,
            "alpha.First"
        );
        assert_eq!(
            property_receiver_type_binding(&index, file, "Second")
                .unwrap()
                .qualified,
            "beta.Second"
        );
        assert!(canonical_type_binding(&index, file, "Same").is_none());
    }
    let replacement =
        "package consumer\nimport beta.Second as Pick\nimport alpha.*\nclass Model: Pick()\n";
    let overlay = index
        .with_overlays(&[SourceOverlay::Replace {
            path: path.clone(),
            language: SourceLanguage::Kotlin,
            source: replacement.into(),
        }])
        .unwrap();
    let current = overlay.source_file(&path).unwrap();
    assert_eq!(
        canonical_type_binding(&overlay, current, "Pick")
            .unwrap()
            .qualified,
        "beta.Second"
    );
    assert_eq!(
        property_receiver_type_binding(&overlay, current, "Second")
            .unwrap()
            .qualified,
        "alpha.Second"
    );
    assert_eq!(
        canonical_type_binding(&index, file, "Pick")
            .unwrap()
            .qualified,
        "alpha.First"
    );
    let mut unowned = file.clone();
    unowned.source_text = replacement.into();
    assert_eq!(
        structural_import_binding(&index, &unowned, "Pick"),
        WrittenImport::Bound("beta.Second".into())
    );
    let ambiguous = index
        .with_overlays(&[SourceOverlay::Replace {
            path: path.clone(),
            language: SourceLanguage::Kotlin,
            source: "package consumer\nimport alpha.*\nimport beta.*\nclass Model\n".into(),
        }])
        .unwrap();
    assert!(
        property_receiver_type_binding(&ambiguous, ambiguous.source_file(&path).unwrap(), "Second")
            .is_none()
    );
}

#[test]
fn cached_bindings_match_the_existing_reader_for_java_comments_and_malformed_headers() {
    let fixture = Fixture::new();
    for (name, source) in [
        (
            "Java.java",
            "package consumer; import alpha.First; import beta.Second; public class Java {}",
        ),
        (
            "Comment.kt",
            "package consumer\nimport\talpha.First /* comment */ as `Pick`\nclass Comment\n",
        ),
        (
            "Broken.kt",
            "package consumer\nimport alpha.First as\nclass Broken\n",
        ),
        (
            "Duplicate.kt",
            "package consumer\nimport alpha.First as Pick\nimport beta.Second as Pick\nclass Duplicate\n",
        ),
    ] {
        fixture.write(name, source);
    }
    let index = SourceIndex::discover(&fixture.0).unwrap();
    for file in &index.files {
        index.source_tree(file).unwrap();
        for name in [
            "First",
            "Second",
            "Pick",
            "Pick.Nested",
            "alpha",
            "class",
            "missing",
            "",
        ] {
            for _ in 0..3 {
                assert_eq!(
                    structural_import_binding(&index, file, name),
                    uncached_binding(file, name),
                    "{}: {name}",
                    file.path.display()
                );
            }
        }
    }
}

#[test]
#[ignore = "requires NOTLIN_IMPORT_BENCH_REFERENCE with an isolated reference snapshot"]
fn benchmark_reference_header_queries_against_the_uncached_reader() {
    use std::time::Instant;
    let root = PathBuf::from(std::env::var_os("NOTLIN_IMPORT_BENCH_REFERENCE").unwrap());
    let index = SourceIndex::discover(&root).unwrap();
    let cold = Instant::now();
    let mut queries = Vec::new();
    let mut selected = 0;
    for language in [SourceLanguage::Kotlin, SourceLanguage::Java] {
        let files = index
            .files
            .iter()
            .filter(|file| file.language == language)
            .collect::<Vec<_>>();
        let step = (files.len() / 64).max(1);
        for file in files.into_iter().step_by(step).take(64) {
            let mut names = vec!["notlinUnboundClassifier".to_owned()];
            for import in index.structural_imports(file).iter() {
                if let StructuralImport::Parsed(header) = import
                    && !header.wildcard
                {
                    names.push(
                        header
                            .alias
                            .as_deref()
                            .unwrap_or_else(|| header.path.rsplit('.').next().unwrap())
                            .to_owned(),
                    );
                }
            }
            names.sort();
            names.dedup();
            names.truncate(16);
            selected += 1;
            queries.extend(names.into_iter().map(|name| (file, name)));
        }
    }
    let cold_seconds = cold.elapsed().as_secs_f64();
    let expected = queries
        .iter()
        .map(|(file, name)| uncached_binding(file, name))
        .collect::<Vec<_>>();
    let mut rounds = Vec::new();
    for round in 0..3 {
        let mut sample = [0.0; 2];
        for mode in [round % 2, 1 - round % 2] {
            let started = Instant::now();
            for ((file, name), expected) in queries.iter().zip(&expected) {
                let actual = if mode == 0 {
                    uncached_binding(file, name)
                } else {
                    structural_import_binding(&index, file, name)
                };
                assert_eq!(&std::hint::black_box(actual), expected);
            }
            sample[mode] = started.elapsed().as_secs_f64();
        }
        rounds.push(serde_json::json!({"uncached_seconds":sample[0],"cached_seconds":sample[1]}));
    }
    let report = serde_json::json!({"indexed_files":index.files.len(),"sampled_files":selected,"queries":queries.len(),"cold_projection_seconds":cold_seconds,"rounds":rounds,"identical_bindings":true});
    if let Some(path) = std::env::var_os("NOTLIN_IMPORT_BENCH_REPORT") {
        fs::write(path, serde_json::to_string_pretty(&report).unwrap()).unwrap();
    }
    println!("IMPORT_BINDING_BENCH {report}");
}
