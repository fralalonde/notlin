//! NOTLIN-MANUAL marker pass on retained .kt files.
//!
//! Generic fixtures (no target repo code): a Java class translated (as if by
//! notlin) must be visible to the marker pass so retained Kotlin consumers
//! get `NOTLIN-MANUAL:` comments for (1) stale smart-cast patterns on
//! Java-class properties, and (2) Kotlin `copy()` calls against Java
//! data classes. After a user applies the suggested local edit, a NEXT run
//! can translate more elements.

use notlin::manual_marks::annotate_manual_spots;
use notlin::workspace::SourceIndex;
use std::path::PathBuf;

fn fixture_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("notlin-manual-marks-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("mkdir");
    dir
}

/// Build an index over a tree with one .kt file. The pass needs a Java class
/// named `java_name` exposing the bean getter for `prop` — provide a .java stub.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
fn index_at(root: &PathBuf, java_name: &str, prop: &str) -> SourceIndex {
    std::fs::write(
        root.join(format!("src/{java_name}.java")),
        format!(
            "public class {java_name} {{ public int get{Prop}() {{ return 0; }} }}",
            Prop = capitalize(prop)
        ),
    )
    .unwrap();
    SourceIndex::discover(root).unwrap()
}

#[test]
fn smart_cast_over_java_property_gets_manual_marker() {
    let root = fixture_root("smartcast");
    let kt = root.join("src/consumer.kt");
    std::fs::write(
        &kt,
        "fun remove(node: Node) {\n\
         \x20   if(node.data is Basket) {\n\
         \x20       return drop(node.data.items)\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let index = index_at(&root, "Node", "data");
    let marks = annotate_manual_spots(&kt, &index);
    assert_eq!(marks, 1, "one smart-cast bundle expected");
    let text = std::fs::read_to_string(&kt).unwrap();
    assert!(text.contains("val data = node.data"));
    assert!(text.contains("if(data is Basket)"));
    assert!(text.contains("drop(data.items)"));
    assert!(!text.contains("NOTLIN-MANUAL:"));
    assert!(!text.contains("node.data.items"));
    assert_eq!(annotate_manual_spots(&kt, &index), 0);
}

#[test]
fn copy_call_on_java_data_class_gets_manual_marker() {
    let root = fixture_root("copycall");
    let kt = root.join("src/consumer.kt");
    std::fs::write(
        &kt,
        "data class Holder(val item: Item) {\n\
         \x20   fun bump(): Holder {\n\
         \x20       val item = item.copy(quantity = 2)\n\
         \x20       return copy(item = item)\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let index = index_at(&root, "Item", "quantity");
    let marks = annotate_manual_spots(&kt, &index);
    assert_eq!(marks, 1, "one copy() bundle expected");
    let text = std::fs::read_to_string(&kt).unwrap();
    assert!(text.contains("NOTLIN-MANUAL:"));
    assert!(text.contains("item.copy(quantity = 2)"));
    assert_eq!(annotate_manual_spots(&kt, &index), 0);
}

#[test]
fn copy_named_argument_rewrites_when_java_copy_constructor_is_indexed() {
    let root = fixture_root("copy-constructor-rewrite");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/manual_copy_constructor.java"),
        root.join("src/InventoryEntry.java"),
    )
    .unwrap();
    let kt = root.join("src/consumer.kt");
    std::fs::write(&kt, "fun bump(entry: InventoryEntry, quantity: Int): InventoryEntry = entry.copy(quantity = quantity)\n").unwrap();
    let index = SourceIndex::discover(&root).unwrap();

    assert_eq!(annotate_manual_spots(&kt, &index), 1);
    let text = std::fs::read_to_string(&kt).unwrap();
    assert!(text.contains("= InventoryEntry(entry, quantity)"), "{text}");
    assert!(!text.contains("entry.copy(quantity = quantity)"), "{text}");
}

#[test]
fn clean_kotlin_sources_get_no_markers() {
    let root = fixture_root("clean");
    let kt = root.join("src/consumer.kt");
    std::fs::write(
        &kt,
        "fun use(node: Node) {\n\
         \x20   val data = node.data\n\
         \x20   if(data is Basket) {\n\
         \x20       return drop(data.items)\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let index = index_at(&root, "Node", "data");
    assert_eq!(annotate_manual_spots(&kt, &index), 0);
}

#[test]
fn marker_insertion_preserves_crlf_and_terminal_newline() {
    let root = fixture_root("crlf");
    let kt = root.join("src/consumer.kt");
    let source = concat!(
        "fun remove(node: Node) {\x0d\x0a",
        "    if(node.data is Basket) {\x0d\x0a",
        "        return drop(node.data.items)\x0d\x0a",
        "    }\x0d\x0a",
        "}\x0d\x0a"
    );
    std::fs::write(&kt, source).unwrap();
    let index = index_at(&root, "Node", "data");

    assert_eq!(annotate_manual_spots(&kt, &index), 1);
    let bytes = std::fs::read(&kt).unwrap();
    assert!(bytes.windows(2).any(|pair| pair == b"\x0d\x0a"));
    assert!(
        !bytes
            .iter()
            .enumerate()
            .any(|(i, byte)| *byte == b'\x0a' && (i == 0 || bytes[i - 1] != b'\x0d'))
    );
    assert!(bytes.ends_with(b"\x0d\x0a"));
}

#[test]
fn repeated_property_smart_cast_rewrites_without_java_index_evidence() {
    let root = fixture_root("unrelated-getter");
    let kt = root.join("src/consumer.kt");
    std::fs::write(
        &kt,
        "fun remove(other: Other) {\n\
         \x20   if(other.data is Basket) {\n\
         \x20       return drop(other.data.items)\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();
    let index = index_at(&root, "Node", "data");

    assert_eq!(annotate_manual_spots(&kt, &index), 1);
    let text = std::fs::read_to_string(&kt).unwrap();
    assert!(text.contains("val data = other.data"));
    assert!(text.contains("drop(data.items)"));
}
