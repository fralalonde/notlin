use std::fs;
use std::path::Path;
use std::process::Command;

#[test]
fn enum_selects_a_concrete_property_default_beside_an_abstract_contract() {
    let root = Path::new("tests/tmp_scratch_enum_default_bridge");
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("types.kt"),
        r#"package neutral.defaults
interface ConcreteCategory {
    val category: String
        get() = "concrete"
}

interface AbstractCategory {
    val category: String
}
enum class Variant : ConcreteCategory, AbstractCategory { ONE }
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_notlin"))
        .args(["--root", root.to_str().unwrap(), "--in-place"])
        .arg(root)
        .output()
        .expect("run notlin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "notlin failed:\n{stderr}");

    let java = fs::read_to_string(root.join("Variant.java")).unwrap_or_default();
    assert!(
        java.contains("public String getCategory()")
            && java.contains("return ConcreteCategory.super.getCategory();"),
        "the enum must explicitly select Kotlin's concrete default:\n{java}\n{stderr}"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn enum_bridge_finds_physical_getter_defaults_but_keeps_abstract_and_non_getters_closed() {
    let root = std::env::temp_dir().join(format!(
        "notlin-enum-method-getter-bridge-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("types.kt"),
        r#"package neutral.defaults
interface DirectGetter {
    fun getCategory(): String = "category"
}
interface CategoryContract { val category: String }
enum class DirectVariant : DirectGetter, CategoryContract { ONE }

interface BooleanGetter {
    fun isReady(): Boolean = true
}
interface ReadyContract { val isReady: Boolean }
enum class BooleanVariant : BooleanGetter, ReadyContract { ONE }

interface RootGetter {
    fun getTitle(): String = "title"
}
interface ChildGetter : RootGetter
interface TitleContract { val title: String }
enum class InheritedVariant : ChildGetter, TitleContract { ONE }

interface AbstractChild : RootGetter {
    override fun getTitle(): String
}
enum class AbstractVariant : AbstractChild, TitleContract { ONE }

interface ParameterizedGetter {
    fun getCode(index: Int): String = index.toString()
}
interface CodeContract { val code: String }
enum class ParameterizedVariant : ParameterizedGetter, CodeContract { ONE }

interface UnitGetter {
    fun getUnit(): Unit = Unit
}
interface UnitContract { val unit: Unit }
enum class UnitVariant : UnitGetter, UnitContract { ONE }

interface GenericGetter<T> { fun getItem(): T = TODO() }
interface ItemContract { val item: String }
enum class GenericVariant : GenericGetter<String>, ItemContract { ONE }
interface Middle<U> : GenericGetter<List<U>>
interface ItemsContract { val item: List<String> }
enum class NestedGenericVariant : Middle<String>, ItemsContract { ONE }
"#,
    )
    .unwrap();
    fs::write(
        root.join("JavaGetters.java"),
        r#"package neutral.defaults;
interface VoidGetter { default void getFlag() {} }
interface StaticGetter { static String getLabel() { return "x"; } }
interface PrivateGetter { private String getSecret() { return "x"; } }
"#,
    )
    .unwrap();
    fs::write(
        root.join("java_contracts.kt"),
        r#"package neutral.defaults
interface FlagContract { val flag: Unit }
interface LabelContract { val label: String }
interface SecretContract { val secret: String }
enum class VoidVariant : VoidGetter, FlagContract { ONE }
enum class StaticVariant : StaticGetter, LabelContract { ONE }
enum class PrivateVariant : PrivateGetter, SecretContract { ONE }
"#,
    )
    .unwrap();

    let index = notlin::workspace::SourceIndex::discover(&root).unwrap();
    let path = root.join("types.kt");
    let bridges = |name: &str| index.enum_default_property_bridges(&path, name);
    assert_eq!(
        bridges("DirectVariant"),
        [notlin::workspace::EnumDefaultBridge {
            method_name: "getCategory".to_owned(),
            return_type: "String".to_owned(),
            provider: "DirectGetter".to_owned(),
        }]
    );
    assert_eq!(
        bridges("InheritedVariant"),
        [notlin::workspace::EnumDefaultBridge {
            method_name: "getTitle".to_owned(),
            return_type: "String".to_owned(),
            provider: "ChildGetter".to_owned(),
        }]
    );
    assert_eq!(
        bridges("BooleanVariant"),
        [notlin::workspace::EnumDefaultBridge {
            method_name: "isReady".to_owned(),
            return_type: "Boolean".to_owned(),
            provider: "BooleanGetter".to_owned(),
        }]
    );
    for name in [
        "AbstractVariant",
        "ParameterizedVariant",
        "UnitVariant",
        "GenericVariant",
        "NestedGenericVariant",
    ] {
        assert!(
            bridges(name).is_empty(),
            "{name} must not gain an unsafe bridge"
        );
    }
    let contract_path = root.join("java_contracts.kt");
    for name in ["VoidVariant", "StaticVariant", "PrivateVariant"] {
        assert!(
            index
                .enum_default_property_bridges(&contract_path, name)
                .is_empty(),
            "{name} must not gain an unsafe bridge"
        );
    }
    fs::remove_dir_all(root).unwrap();
}
