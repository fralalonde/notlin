use crate::semantics::{SourceLocation, SymbolId};
use crate::translation_plan::{
    BackendOwner, PlannedDefaultGetterBridge, PreparationBlockerKind, RetentionReason,
    TranslationPlan,
};
use crate::workspace::SourceIndex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::Node;

#[derive(Clone)]
struct ClassNode {
    symbol: SymbolId,
    name: String,
    start: usize,
    end: usize,
    boundary: SymbolId,
    has_supertypes: bool,
}

pub(super) fn plan_default_getters(
    index: &SourceIndex,
    file: &Path,
    retained: &HashSet<SymbolId>,
    roots: &[std::path::PathBuf],
    plan: &mut TranslationPlan,
) {
    let Some(source_file) = index.source_file(file) else {
        return;
    };
    let source = source_file.source_text();
    let source_hash = *blake3::hash(source.as_bytes()).as_bytes();
    // The plan and index are both snapshot-scoped. Never use indexed AST spans
    // to emit a bridge or retain a boundary after either snapshot has changed.
    if source_hash != plan.source_hash {
        let owners = plan
            .declarations
            .iter()
            .filter(|decision| {
                matches!(
                    decision.symbol_id.kind.as_str(),
                    "class" | "interface" | "object" | "enum"
                )
            })
            .map(|decision| decision.symbol_id.clone())
            .collect::<Vec<_>>();
        for owner in owners {
            retain_boundary(
                plan,
                &owner,
                "default getter analysis has a stale source snapshot",
            );
        }
        return;
    }

    let tree = crate::transpiler::parse_tree(source);
    let classes = collect_class_nodes(tree.root_node(), source, &source_file.path, plan);
    if classes.is_empty() {
        return;
    }

    // A class in a selected Kotlin file is movable only when its containing
    // top-level plan boundary is movable. Keep this map independent of the
    // mutable decisions below so processing one bridge cannot affect another.
    let boundary_decisions = plan
        .declarations
        .iter()
        .map(|decision| (decision.symbol_id.clone(), decision.candidate_owner))
        .collect::<HashMap<_, _>>();
    let class_boundaries = classes
        .iter()
        .map(|class| (class.symbol.clone(), class.boundary.clone()))
        .collect::<HashMap<_, _>>();

    let java_default_owner = |id: &SymbolId| {
        if id
            .file
            .extension()
            .is_some_and(|extension| extension == "java")
        {
            return true;
        }
        if !index.is_selected(&id.file, roots) || index.symbol_retained(id, retained) {
            return false;
        }
        if id.file != source_file.path {
            // Other files have their own preparation decisions. The fixpoint's
            // exact top-level symbol read is their prospective ownership fact.
            return id.owner_path.is_empty();
        }
        // Top-level providers have their own plan decision. Nested providers
        // belong to their containing boundary; only a proven Java boundary in
        // this exact source plan is enough to rely on them.
        let boundary = plan
            .declarations
            .iter()
            .find(|decision| decision.symbol_id == *id)
            .map(|decision| decision.symbol_id.clone())
            .or_else(|| class_boundaries.get(id).cloned());
        let Some(boundary) = boundary else {
            return false;
        };
        !super::retention_queries::contains(retained, &boundary)
            && boundary_decisions.get(&boundary) == Some(&BackendOwner::Java)
    };

    let mut results = Vec::new();
    let mut affected_ambiguous_boundaries = HashSet::<SymbolId>::new();
    for class in &classes {
        if boundary_decisions.get(&class.boundary) != Some(&BackendOwner::Java) {
            continue;
        }
        let declarations = source_file
            .declarations
            .iter()
            .filter(|declaration| declaration.name == class.name)
            .collect::<Vec<_>>();
        let matching = declarations
            .iter()
            .filter(|declaration| {
                same_source_identity(
                    &crate::semantics::workspace_symbol(index, declaration),
                    &class.symbol,
                )
            })
            .copied()
            .collect::<Vec<_>>();

        if matching.len() != 1 {
            // The workspace index intentionally collapses same-name AST
            // declarations to an ambiguous identity. If any such class has a
            // parent list, preserve its whole ownership boundary: a bridge
            // cannot be attributed safely to one nested class.
            if class.has_supertypes {
                affected_ambiguous_boundaries.insert(class.boundary.clone());
            }
            continue;
        }

        let indexed = matching[0];
        let owner = crate::semantics::workspace_symbol(index, indexed);
        let result = index
            .covariant_default_getter_bridges(file, &indexed.name)
            .and_then(|bridges| {
                // The direct super qualifier can remain Kotlin when it
                // inherits this exact Java default. Workspace bridge analysis
                // has already rejected overrides, reabstraction and ambiguity.
                let mut blocked = bridges
                    .iter()
                    .filter(|bridge| !java_default_owner(&bridge.target))
                    .map(|bridge| {
                        let target = &bridge.target;
                        let mut path = target.owner_path.clone();
                        path.push(target.name.clone());
                        let owner = if target.package.is_empty() {
                            path.join(".")
                        } else {
                            format!("{}.{}", target.package, path.join("."))
                        };
                        format!("`{owner}#{}`", bridge.method_name)
                    })
                    .collect::<Vec<_>>();
                blocked.sort();
                blocked.dedup();
                if blocked.is_empty() {
                    Ok(bridges)
                } else {
                    Err(format!(
                        "inherited covariant getter needs a concrete Java bridge, but its default provider remains Kotlin: {}",
                        blocked.join(", ")
                    ))
                }
            });
        results.push((
            owner,
            class.boundary.clone(),
            class.start,
            class.end,
            result,
        ));
    }

    let ambiguous_boundaries = affected_ambiguous_boundaries;
    let mut failed_boundaries = ambiguous_boundaries.clone();
    let mut error_boundaries = HashSet::new();
    let mut successful = Vec::new();
    for (owner, boundary, start, end, result) in results {
        match result {
            Ok(bridges) => successful.push((owner, boundary, start, end, bridges)),
            Err(message) => {
                failed_boundaries.insert(boundary.clone());
                error_boundaries.insert(boundary.clone());
                retain_boundary(plan, &boundary, &message);
            }
        }
    }
    for boundary in ambiguous_boundaries.difference(&error_boundaries) {
        retain_boundary(
            plan,
            boundary,
            "inherited default getter belongs to an ambiguous same-name class identity; retaining its containing declaration boundary",
        );
    }
    for (owner, boundary, start, end, bridges) in successful {
        if failed_boundaries.contains(&boundary) {
            continue;
        }
        for bridge in bridges {
            let location = SourceLocation {
                file: file.to_path_buf(),
                snapshot_hash: plan.source_hash,
                start_byte: start,
                end_byte: end,
            };
            plan.record_generated_bridge(
                &owner,
                format!("covariant-default-getter:{}", bridge.method_name),
                "preserve the concrete covariant getter seen by Java annotation processors",
            );
            plan.default_getter_bridges
                .push(PlannedDefaultGetterBridge {
                    owner: bridge.owner,
                    location,
                    target: bridge.target,
                    provider: bridge.provider_owner,
                    method_name: bridge.method_name,
                    return_type: bridge.return_type,
                    return_type_ref: bridge.return_type_ref,
                });
        }
    }
}

fn retain_boundary(plan: &mut TranslationPlan, boundary: &SymbolId, message: &str) {
    let Some(decision) = plan
        .declarations
        .iter_mut()
        .find(|decision| decision.symbol_id == *boundary)
    else {
        return;
    };
    decision.candidate_owner = BackendOwner::Kotlin;
    decision
        .retention_reasons
        .push(RetentionReason::PreparationBlocker {
            kind: PreparationBlockerKind::CompatibilityRule,
            message: message.to_owned(),
        });
}

fn collect_class_nodes(
    root: Node<'_>,
    source: &str,
    file: &Path,
    plan: &TranslationPlan,
) -> Vec<ClassNode> {
    let mut out = Vec::new();
    let mut stack = vec![(root, None::<SymbolId>)];
    while let Some((node, inherited_boundary)) = stack.pop() {
        let mut boundary = inherited_boundary;
        if is_ownership_boundary(node) {
            boundary = plan
                .declarations
                .iter()
                .filter(|decision| {
                    decision.id.start_byte <= node.start_byte()
                        && decision.id.end_byte >= node.end_byte()
                })
                .min_by_key(|decision| decision.id.end_byte - decision.id.start_byte)
                .map(|decision| decision.symbol_id.clone());
        }
        if is_class_like(node)
            && let Some(boundary_symbol) = boundary.clone()
        {
            let symbol = crate::semantics::symbol_id_for_node(source, node, file);
            let name = symbol.name.clone();
            out.push(ClassNode {
                symbol,
                name,
                start: node.start_byte(),
                end: node.end_byte(),
                boundary: boundary_symbol,
                has_supertypes: has_supertype_list(node, source),
            });
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push((child, boundary.clone()));
        }
    }
    out
}

fn is_ownership_boundary(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_declaration"
            | "object_declaration"
            | "function_declaration"
            | "property_declaration"
    )
}

fn is_class_like(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "class_declaration" | "interface_declaration" | "object_declaration"
    )
}

fn has_supertype_list(node: Node<'_>, source: &str) -> bool {
    (0..node.child_count()).any(|index| {
        let child = node.child(index).expect("child index is in range");
        child.kind() == "delegation_specifier"
            || child.kind() == "delegation_specifiers"
            || (child.kind() == "inheritance_specifier"
                && !child
                    .utf8_text(source.as_bytes())
                    .unwrap_or("")
                    .trim()
                    .is_empty())
    })
}

fn same_source_identity(left: &SymbolId, right: &SymbolId) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translation_plan;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(source: &str) -> (Fixture, PathBuf, SourceIndex, TranslationPlan) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-temp")
            .join(format!(
                "nested-default-getter-plan-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("Model.kt");
        fs::write(&file, source).unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        let tree = crate::transpiler::parse_tree(source);
        let plan = translation_plan::analyze(source, tree.root_node(), &file);
        (Fixture(root.clone()), file, index, plan)
    }

    fn decision<'a>(
        plan: &'a TranslationPlan,
        name: &str,
    ) -> &'a crate::translation_plan::DeclarationDecision {
        plan.declarations
            .iter()
            .find(|decision| decision.symbol_id.name == name)
            .unwrap()
    }

    #[test]
    fn retained_qualifier_can_inherit_but_not_define_the_default() {
        for overrides_default in [false, true] {
            let middle_getter = if overrides_default {
                "override fun getCode(): Code = Code()"
            } else {
                ""
            };
            let source = format!(
                "package sample\nclass Code {{}}\ninterface Wide {{\n    fun getCode(): Any\n}}\n\
                 interface Narrow : Wide {{\n    override fun getCode(): Code = Code()\n}}\n\
                 interface Middle : Narrow {{\n    suspend fun retainedOnly() {{}}\n    {middle_getter}\n}}\n\
                 class Child : Middle, Wide {{}}\n"
            );
            let (_fixture, file, index, mut plan) = fixture(&source);
            let roots = vec![file.parent().unwrap().to_path_buf()];
            let retained = HashSet::from([decision(&plan, "Middle").symbol_id.clone()]);
            plan_default_getters(&index, &file, &retained, &roots, &mut plan);

            if overrides_default {
                assert_eq!(
                    decision(&plan, "Child").candidate_owner,
                    BackendOwner::Kotlin
                );
                assert!(plan.default_getter_bridges.is_empty());
                assert!(
                    decision(&plan, "Child")
                        .retention_reasons
                        .iter()
                        .any(|reason| {
                            matches!(reason, RetentionReason::PreparationBlocker { message, .. }
                            if message.contains("sample.Middle") && message.contains("getCode"))
                        }),
                    "retention must identify the exact default provider and getter"
                );
            } else {
                assert_eq!(decision(&plan, "Child").candidate_owner, BackendOwner::Java);
                let [bridge] = plan.default_getter_bridges.as_slice() else {
                    panic!("expected one bridge: {:?}", plan.default_getter_bridges);
                };
                assert_eq!(bridge.provider.name, "Middle");
                assert_eq!(bridge.target.name, "Narrow");
            }
        }
    }

    #[test]
    fn nested_default_getter_bridge_keeps_nested_owner_and_source_span() {
        let source = r#"
            package sample
            class Code {}
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): Code = Code()
            }
            class Outer {
                class Child : Narrow, Wide {}
            }
        "#;
        let (_fixture, file, index, mut plan) = fixture(source);
        let roots = vec![file.parent().unwrap().to_path_buf()];
        plan_default_getters(&index, &file, &HashSet::new(), &roots, &mut plan);

        assert_eq!(decision(&plan, "Outer").candidate_owner, BackendOwner::Java);
        assert_eq!(plan.default_getter_bridges.len(), 1);
        let bridge = &plan.default_getter_bridges[0];
        assert_eq!(bridge.owner.name, "Child");
        assert_eq!(bridge.owner.owner_path, vec!["Outer"]);
        assert_eq!(
            &source[bridge.location.start_byte..bridge.location.end_byte],
            "class Child : Narrow, Wide {}"
        );
    }

    #[test]
    fn ambiguous_nested_and_top_level_classes_retain_only_their_boundaries() {
        let source = r#"
            package sample
            class Code {}
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): Code = Code()
            }
            class Outer {
                class Duplicate : Narrow, Wide {}
            }
            class Duplicate : Narrow, Wide {}
            class Independent {}
        "#;
        let (_fixture, file, index, mut plan) = fixture(source);
        let roots = vec![file.parent().unwrap().to_path_buf()];
        plan_default_getters(&index, &file, &HashSet::new(), &roots, &mut plan);

        assert_eq!(
            decision(&plan, "Outer").candidate_owner,
            BackendOwner::Kotlin
        );
        assert_eq!(
            decision(&plan, "Duplicate").candidate_owner,
            BackendOwner::Kotlin
        );
        assert_eq!(
            decision(&plan, "Independent").candidate_owner,
            BackendOwner::Java
        );
        assert!(plan.default_getter_bridges.is_empty());
    }
}
