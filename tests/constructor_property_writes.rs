use clap::Parser;
use notlin::{cli::Cli, transpiler::transpile};
use std::path::Path;

#[test]
fn setter_body_can_update_another_constructor_property() {
    let source = r#"
class Entry(var amount: String, var unit: String) {
    fun setAmount(value: String, symbol: String) {
        this.amount = value
        this.unit = symbol
    }
}
"#;
    let cli = Cli::parse_from(["notlin", "--lombok"]);
    let (files, errors, _, _) = transpile(source, Path::new("Entry.kt"), &cli);
    assert_eq!(errors, 0);
    let java = &files
        .iter()
        .find(|(name, _)| name == "Entry.java")
        .expect("Entry accepted")
        .1;
    assert!(java.contains("this.amount = value;"), "{java}");
    assert!(java.contains("this.setUnit(symbol);"), "{java}");
    assert!(!java.contains("getUnit() ="), "{java}");
}

#[test]
fn lombok_overload_does_not_suppress_mutable_constructor_property_setter() {
    let source = r#"
class AmountUpdate(val amount: String, val unit: String)

class Entry(var amount: String, var unit: String) {
    fun setAmount(value: AmountUpdate) {
        this.amount = value.amount
        this.unit = value.unit
    }
}

"#;
    let cli = Cli::parse_from(["notlin", "--lombok"]);
    let (files, errors, _, _) = transpile(source, Path::new("Entry.kt"), &cli);
    assert_eq!(errors, 0);
    let java = &files
        .iter()
        .find(|(name, _)| name == "Entry.java")
        .expect("Entry accepted")
        .1;
    assert!(
        java.contains("public void setAmount(String amount)"),
        "{java}"
    );
    assert!(java.contains("this.amount = value.getAmount();"), "{java}");
    assert!(java.contains("this.setUnit(value.getUnit());"), "{java}");
}

#[test]
fn nullable_reference_does_not_add_a_duplicate_setter_signature() {
    let source = r#"
class Entry {
    private var label: String? = null
    fun setLabel(value: String) { label = value }
}
"#;
    let cli = Cli::parse_from(["notlin"]);
    let (files, errors, _, _) = transpile(source, Path::new("Entry.kt"), &cli);
    assert_eq!(errors, 0);
    let java = &files
        .iter()
        .find(|(name, _)| name == "Entry.java")
        .expect("Entry accepted")
        .1;
    assert_eq!(java.matches("void setLabel(").count(), 1, "{java}");
    assert!(java.contains("label = value;"), "{java}");
}
