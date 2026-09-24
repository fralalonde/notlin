use clap::Parser;
use notlin::cli::Cli;
use notlin::transpiler;
use std::path::PathBuf;

/// `receiver.javaClass` is the Kotlin->Java interop getter spelled as a
/// property: the only valid Java lowering is `getClass()`. Regression for
/// the `getJavaClass()` emission.
#[test]
fn java_class_property_reads_become_get_class() {
    let source = r#"package neutral.work
class Ctx(val contextClass: String)
class JavaClassHolder {
    fun jc(c: Ctx): Class<Ctx> = c.javaClass
}
"#;
    let cli = Cli::parse_from(vec!["notlin", "Ctx.kt"]);
    let path = PathBuf::from("Ctx.kt");
    let (files, errors, _warnings, _cov) = transpiler::transpile(source, &path, &cli);
    assert_eq!(errors, 0);
    let holder = files
        .iter()
        .find(|(name, _)| name == "JavaClassHolder.java")
        .map(|(_, c)| c.as_str())
        .expect("JavaClassHolder.java");
    assert!(holder.contains("c.getClass()"), "{holder}");
    assert!(!holder.contains("getJavaClass"), "{holder}");
}
