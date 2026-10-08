use clap::Parser;
use notlin::{cli::Cli, transpiler};
use std::path::Path;

#[test]
fn unknown_required_call_type_is_retained_by_default() {
    let source =
        "fun uncertain() { val value = unknownApi(); println(value) }\nfun safe(): Int = 1\n";
    let cli = Cli::parse_from(["notlin", "--annotations", "none", "Policy.kt"]);
    let (files, _, _, coverage) = transpiler::transpile(source, Path::new("Policy.kt"), &cli);
    let java = files
        .iter()
        .map(|(_, java)| java.as_str())
        .collect::<String>();
    assert!(java.contains("safe("), "{java}");
    assert!(!java.contains("uncertain("), "{java}");
    assert!(coverage.untranslated.iter().any(|name| name == "uncertain"));
}

#[test]
fn explicit_approximation_mode_accepts_unknown_call_inference() {
    let source = "fun uncertain() { val value = unknownApi(); println(value) }\n";
    let cli = Cli::parse_from([
        "notlin",
        "--allow-approximations",
        "--annotations",
        "none",
        "Policy.kt",
    ]);
    let (files, _, warnings, _) = transpiler::transpile(source, Path::new("Policy.kt"), &cli);
    assert!(files.iter().any(|(_, java)| java.contains("uncertain(")));
    assert!(warnings > 0);
}

#[test]
fn known_unsigned_semantic_loss_requires_explicit_approximation() {
    let source = "class Counter(val count: UInt)\nclass Plain(val count: Int)\n";
    for allow in [false, true] {
        let mut args = vec!["notlin", "--annotations", "none", "Policy.kt"];
        if allow {
            args.push("--allow-approximations");
        }
        let cli = Cli::parse_from(args);
        let (files, errors, warnings, coverage) =
            transpiler::transpile(source, Path::new("Policy.kt"), &cli);
        assert_eq!(errors, 0);
        assert!(files.iter().any(|(name, _)| name == "Plain.java"));
        assert_eq!(files.iter().any(|(name, _)| name == "Counter.java"), allow);
        assert_eq!(
            coverage.untranslated.iter().any(|name| name == "Counter"),
            !allow
        );
        assert!(warnings > 0);
    }
}

#[test]
fn builtin_shadows_retain_the_whole_class_in_the_public_pipeline() {
    let source = "class Example { val println: () -> Unit = {}; fun run() = println() }\nclass Safe(val value: Int)\n";
    let cli = Cli::parse_from(["notlin", "--annotations", "none", "Policy.kt"]);
    let (files, errors, _, coverage) = transpiler::transpile(source, Path::new("Policy.kt"), &cli);
    assert_eq!(errors, 0);
    assert!(files.iter().any(|(name, _)| name == "Safe.java"));
    assert!(!files.iter().any(|(name, _)| name == "Example.java"));
    assert!(coverage.untranslated.iter().any(|name| name == "Example"));
}
