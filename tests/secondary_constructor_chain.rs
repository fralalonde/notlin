use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(label: &str) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "secondary-constructor-chain-{label}-{}-{id}",
                std::process::id()
            ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, name: &str, source: &str) {
        fs::write(self.0.join(name), source).unwrap();
    }

    fn translate(&self, lombok: bool) {
        let out = self.0.join("out");
        let mut inputs = fs::read_dir(&self.0)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
            .collect::<Vec<_>>();
        inputs.sort();
        let mut command = Command::new(env!("CARGO_BIN_EXE_notlin"));
        command
            .arg("--root")
            .arg(&self.0)
            .arg("--out-dir")
            .arg(&out)
            .args(["--annotations", "none"]);
        if lombok {
            command.arg("--lombok");
        }
        let output = command.args(inputs).output().expect("run notlin");
        assert!(
            output.status.success(),
            "notlin failed for Lombok={lombok}:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn translate_in_place(&self) {
        let mut inputs = fs::read_dir(&self.0)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "kt"))
            .collect::<Vec<_>>();
        inputs.sort();
        let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
            .arg("--root")
            .arg(&self.0)
            .args(["--in-place", "--annotations", "none"])
            .args(inputs)
            .output()
            .expect("run notlin");
        assert!(
            output.status.success(),
            "notlin failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn out(&self, name: &str) -> PathBuf {
        self.0.join("out").join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SITE_ID: &str = r#"package sample;
public final class SiteId {
    private final String value;
    public SiteId(String value) { this.value = value; }
    public String getValue() { return value; }
}
"#;

fn write_common(fixture: &Fixture, group: &str) {
    fixture.write("SiteId.java", SITE_ID);
    fixture.write("Marker.java", "package support; public class Marker {}\n");
    fixture.write(
        "Kind.kt",
        "package sample\nenum class Kind { FIRST, SECOND }\n",
    );
    fixture.write("Group.kt", group);
}

const POSITIVE_GROUP: &str = r#"package sample
data class Group(
    val site: SiteId,
    val type: Kind,
    val reference: String,
    val name: String
) {
    constructor(site: SiteId, type: Kind, reference: String) : this(site, type, reference, reference)
    constructor(site: SiteId, type: Kind) : this(site, type, type.name)
    val lookup: String
        get() {
            if (reference == type.name) {
                return "${site.value}/${type}"
            }
            return "${site.value}/${type}/$reference"
        }
}
"#;

#[test]
fn unique_secondary_constructor_chain_translates_in_both_lombok_modes() {
    for lombok in [false, true] {
        let fixture = Fixture::new(if lombok {
            "positive-lombok"
        } else {
            "positive"
        });
        write_common(
            &fixture,
            &POSITIVE_GROUP.replace("package sample", "package sample\nimport support.*"),
        );
        fixture.translate(lombok);

        let group_path = fixture.out("Group.java");
        assert!(group_path.is_file(), "Lombok={lombok}");
        let java = fs::read_to_string(group_path).unwrap();
        assert!(
            java.contains("public Group(SiteId site, Kind type)"),
            "{java}"
        );
        assert!(
            java.contains("public Group(SiteId site, Kind type, String reference)"),
            "{java}"
        );
        assert_eq!(java.matches("this(site, type,").count(), 2, "{java}");
        assert!(
            java.contains("this(site, type, reference, reference);")
                && java.contains("this(site, type, type.name());"),
            "each secondary constructor must delegate to the next unambiguous constructor:\n{java}"
        );
        assert!(java.contains("String getLookup()"), "{java}");
        assert!(
            !java.contains("return;"),
            "value getters must return their expressions:\n{java}"
        );
    }
}

fn assert_retained(label: &str, group: &str) {
    let fixture = Fixture::new(label);
    write_common(&fixture, group);
    fixture.translate_in_place();
    assert!(
        fixture.0.join("Group.kt").is_file(),
        "{label}: unsafe constructor chain must stay Kotlin"
    );
    assert!(
        !fixture.out("Group.java").exists(),
        "{label}: no partial Java class may be emitted"
    );
}

#[test]
fn ambiguous_or_nonpositional_secondary_chains_remain_kotlin() {
    assert_retained(
        "ambiguous-same-arity-targets",
        r#"package sample
interface Left
interface Right
class Both : Left, Right
fun both(): Both = Both()
class Group(val site: SiteId, val type: Kind, val reference: String, val name: String) {
    constructor(site: SiteId, type: Kind, marker: String) : this(site, type, both())
    constructor(site: SiteId, type: Kind, left: Left) : this(site, type, left.toString(), "left")
    constructor(site: SiteId, type: Kind, right: Right) : this(site, type, right.toString(), "right")
}
"#,
    );

    assert_retained(
        "primary-default-competing-arity",
        r#"package sample
data class Group(
    val site: SiteId,
    val type: Kind,
    val reference: String,
    val name: String,
    val note: String = "primary"
) {
    constructor(site: SiteId, type: Kind, reference: String) : this(site, type, reference, reference, "secondary")
    constructor(site: SiteId, type: Kind) : this(site, type, type.name)
}
"#,
    );

    assert_retained(
        "named-reordered-chain",
        r#"package sample
data class Group(val site: SiteId, val type: Kind, val reference: String, val name: String) {
    constructor(site: SiteId, type: Kind, reference: String) : this(site, type, reference, reference)
    constructor(site: SiteId, type: Kind) : this(type = type, site = site, reference = type.name)
}
"#,
    );

    assert_retained(
        "spread-vararg-chain",
        r#"package sample
data class Group(val site: SiteId, val type: Kind, val first: String, val second: String) {
    constructor(site: SiteId, type: Kind, vararg values: String) : this(site, type, values[0], values[1])
    constructor(site: SiteId, type: Kind, marker: Int, values: Array<out String>) : this(site, type, *values)
}
"#,
    );
}
