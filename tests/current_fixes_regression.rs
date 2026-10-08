use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use std::path::PathBuf;

fn transpile(source: &str, file_name: &str) -> String {
    transpile_with_args(source, file_name, &[file_name])
}

fn transpile_lombok(source: &str, file_name: &str) -> String {
    transpile_with_args(source, file_name, &["--lombok", file_name])
}

fn transpile_allow_approximations(source: &str, file_name: &str) -> String {
    transpile_with_args(source, file_name, &["--allow-approximations", file_name])
}

fn transpile_with_args(source: &str, file_name: &str, args: &[&str]) -> String {
    let cli = Cli::parse_from(std::iter::once("notlin").chain(args.iter().copied()));
    let (files, errors, _warnings, _coverage) =
        transpiler::transpile(source, &PathBuf::from(file_name), &cli);
    assert_eq!(errors, 0, "translation reported errors");
    let java_name = file_name.replace(".kt", ".java");
    files
        .into_iter()
        .find(|(name, _)| name == &java_name)
        .map(|(_, java)| java)
        .unwrap_or_else(|| panic!("expected Java output for {file_name}"))
}

#[test]
fn repaired_jvm_field_constructor_property_keeps_explicit_getter() {
    let java = transpile(
        r#"package neutral.regression
data class Item(final @JvmField val x: String) {
    fun getX(): String = x
}
"#,
        "Item.kt",
    );

    assert!(
        !java.contains("@JvmField"),
        "Kotlin annotation leaked into Java:\n{java}"
    );
    assert_eq!(
        java.matches("getX(").count(),
        1,
        "the explicit getter must be emitted once, without an added duplicate:\n{java}"
    );
    assert!(
        !java.contains("return this.getX();") && !java.contains("return getX();"),
        "the getter body must read the backing field instead of recursively calling itself:\n{java}"
    );
}

#[test]
fn repaired_data_class_normalizes_plain_and_computed_property_bridges() {
    let java = transpile_lombok(
        r##"package neutral.regression
open class Identifier
class LookupIdentifier(val value: String) : Identifier()
interface UserType
interface ApplicationUserType : UserType
interface User { val username: String; val type: UserType }
interface ApplicationUser : User { override val type: ApplicationUserType }
interface Request { val id: Identifier }
data class UserDto(
    final @JvmField val username: String,
    final @JvmField val type: ApplicationUserType
) : ApplicationUser, Request {
    @get:kotlin.jvm.JvmName("notlinPropertygetId") val id: Identifier
        get() = LookupIdentifier(username)
    override fun getUsername(): String = username
    override fun getType(): ApplicationUserType = type
    override fun getId(): Identifier = id
}
"##,
        "UserDto.kt",
    );

    for getter in [" getUsername() {", " getType() {", " getId() {"] {
        assert_eq!(
            java.matches(getter).count(),
            1,
            "repair bridge must collapse into one generated accessor for {getter}:\n{java}"
        );
    }
    assert!(
        java.contains("return new LookupIdentifier(") && java.contains("public Identifier getId()"),
        "computed property getter body must survive bridge normalization:\n{java}"
    );
    assert!(
        !java.contains("return this.getUsername();") && !java.contains("return this.getType();"),
        "plain property repair bridges must not become recursive:\n{java}"
    );
}

#[test]
fn repaired_constructor_property_does_not_duplicate_setter() {
    let java = transpile(
        r#"package neutral.regression
class Item(final @JvmField var enabled: Boolean) {
    fun setEnabled(value: Boolean) { enabled = value }
}
"#,
        "Item.kt",
    );

    assert_eq!(
        java.matches("void setEnabled(").count(),
        1,
        "the constructor-property setter and repaired bridge represent one Java method:\n{java}"
    );
}

#[test]
fn repaired_enum_can_translate_on_a_later_pass_without_duplicate_accessors() {
    let java = transpile(
        r#"package neutral.regression
interface Api { fun getBaseAlias(): String }
enum class Item(final @JvmField val alias: String) : Api {
    ONE("one");

    @get:kotlin.jvm.JvmName("notlinPropertygetBaseAlias") val baseAlias: String
        get() = alias + "/item"

    override fun getBaseAlias(): String = baseAlias
    fun getAlias(): String = alias
}
"#,
        "Item.kt",
    );

    assert_eq!(
        java.matches("getBaseAlias(").count(),
        1,
        "the repaired property and its temporary bridge must become one Java getter:\n{java}"
    );
    assert_eq!(
        java.matches("getAlias(").count(),
        1,
        "an explicit enum constructor-property getter must suppress the generated getter:\n{java}"
    );
    assert!(
        !java.contains("JvmName"),
        "repair annotation leaked into Java:\n{java}"
    );
}

#[test]
fn interface_default_getter_reads_sibling_property_through_its_accessor() {
    let java = transpile_allow_approximations(
        "package neutral.regression\ninterface Api {\n    fun getCategory(): Category\n    val baseAlias: String get() = category.getKey()\n}\n",
        "Api.kt",
    );
    assert!(java.contains("this.getCategory().getKey()"), "{java}");
}

#[test]
fn reflection_in_local_initializers_is_wrapped_as_an_unchecked_failure() {
    let java = transpile_allow_approximations(
        "package neutral.regression\ninterface Api {\n    fun create(type: Class<*>): Any {\n        val value = type.getConstructor().newInstance()\n        return value\n    }\n}\n",
        "Api.kt",
    );
    assert!(java.contains("catch (Exception e)"), "{java}");
    assert!(java.contains("throw new RuntimeException(e);"), "{java}");
}

#[test]
fn unit_expression_body_wrappers_do_not_return_void_calls() {
    let java = transpile_allow_approximations(
        r#"package neutral.regression
class Box {
    val values: MutableList<String> = mutableListOf()
    fun add(value: String) { values.add(value) }
    fun blockWrapper(value: String) { return add(value) }
    fun expressionWrapper(value: String) = add(value)
}
"#,
        "Box.kt",
    );

    assert!(
        !java.contains("return add(") && !java.contains("return this.add("),
        "a Java void call cannot be returned as a value:\n{java}"
    );
    assert!(
        java.contains("void blockWrapper(") && java.contains("void expressionWrapper("),
        "Unit wrappers should remain void methods:\n{java}"
    );
}

#[test]
fn chained_collection_filters_stream_only_once() {
    let java = transpile_allow_approximations(
        r#"package neutral.regression
fun filtered(values: List<String>) = values
    .filter { it.isNotEmpty() }
    .filter { it.startsWith("a") }
"#,
        "Filters.kt",
    );

    let stream_calls = java.matches(".stream()").count();
    assert!(
        stream_calls <= 1,
        "two chained filters must not start a second stream:\n{java}"
    );
    assert!(
        !java.contains(".stream().stream()"),
        "a stream receiver must not be streamed again:\n{java}"
    );
}

#[test]
fn explicit_java_stream_pipeline_is_not_collected_before_terminal() {
    let java = transpile_allow_approximations(
        r#"package neutral.regression
import java.util.Optional
fun first(values: List<String>): Optional<String> = values.stream()
    .filter { it.isNotEmpty() }
    .map { it.trim() }
    .findFirst()
fun all(values: List<String>): List<String> = values.stream()
    .filter { it.isNotEmpty() }
    .map { it.trim() }
    .toList()
"#,
        "Streams.kt",
    );

    assert!(
        !java.contains("collect(java.util.stream.Collectors.toList()).findFirst()")
            && !java.contains("collect(java.util.stream.Collectors.toList()).toList()"),
        "an explicit Java stream must stay a stream through its terminal operation:\n{java}"
    );
}

#[test]
fn optional_stream_return_closes_declaration_site_wildcard() {
    let java = transpile_allow_approximations(
        r#"package neutral.regression
import java.util.Optional
interface Nodes {
    val values: List<String>
    fun first(): Optional<String> = values.stream().findAny()
}
"#,
        "Nodes.kt",
    );

    assert!(
        java.contains("(Optional<String>) (Optional<?>)"),
        "an exact Optional<T> return must close a stream wildcard capture:\n{java}"
    );
}
