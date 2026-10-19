use notlin::semantics::{
    FactStatus, SemanticProvider, SemanticSymbol, SyntaxSemanticProvider, plan_enum_when_lowerings,
};
use notlin::translation_plan::TranslationPlan;
use std::path::{Path, PathBuf};

fn provider(files: &[(&str, &str)]) -> SyntaxSemanticProvider {
    for (path, source) in files {
        let language = match Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some("kt") => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Some("java") => tree_sitter_java::LANGUAGE.into(),
            _ => continue,
        };
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(
            !tree.root_node().has_error(),
            "invalid fixture {path}:\n{source}"
        );
    }
    SyntaxSemanticProvider::new(
        files
            .iter()
            .map(|(path, source)| (PathBuf::from(path), (*source).to_owned()))
            .collect::<Vec<_>>(),
    )
}

fn plan(source: &str, file: &str, provider: &SyntaxSemanticProvider) -> TranslationPlan {
    plan_with_provider(source, file, provider)
}

fn plan_with_provider(
    source: &str,
    file: &str,
    provider: &SyntaxSemanticProvider,
) -> TranslationPlan {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let path = Path::new(file);
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), path);
    plan_enum_when_lowerings(source, &tree, path, provider, &mut plan);
    plan
}

fn enum_type<'a>(
    provider: &'a SyntaxSemanticProvider,
    file: &str,
    name: &str,
) -> &'a SemanticSymbol {
    provider
        .symbols()
        .iter()
        .find(|symbol| {
            symbol.id.file == Path::new(file) && symbol.id.kind == "enum" && symbol.id.name == name
        })
        .unwrap_or_else(|| panic!("missing enum {name} in {file}: {:#?}", provider.symbols()))
}

fn assert_no_lowering(source: &str, file: &str) {
    let index = provider(&[(file, source)]);
    let plan = plan(source, file, &index);
    assert!(
        plan.enum_when_lowerings.is_empty(),
        "unsafe enum when lowering was planned for {file}: {:#?}",
        plan.enum_when_lowerings
    );
}

fn verify_snapshot(source: &str, file: &str, plan: &TranslationPlan) {
    let expected_hash = *blake3::hash(source.as_bytes()).as_bytes();
    assert_eq!(plan.source_hash, expected_hash);
    for lowering in &plan.enum_when_lowerings {
        assert_eq!(lowering.location.file, Path::new(file));
        assert_eq!(lowering.subject.file, Path::new(file));
        assert_eq!(lowering.location.snapshot_hash, expected_hash);
        assert_eq!(lowering.subject.snapshot_hash, expected_hash);
        assert!(lowering.location.start_byte < lowering.location.end_byte);
        assert!(lowering.subject.start_byte < lowering.subject.end_byte);
        assert_eq!(
            &source[lowering.location.start_byte..lowering.location.end_byte][.."when".len()],
            "when"
        );
        for arm in &lowering.arms {
            assert_eq!(arm.result.file, Path::new(file));
            assert_eq!(arm.result.snapshot_hash, expected_hash);
            for label in &arm.labels {
                assert_eq!(label.kind, "enum_entry");
                assert_eq!(label.package, lowering.enum_owner.package);
                assert_eq!(label.file, lowering.enum_owner.file);
                assert_eq!(label.owner_path.last(), Some(&lowering.enum_owner.name));
            }
        }
        if let Some(default_result) = &lowering.default_result {
            assert_eq!(default_result.file, Path::new(file));
            assert_eq!(default_result.snapshot_hash, expected_hash);
        }
    }
}

#[test]
fn enum_parameter_plans_exhaustive_grouped_labels_and_exact_identities() {
    let file = "Mode.kt";
    let source = r#"package enumplan
enum class Mode {
    FAST,
    SAFE,
    STOP
}
fun label(mode: Mode): String = when (mode) {
    Mode.FAST, Mode.SAFE -> "go"
    Mode.STOP -> "stop"
}
"#;
    let index = provider(&[(file, source)]);
    let entries = index.enum_entries(&enum_type(&index, file, "Mode").id);
    let FactStatus::Established(entries) = entries else {
        panic!("expected exact enum-entry declarations, got {entries:?}");
    };
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["FAST", "SAFE", "STOP"]
    );
    assert!(entries.iter().all(|entry| entry.kind == "enum_entry"));

    let plan = plan(source, file, &index);
    assert_eq!(plan.enum_when_lowerings.len(), 1);
    let lowering = &plan.enum_when_lowerings[0];
    assert_eq!(lowering.enum_owner.name, "Mode");
    assert_eq!(lowering.enum_owner.kind, "enum");
    assert_eq!(lowering.subject.start_byte, source.find("mode) {").unwrap());
    assert_eq!(lowering.arms.len(), 2);
    assert_eq!(
        lowering.arms[0]
            .labels
            .iter()
            .map(|label| label.name.as_str())
            .collect::<Vec<_>>(),
        ["FAST", "SAFE"]
    );
    assert_eq!(lowering.arms[1].labels[0].name, "STOP");
    assert!(
        lowering
            .arms
            .iter()
            .flat_map(|arm| &arm.labels)
            .all(|label| entries.contains(label)),
        "planned label identities must be members of the enum's established entries"
    );
    assert!(lowering.default_result.is_none());
    verify_snapshot(source, file, &plan);
}

#[test]
fn explicit_else_is_recorded_as_a_default_branch() {
    let file = "Default.kt";
    let source = r#"package enumplan
enum class Mode {
    FAST,
    SAFE,
    STOP
}
fun label(mode: Mode): String = when (mode) {
    Mode.FAST -> "go"
    else -> "other"
}
"#;
    let index = provider(&[(file, source)]);
    let plan = plan(source, file, &index);
    assert_eq!(plan.enum_when_lowerings.len(), 1);
    let lowering = &plan.enum_when_lowerings[0];
    assert_eq!(lowering.arms.len(), 1);
    let default_result = lowering
        .default_result
        .as_ref()
        .expect("explicit else branch");
    assert!(
        &source[default_result.start_byte..default_result.end_byte] == "\"other\"",
        "else result location must point into the exact source branch"
    );
    verify_snapshot(source, file, &plan);
}

#[test]
fn annotated_constructor_property_and_proven_call_subjects_are_planned() {
    let property_file = "Property.kt";
    let property_source = r#"package enumplan
annotation class StoredValue
enum class Mode {
    FAST,
    STOP
}
class Controller(@StoredValue val mode: Mode) {
    fun label(): String = when (mode) {
        Mode.FAST -> "go"
        Mode.STOP -> "stop"
    }
}
"#;
    let property_index = provider(&[(property_file, property_source)]);
    let property_plan = plan(property_source, property_file, &property_index);
    assert_eq!(property_plan.enum_when_lowerings.len(), 1);
    assert_eq!(
        &property_source[property_plan.enum_when_lowerings[0].subject.start_byte
            ..property_plan.enum_when_lowerings[0].subject.end_byte],
        "mode"
    );
    verify_snapshot(property_source, property_file, &property_plan);

    let call_file = "Call.kt";
    let call_source = r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
fun currentMode(): Mode = Mode.FAST
fun label(): String = when (currentMode()) {
    Mode.FAST -> "go"
    Mode.STOP -> "stop"
}
"#;
    let call_index = provider(&[(call_file, call_source)]);
    let call_plan = plan(call_source, call_file, &call_index);
    assert_eq!(call_plan.enum_when_lowerings.len(), 1);
    let subject = &call_plan.enum_when_lowerings[0].subject;
    assert_eq!(
        &call_source[subject.start_byte..subject.end_byte],
        "currentMode()"
    );
    verify_snapshot(call_source, call_file, &call_plan);

    let block_file = "BlockReturn.kt";
    let block_source = r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
fun currentMode(): Mode {
    return Mode.FAST
}
fun label(): String {
    return when (currentMode()) {
        Mode.FAST -> "go"
        Mode.STOP -> "stop"
    }
}
"#;
    let block_index = provider(&[(block_file, block_source)]);
    let block_plan = plan(block_source, block_file, &block_index);
    assert_eq!(block_plan.enum_when_lowerings.len(), 1);
    let subject = &block_plan.enum_when_lowerings[0].subject;
    assert_eq!(
        &block_source[subject.start_byte..subject.end_byte],
        "currentMode()"
    );
    verify_snapshot(block_source, block_file, &block_plan);
}

#[test]
fn nullable_generic_alias_and_incomplete_enum_switches_are_not_planned() {
    let cases = [
        (
            "nullable",
            r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
fun label(mode: Mode?): String = when (mode) {
    Mode.FAST -> "go"
    Mode.STOP -> "stop"
    else -> "other"
}
"#,
        ),
        (
            "generic",
            r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
fun <T> label(mode: T): String = when (mode) {
    Mode.FAST -> "go"
    Mode.STOP -> "stop"
    else -> "other"
}
"#,
        ),
        (
            "alias",
            r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
typealias ModeAlias = Mode
fun label(mode: ModeAlias): String = when (mode) {
    Mode.FAST -> "go"
    Mode.STOP -> "stop"
}
"#,
        ),
        (
            "missing entry",
            r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
fun label(mode: Mode): String = when (mode) {
    Mode.FAST -> "go"
}
"#,
        ),
    ];
    for (label, source) in cases {
        assert_no_lowering(source, &format!("{label}.kt"));
    }
}

#[test]
fn non_enum_qualified_fields_and_duplicate_constants_are_not_switch_cases() {
    let non_enum = r#"package enumplan
class State {
    companion object {
        val READY: State = State()
        val STOPPED: State = State()
    }
}
fun label(state: State): String = when (state) {
    State.READY -> "ready"
    State.STOPPED -> "stopped"
}
"#;
    assert_no_lowering(non_enum, "NonEnum.kt");

    let duplicate = r#"package enumplan
enum class State {
    READY,
    STOPPED
}
fun label(state: State): String = when (state) {
    State.READY -> "ready"
    State.READY -> "again"
    State.STOPPED -> "stopped"
}
"#;
    assert_no_lowering(duplicate, "Duplicate.kt");
}

#[test]
fn local_label_owner_unknown_subject_and_same_named_parameter_are_not_planned() {
    let shadowed_label = r#"package enumplan
enum class State {
    READY,
    STOPPED
}
class Labels {
    val READY: State = State.READY
    val STOPPED: State = State.STOPPED
}
fun label(state: State): String {
    val State = Labels()
    return when (state) {
        State.READY -> "ready"
        State.STOPPED -> "stopped"
    }
}
"#;
    assert_no_lowering(shadowed_label, "ShadowedLabels.kt");

    let unknown_subject = r#"package enumplan
enum class State {
    READY,
    STOPPED
}
fun label(): String {
    val state = TODO()
    return when (state) {
        State.READY -> "ready"
        State.STOPPED -> "stopped"
    }
}
"#;
    assert_no_lowering(unknown_subject, "UnknownSubject.kt");

    let shadowed_subject = r#"package enumplan
enum class State {
    READY,
    STOPPED
}
class Holder(val state: State) {
    fun label(state: String): String = when (state) {
        State.READY -> "ready"
        State.STOPPED -> "stopped"
        else -> "other"
    }
}
"#;
    assert_no_lowering(shadowed_subject, "ShadowedSubject.kt");
}

#[test]
fn ambiguous_implicit_receiver_statement_whens_and_other_conditions_are_rejected() {
    let implicit_receiver = r#"package enumplan
enum class State {
    READY,
    STOPPED
}
class Inner(val state: State)
class Outer(val state: State) {
    fun label(): String = with(Inner(State.READY)) {
        when (state) {
            State.READY -> "ready"
            State.STOPPED -> "stopped"
        }
    }
}
"#;
    assert_no_lowering(implicit_receiver, "ImplicitReceiver.kt");

    let statement = r#"package enumplan
enum class State {
    READY,
    STOPPED
}
fun label(state: State) {
    when (state) {
        State.READY -> println("ready")
        State.STOPPED -> println("stopped")
    }
}
"#;
    assert_no_lowering(statement, "Statement.kt");

    let type_condition = r#"package enumplan
enum class State {
    READY,
    STOPPED
}
fun label(state: State): String = when (state) {
    is State -> "state"
    else -> "other"
}
"#;
    assert_no_lowering(type_condition, "TypeCondition.kt");

    let range_condition = r#"package enumplan
enum class State {
    READY,
    ACTIVE,
    STOPPED
}
fun label(state: State): String = when (state) {
    State.READY -> "ready"
    in State.ACTIVE..State.STOPPED -> "later"
    else -> "other"
}
"#;
    assert_no_lowering(range_condition, "RangeCondition.kt");
}

#[test]
fn unannotated_java_method_return_is_not_a_nonnull_enum_subject() {
    let java_enum = r#"package javalib;
public enum Phase {
    STARTED,
    FINISHED;
}
"#;
    let java_source = r#"package javalib;
public class PhaseSource {
    public Phase current() {
        return Phase.STARTED;
    }
}
"#;
    let kotlin_file = "JavaPlatform.kt";
    let kotlin = r#"package enumplan
import javalib.Phase
import javalib.PhaseSource
fun label(source: PhaseSource): String = when (source.current()) {
    Phase.STARTED -> "started"
    Phase.FINISHED -> "finished"
    else -> "other"
}
"#;
    let index = provider(&[
        ("Phase.java", java_enum),
        ("PhaseSource.java", java_source),
        (kotlin_file, kotlin),
    ]);
    let plan = plan(kotlin, kotlin_file, &index);
    assert!(
        plan.enum_when_lowerings.is_empty(),
        "an unannotated Java return is platform-nullable, not a nonnull Kotlin enum subject"
    );
}

#[test]
fn java_enum_is_supported_for_a_nonnull_kotlin_parameter() {
    let java_file = "JavaPhase.java";
    let java = r#"package javalib;
public enum Phase {
    STARTED,
    FINISHED;
}
"#;
    let kotlin_file = "JavaEnumParameter.kt";
    let kotlin = r#"package enumplan
import javalib.Phase
fun label(phase: Phase): String = when (phase) {
    Phase.STARTED -> "started"
    Phase.FINISHED -> "finished"
}
"#;
    let index = provider(&[(java_file, java), (kotlin_file, kotlin)]);
    let java_enum = enum_type(&index, java_file, "Phase");
    let FactStatus::Established(entries) = index.enum_entries(&java_enum.id) else {
        panic!("expected Java enum constants to have stable entry identities");
    };
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["STARTED", "FINISHED"]
    );
    let plan = plan(kotlin, kotlin_file, &index);
    assert_eq!(plan.enum_when_lowerings.len(), 1);
    assert_eq!(plan.enum_when_lowerings[0].enum_owner, java_enum.id);
    assert_eq!(
        plan.enum_when_lowerings[0]
            .arms
            .iter()
            .flat_map(|arm| &arm.labels)
            .cloned()
            .collect::<Vec<_>>(),
        entries
    );
    verify_snapshot(kotlin, kotlin_file, &plan);
}

#[test]
fn outer_any_context_does_not_infer_an_enum_subject_for_nested_when() {
    let source = r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
fun wrap(value: Any): Any = value
fun label(mode: Mode): Any {
    return wrap(when (mode) {
        Mode.FAST -> 1
        Mode.STOP -> "stop"
        else -> "other"
    })
}
"#;
    assert_no_lowering(source, "NestedArgument.kt");
}

#[test]
fn inferred_function_returns_do_not_prove_enum_call_subjects() {
    let source = r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
fun currentMode() = Mode.FAST
fun label(): String = when (currentMode()) {
    Mode.FAST -> "fast"
    Mode.STOP -> "stop"
}
"#;
    assert_no_lowering(source, "InferredReturn.kt");
}

#[test]
fn local_or_member_classifier_names_shadow_enum_labels_even_after_the_function() {
    let constructor_property = r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
class Fake {
    val FAST: Mode
        get() = Mode.FAST
    val STOP: Mode
        get() = Mode.STOP
}
class Holder(private val Mode: Fake, val current: Mode) {
    fun label(): String = when (current) {
        Mode.FAST -> "fast"
        Mode.STOP -> "stop"
    }
}
"#;
    assert_no_lowering(constructor_property, "ConstructorClassifierShadow.kt");

    let later_member_property = r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
class Fake {
    val FAST: Mode
        get() = Mode.FAST
    val STOP: Mode
        get() = Mode.STOP
}
class Holder(val current: Mode) {
    fun label(): String = when (current) {
        Mode.FAST -> "fast"
        Mode.STOP -> "stop"
    }
    private val Mode: Fake = Fake()
}
"#;
    assert_no_lowering(later_member_property, "LaterClassifierShadow.kt");
}

#[test]
fn imported_or_same_package_enum_names_can_be_shadowed_by_value_properties() {
    let java_or_kotlin_producer = r#"package enumlib
enum class Phase {
    STARTED,
    FINISHED
}
"#;
    let imported = r#"package enumconsumer
import enumlib.Phase
import enumlib.Phase.STARTED
class Holder(private val STARTED: Phase, val current: Phase) {
    fun label(): String = when (current) {
        STARTED -> "started"
        Phase.FINISHED -> "finished"
    }
}
"#;
    let imported_index = provider(&[
        ("Phase.kt", java_or_kotlin_producer),
        ("ImportedShadow.kt", imported),
    ]);
    let imported_plan = plan(imported, "ImportedShadow.kt", &imported_index);
    assert!(
        imported_plan.enum_when_lowerings.is_empty(),
        "the class property named STARTED shadows the imported enum entry"
    );

    let same_package = r#"package enumplan
enum class Phase {
    STARTED,
    FINISHED
}
val STARTED: Phase
    get() = Phase.FINISHED
fun label(current: Phase): String = when (current) {
    STARTED -> "started"
    Phase.FINISHED -> "finished"
}
"#;
    assert_no_lowering(same_package, "TopLevelShadow.kt");
}

#[test]
fn local_qualified_prefix_binding_does_not_mimic_package_enum_entries() {
    let source = r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
class Fake {
    val FAST: Mode
        get() = Mode.FAST
    val STOP: Mode
        get() = Mode.STOP
}
class Catalog(val Mode: Fake) {
}
fun label(mode: Mode, enumplan: Catalog): String = when (mode) {
    enumplan.Mode.FAST -> "fast"
    enumplan.Mode.STOP -> "stop"
}
"#;
    assert_no_lowering(source, "QualifiedPrefixShadow.kt");
}

#[test]
fn wildcard_value_import_can_shadow_an_explicit_enum_type_import() {
    let enum_file = r#"package kinds
enum class Mode {
    FAST,
    STOP
}
"#;
    let values_file = r#"package values
import kinds.Mode
class Labels {
    val FAST: Mode
        get() = Mode.STOP
}
val Mode: Labels = Labels()
"#;
    let caller = r#"package consumer
import kinds.Mode
import values.*
fun label(mode: Mode): String = when (mode) {
    Mode.FAST -> "fast"
    Mode.STOP -> "stop"
}
"#;
    let index = provider(&[
        ("Mode.kt", enum_file),
        ("Labels.kt", values_file),
        ("Caller.kt", caller),
    ]);
    let plan = plan(caller, "Caller.kt", &index);
    assert!(
        plan.enum_when_lowerings.is_empty(),
        "the wildcard-imported value prefix makes Mode.FAST a runtime getter access"
    );
}

#[test]
fn java_ancestor_field_facts_gate_enum_prefixes_conservatively() {
    let marker_without_fields = r#"package javatypes;
public interface Marker {
}
"#;
    let caller = r#"package consumer
import kinds.Mode
import javatypes.Marker
class Controller(val mode: Mode) : Marker {
    fun label(): String = when (mode) {
        Mode.FAST -> "fast"
        Mode.STOP -> "stop"
    }
}
"#;
    let enum_source = r#"package kinds
enum class Mode {
    FAST,
    STOP
}
"#;
    let no_field_index = provider(&[
        ("Marker.java", marker_without_fields),
        ("Mode.kt", enum_source),
        ("Controller.kt", caller),
    ]);
    let marker = no_field_index
        .symbols()
        .iter()
        .find(|symbol| {
            symbol.id.file == Path::new("Marker.java")
                && symbol.id.kind == "interface"
                && symbol.id.name == "Marker"
        })
        .expect("indexed Java marker interface");
    assert!(matches!(
        no_field_index.java_field_named(&marker.id, "Mode"),
        FactStatus::Established(false)
    ));
    let no_field_plan = plan(caller, "Controller.kt", &no_field_index);
    assert_eq!(no_field_plan.enum_when_lowerings.len(), 1);

    let marker_with_field = r#"package javatypes;
public interface Marker {
    Object Mode = new Object();
}
"#;
    let field_index = provider(&[
        ("Marker.java", marker_with_field),
        ("Mode.kt", enum_source),
        ("Controller.kt", caller),
    ]);
    let marker = field_index
        .symbols()
        .iter()
        .find(|symbol| {
            symbol.id.file == Path::new("Marker.java")
                && symbol.id.kind == "interface"
                && symbol.id.name == "Marker"
        })
        .expect("indexed Java marker interface with a field");
    assert!(matches!(
        field_index.java_field_named(&marker.id, "Mode"),
        FactStatus::Established(true)
    ));
    assert!(
        plan(caller, "Controller.kt", &field_index)
            .enum_when_lowerings
            .is_empty(),
        "an inherited Java field named Mode blocks the enum prefix proof"
    );

    let annotated_marker = r#"package javatypes;
@GeneratedMarker
public interface Marker {
}
@interface GeneratedMarker {
}
"#;
    let annotated_index = provider(&[
        ("Marker.java", annotated_marker),
        ("Mode.kt", enum_source),
        ("Controller.kt", caller),
    ]);
    let marker = annotated_index
        .symbols()
        .iter()
        .find(|symbol| {
            symbol.id.file == Path::new("Marker.java")
                && symbol.id.kind == "interface"
                && symbol.id.name == "Marker"
        })
        .expect("indexed annotated Java marker interface");
    assert!(matches!(
        annotated_index.java_field_named(&marker.id, "Mode"),
        FactStatus::Unknown
    ));
    assert!(
        plan(caller, "Controller.kt", &annotated_index)
            .enum_when_lowerings
            .is_empty(),
        "unknown generated interface fields must preserve the Kotlin owner"
    );
}

#[test]
fn known_jdk_wildcard_is_safe_but_unknown_user_wildcard_is_not() {
    let jdk_wildcard = r#"package enumplan
import java.util.*
enum class Mode {
    FAST,
    STOP
}
fun label(mode: Mode): String = when (mode) {
    Mode.FAST -> "fast"
    Mode.STOP -> "stop"
}
"#;
    let index = provider(&[("JdkWildcard.kt", jdk_wildcard)]);
    assert_eq!(
        plan(jdk_wildcard, "JdkWildcard.kt", &index)
            .enum_when_lowerings
            .len(),
        1,
        "a known JDK wildcard package may establish the absent enum prefix"
    );

    let enum_file = r#"package kinds
enum class Mode {
    FAST,
    STOP
}
"#;
    let unknown_wildcard = r#"package consumer
import kinds.Mode
import com.example.persistence.*
fun label(mode: Mode): String = when (mode) {
    Mode.FAST -> "fast"
    Mode.STOP -> "stop"
}
"#;
    let unknown_index = provider(&[
        ("Mode.kt", enum_file),
        ("UnknownWildcard.kt", unknown_wildcard),
    ]);
    assert!(
        plan(unknown_wildcard, "UnknownWildcard.kt", &unknown_index)
            .enum_when_lowerings
            .is_empty(),
        "unknown project wildcard namespaces cannot prove that Mode.FAST is an enum entry"
    );
}

#[test]
fn companion_value_named_like_enum_type_shadows_qualified_case_prefix() {
    let source = r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
class Labels {
    val FAST: Mode
        get() = Mode.STOP
    val STOP: Mode
        get() = Mode.FAST
}
class Controller(val mode: Mode) {
    companion object {
        val Mode: Labels = Labels()
    }
    fun label(): String = when (mode) {
        Mode.FAST -> "fast"
        Mode.STOP -> "stop"
    }
}
"#;
    assert_no_lowering(source, "CompanionClassifierShadow.kt");
}

#[test]
fn typed_local_function_inside_with_does_not_ignore_implicit_receiver_shadow() {
    let source = r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
class Labels {
    val FAST: Mode
        get() = Mode.STOP
    val STOP: Mode
        get() = Mode.FAST
}
class Holder(val Mode: Labels) {
}
fun label(holder: Holder, mode: Mode): String = with(holder) {
    fun render(mode: Mode): String = when (mode) {
        Mode.FAST -> "fast"
        Mode.STOP -> "stop"
    }
    render(mode)
}
"#;
    assert_no_lowering(source, "LocalFunctionImplicitReceiver.kt");
}

#[test]
fn old_provider_cannot_plan_changed_parameter_or_body_snapshots() {
    let file = "StaleProvider.kt";
    let old_source = r#"package enumplan
enum class Mode {
    FAST,
    STOP
}

fun label(mode: Mode): String = when (mode) {
    Mode.FAST -> "old"
    Mode.STOP -> "stop"
}
"#;
    let index = provider(&[(file, old_source)]);

    let changed_parameter = r#"package enumplan
enum class Mode {
    FAST,
    STOP
}
fun label(mode: Any): String = when (mode) {
    Mode.FAST -> "old"
    Mode.STOP -> "stop"
    else -> "other"
}
"#;
    let parameter_plan = plan_with_provider(changed_parameter, file, &index);
    assert!(
        parameter_plan.enum_when_lowerings.is_empty(),
        "the old Mode parameter fact must not apply after the source type changed"
    );

    let changed_body = old_source.replace("\"old\"", "\"new\"");
    assert_ne!(changed_body, old_source);
    let body_plan = plan_with_provider(&changed_body, file, &index);
    assert!(
        body_plan.enum_when_lowerings.is_empty(),
        "unchanged declaration identity does not make old body facts current"
    );
}

#[test]
fn inherited_java_static_getter_name_blocks_enum_classifier_prefix() {
    let enum_source = r#"package kinds
enum class Mode {
    FAST,
    STOP
}
"#;
    let java_base = r#"package javatypes;
public class ModeBase {
    public static Object getMode() {
        return new Object();
    }
}
"#;
    let file = "InheritedGetter.kt";
    let source = r#"package consumer
import kinds.Mode
import javatypes.ModeBase
class Controller(val mode: Mode) : ModeBase() {
    fun label(): String = when (mode) {
        Mode.FAST -> "fast"
        Mode.STOP -> "stop"
    }
}
"#;
    let index = provider(&[
        ("Mode.kt", enum_source),
        ("ModeBase.java", java_base),
        (file, source),
    ]);
    assert!(
        plan(source, file, &index).enum_when_lowerings.is_empty(),
        "an inherited Java static getter named getMode must conservatively block Mode qualification"
    );
}

#[test]
fn enum_entry_named_like_its_classifier_blocks_member_label_qualification() {
    let file = "EnumEntryClassifierShadow.kt";
    let source = r#"package enumplan
enum class Mode {
    FAST,
    STOP,
    Mode;
    fun label(): String = when (this) {
        Mode.FAST -> "fast"
        Mode.STOP -> "stop"
        else -> "other"
    }
}
"#;
    assert_no_lowering(source, file);
}

#[test]
fn indexed_java_util_value_shadow_blocks_enum_prefix_under_jdk_wildcard() {
    let enum_source = r#"package kinds
enum class Mode {
    FAST,
    STOP
}
"#;
    let java_util_values = r#"package java.util
class Labels {
    val FAST: kinds.Mode
        get() = kinds.Mode.STOP
    val STOP: kinds.Mode
        get() = kinds.Mode.FAST
}
val Mode: Labels = Labels()
"#;
    let file = "JdkWildcardShadow.kt";
    let source = r#"package consumer
import kinds.Mode
import java.util.*
fun label(mode: Mode): String = when (mode) {
    Mode.FAST -> "fast"
    Mode.STOP -> "stop"
}
"#;
    let index = provider(&[
        ("Mode.kt", enum_source),
        ("JavaUtilValues.kt", java_util_values),
        (file, source),
    ]);
    assert!(
        plan(source, file, &index).enum_when_lowerings.is_empty(),
        "an indexed java.util value named Mode can shadow the enum despite a JDK wildcard"
    );
}
