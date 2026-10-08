use notlin::semantics::*;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

fn function_symbol(src: &str) -> SymbolId {
    let mut p = tree_sitter::Parser::new();
    p.set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = p.parse(src, None).unwrap();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.kind() == "function_declaration" {
            return symbol_id_for_node(src, n, std::path::Path::new("a.kt"));
        }
        let mut c = n.walk();
        stack.extend(n.named_children(&mut c));
    }
    panic!("missing function")
}

fn required_call_dependency(
    source: &str,
    file: &str,
    provider: &SyntaxSemanticProvider,
    spelling: &str,
) -> FactStatus<SymbolId> {
    let extension = Path::new(file)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("kt");
    let language = if extension == "java" {
        tree_sitter_java::LANGUAGE.into()
    } else {
        tree_sitter_kotlin_ng::LANGUAGE.into()
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), Path::new(file));
    plan_required_calls(source, &tree, Path::new(file), provider, false, &mut plan);
    plan.dependencies
        .into_iter()
        .find(|dependency| dependency.spelling == spelling)
        .map(|dependency| dependency.resolution)
        .unwrap_or(FactStatus::Unknown)
}

#[test]
fn required_calls_resolve_inherited_default_members_for_kotlin_implicit_this() {
    let source = r#"package sem
interface TaskRoot { fun executeTask() = Unit }
interface TaskLayer : TaskRoot {
    fun layerTask() = Unit
}
class TaskRunner : TaskLayer {
    fun runTask() {
        executeTask()
    }
}
"#;
    let file = "TaskRunner.kt";
    let provider = SyntaxSemanticProvider::new(vec![(PathBuf::from(file), source.into())]);
    let resolved = required_call_dependency(source, file, &provider, "executeTask");
    let FactStatus::Established(symbol) = resolved else {
        panic!("expected inherited member, got {resolved:?}")
    };
    assert_eq!(symbol.name, "executeTask");
    assert_eq!(symbol.kind, "function");
    assert_eq!(symbol.owner_path, vec!["TaskRoot"]);
}

#[test]
fn required_calls_resolve_inherited_java_interface_defaults_and_typed_receivers() {
    let base = r#"package sem;
public interface TaskContract { default String readTaskId() { return "id"; } }
"#;
    let child = r#"package sem;
class TaskImpl implements TaskContract { void runTask() { readTaskId(); } }
"#;
    let caller = r#"package sem;
class TaskCaller { void runTask() { TaskImpl receiver = new TaskImpl(); receiver.readTaskId(); } }
"#;
    let provider = SyntaxSemanticProvider::new(vec![
        (PathBuf::from("TaskContract.java"), base.into()),
        (PathBuf::from("TaskImpl.java"), child.into()),
        (PathBuf::from("TaskCaller.java"), caller.into()),
    ]);
    let implicit = required_call_dependency(child, "TaskImpl.java", &provider, "readTaskId");
    let FactStatus::Established(symbol) = implicit else {
        panic!("expected inherited default method, got {implicit:?}")
    };
    assert_eq!(symbol.name, "readTaskId");
    assert_eq!(symbol.kind, "function");
    assert_eq!(symbol.owner_path, vec!["TaskContract"]);

    let explicit = required_call_dependency(caller, "TaskCaller.java", &provider, "readTaskId");
    let FactStatus::Established(symbol) = explicit else {
        panic!("expected typed inherited receiver method, got {explicit:?}")
    };
    assert_eq!(symbol.name, "readTaskId");
    assert_eq!(symbol.file, PathBuf::from("TaskContract.java"));
}

#[test]
fn required_calls_resolve_kotlin_typed_receiver_to_inherited_java_getter() {
    let api = r#"package sem;
public interface LookupContract { String getLookupKey(); }
"#;
    let implementation = r#"package sem;
public class LookupAdapter implements LookupContract { }
"#;
    let caller = r#"package sem
class LookupConsumer(private val adapter: LookupAdapter) {
    fun readLookupKey() {
        adapter.getLookupKey()
    }
}
"#;
    let provider = SyntaxSemanticProvider::new(vec![
        (PathBuf::from("LookupContract.java"), api.into()),
        (PathBuf::from("LookupAdapter.java"), implementation.into()),
        (PathBuf::from("LookupConsumer.kt"), caller.into()),
    ]);
    let resolved = required_call_dependency(caller, "LookupConsumer.kt", &provider, "getLookupKey");
    let FactStatus::Established(symbol) = resolved else {
        panic!("expected inherited Java getter, got {resolved:?}")
    };
    assert_eq!(symbol.name, "getLookupKey");
    assert_eq!(symbol.file, PathBuf::from("LookupContract.java"));
}

#[test]
fn unresolved_uppercase_instance_binding_does_not_resolve_as_companion_type() {
    let source = r#"package sem
interface TaskOwner {
    companion object {
        fun createTask() = Unit
    }
}
fun useTaskOwner() {
    val TaskOwner: Any = Any()
    TaskOwner.createTask()
}
"#;
    let provider =
        SyntaxSemanticProvider::new(vec![(PathBuf::from("TaskOwner.kt"), source.into())]);
    assert!(matches!(
        required_call_dependency(source, "TaskOwner.kt", &provider, "createTask"),
        FactStatus::Unknown
    ));
}

#[test]
fn companion_call_symbols_retain_their_enclosing_owner_identity() {
    let source = r#"package semanticfixture
interface TaskOwner {
    companion object {
        fun createTask(value: String): String = value
    }
}
"#;
    let provider =
        SyntaxSemanticProvider::new(vec![(PathBuf::from("TaskOwner.kt"), source.into())]);
    let owner = provider
        .symbols()
        .iter()
        .find(|symbol| symbol.id.name == "TaskOwner" && symbol.id.kind == "interface")
        .expect("owner interface should be indexed");
    let members = provider.companion_members(&owner.id, "createTask");
    assert_eq!(
        members.len(),
        1,
        "indexed member identities: {:?}",
        provider
            .symbols_named("createTask")
            .iter()
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn required_calls_resolve_type_qualified_unnamed_companion_members() {
    let owner_source = r#"package semanticfixture
interface TaskOwner {
    companion object {
        fun createTask(value: String): String = value
    }
}
"#;
    let source = r#"package semanticfixture
enum class TaskKind(val id: String) {
    FIRST(TaskOwner.createTask("first"))
}
"#;
    let file = "TaskKind.kt";
    let provider = SyntaxSemanticProvider::new(vec![
        (PathBuf::from(file), source.into()),
        (PathBuf::from("TaskOwner.kt"), owner_source.into()),
    ]);
    let resolved = required_call_dependency(source, file, &provider, "createTask");
    let FactStatus::Established(symbol) = resolved else {
        panic!("expected type-qualified companion method, got {resolved:?}")
    };
    assert_eq!(symbol.name, "createTask");
}

#[test]
fn class_qualified_instance_method_is_not_resolved_as_a_static_call() {
    let source = r#"package semanticfixture
class TaskOwner {
    fun createTask(value: String): String = value
}
fun useTaskOwner(): String = TaskOwner.createTask("task")
"#;
    let file = "TaskOwner.kt";
    let provider = SyntaxSemanticProvider::new(vec![(PathBuf::from(file), source.into())]);
    assert!(matches!(
        required_call_dependency(source, file, &provider, "createTask"),
        FactStatus::Unknown
    ));
}
#[test]
fn required_inherited_calls_preserve_arity_and_diamond_ambiguity() {
    let wrong_arity = r#"package sem
interface TaskContract { fun runTask(value: Int) = Unit }
class TaskRunner : TaskContract {
    fun callTask() {
        runTask()
    }
}
"#;
    let wrong_provider =
        SyntaxSemanticProvider::new(vec![(PathBuf::from("TaskRunner.kt"), wrong_arity.into())]);
    assert!(matches!(
        required_call_dependency(wrong_arity, "TaskRunner.kt", &wrong_provider, "runTask"),
        FactStatus::Unknown
    ));

    let diamond = r#"package sem
interface LeftContract {
    fun runTask() = Unit
}
interface RightContract {
    fun runTask() = Unit
}
class TaskRunner : LeftContract, RightContract {
    fun callTask() {
        runTask()
    }
}
"#;
    let diamond_provider =
        SyntaxSemanticProvider::new(vec![(PathBuf::from("TaskRunner.kt"), diamond.into())]);
    let owners = diamond_provider
        .symbols_named("runTask")
        .iter()
        .map(|symbol| symbol.id.owner_path.clone())
        .collect::<Vec<_>>();
    assert!(
        owners.contains(&vec!["LeftContract".to_owned()]),
        "indexed owners: {owners:?}"
    );
    assert!(
        owners.contains(&vec!["RightContract".to_owned()]),
        "indexed owners: {owners:?}"
    );
    let resolved = required_call_dependency(diamond, "TaskRunner.kt", &diamond_provider, "runTask");
    let FactStatus::Ambiguous(candidates) = resolved else {
        panic!("expected inherited diamond ambiguity, got {resolved:?}")
    };
    assert_eq!(candidates.len(), 2);
}

#[test]
fn symbol_ids_ignore_whitespace_and_bodies_but_distinguish_overloads_and_packages() {
    let a = function_symbol("package p\nfun f(x: Int) = x\n");
    let b = function_symbol("package p\nfun   f ( x : Int ) = x + 2\n");
    assert_eq!(a, b);
    let overload = function_symbol("package p\nfun f(x: String) = x\n");
    assert_ne!(a, overload);
    let package = function_symbol("package q\nfun f(x: Int) = x\n");
    assert_ne!(a, package);
    let array = function_symbol("fun f(values: IntArray) = values\n");
    let vararg = function_symbol("fun f(vararg values: Int) = values\n");
    assert_ne!(array, vararg);
}

#[test]
fn annotations_defaults_lambdas_and_interfaces_are_identity_safe() {
    let a = function_symbol(
        "fun f(@Mark(1) value: Int = 1, block: (Int) -> String = { it.toString() }) = block(value)\n",
    );
    let b = function_symbol(
        "fun f(@Mark(2) value: Int = 9, block: (Int) -> String = { x -> x.toString() }) = \"changed\"\n",
    );
    assert_eq!(a, b);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let src = "interface Contract\n";
    let tree = parser.parse(src, None).unwrap();
    let mut nodes = vec![tree.root_node()];
    let mut found = None;
    while let Some(n) = nodes.pop() {
        if n.kind() == "class_declaration" {
            found = Some(n);
            break;
        }
        let mut c = n.walk();
        nodes.extend(n.named_children(&mut c));
    }
    assert_eq!(
        symbol_id_for_node(src, found.unwrap(), std::path::Path::new("a.kt")).kind,
        "interface"
    );
}

#[test]
fn recovered_annotated_interface_keeps_its_name_and_kind_across_reparse() {
    let original = r#"
interface Earlier { val id: String }
enum class Reader { FIRST }
enum class Writer { SECOND }
@Read(using = Reader::class)
@Write(using = Writer::class)
interface Contract {
    val first: String
    val second: String
    fun key(): String = if (first.isNotEmpty()) {
        "${first.lowercase(java.util.Locale.getDefault())}"
    } else second
}
"#;
    let residual = original
        .replace("enum class Reader { FIRST }\n", "")
        .replace("enum class Writer { SECOND }\n", "")
        .replace("interface Earlier { val id: String }\n", "");

    fn parse(source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }
    fn descendants(root: tree_sitter::Node) -> Vec<tree_sitter::Node> {
        let mut nodes = Vec::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            nodes.push(node);
            let mut walk = node.walk();
            stack.extend(node.named_children(&mut walk));
        }
        nodes
    }

    let original_tree = parse(original);
    let recovered = descendants(original_tree.root_node())
        .into_iter()
        .find(|node| {
            matches!(node.kind(), "annotated_expression" | "ERROR")
                && node
                    .utf8_text(original.as_bytes())
                    .is_ok_and(|text| text.contains("interface Contract"))
        })
        .expect("the original fixture should expose parser recovery around Contract");
    let original_id = symbol_id_for_node(original, recovered, Path::new("sample.kt"));

    let residual_tree = parse(&residual);
    let residual_interface = descendants(residual_tree.root_node())
        .into_iter()
        .find(|node| {
            matches!(node.kind(), "interface_declaration" | "class_declaration")
                && node
                    .utf8_text(residual.as_bytes())
                    .is_ok_and(|text| text.contains("interface Contract"))
        })
        .expect("the residual fixture should parse Contract normally");
    let residual_id = symbol_id_for_node(&residual, residual_interface, Path::new("sample.kt"));

    assert_eq!(original_id.name, "Contract");
    assert_eq!(original_id.kind, "interface");
    assert_eq!(original_id, residual_id);
}

#[test]
fn nested_declarations_under_extension_overloads_have_distinct_owner_paths() {
    fn local_type(src: &str) -> SymbolId {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src, None).unwrap();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "class_declaration"
                && node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(src.as_bytes()).ok())
                    == Some("Local")
            {
                return symbol_id_for_node(src, node, Path::new("a.kt"));
            }
            let mut walk = node.walk();
            stack.extend(node.named_children(&mut walk));
        }
        panic!("no local class")
    }
    let left = local_type("fun A.outer(x: Int) { class Local }\n");
    let right = local_type("fun B.outer(x: Int) { class Local }\n");
    assert_ne!(left.owner_path, right.owner_path);
}

#[test]
fn type_parser_preserves_nullability_generics_arrays_and_suspend_function_shape() {
    let t = TypeRef::parse("List<out Array<Int?>>?");
    assert!(t.nullable);
    let f = TypeRef::parse("suspend (String, Int) -> Boolean");
    assert!(matches!(f.ty,KotlinType::Function{suspend:true,parameters,..} if parameters.len()==2));
    assert!(matches!(t.ty,KotlinType::Named{arguments,..} if arguments.len()==1));
    assert!(
        matches!(TypeRef::parse("Widget").ty, KotlinType::Named { .. }),
        "unscoped lowercase identifiers remain named types"
    );
    let parameterized = TypeRef::parse_with_type_parameters("List<T?>", &["T".into()]);
    assert!(
        matches!(parameterized.ty,KotlinType::Named{arguments,..} if matches!(arguments[0].ty.as_ref(),Some(KotlinType::Nullable(inner)) if matches!(inner.as_ref(),KotlinType::TypeParameter(_))))
    );
    let nested = TypeRef::parse("((Int) -> String) -> Boolean");
    assert!(
        matches!(&nested.ty,KotlinType::Function{parameters,..} if matches!(parameters[0],KotlinType::Function{..})),
        "parsed type: {nested:?}"
    );
    let aliases = BTreeMap::from([("Id".to_string(), TypeRef::parse("Long"))]);
    assert_eq!(
        TypeRef::parse("Id?").expand_aliases(&aliases),
        TypeRef::parse("Long?")
    );
    let generic_aliases = BTreeMap::from([(
        "Boxed".to_string(),
        TypeAliasDefinition {
            parameters: vec!["T".into()],
            target: TypeRef::parse_with_type_parameters("List<T?>", &["T".into()]),
        },
    )]);
    assert_eq!(
        TypeRef::parse("Boxed<String>").expand_alias_definitions(&generic_aliases),
        TypeRef::parse("List<String?>")
    );
    let substitutions = BTreeMap::from([("T".to_string(), TypeRef::parse("String?"))]);
    let value = TypeRef::parse_with_type_parameters("T", &["T".into()]).substitute(&substitutions);
    assert_eq!(value, TypeRef::parse("String?"));
    assert_eq!(TypeRef::from_java_name("int"), TypeRef::parse("Int"));
    assert_eq!(
        TypeRef::parse("IntArray"),
        TypeRef {
            ty: KotlinType::Array(Box::new(KotlinType::Primitive("Int".into()))),
            nullable: false
        }
    );
    assert_eq!(
        TypeRef::from_java_name("java.lang.String[]"),
        TypeRef {
            ty: KotlinType::Array(Box::new(KotlinType::Named {
                name: "String".into(),
                arguments: vec![]
            })),
            nullable: false
        }
    );
}

#[test]
fn symbol_keys_encode_structural_fields_without_delimiter_collisions() {
    let mut left = function_symbol("fun f() = Unit\n");
    let mut right = left.clone();
    left.owner_path = vec!["A.B".into()];
    right.owner_path = vec!["A".into(), "B".into()];
    assert_ne!(left.stable_key(), right.stable_key());
}

#[test]
fn provider_reports_ambiguous_names_and_snapshot_locations() {
    let p = SyntaxSemanticProvider::new(vec![
        (
            PathBuf::from("a.kt"),
            "package x\nfun same(a: Int) = a\n".into(),
        ),
        (
            PathBuf::from("b.kt"),
            "package y\nfun same(a: String) = a\n".into(),
        ),
    ]);
    assert!(matches!(p.resolve("same"),FactStatus::Ambiguous(v) if v.len()==2));
    assert!(matches!(p.resolve("missing"), FactStatus::Unknown));
    assert_ne!(p.symbols()[0].location.snapshot_hash, [0; 32]);
    let changed = SyntaxSemanticProvider::new(vec![(
        PathBuf::from("a.kt"),
        "package x\nfun same(a: Int) = a + 1\n".into(),
    )]);
    assert_ne!(
        p.symbols()[0].location.snapshot_hash,
        changed.symbols()[0].location.snapshot_hash
    );
}

#[test]
fn provider_indexes_multiline_nested_declarations_and_preserves_owner() {
    let source = "package p\nclass Box {\n  fun item(\n    value: List<String?>\n  ) = value\n}\n";
    let p = SyntaxSemanticProvider::new(vec![(PathBuf::from("box.kt"), source.into())]);
    let fun = p.symbols().iter().find(|s| s.id.name == "item").unwrap();
    assert_eq!(fun.id.owner_path, vec!["Box"]);
    assert_eq!(fun.id.parameters, vec!["List<String?>"]);
}

#[test]
fn provider_uses_declared_type_nodes_and_in_scope_type_parameters() {
    let source = "class Box<T> { val value: T }\n@Mark(value = call()) fun <R> convert(input: R = make()): R = input\n";
    let provider = SyntaxSemanticProvider::new(vec![(PathBuf::from("typed.kt"), source.into())]);
    let value = provider
        .symbols()
        .iter()
        .find(|s| s.id.name == "value")
        .unwrap();
    assert!(
        matches!(&value.ty,FactStatus::Established(TypeRef{ty:KotlinType::TypeParameter(name),nullable:false}) if name=="T"),
        "{value:?}"
    );
    let convert = provider
        .symbols()
        .iter()
        .find(|s| s.id.name == "convert")
        .unwrap();
    assert!(
        matches!(&convert.ty,FactStatus::Established(TypeRef{ty:KotlinType::TypeParameter(name),nullable:false}) if name=="R"),
        "{convert:?}"
    );
}

#[test]
fn provider_indexes_only_val_var_constructor_parameters_as_properties() {
    let source = "class Box<T>(val value: T, var label: String, plain: Int)\n";
    let provider = SyntaxSemanticProvider::new(vec![(PathBuf::from("box.kt"), source.into())]);
    let properties: Vec<_> = provider
        .symbols()
        .iter()
        .filter(|s| s.id.kind == "property")
        .collect();
    assert_eq!(properties.len(), 2);
    assert!(properties.iter().any(|p|p.id.name=="value"&&p.id.owner_path==vec!["Box"]&&matches!(&p.ty,FactStatus::Established(TypeRef{ty:KotlinType::TypeParameter(name),..}) if name=="T")));
    assert!(properties.iter().any(|p|p.id.name=="label"&&matches!(&p.ty,FactStatus::Established(TypeRef{ty:KotlinType::Named{name,..},..}) if name=="String")));
    assert!(!provider.symbols().iter().any(|s| s.id.name == "plain"));
}

#[test]
fn constructor_ids_ignore_default_expressions_and_distinguish_overloads() {
    let a = SyntaxSemanticProvider::new(vec![(
        PathBuf::from("C.kt"),
        "class C(val id: Int = compute(1)) { constructor(name: String) : this(0) }\n".into(),
    )]);
    let b = SyntaxSemanticProvider::new(vec![(
        PathBuf::from("C.kt"),
        "class C(val id:Int=compute(99)) { constructor(name:String) : this(99) }\n".into(),
    )]);
    let mut aa: Vec<_> = a
        .symbols()
        .iter()
        .filter(|s| s.id.kind == "constructor")
        .map(|s| s.id.clone())
        .collect();
    aa.sort();
    let mut bb: Vec<_> = b
        .symbols()
        .iter()
        .filter(|s| s.id.kind == "constructor")
        .map(|s| s.id.clone())
        .collect();
    bb.sort();
    assert_eq!(aa, bb);
    assert_eq!(aa.len(), 2);
    assert_ne!(aa[0].parameters, aa[1].parameters);
    assert!(
        aa.iter()
            .all(|id| id.name == "<init>" && id.owner_path == vec!["C"])
    );
}

#[test]
fn nested_declarations_include_enclosing_callable_signature() {
    let source =
        "fun outer(value: Int) { class Local }\nfun outer(value: String) { class Local }\n";
    let p = SyntaxSemanticProvider::new(vec![(PathBuf::from("nested.kt"), source.into())]);
    let mut locals: Vec<_> = p
        .symbols()
        .iter()
        .filter(|s| s.id.name == "Local")
        .map(|s| s.id.clone())
        .collect();
    locals.sort();
    assert_eq!(locals.len(), 2);
    assert_ne!(locals[0], locals[1]);
    assert_ne!(locals[0].owner_path, locals[1].owner_path);
}

#[test]
fn deep_owner_identity_is_iterative_and_java_parameters_distinguish_overloads() {
    let source = "class A {\n class B {\n  class C {\n   class D {\n    class E {\n     fun target(x: Int) = x\n    }\n   }\n  }\n }\n}";
    let p = SyntaxSemanticProvider::new(vec![(PathBuf::from("deep.kt"), source.into())]);
    let target = p.symbols().iter().find(|s| s.id.name == "target").unwrap();
    assert_eq!(target.id.owner_path, vec!["A", "B", "C", "D", "E"]);

    let java = "class JavaBox { void f(int value) {} void f(String value) {} }";
    let p = SyntaxSemanticProvider::new(vec![(PathBuf::from("JavaBox.java"), java.into())]);
    let mut functions: Vec<_> = p
        .symbols()
        .iter()
        .filter(|s| s.id.name == "f")
        .map(|s| s.id.clone())
        .collect();
    functions.sort();
    assert_eq!(functions.len(), 2);
    assert_ne!(functions[0].parameters, functions[1].parameters);
    assert!(functions.iter().all(|id| !id.parameters[0].is_empty()));
}

#[test]
fn analysis_wire_types_serialize_and_generated_ids_track_origin() {
    let origin = SymbolId {
        module: "m".into(),
        package: "".into(),
        file: "a.kt".into(),
        owner_path: vec![],
        kind: "function".into(),
        name: "work".into(),
        receiver: None,
        parameters: vec![],
    };
    let generated = SymbolId::generated(&origin, "bridge");
    let map = OriginMap {
        generated: generated.clone(),
        origin: origin.clone(),
        reason: "bridge".into(),
    };
    let req = AnalysisRequest {
        version: 1,
        config: AnalysisConfig {
            version: 1,
            module: "m".into(),
            classpath: vec![],
            compiler_args: vec![],
            language_targets: vec![],
        },
        files: BTreeMap::from([(PathBuf::from("a.kt"), "fun work() = Unit".into())]),
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(serde_json::from_str::<AnalysisRequest>(&json).is_ok());
    assert_eq!(map.origin, origin);
    assert!(generated.kind.starts_with("generated:"));
}

#[test]
fn unresolved_required_fact_retains_declaration_and_bridge_has_origin_map() {
    let source = "fun caller() = mystery()\n";
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan =
        notlin::translation_plan::analyze(source, tree.root_node(), std::path::Path::new("a.kt"));
    assert!(!plan.require_symbol(0, "mystery", FactStatus::Unknown));
    let span = (
        plan.declarations[0].id.start_byte,
        plan.declarations[0].id.end_byte,
    );
    let coverage = notlin::diagnostics::FileCoverage {
        translated_spans: vec![span],
        ..Default::default()
    };
    plan.reconcile(source, &coverage).unwrap();
    assert_eq!(
        plan.declarations[0].final_owner,
        Some(notlin::translation_plan::BackendOwner::Kotlin)
    );
    let origin = plan.declarations[0].symbol_id.clone();
    let generated = plan.record_generated_bridge(&origin, "bridge", "callable adapter");
    assert_eq!(plan.provenance[0].generated, generated);
    assert_eq!(plan.provenance[0].origin, origin);
}

#[test]
fn required_symbol_provider_keeps_unique_dependency_and_retains_ambiguous_one() {
    let source = "package p\nfun target() = Unit\nfun helper() = Unit\n";
    let provider = SyntaxSemanticProvider::new(vec![
        (PathBuf::from("a.kt"), source.into()),
        (
            PathBuf::from("b.kt"),
            "package q\nfun helper() = Unit\n".into(),
        ),
    ]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan =
        notlin::translation_plan::analyze(source, tree.root_node(), std::path::Path::new("a.kt"));
    let outcomes = plan.require_symbols(
        0,
        &provider,
        vec!["target".to_string(), "helper".to_string()],
    );
    assert_eq!(outcomes, vec![true, false]);
    assert_eq!(plan.dependencies.len(), 2);
    assert!(
        plan.declarations[0]
            .retention_reasons
            .iter()
            .any(|r| matches!(
                r,
                notlin::translation_plan::RetentionReason::AmbiguousRequiredFact { .. }
            ))
    );
}

#[test]
fn required_calls_resolve_same_package_constructors_and_local_functions_without_global_false_ambiguity()
 {
    let source = "package p\nclass Widget\nfun outer() { fun helper() = 1; helper(); Widget() }\n";
    let provider = SyntaxSemanticProvider::new(vec![(PathBuf::from("calls.kt"), source.into())]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan =
        notlin::translation_plan::analyze(source, tree.root_node(), Path::new("calls.kt"));
    plan_required_calls(
        source,
        &tree,
        Path::new("calls.kt"),
        &provider,
        false,
        &mut plan,
    );
    let outer = plan
        .declarations
        .iter()
        .find(|d| d.symbol_id.name == "outer")
        .unwrap();
    assert!(
        outer.retention_reasons.is_empty(),
        "{:?}",
        outer.retention_reasons
    );
    assert!(
        plan.dependencies
            .iter()
            .any(|d| d.spelling == "Widget" && matches!(d.resolution, FactStatus::Established(_)))
    );
    assert!(
        plan.dependencies
            .iter()
            .any(|d| d.spelling == "helper" && matches!(d.resolution, FactStatus::Established(_)))
    );
}

#[test]
fn required_calls_retain_true_same_package_overload_ambiguity_but_skip_annotation_arguments() {
    let source = "package p\nfun work(value: Int) = value\nfun work(value: String) = value\n@Mark(value = unknownCall()) fun caller() = work(1)\n";
    let provider = SyntaxSemanticProvider::new(vec![(PathBuf::from("calls.kt"), source.into())]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan =
        notlin::translation_plan::analyze(source, tree.root_node(), Path::new("calls.kt"));
    plan_required_calls(
        source,
        &tree,
        Path::new("calls.kt"),
        &provider,
        false,
        &mut plan,
    );
    let caller = plan
        .declarations
        .iter()
        .find(|d| d.symbol_id.name == "caller")
        .unwrap();
    assert!(caller.retention_reasons.iter().any(|r|matches!(r,notlin::translation_plan::RetentionReason::AmbiguousRequiredFact{name,..} if name=="work")));
    assert!(
        !plan
            .dependencies
            .iter()
            .any(|d| d.spelling == "unknownCall")
    );
}

#[test]
fn retained_same_file_and_cross_file_top_level_property_reads_retain_owner() {
    let same = "package p\nprivate val evaluationOrder = 0\nfun first() = evaluationOrder\nfun shadow(evaluationOrder: Int) = evaluationOrder\n";
    let provider = SyntaxSemanticProvider::new(vec![(PathBuf::from("same.kt"), same.into())]);
    let property = provider
        .symbols()
        .iter()
        .find(|s| s.id.kind == "property" && s.id.name == "evaluationOrder")
        .unwrap()
        .id
        .clone();
    let retained = HashSet::from([property]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(same, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(same, tree.root_node(), Path::new("same.kt"));
    plan_required_property_references(
        same,
        &tree,
        Path::new("same.kt"),
        &provider,
        &retained,
        &mut plan,
    );
    let first = plan
        .declarations
        .iter()
        .find(|d| d.symbol_id.name == "first")
        .unwrap();
    assert!(
        first
            .retention_reasons
            .iter()
            .any(|r| r.message().contains("evaluationOrder"))
    );
    let shadow = plan
        .declarations
        .iter()
        .find(|d| d.symbol_id.name == "shadow")
        .unwrap();
    assert!(
        shadow.retention_reasons.is_empty(),
        "parameter shadow was not recognized: {:?}",
        shadow.retention_reasons
    );

    let other = "package p\nval sharedSetting = true\n";
    let caller = "package p\nfun readSetting() = sharedSetting\n";
    let provider = SyntaxSemanticProvider::new(vec![
        (PathBuf::from("other.kt"), other.into()),
        (PathBuf::from("caller.kt"), caller.into()),
    ]);
    let tree = parser.parse(caller, None).unwrap();
    let mut plan =
        notlin::translation_plan::analyze(caller, tree.root_node(), Path::new("caller.kt"));
    plan_required_property_references(
        caller,
        &tree,
        Path::new("caller.kt"),
        &provider,
        &HashSet::new(),
        &mut plan,
    );
    let caller_plan = plan
        .declarations
        .iter()
        .find(|d| d.symbol_id.name == "readSetting")
        .unwrap();
    assert!(
        caller_plan
            .retention_reasons
            .iter()
            .any(|r| r.message().contains("cross-file"))
    );
}

#[test]
fn retained_property_navigation_receivers_and_outer_names_survive_nested_lambda() {
    let src = "package p\nprivate val evaluationOrder = 0\nfun nav() = evaluationOrder.toString()\nfun nested(items: List<Int>) { items.map { evaluationOrder -> evaluationOrder }; println(evaluationOrder) }\n";
    let provider = SyntaxSemanticProvider::new(vec![(PathBuf::from("a.kt"), src.into())]);
    let retained = HashSet::from([provider
        .symbols()
        .iter()
        .find(|s| s.id.kind == "property" && s.id.name == "evaluationOrder")
        .unwrap()
        .id
        .clone()]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(src, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(src, tree.root_node(), Path::new("a.kt"));
    plan_required_property_references(
        src,
        &tree,
        Path::new("a.kt"),
        &provider,
        &retained,
        &mut plan,
    );
    for name in ["nav", "nested"] {
        let declaration = plan
            .declarations
            .iter()
            .find(|d| d.symbol_id.name == name)
            .unwrap();
        assert!(
            declaration
                .retention_reasons
                .iter()
                .any(|r| r.message().contains("evaluationOrder")),
            "{name} was not retained: {:?}",
            declaration.retention_reasons
        );
    }
}

#[test]
fn wildcard_import_property_references_are_conservatively_retained() {
    let target = "package library\nval shared = 1\n";
    let caller = "package consumer\nimport library.*\nfun read() = shared\n";
    let provider = SyntaxSemanticProvider::new(vec![
        (PathBuf::from("Target.kt"), target.into()),
        (PathBuf::from("Caller.kt"), caller.into()),
    ]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(caller, None).unwrap();
    let mut plan =
        notlin::translation_plan::analyze(caller, tree.root_node(), Path::new("Caller.kt"));
    plan_required_property_references(
        caller,
        &tree,
        Path::new("Caller.kt"),
        &provider,
        &HashSet::new(),
        &mut plan,
    );
    let caller_plan = plan
        .declarations
        .iter()
        .find(|d| d.symbol_id.name == "read")
        .unwrap();
    assert!(
        caller_plan
            .retention_reasons
            .iter()
            .any(|r| r.message().contains("cross-file"))
    );
}

#[test]
fn provider_indexes_structured_generic_alias_targets() {
    let src = "package p\ntypealias Boxed<T> = List<T?>\n";
    let provider = SyntaxSemanticProvider::new(vec![(PathBuf::from("Aliases.kt"), src.into())]);
    let aliases = provider.aliases_in_package("p");
    assert_eq!(aliases["Boxed"].parameters, vec!["T"]);
    assert_eq!(
        TypeRef::parse("Boxed<String>").expand_alias_definitions(&aliases),
        TypeRef::parse("List<String?>")
    );
}

#[test]
fn lossy_type_alias_blocks_declaration_using_alias() {
    let alias_source = "package p\ntypealias Unsigned = UInt\n";
    let caller = "package p\nfun convert(value: Unsigned) = value\n";
    let provider = SyntaxSemanticProvider::new(vec![
        (PathBuf::from("Alias.kt"), alias_source.into()),
        (PathBuf::from("Use.kt"), caller.into()),
    ]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(caller, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(caller, tree.root_node(), Path::new("Use.kt"));
    plan_alias_type_losses(caller, &tree, &provider, &mut plan, false);
    let decision = plan
        .declarations
        .iter()
        .find(|d| d.symbol_id.name == "convert")
        .unwrap();
    assert!(
        decision
            .retention_reasons
            .iter()
            .any(|r| r.message().contains("type alias `Unsigned`"))
    );
}

#[test]
fn wildcard_builtin_collision_and_unindexed_wildcard_stay_conservative() {
    for (files, caller) in [
        (
            vec![
                (
                    PathBuf::from("Library.kt"),
                    "package library\nfun print() = Unit\n".to_string(),
                ),
                (
                    PathBuf::from("Caller.kt"),
                    "package consumer\nimport library.*\nfun invoke() = print()\n".to_string(),
                ),
            ],
            "package consumer\nimport library.*\nfun invoke() = print()\n",
        ),
        (
            vec![(
                PathBuf::from("Caller.kt"),
                "package consumer\nimport external.*\nfun invoke() = print()\n".to_string(),
            )],
            "package consumer\nimport external.*\nfun invoke() = print()\n",
        ),
    ] {
        let provider = SyntaxSemanticProvider::new(files);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(caller, None).unwrap();
        let mut plan =
            notlin::translation_plan::analyze(caller, tree.root_node(), Path::new("Caller.kt"));
        plan_required_calls(
            caller,
            &tree,
            Path::new("Caller.kt"),
            &provider,
            false,
            &mut plan,
        );
        let decision = plan
            .declarations
            .iter()
            .find(|d| d.symbol_id.name == "invoke")
            .unwrap();
        assert_eq!(
            decision.candidate_owner,
            notlin::translation_plan::BackendOwner::Kotlin
        );
        assert!(
            decision
                .retention_reasons
                .iter()
                .any(|r| r.message().contains("print"))
        );
    }
}

#[test]
fn user_builtin_shadow_and_local_function_value_are_not_mislowered() {
    let source = "package p\nfun print() = Unit\nfun top() = Unit\nfun caller() { val top = { Unit }; top(); print() }\n";
    let provider = SyntaxSemanticProvider::new(vec![(PathBuf::from("a.kt"), source.into())]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), Path::new("a.kt"));
    plan_required_calls(
        source,
        &tree,
        Path::new("a.kt"),
        &provider,
        false,
        &mut plan,
    );
    let caller = plan
        .declarations
        .iter()
        .find(|d| d.symbol_id.name == "caller")
        .unwrap();
    assert_eq!(
        caller.candidate_owner,
        notlin::translation_plan::BackendOwner::Kotlin
    );
    assert!(
        caller
            .retention_reasons
            .iter()
            .any(|r| r.message().contains("top"))
    );
    assert!(
        caller
            .retention_reasons
            .iter()
            .any(|r| r.message().contains("shadows a builtin"))
    );
}

#[test]
fn member_property_call_cannot_bind_to_same_named_top_level_function() {
    let source = "package p\nfun invoke() = Unit\nclass Example { val invoke: () -> Unit = {}; fun run() = invoke() }\n";
    let provider = SyntaxSemanticProvider::new(vec![(PathBuf::from("a.kt"), source.into())]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), Path::new("a.kt"));
    plan_required_calls(
        source,
        &tree,
        Path::new("a.kt"),
        &provider,
        false,
        &mut plan,
    );
    let run = plan
        .declarations
        .iter()
        .find(|d| d.symbol_id.name == "Example")
        .unwrap();
    assert_eq!(
        run.candidate_owner,
        notlin::translation_plan::BackendOwner::Kotlin
    );
    assert!(
        run.retention_reasons
            .iter()
            .any(|r| r.message().contains("invoke"))
    );
}

#[test]
fn indexed_name_lookup_matches_full_symbol_filter_across_scopes_and_overloads() {
    let provider = SyntaxSemanticProvider::new(vec![
        (
            PathBuf::from("One.kt"),
            "package first\nfun choose(value: Int) = value\nfun choose(value: String) = value\nclass Outer { fun choose() = Unit }\n".into(),
        ),
        (
            PathBuf::from("Two.kt"),
            "package second\nfun choose(value: Long) = value\n".into(),
        ),
    ]);
    let indexed = provider
        .symbols_named("choose")
        .into_iter()
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    let scanned = provider
        .symbols()
        .iter()
        .filter(|symbol| symbol.id.name == "choose")
        .map(|symbol| symbol.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(indexed, scanned);
    assert_eq!(indexed.len(), 4);
    assert!(provider.symbols_named("absent").is_empty());
}

#[test]
fn duplicate_normalized_declarations_remain_distinct_and_are_retained_as_ambiguous() {
    let source = "package p\nfun same(value: Int = 1) = value\nfun same(value:Int=2) = value + 1\n";
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let plan =
        notlin::translation_plan::analyze(source, tree.root_node(), std::path::Path::new("a.kt"));
    assert_eq!(plan.declarations.len(), 2);
    assert_eq!(
        plan.declarations[0].symbol_id,
        plan.declarations[1].symbol_id
    );
    assert!(plan.declarations.iter().all(|d| d.candidate_owner
        == notlin::translation_plan::BackendOwner::Kotlin
        && d.retention_reasons.iter().any(|r| matches!(
            r,
            notlin::translation_plan::RetentionReason::AmbiguousRequiredFact { .. }
        ))));
}

#[test]
fn imported_property_alias_resolves_to_its_original_symbol() {
    let caller =
        "package consumer\nimport library.count as localCount\nfun value(): Int = localCount\n";
    let provider = SyntaxSemanticProvider::new(vec![
        (
            PathBuf::from("Library.kt"),
            "package library\nval count: Int = 1\n".into(),
        ),
        (PathBuf::from("Caller.kt"), caller.into()),
    ]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(caller, None).unwrap();
    let mut plan =
        notlin::translation_plan::analyze(caller, tree.root_node(), Path::new("Caller.kt"));
    notlin::semantics::plan_required_property_references(
        caller,
        &tree,
        Path::new("Caller.kt"),
        &provider,
        &std::collections::HashSet::new(),
        &mut plan,
    );
    assert_eq!(
        plan.declarations[0].candidate_owner,
        notlin::translation_plan::BackendOwner::Kotlin
    );
    assert!(plan.dependencies.iter().any(|dependency| {
        matches!(&dependency.resolution, FactStatus::Established(symbol)
            if symbol.name == "count" && symbol.package == "library")
    }));
}

#[test]
fn required_calls_accept_proven_uuid_and_string_split_chain_members() {
    let source = r#"package sem
import java.util.*
class ExternalId(val objectId: UUID) {
    fun lookupId(): String = "prefix/id"
    fun externalId(): String = lookupId().split("/").last()
    fun externalIdWithDelimiter(delimiter: String): String = lookupId().split(delimiter).last()
    fun uuidText(): String = objectId.toString()
}
"#;
    let file = Path::new("ExternalId.kt");
    let provider = SyntaxSemanticProvider::new(vec![(file.to_path_buf(), source.into())]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), file);
    plan_required_calls(source, &tree, file, &provider, false, &mut plan);
    assert!(
        !plan
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "S001"),
        "known standard-library chain should not be retained: {:?}",
        plan.diagnostics
    );
}

#[test]
fn external_capabilities_match_exact_split_and_mutable_collection_forms() {
    let cases = [
        (
            "typed-delimiter",
            r#"package sem
class Splitter {
  fun lookupId(): String = "prefix/id"
  fun externalId(delimiter: String): String = lookupId().split(delimiter).last()
}
"#,
            false,
        ),
        (
            "same-file-string-const",
            r#"package sem
private const val SEPARATOR: String = "/"
class Splitter {
  fun lookupId(): String = "prefix/id"
  fun externalId(): String = lookupId().split(SEPARATOR).last()
}
"#,
            false,
        ),
        (
            "same-name-non-string-shadow",
            r#"package sem
private const val SEPARATOR: String = "/"
class Splitter {
  fun lookupId(): String = "prefix/id"
  fun externalId(SEPARATOR: Int): String = lookupId().split(SEPARATOR).last()
}
"#,
            true,
        ),
        (
            "readonly-list-add",
            r#"package sem
class Mutator(val values: List<String>) { fun addOne(value: String) { values.add(value) } }
"#,
            true,
        ),
        (
            "readonly-set-add",
            r#"package sem
class Mutator(val values: Set<String>) { fun addOne(value: String) { values.add(value) } }
"#,
            true,
        ),
        (
            "mutable-list-add",
            r#"package sem
class Mutator(val values: MutableList<String>) { fun addOne(value: String) { values.add(value) } }
"#,
            false,
        ),
        (
            "mutable-set-add",
            r#"package sem
class Mutator(val values: MutableSet<String>) { fun addOne(value: String) { values.add(value) } }
"#,
            false,
        ),
        (
            "split-limit-overload",
            r#"package sem
class Splitter {
  fun lookupId(): String = "prefix/id"
  fun externalId(): String = lookupId().split("/", 2).last()
}
"#,
            true,
        ),
        (
            "split-char-overload",
            r#"package sem
class Splitter {
  fun lookupId(): String = "prefix/id"
  fun externalId(): String = lookupId().split('/').last()
}
"#,
            true,
        ),
        (
            "aliased-mutable-list",
            r#"package sem
typealias WritableNames = MutableList<String>
class Mutator(val values: WritableNames) { fun addOne(value: String) { values.add(value) } }
"#,
            true,
        ),
    ];
    for (name, source, expect_retained) in cases {
        let file = Path::new("ExternalCapability.kt");
        let provider = SyntaxSemanticProvider::new(vec![(file.to_path_buf(), source.into())]);
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), file);
        plan_required_calls(source, &tree, file, &provider, false, &mut plan);
        let retained = plan
            .diagnostics
            .iter()
            .any(|diagnostic| matches!(diagnostic.code.as_str(), "S001" | "S005"));
        assert_eq!(
            retained, expect_retained,
            "unexpected external capability result for {name}: {:?}",
            plan.diagnostics
        );
    }
}

#[test]
fn standard_member_capability_does_not_override_a_workspace_type_collision() {
    let source = r#"package sem
class String
class StringCaller(val value: String) {
    fun lower(): String = value.lowercase()
}
"#;
    let file = Path::new("ShadowString.kt");
    let provider = SyntaxSemanticProvider::new(vec![(file.to_path_buf(), source.into())]);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(source, None).unwrap();
    let mut plan = notlin::translation_plan::analyze(source, tree.root_node(), file);
    plan_required_calls(source, &tree, file, &provider, false, &mut plan);
    assert!(plan.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "S001" && diagnostic.message.contains("lowercase")
    }));
}
