//! Keep retained Kotlin property readers working after their models become Java.
//! Kotlin reflection exposes Java backing fields, without connecting them to
//! public bean getters. Repair only bound KProperty1.get calls, on demand.

use crate::property_abi::PlannedAccessorRepairs;
use crate::semantics::{self, KotlinType};
use crate::smart_cast::Edit;
use crate::transpiler::{self, kt};
use crate::workspace::{DeclarationKind, MemberKind, SourceIndex};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tree_sitter::Node;

const PROPERTY: &str = "kotlin.reflect.KProperty1";
const HELPER: &str = r#"
fun <T, R> __NOTLIN_READ_PROPERTY__(property: kotlin.reflect.KProperty1<T, R>, receiver: T): R {
    if (receiver == null) return property.getter.call(receiver)
    val owner = (receiver as kotlin.Any).javaClass
    if (owner.getDeclaredAnnotation(kotlin.Metadata::class.java) != null) {
        return property.getter.call(receiver)
    }
    val name = property.name
    val capitalized = name.replaceFirstChar { it.uppercaseChar() }
    val names = if (name.startsWith("is") && name.length > 2 && name[2].isUpperCase()) {
        kotlin.collections.listOf(name, "get" + capitalized)
    } else {
        kotlin.collections.listOf("get" + capitalized, "is" + capitalized)
    }
    for (getterName in names) {
        val getter = try { owner.getMethod(getterName) } catch (_: java.lang.NoSuchMethodException) { continue }
        if (java.lang.reflect.Modifier.isStatic(getter.modifiers)) continue
        if (getterName.startsWith("is") && getterName != name &&
            getter.returnType != java.lang.Boolean.TYPE && getter.returnType != java.lang.Boolean::class.java) continue
        @kotlin.Suppress("UNCHECKED_CAST")
        return getter.invoke(receiver) as R
    }
    return property.getter.call(receiver)
}
"#;

fn canonical_property(
    index: &SourceIndex,
    path: &std::path::Path,
    source: &str,
    root: Node,
    call: Node,
    name: &str,
) -> bool {
    if !crate::receiver_binding::file_scoped_type_reference(call, name, source) {
        return false;
    }
    if name == PROPERTY {
        return true;
    }
    let Some(file) = index.source_file(path) else {
        return false;
    };
    if file.declarations.iter().any(|decl| decl.name == name)
        || index
            .type_aliases_for(path)
            .iter()
            .any(|(alias, _)| alias == name)
    {
        return false;
    }
    root.named_children(&mut root.walk()).any(|node| {
        if node.kind() != "import" {
            return false;
        }
        let Some(import) = semantics::java_member_calls::import_header(source, node) else {
            return false;
        };
        import.path == PROPERTY
            && !import.wildcard
            && import.alias.as_deref().unwrap_or("KProperty1") == name
    })
}

fn function_block(mut node: Node) -> Option<Node> {
    while let Some(parent) = node.parent() {
        if parent.kind() == "function_declaration" {
            return kt::child(parent, "function_body").and_then(|body| kt::child(body, "block"));
        }
        node = parent;
    }
    None
}

pub(crate) fn repair_virtual_sources(
    index: &SourceIndex,
    sources: &mut [(PathBuf, String)],
) -> PlannedAccessorRepairs {
    let mut report = PlannedAccessorRepairs::default();
    if !index.java_files().any(|file| {
        file.source_text().starts_with("// NOTLIN: generated from ")
            && file.declarations.iter().any(|decl| {
                decl.kind == DeclarationKind::Record
                    || matches!(decl.kind, DeclarationKind::Class | DeclarationKind::Enum)
                        && decl.members.iter().any(|member| {
                            matches!(member.kind, MemberKind::Field | MemberKind::Property)
                        })
            })
    }) {
        return report;
    }
    for (path, source) in sources {
        if !source.contains("KProperty1") {
            continue;
        }
        let tree = transpiler::parse_tree(source);
        let mut pending = vec![tree.root_node()];
        let mut groups = BTreeMap::<usize, (Node, Vec<Edit>)>::new();
        while let Some(call) = pending.pop() {
            pending.extend(call.named_children(&mut call.walk()));
            if call.kind() != "call_expression" || semantics::call_argument_count(call) != 1 {
                continue;
            }
            let Some(callee) = semantics::call_callee_node(call) else {
                continue;
            };
            if kt::text(callee, source) != "get" {
                continue;
            }
            let Some(receiver) = semantics::explicit_receiver_node(call, callee) else {
                continue;
            };
            if receiver.kind() != "identifier" || kt::text(call, source).contains("?.") {
                continue;
            }
            let Some(ty) =
                semantics::lexical_binding_type(source, call, kt::text(receiver, source))
            else {
                continue;
            };
            let KotlinType::Named { name, .. } = ty.ty else {
                continue;
            };
            if ty.nullable
                || !canonical_property(index, path, source, tree.root_node(), call, &name)
            {
                continue;
            }
            let Some(block) = function_block(call) else {
                continue;
            };
            let Some(arguments) = kt::child(call, "value_arguments") else {
                continue;
            };
            let Some(argument) = kt::child(arguments, "value_argument") else {
                continue;
            };
            let group = groups
                .entry(block.start_byte())
                .or_insert_with(|| (block, Vec::new()));
            group.1.push(Edit {
                start: call.start_byte(),
                end: call.end_byte(),
                text: format!(
                    "__NOTLIN_READ_PROPERTY__({}, {})",
                    kt::text(receiver, source),
                    kt::text(argument, source)
                ),
            });
        }
        let mut edits = Vec::new();
        let mut repairs = Vec::new();
        for (number, (block, calls)) in groups.values().enumerate() {
            let mut helper = format!("notlinReadKotlinProperty{number}");
            while source.contains(&helper) {
                helper.push('_');
            }
            edits.extend(calls.iter().map(|edit| Edit {
                start: edit.start,
                end: edit.end,
                text: edit.text.replace("__NOTLIN_READ_PROPERTY__", &helper),
            }));
            let line = source[..block.start_byte()]
                .rsplit('\n')
                .next()
                .unwrap_or("");
            let indent = format!(
                "{}    ",
                line.chars()
                    .take_while(|c| c.is_whitespace())
                    .collect::<String>()
            );
            let helper_body = HELPER
                .replace("__NOTLIN_READ_PROPERTY__", &helper)
                .lines()
                .map(|line| format!("{indent}{line}\n"))
                .collect::<String>();
            edits.push(Edit {
                start: block.start_byte() + 1,
                end: block.start_byte() + 1,
                text: format!("\n{helper_body}"),
            });
            let mut owner = *block;
            while let Some(parent) = owner.parent().filter(|parent| parent.parent().is_some()) {
                owner = parent;
            }
            repairs.push(crate::translation_plan::PlannedRepair {
                target:semantics::symbol_id_for_node(source,owner,path),
                kind:"kotlin-reflection-getter".into(),
                detail:format!("{} bound property reads use public Java getters; Kotlin receivers keep Kotlin reflection",calls.len())
            });
        }
        if !edits.is_empty() {
            edits.sort_by_key(|edit| (edit.start, edit.end));
            if edits.windows(2).any(|pair| pair[0].end > pair[1].start) {
                continue;
            }
            let changed = crate::smart_cast::apply_edits(source, edits);
            if !transpiler::parse_tree(&changed).root_node().has_error() {
                report.count += groups.values().map(|(_, calls)| calls.len()).sum::<usize>();
                report.repairs.extend(repairs);
                *source = changed;
            }
        }
    }
    report
}
