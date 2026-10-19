use notlin::semantics::{
    FactStatus, KotlinType, SemanticProvider, SemanticSymbol, SymbolId, SyntaxSemanticProvider,
    TypeRef, plan_required_calls,
};
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

fn callable<'a>(
    provider: &'a SyntaxSemanticProvider,
    file: &str,
    name: &str,
) -> &'a SemanticSymbol {
    provider
        .symbols()
        .iter()
        .find(|symbol| {
            symbol.id.file == Path::new(file)
                && symbol.id.kind == "function"
                && symbol.id.name == name
        })
        .unwrap_or_else(|| panic!("missing {name} in {file}: {:#?}", provider.symbols()))
}

fn member<'a>(provider: &'a SyntaxSemanticProvider, file: &str, name: &str) -> &'a SemanticSymbol {
    provider
        .symbols()
        .iter()
        .find(|symbol| symbol.id.file == Path::new(file) && symbol.id.name == name)
        .unwrap_or_else(|| panic!("missing member {name} in {file}: {:#?}", provider.symbols()))
}

fn required_call(
    source: &str,
    file: &str,
    provider: &SyntaxSemanticProvider,
    spelling: &str,
) -> FactStatus<SymbolId> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let path = Path::new(file);
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), path);
    plan_required_calls(source, &tree, path, provider, false, &mut plan);
    plan.dependencies
        .into_iter()
        .find(|dependency| dependency.spelling == spelling)
        .map(|dependency| dependency.resolution)
        .unwrap_or(FactStatus::Unknown)
}

fn assert_not_established<T: std::fmt::Debug>(fact: FactStatus<T>, context: &str) {
    assert!(
        !matches!(fact, FactStatus::Established(_)),
        "unsafe established fact for {context}: {fact:?}"
    );
}

#[test]
fn bounded_type_parameter_return_grants_only_member_lookup_capability() {
    let file = "GenericOwner.kt";
    let source = r#"package capability
interface ObjectId {
    fun getObjectId(): String
}

interface HasId<T : ObjectId> {
    fun getId(): T
    fun describeId(): String = getId().getObjectId()
}
"#;
    let index = provider(&[(file, source)]);
    let producer = callable(&index, file, "getId");
    let capability = index.returned_receiver_capability(&producer.id);
    let FactStatus::Established(capability) = capability else {
        panic!("expected a proven member-lookup capability, got {capability:?}");
    };
    assert_eq!(capability.owner.name, "ObjectId");

    // This capability is not an instantiated expression type. Keep the exact
    // declaration return fact as T so no downstream Java signature becomes ObjectId.
    assert_eq!(
        producer.ty,
        FactStatus::Established(TypeRef {
            ty: KotlinType::TypeParameter("T".into()),
            nullable: false,
        })
    );
    assert_eq!(
        required_call(source, file, &index, "getObjectId"),
        FactStatus::Established(
            index
                .symbols()
                .iter()
                .find(|symbol| symbol.id.name == "getObjectId")
                .unwrap()
                .id
                .clone()
        )
    );
}

#[test]
fn caller_type_parameter_bound_is_used_for_chained_member_lookup() {
    let owner_file = "Owner.kt";
    let caller_file = "Caller.kt";
    let owner = r#"package capability
interface ObjectId {
    fun getObjectId(): String
}
interface HasId<T : ObjectId> {
    fun getId(): T
}
class SpecificId : ObjectId {
    override fun getObjectId(): String = "id"
}
"#;
    let caller = r#"package capability
fun describeId(value: HasId<SpecificId>): String = value.getId().getObjectId()
"#;
    let index = provider(&[(owner_file, owner), (caller_file, caller)]);
    let resolved = required_call(caller, caller_file, &index, "getObjectId");
    let FactStatus::Established(id) = resolved else {
        panic!("expected HasId<SpecificId> chained lookup to resolve, got {resolved:?}");
    };
    assert_eq!(id.owner_path, vec!["ObjectId"]);
    assert_eq!(id.name, "getObjectId");

    let generic_caller = r#"package capability
fun <T : ObjectId> describeGeneric(value: HasId<T>): String = value.getId().getObjectId()
"#;
    let generic_index = provider(&[(owner_file, owner), (caller_file, generic_caller)]);
    let generic_resolution =
        required_call(generic_caller, caller_file, &generic_index, "getObjectId");
    let FactStatus::Established(generic_id) = generic_resolution else {
        panic!("expected generic HasId<T> chained lookup to resolve, got {generic_resolution:?}");
    };
    assert_eq!(generic_id, id);
}

#[test]
fn bound_lookup_is_owner_scoped_and_import_resolution_is_producer_scoped() {
    let left = r#"package left
interface Token {
    fun readToken(): String
}
interface LeftBox<T : Token> {
    fun getValue(): T
}
"#;
    let right = r#"package right
interface Token {
    fun readToken(): Int
}
interface RightBox<T : Token> {
    fun getValue(): T
}
"#;
    let caller = r#"package consumer
import left.LeftBox
import right.RightBox
fun leftRead(box: LeftBox<left.Token>): String = box.getValue().readToken()
fun rightRead(box: RightBox<right.Token>): Int = box.getValue().readToken()
"#;
    let left_only = caller.replace(
        "fun rightRead(box: RightBox<right.Token>): Int = box.getValue().readToken()\n",
        "",
    );
    let index = provider(&[
        ("Left.kt", left),
        ("Right.kt", right),
        ("Caller.kt", &left_only),
    ]);

    let left_resolution = required_call(&left_only, "Caller.kt", &index, "readToken");
    let FactStatus::Established(left_id) = left_resolution else {
        panic!("expected left producer bound resolution, got {left_resolution:?}");
    };
    assert_eq!(left_id.package, "left");
    assert_eq!(left_id.owner_path, vec!["Token"]);

    // Resolve the second call independently; same simple names must not merge
    // bounds from different packages into one receiver capability.
    let right_only = caller.replace(
        "fun leftRead(box: LeftBox<left.Token>): String = box.getValue().readToken()\n",
        "",
    );
    let right_index = provider(&[
        ("Left.kt", left),
        ("Right.kt", right),
        ("Caller.kt", &right_only),
    ]);
    let right_resolution = required_call(&right_only, "Caller.kt", &right_index, "readToken");
    let FactStatus::Established(right_id) = right_resolution else {
        panic!("expected right producer bound resolution, got {right_resolution:?}");
    };
    assert_eq!(right_id.package, "right");
    assert_eq!(right_id.owner_path, vec!["Token"]);
}

#[test]
fn unsafe_type_parameter_bounds_do_not_create_capabilities() {
    let cases = [
        (
            "method shadows owner parameter",
            r#"interface Id {
    fun read(): String
}
interface Holder<T : Id> {
    fun <T> getValue(): T
}
"#,
        ),
        (
            "nullable return",
            r#"interface Id {
    fun read(): String
}
interface Holder<T : Id> {
    fun getValue(): T?
}
"#,
        ),
        (
            "nullable bound",
            r#"interface Id {
    fun read(): String
}
interface Holder<T : Id?> {
    fun getValue(): T
}
"#,
        ),
        (
            "type parameter bound",
            r#"interface Id {
    fun read(): String
}
interface Holder<U, T : U> {
    fun getValue(): T
}
"#,
        ),
        (
            "where constrained owner",
            r#"interface Id {
    fun read(): String
}
interface Holder<T> where T : Id {
    fun getValue(): T
}
"#,
        ),
        (
            "where constrained method",
            r#"interface Id {
    fun read(): String
}
interface Holder<T> {
    fun <U> getValue(): U where U : Id
}
"#,
        ),
        (
            "unresolved bound",
            r#"interface Holder<T : MissingId> {
    fun getValue(): T
}
"#,
        ),
        (
            "unsafe inherited ancestry",
            r#"interface Id : ExternalId {
    fun read(): String
}
interface Holder<T : Id> {
    fun getValue(): T
}
"#,
        ),
    ];

    for (label, declaration) in cases {
        let file = "Unsafe.kt";
        let source = format!("package capability\n{declaration}");
        let index = provider(&[(file, &source)]);
        let getter = callable(&index, file, "getValue");
        assert_not_established(index.returned_receiver_capability(&getter.id), label);
    }
}

#[test]
fn aliases_and_import_collisions_do_not_rebind_producer_bounds() {
    let external = r#"package external
interface Marker {
    fun read(): String
}
"#;
    let alias = r#"package producer
import external.Marker as LocalMarker
interface LocalMarker {
    fun localRead(): Int
}
interface Holder<T : LocalMarker> {
    fun getValue(): T
}
"#;
    let index = provider(&[("External.kt", external), ("Alias.kt", alias)]);
    let getter = callable(&index, "Alias.kt", "getValue");
    assert_not_established(
        index.returned_receiver_capability(&getter.id),
        "aliased imported bound colliding with local declaration",
    );

    let typealias_source = r#"package producer
interface Marker {
    fun read(): String
}
typealias AliasMarker = Marker
interface Holder<T : AliasMarker> {
    fun getValue(): T
}
"#;
    let index = provider(&[("AliasBound.kt", typealias_source)]);
    let getter = callable(&index, "AliasBound.kt", "getValue");
    assert_not_established(
        index.returned_receiver_capability(&getter.id),
        "typealias bound",
    );
}

#[test]
fn ambiguous_or_non_instance_bound_members_are_not_proven() {
    let ambiguous = r#"package capability
interface Left {
    fun read(): String
}
interface Right {
    fun read(): String
}
interface Id : Left, Right
interface Holder<T : Id> {
    fun getValue(): T
}
fun readValue(holder: Holder<Id>): String = holder.getValue().read()
"#;
    let index = provider(&[("Ambiguous.kt", ambiguous)]);
    assert_not_established(
        required_call(ambiguous, "Ambiguous.kt", &index, "read"),
        "same-arity inherited overload ambiguity",
    );

    let java_static = r#"package capability;
public interface StaticId { static String read() { return "static"; } }
"#;
    let holder = r#"package capability
interface Holder<T : StaticId> {
    fun getValue(): T
}
fun readValue(holder: Holder<StaticId>): String = holder.getValue().read()
"#;
    let index = provider(&[("StaticId.java", java_static), ("Holder.kt", holder)]);
    assert_not_established(
        required_call(holder, "Holder.kt", &index, "read"),
        "Java static method cannot be selected as a receiver member",
    );

    let extension = r#"package capability
interface Id
fun Id.read(): String = "extension"
interface Holder<T : Id> {
    fun getValue(): T
}
fun readValue(holder: Holder<Id>): String = holder.getValue().read()
"#;
    let index = provider(&[("Extension.kt", extension)]);
    assert_not_established(
        required_call(extension, "Extension.kt", &index, "read"),
        "top-level extension is not an indexed bound member",
    );
}

#[test]
fn invisible_bound_members_and_extensions_do_not_resolve_as_public_calls() {
    let cases = [
        (
            "private",
            "interface Id {\n    private fun read(): String = \"private\"\n}\n",
        ),
        (
            "internal",
            "interface Id {\n    internal fun read(): String = \"internal\"\n}\n",
        ),
        (
            "protected",
            "abstract class Id {\n    protected fun read(): String = \"protected\"\n}\n",
        ),
    ];
    for (label, declaration) in cases {
        let owner = format!(
            "package producer\n{declaration}interface Holder<T : Id> {{\n    fun getValue(): T\n}}\n"
        );
        let caller = r#"package consumer
import producer.Holder
import producer.Id
fun readValue(holder: Holder<Id>): String = holder.getValue().read()
"#;
        let index = provider(&[("Owner.kt", &owner), ("Caller.kt", caller)]);
        assert_not_established(
            required_call(caller, "Caller.kt", &index, "read"),
            &format!("{label} bound member from unrelated caller"),
        );
    }

    let extension = r#"package producer
interface Id
fun Id.read(): String = "extension"
interface Holder<T : Id> {
    fun getValue(): T
}
"#;
    let caller = r#"package consumer
import producer.Holder
import producer.Id
fun readValue(holder: Holder<Id>): String = holder.getValue().read()
"#;
    let index = provider(&[("Owner.kt", extension), ("Caller.kt", caller)]);
    assert_not_established(
        required_call(caller, "Caller.kt", &index, "read"),
        "same-name extension imported from bound producer package",
    );
}

#[test]
fn java_generic_getter_bound_supports_mixed_kotlin_callers_without_erasing_exact_type() {
    let object_id = r#"package capability;
public interface ObjectId {
    String getObjectId();
}
"#;
    let has_id = r#"package capability;
public interface HasId<T extends ObjectId> {
    T getId();
}
"#;
    let specific_id = r#"package capability
class SpecificId : ObjectId {
    override fun getObjectId(): String = "specific"
}
"#;
    let caller = r#"package capability
fun describe(value: HasId<SpecificId>): String = value.getId().getObjectId()
"#;
    let index = provider(&[
        ("ObjectId.java", object_id),
        ("HasId.java", has_id),
        ("SpecificId.kt", specific_id),
        ("Caller.kt", caller),
    ]);
    let getter = member(&index, "HasId.java", "getId");
    assert_eq!(
        getter.ty,
        FactStatus::Established(TypeRef {
            ty: KotlinType::TypeParameter("T".into()),
            nullable: false,
        })
    );
    let capability = index.returned_receiver_capability(&getter.id);
    let FactStatus::Established(capability) = capability else {
        panic!("expected Java generic getter bound capability, got {capability:?}");
    };
    assert_eq!(capability.owner.name, "ObjectId");

    let resolved = required_call(caller, "Caller.kt", &index, "getObjectId");
    let FactStatus::Established(id) = resolved else {
        panic!("expected mixed Kotlin/Java generic member chain, got {resolved:?}");
    };
    assert_eq!(id.file, Path::new("ObjectId.java"));

    let generic_caller = r#"package capability
fun <T : ObjectId> describeGeneric(value: HasId<T>): String = value.getId().getObjectId()
"#;
    let generic = required_call(generic_caller, "Caller.kt", &index, "getObjectId");
    assert_eq!(generic, FactStatus::Established(id));
}

#[test]
fn java_generic_getter_capability_rejects_unsafe_declaration_shapes() {
    let object_id = r#"package capability;
public interface ObjectId {
    String getObjectId();
}
"#;
    let cases = [
        (
            "method type parameter shadows class parameter",
            "public interface HasId<T extends ObjectId> { <T> T getId(); }",
        ),
        (
            "static generic getter",
            "public interface HasId<T extends ObjectId> { static <T extends ObjectId> T getId() { return null; } }",
        ),
        (
            "nullable annotated getter",
            "public interface HasId<T extends ObjectId> { @Nullable T getId(); }",
        ),
        (
            "annotated bound",
            "public interface HasId<T extends @NonNull ObjectId> { T getId(); }",
        ),
        (
            "intersection bound",
            "public interface HasId<T extends ObjectId & java.io.Serializable> { T getId(); }",
        ),
        (
            "private getter",
            "public abstract class HasId<T extends ObjectId> { private T getId() { return null; } }",
        ),
        (
            "protected getter",
            "public abstract class HasId<T extends ObjectId> { protected abstract T getId(); }",
        ),
        (
            "package getter",
            "public abstract class HasId<T extends ObjectId> { abstract T getId(); }",
        ),
    ];
    for (label, declaration) in cases {
        let holder = format!("package capability;\n{declaration}\n");
        let index = provider(&[("ObjectId.java", object_id), ("HasId.java", &holder)]);
        let getter = member(&index, "HasId.java", "getId");
        assert_not_established(index.returned_receiver_capability(&getter.id), label);
    }
}

#[test]
fn empty_platform_marker_ancestry_is_proven_only_with_exact_type_evidence() {
    for (label, parent, imports, proven) in [
        ("qualified marker", "java.io.Serializable", "", true),
        (
            "explicit marker import",
            "Serializable",
            "import java.io.Serializable\n",
            true,
        ),
        ("unresolved simple name", "Serializable", "", false),
        (
            "wildcard alone",
            "Serializable",
            "import java.io.*\n",
            false,
        ),
        (
            "aliased import",
            "Serializable",
            "import java.io.Serializable as Marker\n",
            false,
        ),
        (
            "ambiguous imports",
            "Serializable",
            "import java.io.Serializable\nimport external.Serializable\n",
            false,
        ),
    ] {
        let source = format!(
            "package capability\n{imports}interface Id : {parent} {{\n    fun read(): String\n}}\ninterface Holder<T : Id> {{\n    fun getValue(): T\n}}\nfun readValue(holder: Holder<Id>): String = holder.getValue().read()\n"
        );
        let index = provider(&[("Marker.kt", &source)]);
        let getter = callable(&index, "Marker.kt", "getValue");
        assert_eq!(
            matches!(
                index.returned_receiver_capability(&getter.id),
                FactStatus::Established(_)
            ),
            proven,
            "{label}"
        );
        assert_eq!(
            matches!(
                required_call(&source, "Marker.kt", &index, "read"),
                FactStatus::Established(_)
            ),
            proven,
            "{label}"
        );
    }
}

#[test]
fn bound_capability_does_not_guess_argument_applicability_over_extensions() {
    let source = r#"package capability
interface Id {
    fun read(value: String): String
}
interface Holder<T : Id> {
    fun getValue(): T
}
fun Id.read(value: Int): String = "extension"
fun readValue(holder: Holder<Id>): String = holder.getValue().read(1)
"#;
    let index = provider(&[("Arguments.kt", source)]);
    assert_not_established(
        required_call(source, "Arguments.kt", &index, "read"),
        "unproven argument type must not pick a member over the applicable extension",
    );
}

#[test]
fn generic_upper_bounds_preserve_arguments_without_instantiating_return_types() {
    let contract = r#"package capability
interface Entity<I> {
    fun getId(): I
    fun describe(): String
}
interface Holder<I, T : Entity<I>> {
    fun getValue(): T
    fun summary(): String = getValue().describe()
}
fun <I, T : Entity<I>> summary(holder: Holder<I, T>): String = holder.getValue().describe()
"#;
    let index = provider(&[("Generic.kt", contract)]);
    let getter = callable(&index, "Generic.kt", "getValue");
    let FactStatus::Established(capability) = index.returned_receiver_capability(&getter.id) else {
        panic!("missing generic upper-bound capability");
    };
    assert_eq!(capability.owner.name, "Entity");
    assert_eq!(
        capability.declared_bound,
        TypeRef::parse_with_type_parameters("Entity<I>", &["I".into(), "T".into()])
    );
    assert_eq!(
        getter.ty,
        FactStatus::Established(TypeRef::parse_with_type_parameters("T", &["T".into()]))
    );
    let description = callable(&index, "Generic.kt", "describe").id.clone();
    assert_eq!(
        required_call(contract, "Generic.kt", &index, "describe"),
        FactStatus::Established(description)
    );
    // An argument in the bound does not grant a second capability for I or
    // substitute I with a concrete type at callers.
    let entity_getter = callable(&index, "Generic.kt", "getId");
    assert_eq!(
        entity_getter.ty,
        FactStatus::Established(TypeRef::parse_with_type_parameters("I", &["I".into()]))
    );
    assert_not_established(
        index.returned_receiver_capability(&entity_getter.id),
        "unbounded generic result inside the upper bound",
    );
}

#[test]
fn java_named_generic_upper_bound_grants_only_classifier_member_availability() {
    let entity = r#"package capability;
public interface Entity<I> {
    I getId();
    String describe();
}
"#;
    let holder = r#"package capability;
public interface Holder<I, T extends Entity<I>> {
    T getValue();
}
"#;
    let caller = r#"package capability
fun <I, T : Entity<I>> summary(holder: Holder<I, T>): String = holder.getValue().describe()
"#;
    let index = provider(&[
        ("Entity.java", entity),
        ("Holder.java", holder),
        ("Caller.kt", caller),
    ]);
    let getter = callable(&index, "Holder.java", "getValue");
    let FactStatus::Established(capability) = index.returned_receiver_capability(&getter.id) else {
        panic!("missing Java generic upper-bound capability");
    };
    assert_eq!(capability.owner.file, Path::new("Entity.java"));
    assert_eq!(
        capability.declared_bound,
        TypeRef::parse_with_type_parameters("Entity<I>", &["I".into(), "T".into()])
    );
    assert_eq!(
        required_call(caller, "Caller.kt", &index, "describe"),
        FactStatus::Established(callable(&index, "Entity.java", "describe").id.clone())
    );
    assert_eq!(
        getter.ty,
        FactStatus::Established(TypeRef::parse_with_type_parameters("T", &["T".into()]))
    );
}

#[test]
fn value_class_bound_and_mangled_value_class_returns_do_not_prove_java_instance_calls() {
    let source = r#"package capability
@JvmInline
value class TokenValue(val raw: String) {
    fun render(): String = raw
}
interface Provider<T : TokenValue> {
    fun token(): T
}
interface Container {
    fun tokenValue(): TokenValue
}
fun renderToken(provider: Provider<TokenValue>): String = provider.token().render()
"#;
    let index = provider(&[("Values.kt", source)]);
    let getter = callable(&index, "Values.kt", "token");
    assert!(!index.is_public_instance_callable(&getter.id));
    assert!(!index.has_stable_noarg_jvm_signature(&getter.id));
    assert_not_established(
        index.returned_receiver_capability(&getter.id),
        "value class upper bound has static implementation members",
    );
    let render = callable(&index, "Values.kt", "render");
    assert!(!index.is_public_instance_callable(&render.id));
    assert!(!index.has_stable_noarg_jvm_signature(&render.id));
    let mangled_return = callable(&index, "Values.kt", "tokenValue");
    assert!(!index.is_public_instance_callable(&mangled_return.id));
    assert!(!index.has_stable_noarg_jvm_signature(&mangled_return.id));
    assert_not_established(
        required_call(source, "Values.kt", &index, "render"),
        "value class member needs a JVM-specific boxed call proof",
    );

    let ordinary = source.replace("@JvmInline\nvalue class", "class");
    let ordinary_index = provider(&[("Values.kt", &ordinary)]);
    assert!(
        ordinary_index
            .is_public_instance_callable(&callable(&ordinary_index, "Values.kt", "render").id)
    );
    assert!(matches!(
        required_call(&ordinary, "Values.kt", &ordinary_index, "render"),
        FactStatus::Established(_)
    ));
}
