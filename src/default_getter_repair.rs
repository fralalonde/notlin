//! Explicit Kotlin overrides needed after default getter providers move to Java.
//!
//! Kotlin metadata can make a concrete covariant accessor visible to annotation
//! processors even when source Java method resolution would inherit it. Preserve
//! that accessor on retained Kotlin classes before accepting a mixed migration.

use crate::property_abi::PlannedAccessorRepairs;
use crate::semantics::{OriginMap, SymbolId};
use crate::translation_plan::{PlannedBridge, PlannedRepair};
use crate::workspace::{DeclarationKind, SourceIndex, SourceLanguage};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Prospective eligibility: a retained Kotlin `val` contract cannot be
/// implemented by an ordinary Kotlin `fun getX()`. In that case its default
/// provider must retain Kotlin metadata along with the descendant. The direct
/// super qualifier may stay Kotlin: bridge analysis already proves that it
/// inherits the selected default without reabstracting it. A default declared
/// as a Kotlin function already has method semantics and can be forwarded to
/// before its provider moves; a Kotlin property still needs conversion first.
pub(crate) fn can_repair_contract(
    index: &SourceIndex,
    descendant: &crate::workspace::Declaration,
    moving_provider: &crate::workspace::Declaration,
    method_name: &str,
    retained: &HashSet<SymbolId>,
    roots: &[PathBuf],
) -> bool {
    let Some(file) = index.declaration_source_file(descendant) else {
        return false;
    };
    if !index.is_selected(&file.path, roots) {
        return false;
    }
    let Ok(bridges) = index.covariant_default_getter_bridges(&file.path, &descendant.name) else {
        return false;
    };
    let moving_provider = crate::semantics::workspace_symbol(index, moving_provider);
    if !moving_provider.owner_path.is_empty() && index.symbol_retained(&moving_provider, retained) {
        return false;
    }
    bridges.iter().any(|bridge| {
        bridge.method_name == method_name
            && bridge.property_contract_owners.iter().all(|owner| {
                is_java_file(&owner.file)
                    || (index.is_selected(&owner.file, roots)
                        && (owner == &moving_provider || !index.symbol_retained(owner, retained)))
            })
            && (is_java_file(&bridge.target.file)
                || bridge.target == moving_provider
                || kotlin_method_default(index, &bridge.target, method_name))
    })
}

/// A Kotlin `fun getX()` already has method override semantics. Its provider
/// need not move first: a retained implementer can explicitly forward to that
/// method while parallel Kotlin `val` contracts still follow the property path.
fn kotlin_method_default(index: &SourceIndex, owner: &SymbolId, name: &str) -> bool {
    let Some(file) = index.source_file(&owner.file) else {
        return false;
    };
    file.language == SourceLanguage::Kotlin
        && file.declarations.iter().any(|declaration| {
            crate::semantics::workspace_symbol(index, declaration) == *owner
                && declaration.members.iter().any(|member| {
                    member.kind == crate::workspace::MemberKind::Method
                        && member.name == name
                        && member.parameter_types.is_empty()
                        && member.has_body
                        && !member.is_static
                        && member.visibility.as_deref() != Some("private")
                })
        })
}

fn is_java_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "java")
}

pub(crate) fn repair_virtual_sources(
    index: &SourceIndex,
    sources: &mut [(PathBuf, String)],
    excluded_files: &HashSet<PathBuf>,
) -> Result<PlannedAccessorRepairs, String> {
    let mut report = PlannedAccessorRepairs::default();
    for (path, source) in sources {
        let Some(file) = index.source_file(path) else {
            continue;
        };
        if file.language != SourceLanguage::Kotlin
            || excluded_files.contains(path)
            || excluded_files.contains(&file.path)
        {
            continue;
        }
        let mut candidates = Vec::new();
        for declaration in &file.declarations {
            if declaration.kind == DeclarationKind::Enum {
                let owner = crate::semantics::workspace_symbol(index, declaration);
                for bridge in index.retained_enum_default_method_bridges(path, &declaration.name) {
                    candidates.push((
                        owner.clone(),
                        RetainedGetterBridge {
                            method_name: bridge.method_name,
                            // Kotlin infers the selected super method's exact
                            // type, including platform nullability and generics.
                            return_type: None,
                            provider: bridge.provider,
                            kind: "enum-default-getter",
                        },
                    ));
                }
                continue;
            }
            if declaration.kind != DeclarationKind::Class {
                continue;
            }
            let Ok(bridges) = index.covariant_default_getter_bridges(path, &declaration.name)
            else {
                // The provider eligibility check retains Kotlin providers when
                // a required concrete accessor cannot be proved repairable.
                continue;
            };
            let owner = crate::semantics::workspace_symbol(index, declaration);
            for bridge in bridges {
                if !(is_java_file(&bridge.target.file)
                    || kotlin_method_default(index, &bridge.target, &bridge.method_name))
                    || !bridge
                        .property_contract_owners
                        .iter()
                        .all(|owner| is_java_file(&owner.file))
                {
                    continue;
                }
                let crate::semantics::KotlinType::Named { name, arguments } =
                    &bridge.return_type_ref.ty
                else {
                    return Err("accepted Kotlin getter bridge has a non-reference return".into());
                };
                if bridge.return_type_ref.nullable || !arguments.is_empty() {
                    return Err(
                        "accepted Kotlin getter bridge has an unsupported return contract".into(),
                    );
                }
                candidates.push((
                    owner.clone(),
                    RetainedGetterBridge {
                        method_name: bridge.method_name,
                        return_type: Some(name.clone()),
                        provider: qualified_name(&bridge.provider_owner),
                        kind: "covariant-default-getter",
                    },
                ));
            }
        }
        if candidates.is_empty() {
            continue;
        }
        if source != file.source_text() {
            return Err(format!(
                "stale source snapshot for retained getter repair: {}",
                crate::paths::display(path)
            ));
        }
        let tree = crate::transpiler::parse_tree(source);
        let mut edits = Vec::new();
        let mut matched = 0;
        let mut pending = vec![tree.root_node()];
        while let Some(node) = pending.pop() {
            if node.kind() != "class_declaration" {
                pending.extend(node.named_children(&mut node.walk()));
                continue;
            }
            let owner = crate::semantics::symbol_id_for_node(source, node, &file.path);
            let bridges = candidates
                .iter()
                .filter(|(candidate, _)| candidate == &owner)
                .collect::<Vec<_>>();
            if !bridges.is_empty() {
                let body = node
                    .named_children(&mut node.walk())
                    .find(|child| matches!(child.kind(), "class_body" | "enum_class_body"));
                let insertion = body.map_or(node.end_byte(), |body| body.end_byte() - 1);
                let mut replacement = if body.is_some() {
                    String::new()
                } else {
                    " {".to_owned()
                };
                if owner.kind == "enum"
                    && body.is_none_or(|body| {
                        !body
                            .children(&mut body.walk())
                            .any(|child| child.kind() == ";")
                    })
                {
                    replacement.push_str("\n    ;");
                }
                for (_, bridge) in bridges {
                    let provider = &bridge.provider;
                    let return_type = bridge
                        .return_type
                        .as_ref()
                        .map(|ty| format!(": {ty}"))
                        .unwrap_or_default();
                    replacement.push_str(&format!(
                        "\n    override fun {}(){} = super<{provider}>.{}()\n",
                        bridge.method_name, return_type, bridge.method_name
                    ));
                    record_bridge(&mut report, &owner, &bridge.method_name, bridge.kind);
                    matched += 1;
                }
                if body.is_none() {
                    replacement.push('}');
                }
                edits.push((insertion, replacement));
            }
            pending.extend(node.named_children(&mut node.walk()));
        }
        if matched != candidates.len() {
            return Err(format!(
                "cannot locate every accepted retained getter owner in {}",
                crate::paths::display(path)
            ));
        }
        edits.sort_by_key(|(offset, _)| *offset);
        for (offset, replacement) in edits.into_iter().rev() {
            source.insert_str(offset, &replacement);
        }
    }
    Ok(report)
}

struct RetainedGetterBridge {
    method_name: String,
    return_type: Option<String>,
    provider: String,
    kind: &'static str,
}

fn qualified_name(symbol: &SymbolId) -> String {
    std::iter::once(symbol.package.as_str())
        .filter(|part| !part.is_empty())
        .chain(symbol.owner_path.iter().map(String::as_str))
        .chain(std::iter::once(symbol.name.as_str()))
        .collect::<Vec<_>>()
        .join(".")
}

fn record_bridge(
    report: &mut PlannedAccessorRepairs,
    owner: &SymbolId,
    method_name: &str,
    repair_kind: &str,
) {
    let kind = format!("{repair_kind}:{method_name}");
    let id = SymbolId::generated(owner, &kind);
    report.bridges.push(PlannedBridge {
        id: id.clone(),
        origin: owner.clone(),
        kind,
    });
    let mut generated = owner.clone();
    generated.owner_path.push(owner.name.clone());
    generated.kind = "function".into();
    generated.name = method_name.to_owned();
    generated.parameters.clear();
    report.provenance.push(OriginMap {
        generated: id,
        origin: owner.clone(),
        reason: "stable identity of a retained default getter bridge".into(),
    });
    report.provenance.push(OriginMap {
        generated,
        origin: owner.clone(),
        reason: "preserve a concrete getter after its default provider moves to Java".into(),
    });
    report.repairs.push(PlannedRepair {
        target: owner.clone(),
        kind: repair_kind.into(),
        detail: format!("preserve concrete accessor `{method_name}` on retained Kotlin"),
    });
    report.count += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::SourceOverlay;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(child: &str) -> Self {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("test-temp")
                .join(format!(
                    "retained-getter-repair-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&root).unwrap();
            for (name, source) in [
                (
                    "Identifier.java",
                    "package sample;\npublic interface Identifier {}\n",
                ),
                (
                    "StoredId.java",
                    "package sample;\npublic class StoredId implements Identifier {}\n",
                ),
                (
                    "Wide.java",
                    "package sample;\npublic interface Wide { Identifier getOwner(); }\n",
                ),
                (
                    "Narrow.java",
                    "package sample;\npublic interface Narrow extends Wide { default StoredId getOwner() { return new StoredId(); } }\n",
                ),
                (
                    "Step.java",
                    "package sample;\npublic interface Step extends Narrow {}\n",
                ),
                (
                    "Output.java",
                    "package sample;\npublic interface Output extends Wide {}\n",
                ),
                ("Child.kt", child),
            ] {
                fs::write(root.join(name), source).unwrap();
            }
            Self(root)
        }

        fn indexed_sources(&self) -> (SourceIndex, Vec<(PathBuf, String)>) {
            let index = SourceIndex::discover(&self.0).unwrap();
            let file = index.source_file(&self.0.join("Child.kt")).unwrap();
            let sources = vec![(file.path.clone(), file.source_text().to_owned())];
            (index, sources)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            assert!(
                self.0
                    .starts_with(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"))
            );
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn retained_enums_select_the_first_default_and_add_a_missing_entry_separator() {
        for (parents, provider) in [("Step, Other", "Step"), ("Other, Step", "Other")] {
            let fixture = Fixture::new(&format!(
                "package sample\nenum class Child : {parents} {{ ONE }}\n"
            ));
            fs::write(fixture.0.join("Other.java"), "package sample; public interface Other { default StoredId getOwner() { return new StoredId(); } }\n").unwrap();
            let (index, mut sources) = fixture.indexed_sources();
            let report = repair_virtual_sources(&index, &mut sources, &HashSet::new()).unwrap();
            assert_eq!(report.count, 1);
            assert_eq!(report.repairs[0].kind, "enum-default-getter");
            assert!(
                sources[0].1.contains(&format!(
                    "override fun getOwner() = super<sample.{provider}>.getOwner()"
                )),
                "{}",
                sources[0].1
            );
            assert!(sources[0].1.contains(';'));
            assert!(
                !crate::transpiler::parse_tree(&sources[0].1)
                    .root_node()
                    .has_error()
            );
            let next = index
                .with_overlays(&[SourceOverlay::Replace {
                    path: sources[0].0.clone(),
                    language: SourceLanguage::Kotlin,
                    source: sources[0].1.clone(),
                }])
                .unwrap();
            assert_eq!(
                repair_virtual_sources(&next, &mut sources, &HashSet::new())
                    .unwrap()
                    .count,
                0
            );

            let mut property_source = vec![(
                fixture.0.join("Child.kt"),
                format!(
                    "package sample\ninterface PropertyRoot {{ val owner: StoredId }}\nenum class Child : {parents}, PropertyRoot {{ ONE }}\n"
                ),
            )];
            let property_index = index
                .with_overlays(&[SourceOverlay::Replace {
                    path: property_source[0].0.clone(),
                    language: SourceLanguage::Kotlin,
                    source: property_source[0].1.clone(),
                }])
                .unwrap();
            assert_eq!(
                repair_virtual_sources(&property_index, &mut property_source, &HashSet::new())
                    .unwrap()
                    .count,
                0
            );
        }
    }

    #[test]
    fn adds_one_explicit_bridge_with_or_without_a_class_body() {
        for child in [
            "package sample\nclass Child : Step, Output\n",
            "package sample\nclass Child : Step, Output {\n    suspend fun retainedOnly() {}\n}\n",
        ] {
            let fixture = Fixture::new(child);
            let (index, mut sources) = fixture.indexed_sources();
            let report = repair_virtual_sources(&index, &mut sources, &HashSet::new()).unwrap();
            assert_eq!(report.count, 1);
            assert!(sources[0].1.contains(
                "override fun getOwner(): sample.StoredId = super<sample.Step>.getOwner()"
            ));
            assert_eq!(report.bridges.len(), 1);
            assert_eq!(report.provenance.len(), 2);
            assert_eq!(report.repairs[0].kind, "covariant-default-getter");
            let next = index
                .with_overlays(&[SourceOverlay::Replace {
                    path: sources[0].0.clone(),
                    language: SourceLanguage::Kotlin,
                    source: sources[0].1.clone(),
                }])
                .unwrap();
            let previous = sources[0].1.clone();
            assert_eq!(
                repair_virtual_sources(&next, &mut sources, &HashSet::new())
                    .unwrap()
                    .count,
                0
            );
            assert_eq!(sources[0].1, previous);
        }
    }

    #[test]
    fn rejects_stale_input_and_preserves_vetoed_files() {
        let fixture = Fixture::new("package sample\nclass Child : Step, Output\n");
        let (index, mut sources) = fixture.indexed_sources();
        sources[0].1.push_str("// concurrent edit\n");
        let original = sources[0].1.clone();
        assert!(
            repair_virtual_sources(&index, &mut sources, &HashSet::new())
                .unwrap_err()
                .contains("stale source snapshot")
        );
        assert_eq!(sources[0].1, original);
        let excluded = HashSet::from([sources[0].0.clone()]);
        assert_eq!(
            repair_virtual_sources(&index, &mut sources, &excluded)
                .unwrap()
                .count,
            0
        );
        assert_eq!(sources[0].1, original);
    }

    #[test]
    fn provider_retains_when_kotlin_descendant_cannot_receive_a_source_bridge() {
        let fixture = Fixture::new(
            "package sample\nclass Child : Step, Output {\n    suspend fun retainedOnly() {}\n}\n",
        );
        fs::remove_file(fixture.0.join("Narrow.java")).unwrap();
        fs::write(fixture.0.join("Narrow.kt"), "package sample\ninterface Narrow : Wide {\n    override fun getOwner(): StoredId = StoredId()\n}\n").unwrap();
        let (index, _) = fixture.indexed_sources();
        let provider = index
            .declarations()
            .find(|declaration| declaration.name == "Narrow")
            .unwrap();
        let child = index
            .declarations()
            .find(|declaration| declaration.name == "Child")
            .unwrap();
        let retained = HashSet::from([crate::semantics::workspace_symbol(&index, child)]);
        let roots = std::slice::from_ref(&fixture.0);
        assert!(
            index
                .default_property_provider_retention_reason(provider, &retained, roots, false)
                .is_some()
        );
        assert!(
            index
                .default_property_provider_retention_reason(provider, &retained, roots, true)
                .is_none()
        );
        assert!(
            index
                .default_property_provider_retention_reason(provider, &retained, &[], true)
                .is_some()
        );
    }

    #[test]
    fn retained_direct_qualifier_can_inherit_a_moving_java_default() {
        let fixture = Fixture::new("package sample\nclass Child : Step, Output\n");
        fs::remove_file(fixture.0.join("Narrow.java")).unwrap();
        fs::remove_file(fixture.0.join("Step.java")).unwrap();
        fs::write(fixture.0.join("Narrow.kt"), "package sample\ninterface Narrow : Wide {\n    override fun getOwner(): StoredId = StoredId()\n}\n").unwrap();
        fs::write(
            fixture.0.join("Step.kt"),
            "package sample\ninterface Step : Narrow {}\n",
        )
        .unwrap();
        let (index, _) = fixture.indexed_sources();
        let provider = index.declarations_named("Narrow").next().unwrap();
        let child = index.declarations_named("Child").next().unwrap();
        let qualifier = index.declarations_named("Step").next().unwrap();
        let child_id = crate::semantics::workspace_symbol(&index, child);
        let provider_id = crate::semantics::workspace_symbol(&index, provider);
        let qualifier_id = crate::semantics::workspace_symbol(&index, qualifier);
        let roots = std::slice::from_ref(&fixture.0);
        let retained = HashSet::from([child_id.clone(), provider_id.clone()]);
        assert!(can_repair_contract(
            &index, child, provider, "getOwner", &retained, roots
        ));
        let retained = HashSet::from([child_id, provider_id, qualifier_id]);
        assert!(can_repair_contract(
            &index, child, provider, "getOwner", &retained, roots
        ));
    }

    #[test]
    fn retained_interface_qualifier_inherits_java_default_without_moving() {
        let fixture = Fixture::new("package sample\nclass Child : Step, Output\n");
        fs::remove_file(fixture.0.join("Step.java")).unwrap();
        fs::write(
            fixture.0.join("Step.kt"),
            "package sample\ninterface Step : Narrow {\n    suspend fun retainedOnly() {}\n}\n",
        )
        .unwrap();
        let (index, mut sources) = fixture.indexed_sources();
        let report = repair_virtual_sources(&index, &mut sources, &HashSet::new()).unwrap();
        assert_eq!(report.count, 1);
        assert!(
            sources[0].1.contains(
                "override fun getOwner(): sample.StoredId = super<sample.Step>.getOwner()"
            )
        );
    }

    #[test]
    fn kotlin_qualifier_can_forward_its_replacement_method_but_not_reabstract_it() {
        for getter in [
            "override fun getOwner(): StoredId",
            "override fun getOwner(): StoredId = StoredId()",
        ] {
            let fixture = Fixture::new("package sample\nclass Child : Step, Output\n");
            fs::remove_file(fixture.0.join("Step.java")).unwrap();
            fs::write(
                fixture.0.join("Step.kt"),
                format!("package sample\ninterface Step : Narrow {{\n    {getter}\n}}\n"),
            )
            .unwrap();
            let (index, mut sources) = fixture.indexed_sources();
            let original = sources.clone();
            let report = repair_virtual_sources(&index, &mut sources, &HashSet::new()).unwrap();
            if getter.contains('=') {
                assert_eq!(report.count, 1, "{getter}");
                assert!(
                    sources
                        .iter()
                        .any(|(_, source)| source.contains("super<sample.Step>.getOwner()")),
                    "{sources:?}"
                );
            } else {
                assert_eq!(report.count, 0, "{getter}");
                assert_eq!(sources, original, "{getter}");
            }
        }
    }

    #[test]
    fn nested_class_uses_its_enclosing_ownership_boundary() {
        let fixture =
            Fixture::new("package sample\nclass Outer {\n    class Child : Step, Output {}\n}\n");
        let (index, _) = fixture.indexed_sources();
        let outer = index.declarations_named("Outer").next().unwrap();
        let child = index.declarations_named("Child").next().unwrap();
        assert!(!index.declaration_retained(child, &HashSet::new()));
        let retained = HashSet::from([crate::semantics::workspace_symbol(&index, outer)]);
        assert!(index.declaration_retained(child, &retained));
    }
}
