use clap::Parser;
use std::path::Path;

fn java(source: &str, name: &str) -> String {
    let cli = notlin::cli::Cli::parse_from(["notlin", "fixture.kt"]);
    let (files, errors, _, _) =
        notlin::transpiler::transpile(source, Path::new("fixture.kt"), &cli);
    assert_eq!(errors, 0);
    files
        .into_iter()
        .find(|(file, _)| file == &format!("{name}.java"))
        .unwrap_or_else(|| panic!("{name} retained"))
        .1
}

fn java_allow_approximations(source: &str, name: &str) -> String {
    let cli = notlin::cli::Cli::parse_from(["notlin", "--allow-approximations", "fixture.kt"]);
    let (files, errors, _, _) =
        notlin::transpiler::transpile(source, Path::new("fixture.kt"), &cli);
    assert_eq!(errors, 0);
    files
        .into_iter()
        .find(|(file, _)| file == &format!("{name}.java"))
        .unwrap_or_else(|| panic!("{name} retained"))
        .1
}

#[test]
fn lombok_keeps_data_initialization_convertible_with_explicit_java_fallback() {
    for source in [
        "data class Entry(val count: Int) { init { if (count < 0) throw IllegalArgumentException(\"negative\") } }",
        "data class Entry(var count: Int) { init { count = count + 1 }; val saved: Int = count }",
        "data class Entry(val count: Int) { val saved: Int = count + 1 }",
    ] {
        let cli = notlin::cli::Cli::parse_from([
            "notlin",
            "--lombok",
            "--annotations",
            "none",
            "fixture.kt",
        ]);
        let (files, errors, _, _) =
            notlin::transpiler::transpile(source, Path::new("fixture.kt"), &cli);
        assert_eq!(errors, 0);
        let output = &files
            .iter()
            .find(|(name, _)| name == "Entry.java")
            .expect("Lombok must not change data-class convertibility")
            .1;
        assert!(output.contains("final class Entry"), "{output}");
        assert!(
            !output.contains("@Value") && !output.contains("@Data"),
            "{output}"
        );
        assert!(output.contains("this.count = count;"), "{output}");
        assert!(output.contains("__notlin_initializeData"), "{output}");
        assert!(!java(source, "Entry").is_empty());
    }
}

#[test]
fn record_secondary_constructors_preserve_constructor_calls() {
    let output = java(
        r#"data class Quantity(val amount: Int, val unit: String) {
        constructor(amount: Int) : this(amount, "each")
        constructor(other: Quantity, unit: String) : this(other.amount, unit)
    }"#,
        "Quantity",
    );
    assert!(
        output.contains("record Quantity(int amount, String unit)"),
        "{output}"
    );
    assert!(output.contains("public Quantity(int amount)"), "{output}");
    assert!(output.contains("this(amount, \"each\");"), "{output}");
    assert!(output.contains("this(other.amount(), unit);"), "{output}");
}

#[test]
fn mutable_data_class_preserves_setters_and_primary_property_identity() {
    let output = java(
        r#"data class Entry(var count: Int, val label: String) {
        var cached: Int = count
        init { cached = count + 1 }
        val display: String get() = label
    }"#,
        "Entry",
    );
    assert!(output.contains("final class Entry"), "{output}");
    assert!(
        output.contains("public void setCount(int count)"),
        "{output}"
    );
    let assign = output.find("this.count = count;").unwrap();
    let initialize = output.find("this.cached = count;").unwrap();
    assert!(assign < initialize, "{output}");
    assert!(!output.contains("private int cached ="), "{output}");
    assert!(
        output.contains("public Entry copy(int count, String label)"),
        "{output}"
    );
    assert!(output.contains("public int component1()"), "{output}");
    let equality = output
        .split("boolean equals(Object other)")
        .nth(1)
        .unwrap()
        .split("@Override")
        .next()
        .unwrap();
    assert!(!equality.contains("cached"), "{equality}");
    assert!(!equality.contains("display"), "{equality}");
}

#[test]
fn record_init_block_runs_after_components_are_assigned() {
    let output = java(
        r#"data class Entry(val count: Int) {
        init { if (count < 0) throw IllegalArgumentException("negative") }
    }"#,
        "Entry",
    );
    assert!(output.contains("record Entry(int count)"), "{output}");
    assert!(output.contains("this.count = count;"), "{output}");
    assert!(
        output.contains("throw new IllegalArgumentException(\"negative\")"),
        "{output}"
    );
}

#[test]
fn data_subclass_forwards_super_constructor_and_generates_value_methods() {
    let output = java(
        r#"open class Base(val value: String)
        data class Entry(val label: String, var count: Int) : Base(label)
    "#,
        "Entry",
    );
    assert!(output.contains("super(label);"), "{output}");
    assert!(output.contains("boolean equals(Object other)"), "{output}");
    assert!(!output.contains("super.equals"), "{output}");
}

#[test]
fn generic_data_class_copy_keeps_type_parameters() {
    let output = java("data class Entry<T>(val value: T)", "Entry");
    assert!(output.contains("public Entry<T> copy(T value)"), "{output}");
}

#[test]
fn record_init_preconditions_keep_lazy_failure_messages() {
    let output = java_allow_approximations(
        r#"data class Entry(val count: Int) {
        init { require(count >= 0) { "negative" }; check(count < 10) }
    }"#,
        "Entry",
    );
    assert!(
        output.contains("throw new IllegalArgumentException(String.valueOf(\"negative\"));"),
        "{output}"
    );
    assert!(
        output.contains("throw new IllegalStateException(String.valueOf(\"Check failed.\"));"),
        "{output}"
    );
    assert!(!output.contains("require("), "{output}");
}

#[test]
fn mutable_data_copy_uses_bean_accessors_for_unchanged_properties() {
    let output = java_allow_approximations(
        "data class Entry(var count: Int, val label: String) { fun renamed(label: String): Entry = copy(label = label) }",
        "Entry",
    );
    assert!(
        output.contains("new Entry(this.getCount(), label)"),
        "{output}"
    );
}

#[test]
fn mutable_primary_properties_are_fields_during_initialization() {
    let output = java(
        "data class Entry(var count: Int) { init { count = count + 1 }; val saved: Int = count }",
        "Entry",
    );
    assert!(
        output.contains("private void __notlin_initializeData()"),
        "{output}"
    );
    let initialization = output
        .split("private void __notlin_initializeData()")
        .nth(1)
        .unwrap()
        .split("public int getCount")
        .next()
        .unwrap();
    assert!(
        initialization.contains("count = count + 1;"),
        "{initialization}"
    );
    assert!(
        initialization.contains("this.saved = count;"),
        "{initialization}"
    );
}

#[test]
fn enum_constants_to_list_needs_no_optional_library() {
    let output = java_allow_approximations(
        "data class Entry(val type: Class<out Enum<*>>) { fun values(): List<Enum<*>> = type.enumConstants.toList() }",
        "Entry",
    );
    assert!(
        output.contains("java.util.Arrays.asList(this.type().getEnumConstants())"),
        "{output}"
    );
}

#[test]
fn custom_data_equality_narrows_conjunction_receiver_and_hashes_nullable_values_once() {
    let output = java_allow_approximations(
        r#"data class Entry(val name: String?, var count: Int) {
        override fun equals(other: Any?): Boolean = other is Entry && other.name == name && other.count == count
        override fun hashCode(): Int { return name?.hashCode() ?: 0 }
    }"#,
        "Entry",
    );
    assert!(output.contains("((Entry) other).getName()"), "{output}");
    assert!(output.contains("((Entry) other).getCount()"), "{output}");
    assert!(
        output.contains("java.util.Objects.hashCode(this.getName())"),
        "{output}"
    );
    assert!(!output.contains("hashCode() != null"), "{output}");
}
