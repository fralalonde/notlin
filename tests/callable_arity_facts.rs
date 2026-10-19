use notlin::workspace::{CallableArity, Member, MemberKind, SourceIndex};
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_root() -> PathBuf {
    let root = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!(
            "notlin-callable-arity-facts-{}",
            std::process::id()
        ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn write_sources(root: &Path) {
    fs::write(
        root.join("KotlinShapes.kt"),
        r#"package sample
class KotlinShapes {
    fun required(first: Int, second: String) {}
    fun defaults(first: Int = 0, second: String = "") {}
    fun middle(first: Int = 0, required: String, last: Int = 1) {}
    fun commented(first: Int = 0, // this default is optional
                  required: String) {}
    @JvmOverloads fun jvm(first: Int = 0, second: Int = 1) {}
    fun zero() {}
    fun trailing(first: Int = 0, vararg rest: String) {}
    fun nonfinal(vararg values: Int, tail: String) {}
    fun String.memberExtension(value: Int = 0) {}
    fun overload() {}
    fun overload(value: Int) {}
}
"#,
    )
    .unwrap();
    fs::write(
        root.join("JavaShapes.java"),
        r#"package sample;
public class JavaShapes {
    public void required(int first, String second) {}
    public void noarg() {}
    public void trailing(int first, String... rest) {}
    public void setValue(String value, int index) {}
}
"#,
    )
    .unwrap();
    // These newer/invalid forms may not produce a complete method node in the
    // pinned grammar. If one is indexed, its arity must remain unknown.
    fs::write(
        root.join("Unsupported.kt"),
        r#"package sample
class Unsupported {
    context(String) fun contextual() {}
    fun malformed(value: Int = ) {}
}
"#,
    )
    .unwrap();
}

fn declaration<'a>(index: &'a SourceIndex, name: &str) -> &'a notlin::workspace::Declaration {
    index
        .files
        .iter()
        .flat_map(|file| file.declarations.iter())
        .find(|declaration| declaration.name == name)
        .unwrap_or_else(|| panic!("missing indexed declaration {name}"))
}

fn methods<'a>(index: &'a SourceIndex, owner: &str, name: &str) -> Vec<&'a Member> {
    declaration(index, owner)
        .members
        .iter()
        .filter(|member| member.kind == MemberKind::Method && member.name == name)
        .collect()
}

fn arity(index: &SourceIndex, owner: &str, name: &str) -> CallableArity {
    methods(index, owner, name)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("missing method {owner}.{name}"))
        .callable_arity
        .unwrap_or_else(|| panic!("missing callable arity for {owner}.{name}"))
}

#[test]
fn indexed_kotlin_and_java_methods_record_source_accepted_arities() {
    let root = fixture_root();
    write_sources(&root);
    let index = SourceIndex::discover(&root).unwrap();

    let required = arity(&index, "KotlinShapes", "required");
    assert_eq!(
        required,
        CallableArity {
            required: 2,
            fixed: 2,
            vararg: false
        }
    );
    assert!(!required.accepts_arity(0));
    assert!(!required.accepts_arity(1));
    assert!(required.accepts_arity(2));
    assert!(!required.accepts_arity(3));

    let defaults = arity(&index, "KotlinShapes", "defaults");
    assert_eq!(
        defaults,
        CallableArity {
            required: 0,
            fixed: 2,
            vararg: false
        }
    );
    assert!((0..=2).all(|count| defaults.accepts_arity(count)));
    assert!(!defaults.accepts_arity(3));

    let middle = arity(&index, "KotlinShapes", "middle");
    assert_eq!(
        middle,
        CallableArity {
            required: 1,
            fixed: 3,
            vararg: false
        }
    );
    assert!(!middle.accepts_arity(0));
    assert!((1..=3).all(|count| middle.accepts_arity(count)));
    assert!(!middle.accepts_arity(4));
    let commented = arity(&index, "KotlinShapes", "commented");
    assert_eq!(
        commented,
        CallableArity {
            required: 1,
            fixed: 2,
            vararg: false
        }
    );

    let jvm_overloads = arity(&index, "KotlinShapes", "jvm");
    assert_eq!(
        jvm_overloads,
        CallableArity {
            required: 0,
            fixed: 2,
            vararg: false
        }
    );
    let noarg = arity(&index, "KotlinShapes", "zero");
    assert!(noarg.accepts_arity(0));
    assert!(!noarg.accepts_arity(1));

    let trailing = arity(&index, "KotlinShapes", "trailing");
    assert_eq!(
        trailing,
        CallableArity {
            required: 0,
            fixed: 1,
            vararg: true
        }
    );
    assert!((0..=5).all(|count| trailing.accepts_arity(count)));

    let java_required = arity(&index, "JavaShapes", "required");
    assert_eq!(
        java_required,
        CallableArity {
            required: 2,
            fixed: 2,
            vararg: false
        }
    );
    assert!(java_required.accepts_arity(2));
    assert!(!java_required.accepts_arity(1));
    let java_noarg = arity(&index, "JavaShapes", "noarg");
    assert!(java_noarg.accepts_arity(0));
    let java_vararg = arity(&index, "JavaShapes", "trailing");
    assert_eq!(
        java_vararg,
        CallableArity {
            required: 1,
            fixed: 1,
            vararg: true
        }
    );
    assert!(java_vararg.accepts_arity(1));
    assert!(java_vararg.accepts_arity(4));
    assert!(!java_vararg.accepts_arity(0));
    let setter = arity(&index, "JavaShapes", "setValue");
    assert!(setter.accepts_arity(2));
    assert!(!setter.accepts_arity(1));

    let overloads = methods(&index, "KotlinShapes", "overload");
    assert_eq!(overloads.len(), 2);
    let mut overload_arities = overloads
        .iter()
        .map(|member| member.callable_arity.unwrap())
        .collect::<Vec<_>>();
    overload_arities.sort_by_key(|fact| fact.fixed);
    assert_eq!(
        overload_arities[0],
        CallableArity {
            required: 0,
            fixed: 0,
            vararg: false
        }
    );
    assert_eq!(
        overload_arities[1],
        CallableArity {
            required: 1,
            fixed: 1,
            vararg: false
        }
    );

    let member_extension = methods(&index, "KotlinShapes", "memberExtension");
    assert_eq!(member_extension.len(), 1);
    assert_eq!(member_extension[0].callable_arity, None);
    let nonfinal_vararg = methods(&index, "KotlinShapes", "nonfinal");
    assert_eq!(nonfinal_vararg.len(), 1);
    assert_eq!(nonfinal_vararg[0].callable_arity, None);
    if let Some(unsupported) = index
        .files
        .iter()
        .flat_map(|file| file.declarations.iter())
        .find(|declaration| declaration.name == "Unsupported")
    {
        for member in unsupported
            .members
            .iter()
            .filter(|member| matches!(member.name.as_str(), "contextual" | "malformed"))
        {
            assert_eq!(member.callable_arity, None, "{}", member.name);
        }
    }

    let encoded = serde_json::to_vec(overloads[1]).unwrap();
    let decoded: Member = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, *overloads[1]);

    let (cached, stats) = SourceIndex::discover_with_stats(&root).unwrap();
    assert!(stats.reused_files >= 2, "{stats:?}");
    assert_eq!(arity(&cached, "KotlinShapes", "defaults"), defaults);
    assert_eq!(arity(&cached, "JavaShapes", "trailing"), java_vararg);
    fs::remove_dir_all(root).unwrap();
}
