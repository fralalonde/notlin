use notlin::semantics::{
    FactStatus, SemanticProvider, SemanticSymbol, SymbolId, SyntaxSemanticProvider,
    plan_required_calls,
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

fn class<'a>(provider: &'a SyntaxSemanticProvider, file: &str, name: &str) -> &'a SemanticSymbol {
    provider
        .symbols()
        .iter()
        .find(|symbol| {
            symbol.id.file == Path::new(file)
                && matches!(symbol.id.kind.as_str(), "class" | "interface")
                && symbol.id.name == name
        })
        .unwrap_or_else(|| panic!("missing type {name} in {file}: {:#?}", provider.symbols()))
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

fn assert_unestablished<T: std::fmt::Debug>(fact: FactStatus<T>, context: &str) {
    assert!(
        !matches!(fact, FactStatus::Established(_)),
        "unsafe established fact for {context}: {fact:?}"
    );
}

fn inherited_access(
    provider: &SyntaxSemanticProvider,
    owner: &SymbolId,
    property: &str,
) -> FactStatus<notlin::semantics::InheritedPropertyAccess> {
    provider.inherited_property_access(owner, property)
}

#[test]
fn annotated_concrete_override_uses_ordinary_root_interface_getter() {
    let file = "Inherited.kt";
    let source = r#"package inherited
annotation class PersistenceMarker
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
open class AnnotatedBase : Root {
    @PersistenceMarker
    override val token: Reader = object : Reader {
        override fun read(): String = "ok"
    }
}
class Derived(token: String) : AnnotatedBase() {
    fun readToken(): String = token.read()
}
"#;
    let index = provider(&[(file, source)]);
    let derived = class(&index, file, "Derived");
    let FactStatus::Established(access) = inherited_access(&index, &derived.id, "token") else {
        panic!("expected proven interface getter through annotated override");
    };
    assert_eq!(access.source_property.name, "token");
    assert_eq!(access.getter_contract.name, "token");
    assert_eq!(access.source_property.owner_path, vec!["AnnotatedBase"]);
    assert_eq!(access.contract_owner.name, "Root");
    assert_eq!(access.value_owner.name, "Reader");

    let read = required_call(source, file, &index, "read");
    let FactStatus::Established(read) = read else {
        panic!(
            "constructor parameter must not shadow the inherited property in a member: {read:?}"
        );
    };
    assert_eq!(read.owner_path, vec!["Reader"]);
}

#[test]
fn repeated_interface_paths_with_one_root_getter_contract_remain_proven() {
    let file = "SharedRoot.kt";
    let source = r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
interface Left : Root {
    override val token: Reader
}
interface Right : Root {
    override val token: Reader
}
class Derived(override val token: Reader) : Left, Right {
}
"#;
    let index = provider(&[(file, source)]);
    let derived = class(&index, file, "Derived");
    let FactStatus::Established(access) = inherited_access(&index, &derived.id, "token") else {
        panic!("the two override paths share Root.token and should converge");
    };
    assert_eq!(access.contract_owner.name, "Root");
}

#[test]
fn nullable_generic_and_aliased_returns_do_not_prove_a_concrete_receiver() {
    let cases = [
        (
            "nullable",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader?
}
class Derived(override val token: Reader?) : Root {
}
"#,
        ),
        (
            "generic",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root<T : Reader> {
    val token: T
}
class Derived<T : Reader>(override val token: T) : Root<T> {
}
"#,
        ),
        (
            "alias",
            r#"package inherited
interface Reader {
    fun read(): String
}
typealias TokenReader = Reader
interface Root {
    val token: TokenReader
}
class Derived(override val token: TokenReader) : Root {
}
"#,
        ),
    ];

    for (label, source) in cases {
        let file = format!("{label}.kt");
        let index = provider(&[(&file, source)]);
        let derived = class(&index, &file, "Derived");
        assert_unestablished(inherited_access(&index, &derived.id, "token"), label);
    }
}

#[test]
fn ambiguous_or_unavailable_contracts_do_not_prove_inherited_access() {
    let unrelated = r#"package inherited
interface Reader {
    fun read(): String
}
interface LeftRoot {
    val token: Reader
}
interface RightRoot {
    val token: Reader
}
class Derived(override val token: Reader) : LeftRoot, RightRoot {
}
"#;
    let unrelated_index = provider(&[("Unrelated.kt", unrelated)]);
    let derived = class(&unrelated_index, "Unrelated.kt", "Derived");
    assert_unestablished(
        inherited_access(&unrelated_index, &derived.id, "token"),
        "unrelated property contracts with matching spelling and type",
    );

    let unknown_ancestry = r#"package inherited
class Derived : MissingRoot {
}
"#;
    let unknown_index = provider(&[("Unknown.kt", unknown_ancestry)]);
    let derived = class(&unknown_index, "Unknown.kt", "Derived");
    assert_unestablished(
        inherited_access(&unknown_index, &derived.id, "token"),
        "unknown ancestry",
    );
}

#[test]
fn inaccessible_or_jvm_hidden_getters_do_not_establish_a_contract() {
    let cases = [
        (
            "private override",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
class Derived : Root {
    private override val token: Reader = TODO()
}
"#,
        ),
        (
            "internal override",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
class Derived : Root {
    internal override val token: Reader = TODO()
}
"#,
        ),
        (
            "synthetic root getter",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    @get:JvmSynthetic
    val token: Reader
}
class Derived(override val token: Reader) : Root {
}
"#,
        ),
        (
            "renamed root getter",
            r#"package inherited
import kotlin.jvm.JvmName
interface Reader {
    fun read(): String
}
interface Root {
    @get:JvmName("fetchToken")
    val token: Reader
}
class Derived(override val token: Reader) : Root {
}
"#,
        ),
        (
            "aliased synthetic root getter",
            r#"package inherited
import kotlin.jvm.JvmSynthetic as HiddenGetter
interface Reader {
    fun read(): String
}
interface Root {
    @get:HiddenGetter
    val token: Reader
}
class Derived(override val token: Reader) : Root {
}
"#,
        ),
    ];
    for (label, source) in cases {
        let file = format!("{label}.kt");
        let index = provider(&[(&file, source)]);
        let derived = class(&index, &file, "Derived");
        assert_unestablished(inherited_access(&index, &derived.id, "token"), label);
    }
}

#[test]
fn real_local_parameter_and_vararg_shadows_do_not_use_the_inherited_property() {
    let cases = [
        (
            "local",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
open class Base : Root {
    override val token: Reader = TODO()
}
class Derived : Base() {
    fun use(): String {
        val token: String = "local"
        return token.read()
    }
}
"#,
        ),
        (
            "outer local nested block",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
open class Base : Root {
    override val token: Reader = TODO()
}
class Derived : Base() {
    fun use(flag: Boolean): String {
        val token: String = "local"
        if (flag) {
            return token.read()
        }
        return ""
    }
}
"#,
        ),
        (
            "parameter",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
open class Base : Root {
    override val token: Reader = TODO()
}
class Derived : Base() {
    fun use(token: String): String = token.read()
}
"#,
        ),
        (
            "vararg",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
open class Base : Root {
    override val token: Reader = TODO()
}
class Derived : Base() {
    fun use(vararg token: String): String = token.read()
}
"#,
        ),
        (
            "vararg reader",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
open class Base : Root {
    override val token: Reader = TODO()
}
class Derived : Base() {
    fun use(vararg token: Reader): String = token.read()
}
"#,
        ),
        (
            "nullable reader",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
open class Base : Root {
    override val token: Reader = TODO()
}
class Derived : Base() {
    fun use(token: Reader?): String = token.read()
}
"#,
        ),
        (
            "method",
            r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
open class Base : Root {
    override val token: Reader = TODO()
}
class Derived : Base() {
    fun use(): String = token().read()
    fun token(): String = "method"
}
"#,
        ),
    ];
    for (label, source) in cases {
        let file = format!("{label}Shadow.kt");
        let index = provider(&[(&file, source)]);
        let call = required_call(source, &file, &index, "read");
        assert_unestablished(call, &format!("{label} shadow"));
    }
}

#[test]
fn class_implementation_and_extra_interface_override_share_the_root_contract() {
    let file = "ExtraInterface.kt";
    let source = r#"package inherited
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
annotation class PersistenceMarker
open class Base(@PersistenceMarker override val token: Reader) : Root
interface Extra : Root {
    override val token: Reader
}
class Derived(token: Reader) : Base(token), Extra {
    fun readToken(): String = token.read()
}
"#;
    let index = provider(&[(file, source)]);
    let derived = class(&index, file, "Derived");
    let FactStatus::Established(access) = inherited_access(&index, &derived.id, "token") else {
        panic!("class and extra-interface paths share the root property contract");
    };
    assert_eq!(access.source_property.owner_path, vec!["Base"]);
    assert_eq!(access.contract_owner.name, "Root");
    assert_eq!(access.value_owner.name, "Reader");
}

#[test]
fn inherited_value_classifier_stays_in_its_declaring_package() {
    let producer = r#"package producers
interface Reader {
    fun read(): String
}
interface Root {
    val token: Reader
}
"#;
    let consumer = r#"package consumers
interface Reader {
    fun read(): Int
}
class Derived(override val token: producers.Reader) : producers.Root {
    fun readToken(): String = token.read()
}
"#;
    let index = provider(&[("Producer.kt", producer), ("Consumer.kt", consumer)]);
    let derived = class(&index, "Consumer.kt", "Derived");
    let FactStatus::Established(access) = inherited_access(&index, &derived.id, "token") else {
        panic!("the qualified property return type should be established");
    };
    assert_eq!(access.value_owner.package, "producers");
    assert_eq!(access.value_owner.name, "Reader");

    let read = required_call(consumer, "Consumer.kt", &index, "read");
    let FactStatus::Established(read) = read else {
        panic!("expected receiver lookup through producers.Reader, got {read:?}");
    };
    assert_eq!(read.package, "producers");
}
