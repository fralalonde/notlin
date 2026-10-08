//! Kotlin's collection types must reach Java as their java.util equivalents.
//!
//! `MutableList<T>` and `List<T>` are the SAME JVM type (`java.util.List`), so
//! the Java form is the interface. Emitting `ArrayList`/`HashMap`/`HashSet`
//! instead changed the signature a Java override must reproduce — a retained
//! Kotlin interface member declared `fun addAll(items: MutableList<T>)` is
//! `List` in its descriptor and no Java method taking `ArrayList` overrides it
//! — and leaked a concrete implementation into every emitted field, getter and
//! parameter. `MutableCollection`, `MutableIterator`, `MutableListIterator` and
//! `MutableMap.MutableEntry` had no mapping at all, so those names reached Java
//! verbatim and the file did not compile.

use std::fs;
use std::path::Path;
use std::process::Command;

const FIXTURE: &str = r#"package neutral.collmap

class Repo(
    val mutableItems: MutableList<String>,
    val mutableIndex: MutableMap<String, Int>,
    val mutableTags: MutableSet<String>,
    val mutableBag: MutableCollection<String>,
    val mutableSeq: MutableIterable<String>,
    val nested: MutableMap<String, MutableList<Int>>,
    val plainItems: List<String>,
    val plainIndex: Map<String, Int>,
) {
    fun addAll(extra: MutableList<String>): MutableList<String> {
        mutableItems.addAll(extra)
        mutableBag.add("b")
        return mutableItems
    }

    fun walk(it: MutableIterator<String>, lit: MutableListIterator<String>): String {
        while (it.hasNext()) it.next()
        lit.add("z")
        return lit.previous()
    }

    fun entry(e: MutableMap.MutableEntry<String, Int>): MutableMap.MutableEntry<String, Int> = e

    fun local(): List<String> {
        val sink: MutableList<String> = mutableListOf()
        sink.add("x")
        val map: MutableMap<String, String> = mutableMapOf()
        map["k"] = "v"
        return sink
    }
}
"#;

#[test]
fn kotlin_collections_become_java_equivalents() {
    let root = Path::new("tests/tmp_scratch_collection_mapping");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("repo.kt"), FIXTURE).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args([
            "--root",
            root.to_str().unwrap(),
            "--in-place",
            "--allow-approximations",
        ])
        .arg(root.as_os_str())
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let java = fs::read_to_string(root.join("Repo.java")).unwrap_or_default();
    assert!(
        out.status.success() && !java.is_empty(),
        "notlin must translate the fixture.\nstderr:\n{stderr}"
    );

    // The declared type is the interface, in fields and in the constructor.
    for expected in [
        "private final List<String> mutableItems;",
        "private final Map<String,Integer> mutableIndex;",
        "private final Set<String> mutableTags;",
        "private final Collection<String> mutableBag;",
        "private final Iterable<String> mutableSeq;",
        "private final Map<String,List<Integer>> nested;",
        "public List<String> getMutableItems()",
        "public List<String> addAll(List<String> extra)",
        "public String walk(Iterator<String> it, ListIterator<String> lit)",
        "public Map.Entry<String,Integer> entry(Map.Entry<String,Integer> e)",
    ] {
        assert!(java.contains(expected), "expected `{expected}` in:\n{java}");
    }

    // A CONSTRUCTOR may stay concrete: `new ArrayList<>()` is the faithful
    // runtime type for `mutableListOf()`, and it satisfies the interface.
    assert!(
        java.contains("List<String> sink = new ArrayList<>();"),
        "the local's declared type is the interface while the construction stays concrete:\n{java}"
    );

    // No Kotlin collection name may survive into Java: every one of these is
    // either an unresolvable symbol for javac or a wrong type for an override.
    for leaked in [
        "MutableList",
        "MutableMap",
        "MutableSet",
        "MutableCollection",
        "MutableIterable",
        "MutableIterator",
        "MutableListIterator",
        "ArrayList<String> mutableItems",
        "HashMap<String,Integer> mutableIndex",
        "HashSet<String> mutableTags",
    ] {
        assert!(
            !java.contains(leaked),
            "`{leaked}` must not appear in the emitted Java:\n{java}"
        );
    }
    let _ = fs::remove_dir_all(root);
}
