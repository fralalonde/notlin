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
                "returned-receiver-string-{label}-{}-{id}",
                std::process::id()
            ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, name: &str, source: &str) {
        fs::write(self.0.join(name), source).unwrap();
    }

    fn translate_in_place(&self, lombok: bool) -> std::process::Output {
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
            .args(["--in-place", "--annotations", "none"]);
        if lombok {
            command.arg("--lombok");
        }
        command.args(inputs).output().expect("run notlin")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const NOT_NULL: &str = r#"package org.jetbrains.annotations;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
@Retention(RetentionPolicy.CLASS)
@Target({ElementType.METHOD, ElementType.PARAMETER, ElementType.FIELD, ElementType.TYPE_USE})
public @interface NotNull {}
"#;

const JAVA_PROVIDER: &str = r#"package sample;
import java.util.UUID;
import org.jetbrains.annotations.NotNull;
public interface Provider {
    @NotNull UUID readId();
}
"#;

const POSITIVE_KOTLIN: [(&str, &str); 4] = [
    (
        "Trace.kt",
        r#"package sample
class Trace {
    var calls: Int = 0
}
"#,
    ),
    (
        "ProviderImpl.kt",
        r#"package sample
class ProviderImpl(private val trace: Trace) : Provider {
    override fun readId(): java.util.UUID {
        trace.calls += 1
        return java.util.UUID(0L, trace.calls.toLong())
    }

    suspend fun retainedMarker() {}
}
"#,
    ),
    (
        "Snapshot.kt",
        r#"package sample
data class Snapshot(val value: String) {
    constructor(provider: Provider) : this(provider.readId().toString())
}
"#,
    ),
    (
        "Runner.kt",
        r#"package sample
fun main() {
    suspend fun retainedHarness() {}
    val trace = Trace()
    val snapshot = Snapshot(ProviderImpl(trace))
    println("${snapshot.value}|${trace.calls}")
}
"#,
    ),
];

fn write_positive(fixture: &Fixture) {
    fixture.write("NotNull.java", NOT_NULL);
    fixture.write("Provider.java", JAVA_PROVIDER);
    for (name, source) in POSITIVE_KOTLIN {
        fixture.write(name, source);
    }
}

fn assert_retained(label: &str, kotlin_sources: &[(&str, &str)], java_sources: &[(&str, &str)]) {
    let fixture = Fixture::new(label);
    for (name, source) in java_sources {
        fixture.write(name, source);
    }
    for (name, source) in kotlin_sources {
        fixture.write(name, source);
    }
    let translated = fixture.translate_in_place(false);
    assert!(
        translated.status.success(),
        "{label}: {}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(
        fixture.0.join("Snapshot.kt").is_file(),
        "{label}: uncertain receiver result must keep the Kotlin caller"
    );
    assert!(
        !fixture.0.join("Snapshot.java").exists(),
        "{label}: no partial Java caller may be emitted"
    );
}

#[test]
fn nonnull_source_result_to_string_translates_in_both_lombok_modes() {
    for lombok in [false, true] {
        let fixture = Fixture::new(if lombok {
            "positive-lombok"
        } else {
            "positive"
        });
        write_positive(&fixture);
        let translated = fixture.translate_in_place(lombok);
        assert!(
            translated.status.success(),
            "Lombok={lombok}: {}",
            String::from_utf8_lossy(&translated.stderr)
        );
        assert!(fixture.0.join("Snapshot.java").is_file());
        assert!(fixture.0.join("ProviderImpl.kt").is_file());
        let java = fs::read_to_string(fixture.0.join("Snapshot.java")).unwrap();
        assert!(
            java.contains("provider.readId().toString()"),
            "the non-null typed source result should lower to a direct Java chain:\n{java}"
        );
    }
}

#[test]
fn platform_source_result_to_string_preserves_virtual_dispatch_in_both_modes() {
    for lombok in [false, true] {
        let fixture = Fixture::new("platform");
        fixture.write(
            "Provider.java",
            &JAVA_PROVIDER.replace("@NotNull UUID", "UUID"),
        );
        fixture.write("Snapshot.kt", POSITIVE_KOTLIN[2].1);
        let translated = fixture.translate_in_place(lombok);
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        let java = fs::read_to_string(fixture.0.join("Snapshot.java")).unwrap();
        assert!(java.contains("provider.readId().toString()"), "{java}");
    }
}

#[test]
fn nullable_source_result_to_string_uses_the_object_overload_in_both_modes() {
    for lombok in [false, true] {
        let fixture = Fixture::new("nullable");
        fixture.write(
            "Provider.java",
            &JAVA_PROVIDER.replace("@NotNull UUID", "@org.jetbrains.annotations.Nullable UUID"),
        );
        fixture.write("Snapshot.kt", POSITIVE_KOTLIN[2].1);
        let translated = fixture.translate_in_place(lombok);
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        let java = fs::read_to_string(fixture.0.join("Snapshot.java")).unwrap();
        assert!(
            java.contains("java.lang.String.valueOf((java.lang.Object) provider.readId())"),
            "{java}"
        );
    }
}

#[test]
fn caller_wildcards_do_not_hide_proven_platform_or_nonnull_members() {
    for annotation in ["", "@NotNull"] {
        for lombok in [false, true] {
            let fixture = Fixture::new("caller-wildcard");
            fixture.write("NotNull.java", NOT_NULL);
            fixture.write(
                "Provider.java",
                &JAVA_PROVIDER.replace("@NotNull", annotation),
            );
            fixture.write("Marker.java", "package unrelated; public class Marker {}\n");
            fixture.write(
                "Snapshot.kt",
                &POSITIVE_KOTLIN[2]
                    .1
                    .replace("package sample\n", "package sample\nimport unrelated.*\n"),
            );
            let translated = fixture.translate_in_place(lombok);
            assert!(
                translated.status.success(),
                "{}",
                String::from_utf8_lossy(&translated.stderr)
            );
            let java = fs::read_to_string(fixture.0.join("Snapshot.java")).unwrap();
            assert!(java.contains("provider.readId().toString()"), "{java}");
        }
    }
    let fixture = Fixture::new("nullable-caller-wildcard");
    fixture.write(
        "Provider.java",
        &JAVA_PROVIDER.replace("@NotNull UUID", "@org.jetbrains.annotations.Nullable UUID"),
    );
    fixture.write("Marker.java", "package unrelated; public class Marker {}\n");
    fixture.write(
        "Snapshot.kt",
        &POSITIVE_KOTLIN[2]
            .1
            .replace("package sample\n", "package sample\nimport unrelated.*\n"),
    );
    let translated = fixture.translate_in_place(false);
    assert!(
        translated.status.success(),
        "{}",
        String::from_utf8_lossy(&translated.stderr)
    );
    assert!(fixture.0.join("Snapshot.kt").is_file());
    assert!(!fixture.0.join("Snapshot.java").exists());
}

#[test]
fn same_file_extensions_cannot_replace_planned_jdk_member_dispatch() {
    for lombok in [false, true] {
        let fixture = Fixture::new("same-file-extension");
        fixture.write(
            "Provider.java",
            &JAVA_PROVIDER.replace("@NotNull UUID", "UUID"),
        );
        fixture.write(
            "Snapshot.kt",
            &format!(
                "{}\nfun java.util.UUID?.toString(): String = \"wrong\"\n",
                POSITIVE_KOTLIN[2].1
            ),
        );
        let translated = fixture.translate_in_place(lombok);
        assert!(
            translated.status.success(),
            "{}",
            String::from_utf8_lossy(&translated.stderr)
        );
        let java = fs::read_to_string(fixture.0.join("Snapshot.java")).unwrap();
        assert!(java.contains("provider.readId().toString()"), "{java}");
    }
}

#[test]
fn uncertain_or_non_instance_source_results_keep_the_caller_kotlin() {
    let snapshot = r#"package sample
import java.util.UUID
data class Snapshot(val value: String) {
    constructor(provider: Provider) : this(provider.readId().toString())
}
"#;
    let checked = r#"package sample;
import java.util.UUID;
import org.jetbrains.annotations.NotNull;
public interface Provider {
    @NotNull UUID readId() throws java.io.IOException;
}
"#;
    assert_retained(
        "checked-exception",
        &[("Snapshot.kt", snapshot)],
        &[("NotNull.java", NOT_NULL), ("Provider.java", checked)],
    );

    for parent_annotation in ["@org.jetbrains.annotations.Nullable", "@NotNull"] {
        assert_retained(
            "unannotated-override",
            &[("Snapshot.kt", snapshot)],
            &[
                ("NotNull.java", NOT_NULL),
                (
                    "Base.java",
                    &format!(
                        "package sample; import java.util.UUID; import org.jetbrains.annotations.NotNull; public interface Base {{ {parent_annotation} UUID readId(); }}"
                    ),
                ),
                (
                    "Provider.java",
                    "package sample; public interface Provider extends Base { java.util.UUID readId(); }",
                ),
            ],
        );
    }

    let generic = r#"package sample;
import org.jetbrains.annotations.NotNull;
public interface Provider<T> {
    @NotNull T readId();
}
"#;
    assert_retained(
        "generic-return",
        &[(
            "Snapshot.kt",
            &snapshot.replace("Provider)", "Provider<java.util.UUID>)"),
        )],
        &[("NotNull.java", NOT_NULL), ("Provider.java", generic)],
    );

    assert_retained(
        "source-alias",
        &[
            (
                "Provider.kt",
                "package source\nclass SourceProvider { fun readId(): java.util.UUID = java.util.UUID(0L, 1L); suspend fun retainedMarker() {} }\n",
            ),
            (
                "Snapshot.kt",
                "package sample\nimport source.SourceProvider as Provider\ndata class Snapshot(val value: String) {\n    constructor(provider: Provider) : this(provider.readId().toString())\n}\n",
            ),
        ],
        &[],
    );

    assert_retained(
        "private-extension",
        &[
            (
                "Provider.kt",
                "package sample\nclass Provider { suspend fun retainedMarker() {} }\n",
            ),
            (
                "Snapshot.kt",
                "package sample\nprivate fun Provider.readId(): java.util.UUID = java.util.UUID(0L, 1L)\ndata class Snapshot(val value: String) {\n    constructor(provider: Provider) : this(provider.readId().toString())\n}\n",
            ),
        ],
        &[],
    );

    assert_retained(
        "static-source",
        &[(
            "Snapshot.kt",
            "package sample\nimport java.util.UUID\ndata class Snapshot(val value: String) {\n    constructor(marker: Int) : this(Provider.readId().toString())\n}\n",
        )],
        &[
            ("NotNull.java", NOT_NULL),
            (
                "Provider.java",
                "package sample; import java.util.UUID; import org.jetbrains.annotations.NotNull; public final class Provider { @NotNull public static UUID readId() { return new UUID(0L, 1L); } }\n",
            ),
        ],
    );

    assert_retained(
        "changed-jvm-name",
        &[
            (
                "Provider.kt",
                "package sample\nclass Provider { @kotlin.jvm.JvmName(\"renamedReadId\") fun readId(): java.util.UUID = java.util.UUID(0L, 1L); suspend fun retainedMarker() {} }\n",
            ),
            ("Snapshot.kt", snapshot),
        ],
        &[],
    );

    assert_retained(
        "unknown-producer-import",
        &[(
            "Snapshot.kt",
            "package sample\nimport missing.Provider\nimport java.util.UUID\ndata class Snapshot(val value: String) {\n    constructor(provider: Provider) : this(provider.readId().toString())\n}\n",
        )],
        &[],
    );

    let nullable_provider = r#"package sample
data class Snapshot(val value: String) {
    constructor(provider: Provider?) : this(provider?.readId().toString())
}
"#;
    assert_retained(
        "nullable-receiver",
        &[("Snapshot.kt", nullable_provider)],
        &[("NotNull.java", NOT_NULL), ("Provider.java", JAVA_PROVIDER)],
    );

    let unknown_receiver = r#"package sample
import java.util.UUID
data class Snapshot(val value: String) {
    constructor(provider: Any) : this((provider as Provider).readId().toString())
}
"#;
    assert_retained(
        "unknown-receiver",
        &[("Snapshot.kt", unknown_receiver)],
        &[("NotNull.java", NOT_NULL), ("Provider.java", JAVA_PROVIDER)],
    );
}
