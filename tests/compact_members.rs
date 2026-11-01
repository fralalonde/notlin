use notlin::semantics::{SemanticProvider, SyntaxSemanticProvider};
use notlin::workspace::{SourceIndex, SourceLanguage, SourceOverlay};

fn parse(source: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    parser.parse(source, None).unwrap()
}

#[test]
fn closing_braces_terminate_members_without_a_separator() {
    let declarations = [
        "interface Root { val alias: String",
        "interface Defaulted { val alias: String get() = \"fixed\"",
        "interface Block { val alias: String get() { return \"fixed\" }",
        "class Methods { fun answer(): Int = 42",
        "object Tokens { const val KEY: String = \"}\"",
        "enum class Kind { ONE; val label: String get() = \"ok\"",
        "class Outer { interface Inner { val key: String}",
    ];
    for prefix in declarations {
        for separator in [
            "",
            " ",
            "\t",
            ";",
            "/* trailing */",
            "/* outer /* inner */ end */",
            "\n",
            "\r\n",
        ] {
            let source = format!("package sample\n{prefix}{separator}}}\nclass Following\n");
            let tree = parse(&source);
            assert!(
                !tree.root_node().has_error(),
                "{source}\n{}",
                tree.root_node().to_sexp()
            );
            let following = tree
                .root_node()
                .named_children(&mut tree.root_node().walk())
                .filter_map(|node| node.child_by_field_name("name"))
                .any(|node| node.utf8_text(source.as_bytes()).unwrap() == "Following");
            assert!(
                following,
                "the next declaration must remain visible: {source}"
            );
        }
    }
}

#[test]
fn parsing_preserves_original_spans_and_literal_contents() {
    let source = "package sample\r\ninterface Café{val title:String get()=\"} café\"}\r\nclass Following\r\n";
    let tree = parse(source);
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    let mut stack = vec![tree.root_node()];
    let mut literal = None;
    while let Some(node) = stack.pop() {
        if node.kind() == "string_literal" {
            literal = Some(node.byte_range());
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    assert_eq!(&source[literal.unwrap()], "\"} café\"");
    let path = std::env::current_dir().unwrap().join("Compact.kt");
    let index = SourceIndex::default()
        .with_overlays(&[SourceOverlay::Replace {
            path: path.clone(),
            language: SourceLanguage::Kotlin,
            source: source.into(),
        }])
        .unwrap();
    let provider = SyntaxSemanticProvider::new([(path.clone(), source.into())]);
    for name in ["Café", "Following"] {
        let declaration = index.declarations().find(|d| d.name == name).unwrap();
        let id = notlin::semantics::workspace_symbol(&index, declaration);
        let symbols = provider.symbols_in_file(&path);
        let symbol = symbols.iter().find(|s| s.id == id).unwrap();
        let bytes = &source[symbol.location.start_byte..symbol.location.end_byte];
        assert!(bytes.contains(name), "{bytes}");
    }
}

#[test]
fn missing_types_bodies_and_comment_terminators_still_fail_parsing() {
    for source in [
        "interface Broken { val value: }",
        "interface Broken { val value: String",
        "interface Broken { val value: String // swallowed brace }",
        "interface Broken { val value: String /* unfinished }",
        "interface Broken { val value: String get() = \"unfinished }",
    ] {
        assert!(
            parse(source).root_node().has_error(),
            "invalid source became valid: {source}"
        );
    }
}
