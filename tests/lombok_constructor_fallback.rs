use clap::Parser;
use notlin::cli::Cli;
use std::path::Path;

#[test]
fn custom_object_method_keeps_explicit_constructor_and_accessors_in_lombok_mode() {
    let source = r#"package sample
class Payload(private val text: String, var count: Int) {
    override fun toString(): String = text
}
"#;
    for lombok in [false, true] {
        let mut args = vec!["notlin", "--annotations", "none"];
        if lombok {
            args.push("--lombok");
        }
        args.push("Payload.kt");
        let cli = Cli::parse_from(args);
        let (files, errors, warnings, _) =
            notlin::transpiler::transpile(source, Path::new("Payload.kt"), &cli);
        assert_eq!((errors, warnings), (0, 0));
        let java = &files
            .iter()
            .find(|(name, _)| name == "Payload.java")
            .unwrap()
            .1;
        assert!(
            java.contains("public Payload(String text, int count)"),
            "{java}"
        );
        assert!(java.contains("String getText()"), "{java}");
        assert!(java.contains("int getCount()"), "{java}");
        assert!(java.contains("void setCount(int count)"), "{java}");
        assert!(
            !java.contains("@AllArgsConstructor") && !java.contains("@Getter"),
            "{java}"
        );
    }
}

#[test]
fn uniform_setters_move_to_the_class_without_changing_equality_or_hidden_fields() {
    let source = r#"package sample
class Mutable(var first: String, var second: Int)
class Mixed(val id: String, var count: Int)
class Computed(var label: String) {
    val length: Int get() = label.length
}
data class Value(var first: String, var second: Int)
class Hidden(var label: String) {
    private var count: Int = 0
    fun bump(): Int { count++; return count }
}
"#;
    let cli = Cli::parse_from(["notlin", "--annotations", "none", "--lombok", "Types.kt"]);
    let (files, errors, _, _) = notlin::transpiler::transpile(source, Path::new("Types.kt"), &cli);
    assert_eq!(errors, 0);
    let java = |name: &str| {
        files
            .iter()
            .find(|(file, _)| file == &format!("{name}.java"))
            .unwrap_or_else(|| panic!("{name} must translate"))
            .1
            .as_str()
    };
    for name in ["Mutable", "Mixed", "Computed"] {
        let text = java(name);
        assert_eq!(text.matches("@Setter").count(), 1, "{text}");
        assert!(
            text.find("@Setter").unwrap() < text.find(&format!("class {name}")).unwrap(),
            "{text}"
        );
        assert!(
            !text.contains("@Data"),
            "ordinary identity equality must remain: {text}"
        );
    }
    assert!(java("Mixed").contains("final String id"));
    let value = java("Value");
    assert!(
        value.contains("@Data") && !value.contains("@Setter"),
        "{value}"
    );
    let hidden = java("Hidden");
    assert!(
        hidden.find("@Setter").unwrap() > hidden.find("class Hidden").unwrap(),
        "{hidden}"
    );
}
