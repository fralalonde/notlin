//! #5 vertical slice: a literal enum plus one companion `setOf` group becomes
//! Java statics with Kotlin's `setOf` semantics (shared, ordered, mutable), and
//! the DECLARED type is preserved (aliases expanded, never narrowed).

use clap::Parser;
use std::path::Path;

fn translate(source: &str) -> String {
    let cli = notlin::cli::Cli::parse_from(["notlin", "--annotations", "none", "Kind.kt"]);
    let (files, errors, _, _) = notlin::transpiler::transpile(source, Path::new("Kind.kt"), &cli);
    assert_eq!(errors, 0);
    files.into_iter().map(|(_, java)| java).collect()
}

fn enum_with_group(declared: &str) -> String {
    format!(
        "enum class Kind {{\n    K00, K01, K02, K03, K04, K05, K06, K07, K08, K09, K10, K11;\n    companion object {{\n        val GROUP: {declared} = setOf(K00, K01, K02, K03, K04, K05, K06, K07)\n    }}\n}}\n"
    )
}

#[test]
fn companion_set_of_group_becomes_shared_ordered_mutable_statics() {
    let java = translate(&format!(
        "package fixture\n\n{}",
        enum_with_group("Set<Kind>")
    ));
    assert!(
        java.contains("public static final java.util.Set<Kind> GROUP = new java.util.LinkedHashSet<Kind>(java.util.Arrays.<Kind>asList(K00, K01, K02, K03, K04, K05, K06, K07));"),
        "{java}"
    );
    assert!(
        java.contains("public static java.util.Set<Kind> getGROUP()"),
        "{java}"
    );
    assert!(!java.contains("setOf"), "{java}");
    // Declared entries only: the group stops at K07.
    assert!(
        !java.contains("asList(K00, K01, K02, K03, K04, K05, K06, K07, K08"),
        "{java}"
    );
}

#[test]
fn singleton_group_uses_java_immutable_factory() {
    let source = format!(
        "package fixture\n\n{}",
        enum_with_group("Set<Kind>").replace(
            "setOf(K00, K01, K02, K03, K04, K05, K06, K07)",
            "setOf(K00)"
        )
    );
    let java = translate(&source);
    assert!(
        java.contains("GROUP = java.util.Set.<Kind>of(K00);"),
        "{java}"
    );
}

#[test]
fn declared_collection_alias_is_expanded_and_not_narrowed() {
    let source = format!(
        "package fixture\n\ntypealias GroupSet<T> = kotlin.collections.Collection<T>\n\n{}",
        enum_with_group("GroupSet<Kind>")
    );
    let java = translate(&source);
    assert!(
        java.contains("public static final java.util.Collection<Kind> GROUP"),
        "{java}"
    );
    assert!(!java.contains("java.util.Set<Kind> GROUP"), "{java}");
}

#[test]
fn unsupported_companion_shapes_keep_kotlin() {
    // A genuine companion value use is not something statics can express.
    let java = translate(
        "package fixture\n\nenum class Kind { A, B; companion object { val GROUP: Set<Kind> = setOf(A, B) } }\n\nobject Holder { val holder = Kind.Companion }\n",
    );
    assert!(!java.contains("enum Kind"), "{java}");
}

#[test]
fn production_cli_retains_a_companion_used_as_a_value() {
    let root =
        std::env::temp_dir().join(format!("notlin-enum-group-consumer-{}", std::process::id()));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("Kind.kt"),
        format!("package fixture\n{}", enum_with_group("Set<Kind>")),
    )
    .unwrap();
    std::fs::write(
        root.join("Holder.kt"),
        "package fixture\nobject Holder { val holder = Kind.Companion }\n",
    )
    .unwrap();
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("NOTLIN_") {
            cmd.env_remove(key);
        }
    }
    let output = cmd
        .args(["--jvm-facts=off", "--in-place", "--lombok", "."])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !root.join("Kind.java").exists(),
        "companion value consumer requires retaining Kind"
    );
    assert!(
        !root.join("Holder.java").exists(),
        "unsupported companion value must retain its consumer too"
    );
}

#[test]
fn production_cli_keeps_java_companion_getter_callers_working() {
    let root = std::env::temp_dir().join(format!("notlin-enum-java-getter-{}", std::process::id()));
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("Kind.kt"),
        format!("package fixture\n{}", enum_with_group("Set<Kind>")),
    )
    .unwrap();
    std::fs::write(root.join("Consumer.java"), "package fixture; public class Consumer { public static java.util.Set<Kind> read() { return Kind.Companion.getGROUP(); } }\n").unwrap();
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_notlin"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("NOTLIN_") {
            cmd.env_remove(key);
        }
    }
    let output = cmd
        .args(["--jvm-facts=off", "--in-place", "--lombok", "."])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let java = std::fs::read_to_string(root.join("Kind.java"))
        .expect("supported Java getter must not retain the enum");
    assert!(
        java.contains("public static final class Companion"),
        "{java}"
    );
}

#[test]
fn another_factory_is_not_replaced_by_a_set_literal() {
    let source = format!(
        "package fixture\n{}\nfun makeGroup(vararg values: Kind): Set<Kind> = emptySet()\n",
        enum_with_group("Set<Kind>").replace("setOf(", "makeGroup(")
    );
    let java = translate(&source);
    assert!(
        !java.contains("public static final java.util.Set<Kind> GROUP"),
        "{java}"
    );
}

#[test]
fn group_must_list_entries_of_the_owning_enum() {
    let java = translate(
        "package fixture\n\nenum class Kind { A, B; companion object { val GROUP: Set<Kind> = listOf(A, B) } }\n",
    );
    // Not the proven `setOf` shape: the group proof does not apply.
    assert!(!java.contains("java.util.Arrays.<Kind>asList"), "{java}");
}
