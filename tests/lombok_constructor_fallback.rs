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
