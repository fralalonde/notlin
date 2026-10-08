use notlin::java_ir::{
    JavaCategory, binary_expression, call_expression, identifier, parse_java, render,
    string_literal, unary_expression,
};

#[test]
fn parses_and_renders_structured_java_losslessly() {
    let source = r#"package demo;
import java.util.*;
@Deprecated
public record Box<T>(T value) {
  public static final int N = 3;
  public <R> R map(java.util.function.Function<T,R> f) { return f.apply(value); }
  public static void main(String[] args) {
    var xs = List.of(1, 2, 3).stream().filter(x -> x > 1).map(x -> x * 2).toList();
    for (var x : xs) { System.out.println(x); }
  }
}
enum E { A, B }
@interface Mark { String value() default "ok"; }
"#;
    let unit = parse_java(source).expect("valid Java");
    assert_eq!(render(&unit), source);
    assert_eq!(unit.root.category, JavaCategory::CompilationUnit);
    let mut categories = Vec::new();
    unit.root
        .clone()
        .visit_mut(&mut |node| categories.push(node.category));
    assert!(categories.contains(&JavaCategory::Declaration));
    assert!(categories.contains(&JavaCategory::Expression));
    assert!(categories.contains(&JavaCategory::Statement));
    assert!(categories.contains(&JavaCategory::Type));
    assert!(categories.contains(&JavaCategory::Import));
    assert!(categories.contains(&JavaCategory::Annotation));
}

#[test]
fn rejects_parser_errors_and_missing_nodes() {
    assert!(parse_java("class Broken { void f( { }").is_err());
    assert!(parse_java("class Broken { int x = ; }").is_err());
}

#[test]
fn string_literal_constructor_escapes_java_controls() {
    let literal = string_literal("a\"\\\n\t\u{0001}");
    let unit = notlin::java_ir::JavaCompilationUnit {
        root: notlin::java_ir::JavaSyntaxNode::new(
            JavaCategory::CompilationUnit,
            "program",
            vec![literal],
        ),
    };
    assert_eq!(render(&unit), "\"a\\\"\\\\\\n\\t\\001\"");
}

#[test]
fn constructed_expressions_keep_precedence_and_escape_call_arguments() {
    let sum = binary_expression("+", identifier("a"), identifier("b"));
    let product = binary_expression("*", sum, identifier("c"));
    let right_subtraction = binary_expression(
        "-",
        identifier("a"),
        binary_expression("-", identifier("b"), identifier("c")),
    );
    let call = call_expression(
        identifier("consume"),
        vec![
            string_literal("line\n\""),
            binary_expression("+", identifier("x"), identifier("y")),
        ],
    );
    let nested_unary = unary_expression("-", unary_expression("-", identifier("x")));
    let assignment = binary_expression(
        "=",
        identifier("a"),
        binary_expression("=", identifier("b"), identifier("c")),
    );
    let make_unit = |expression| notlin::java_ir::JavaCompilationUnit {
        root: notlin::java_ir::JavaSyntaxNode::new(
            JavaCategory::CompilationUnit,
            "program",
            vec![expression],
        ),
    };
    assert_eq!(render(&make_unit(product)), "(a + b) * c");
    assert_eq!(render(&make_unit(right_subtraction)), "a - (b - c)");
    assert_eq!(render(&make_unit(call)), "consume(\"line\\n\\\"\", x + y)");
    assert_eq!(render(&make_unit(nested_unary)), "-(-x)");
    assert_eq!(render(&make_unit(assignment)), "a = b = c");
}

#[test]
fn prunes_only_unused_lombok_imports_from_structured_references() {
    let source = "import lombok.Data;\nimport lombok.Value;\nclass Model {\n    @Data String field;\n    String note = \"Value\";\n}\n";
    let mut unit = parse_java(source).expect("valid Java");
    assert_eq!(notlin::java_ir::remove_unused_lombok_imports(&mut unit), 1);
    let rendered = render(&unit);
    assert!(rendered.contains("import lombok.Data;"));
    assert!(!rendered.contains("import lombok.Value;"));
    assert!(rendered.contains("String note = \"Value\";"));
}
