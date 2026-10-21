//! Offset alignment with the Track B helper.
//!
//! The helper keys its batch on `KtCallExpression::getTextOffset`, mapped to the
//! file's UTF-8 bytes. notlin asks about `kotlin_call_site_offset(call)`, which
//! is the start of the callee *selector* (`add` in `list.add(1)`, not `list`).
//! The two agree only if that node choice is right, so it is pinned twice:
//!
//! * [`selector_offsets_are_the_call_expression_start`] — no helper needed:
//!   the offset is the first byte of the callee text, and for a qualified call
//!   it is after the dot, never at the receiver.
//! * [`helper_batch_offsets_line_up_exactly`] — with a real full helper jar
//!   (`#[ignore]`d; needs `NOTLIN_JVM_FACTS_JAR` and a JDK whose VM supports
//!   K2 — Temurin 25 is what this repo builds against): every call expression
//!   in the fixture finds a site at exactly that offset, the helper's `text` is
//!   the source from that offset, and every site the helper reports is one of
//!   our call expressions.

use notlin::semantics::kotlin_call_site_offset;

const FIXTURE: &str = r#"package fixture

class Counter {
    private val seen = mutableListOf<String>()

    fun add(value: Int): String {
        seen.add(value.toString())
        return value.takeIf { it > 0 }?.let { "$it" } ?: "none"
    }
}
"#;

fn parse(source: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    parser.parse(source, None).unwrap()
}

/// `(selector byte offset, call node)` for every call in the fixture, ordered by
/// offset (the helper emits outer-first, so sorting makes the two lists
/// comparable).
fn calls<'a>(tree: &'a tree_sitter::Tree) -> Vec<(usize, tree_sitter::Node<'a>)> {
    let mut found = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "call_expression"
            && let Some(offset) = kotlin_call_site_offset(node)
        {
            found.push((offset, node));
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    found.sort_by_key(|(offset, _)| *offset);
    found
}

#[test]
fn selector_offsets_are_the_call_expression_start() {
    let tree = parse(FIXTURE);
    let found = calls(&tree);
    let texts = found
        .iter()
        .map(|(offset, _)| &FIXTURE[*offset..(*offset + 16).min(FIXTURE.len())])
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        vec![
            "mutableListOf<St",
            "add(value.toStri",
            "toString())\n    ",
            "takeIf { it > 0 ",
            "let { \"$it\" } ?:",
        ],
        "the callee selector offsets moved; the helper keys on the same node"
    );
    // A qualified call starts after its receiver: the dot is the byte before.
    for (offset, node) in &found {
        if node
            .named_child(0)
            .is_some_and(|callee| callee.kind() == "navigation_expression")
        {
            assert_eq!(
                &FIXTURE[offset - 1..*offset],
                ".",
                "expected a dot before {offset}"
            );
        } else {
            assert_eq!(*offset, node.start_byte());
        }
    }
}

/// Real helper, real Analysis API. Run with:
///
/// ```sh
/// NOTLIN_JVM_FACTS_JAR=jvm-facts/build/notlin-jvm-facts-full.jar \
/// NOTLIN_JAVA=~/.rsdk/tools/java/25.0.4-tem/bin/java \
///   cargo test --test facts_call_site_alignment -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires the built full helper jar and a JVM whose VM supports K2 (NOTLIN_JAVA)"]
fn helper_batch_offsets_line_up_exactly() {
    use notlin::facts::classpath::file_hash;
    use notlin::facts::client::FactsClient;
    use notlin::facts::runtime::{self, FactsRuntime};

    let Some(jar) = FactsClient::jar_path() else {
        eprintln!("NOTLIN_JVM_FACTS_JAR unset; nothing to align against");
        return;
    };
    let root = std::env::temp_dir().join(format!("notlin-offset-align-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("Counter.kt");
    std::fs::write(&file, FIXTURE).unwrap();
    runtime::configure_kotlin_sources([root.clone()]);

    let runtime = FactsRuntime::start(&jar).expect("helper starts");
    let batch = runtime.call_sites(&file).expect("call-site batch");
    let tree = parse(FIXTURE);
    let ours = calls(&tree);
    assert_eq!(
        batch.len(),
        ours.len(),
        "helper saw {} sites, tree-sitter saw {} calls: {:?}",
        batch.len(),
        ours.len(),
        batch.iter().map(|s| &s.text).collect::<Vec<_>>()
    );
    for ((offset, _), site) in ours.iter().zip(batch.iter()) {
        assert_eq!(
            site.byte_offset as usize, *offset,
            "offset mismatch for {:?}",
            site.text
        );
        assert!(
            FIXTURE[*offset..].starts_with(&site.text),
            "site text {:?} is not the source at {offset}",
            site.text
        );
    }
    // The hash guard refuses bytes that are not the file's.
    let hash = file_hash(&file).expect("file hash");
    assert!(
        runtime
            .call_site_at(&file, ours[1].0 as u64, &hash)
            .is_some()
    );
    assert!(
        runtime
            .call_site_at(&file, ours[1].0 as u64, &[0u8; 32])
            .is_none()
    );
}
