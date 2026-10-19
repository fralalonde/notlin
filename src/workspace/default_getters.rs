use super::{
    Declaration, DeclarationKind, Member, MemberKind, SourceFile, SourceIndex, SourceLanguage,
    getter_is_public, is_physical_zero_arg_getter, property_accessor_name,
};
use crate::semantics::{self, SymbolId};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// A concrete implementation needed to preserve a covariant inherited default
/// getter when Java resolves a wider abstract declaration from another branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CovariantDefaultGetterBridge {
    pub owner: SymbolId,
    /// The direct interface qualifier used by `Provider.super.getX()`.
    pub provider_owner: SymbolId,
    /// The interface that actually declares the selected effective default.
    pub target: SymbolId,
    pub method_name: String,
    /// The exact source FQName of the selected default's reference return.
    pub return_type: String,
}

#[derive(Clone, Copy)]
struct Getter<'a> {
    owner: &'a Declaration,
    member: &'a Member,
    return_type: &'a str,
}

fn source_getter<'a>(owner: &'a Declaration, member: &'a Member) -> Option<Getter<'a>> {
    if member.is_static
        || member.visibility.as_deref() == Some("private")
        || !getter_is_public(member, owner)
    {
        return None;
    }
    let method_name = if member.kind == MemberKind::Property {
        property_accessor_name(&member.name)
    } else {
        member.name.clone()
    };
    match member.kind {
        MemberKind::Property => {
            let ty = member.type_name.as_deref()?.trim();
            if ty.is_empty() || matches!(ty, "Unit" | "kotlin.Unit" | "void") {
                return None;
            }
        }
        MemberKind::Method if !is_physical_zero_arg_getter(member, &method_name) => return None,
        MemberKind::Method => {}
        _ => return None,
    }
    Some(Getter {
        owner,
        member,
        return_type: member.type_name.as_deref()?.trim(),
    })
}

fn external_root(name: &str) -> bool {
    matches!(
        name.trim().trim_end_matches("()"),
        "Any" | "kotlin.Any" | "Object" | "java.lang.Object"
    )
}

fn known_methodless_external(file: &SourceFile, source_type: &str) -> bool {
    const MARKERS: [&str; 2] = ["java.io.Serializable", "java.lang.Cloneable"];

    let base = raw_base_type(source_type);
    if MARKERS.contains(&base) {
        return true;
    }

    file.imports.iter().any(|import| {
        let mut parts = import.split_whitespace();
        let Some(path) = parts.next() else {
            return false;
        };
        let alias = match (parts.next(), parts.next()) {
            (Some("as"), Some(alias)) => Some(alias),
            _ => None,
        };
        MARKERS.contains(&path)
            && (base == alias.unwrap_or_else(|| path.rsplit('.').next().unwrap_or(path))
                || base == path)
    })
}

fn raw_base_type(name: &str) -> &str {
    name.trim()
        .trim_end_matches("()")
        .split('<')
        .next()
        .unwrap_or(name)
        .trim()
}

impl SourceIndex {
    /// Find concrete covariant getter bridges for a source class. Unknown
    /// relevant inheritance returns `Err`, allowing planning to retain the
    /// class rather than emit a guessed JVM contract.
    pub(crate) fn covariant_default_getter_bridges(
        &self,
        declaring: &Path,
        name: &str,
    ) -> Result<Vec<CovariantDefaultGetterBridge>, String> {
        let file = self
            .source_file(declaring)
            .ok_or_else(|| "declaring source file is not indexed".to_string())?;
        let owner = file
            .declarations
            .iter()
            .find(|declaration| declaration.name == name)
            .ok_or_else(|| {
                let indexed = file
                    .declarations
                    .iter()
                    .map(|declaration| declaration.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("declaration `{name}` is not indexed in this file; indexed: {indexed}")
            })?;
        if owner.kind != DeclarationKind::Class {
            return Ok(Vec::new());
        }

        fn visit<'a>(
            index: &'a SourceIndex,
            context: &'a SourceFile,
            declaration: &'a Declaration,
            active: &mut HashSet<SymbolId>,
            visited: &mut HashSet<SymbolId>,
            hierarchy: &mut Vec<&'a Declaration>,
        ) -> bool {
            if hierarchy.len() >= 256 {
                return true;
            }
            let key = semantics::workspace_symbol(index, declaration);
            if active.contains(&key) {
                return true;
            }
            if !visited.insert(key.clone()) {
                return false;
            }
            active.insert(key.clone());
            hierarchy.push(declaration);
            let declaration_file = index
                .declaration_source_file(declaration)
                .unwrap_or(context);
            let mut incomplete = false;
            for supertype in &declaration.supertypes {
                if external_root(supertype) {
                    continue;
                }
                let Some(parent) = index.resolve_type(declaration_file, supertype) else {
                    if known_methodless_external(declaration_file, supertype) {
                        continue;
                    }
                    incomplete = true;
                    continue;
                };
                incomplete |= visit(index, declaration_file, parent, active, visited, hierarchy);
            }
            active.remove(&key);
            incomplete
        }

        let mut hierarchy = Vec::new();
        let mut visited = HashSet::new();
        let mut active = HashSet::new();
        let mut incomplete_hierarchy = false;
        for supertype in &owner.supertypes {
            if external_root(supertype) {
                continue;
            }
            let Some(parent) = self.resolve_type(file, supertype) else {
                if known_methodless_external(file, supertype) {
                    continue;
                }
                incomplete_hierarchy = true;
                continue;
            };
            incomplete_hierarchy |= visit(
                self,
                file,
                parent,
                &mut active,
                &mut visited,
                &mut hierarchy,
            );
        }

        // A declaration-level getter already decides the concrete class ABI;
        // this pass only fills the inherited-default gap.
        let own_getters = owner
            .members
            .iter()
            .filter_map(|member| source_getter(owner, member))
            .map(|getter| {
                if getter.member.kind == MemberKind::Property {
                    property_accessor_name(&getter.member.name)
                } else {
                    getter.member.name.clone()
                }
            })
            .collect::<HashSet<_>>();

        let mut names = BTreeMap::<String, Vec<Getter<'_>>>::new();
        for ancestor in &hierarchy {
            for member in &ancestor.members {
                if let Some(getter) = source_getter(ancestor, member) {
                    let method_name = if member.kind == MemberKind::Property {
                        property_accessor_name(&member.name)
                    } else {
                        member.name.clone()
                    };
                    if !own_getters.contains(&method_name) {
                        names.entry(method_name).or_default().push(getter);
                    }
                }
            }
        }

        let mut bridges = Vec::new();
        for (method_name, contracts) in names {
            if contracts.len() < 2
                || !contracts.iter().any(|contract| {
                    contract.owner.kind == DeclarationKind::Interface && contract.member.has_body
                })
                || own_getters.contains(&method_name)
            {
                continue;
            }
            let direct_interfaces = owner
                .supertypes
                .iter()
                .filter_map(|supertype| {
                    let provider = self.resolve_type(file, supertype)?;
                    (provider.kind == DeclarationKind::Interface).then_some((supertype, provider))
                })
                .filter(|(provider_type, provider)| {
                    !owner
                        .supertypes
                        .iter()
                        .filter_map(|supertype| {
                            let other = self.resolve_type(file, supertype)?;
                            (other.kind == DeclarationKind::Interface).then_some((supertype, other))
                        })
                        .any(|other| {
                            let (other_type, other) = other;
                            !provider_type.contains('<')
                                && !other_type.contains('<')
                                && provider.type_params.is_empty()
                                && other.type_params.is_empty()
                                && semantics::workspace_symbol(self, other)
                                    != semantics::workspace_symbol(self, provider)
                                && super::proven_reference_subtype(
                                    self,
                                    self.declaration_source_file(other).unwrap_or(file),
                                    &super::declaration_key(other),
                                    self.declaration_source_file(provider).unwrap_or(file),
                                    &super::declaration_key(provider),
                                )
                        })
                })
                .collect::<Vec<_>>();
            let mut defaults = Vec::new();
            let mut blocked_default = false;
            for (direct_type, provider) in direct_interfaces {
                let Some((target, member)) =
                    self.effective_default_getter(provider, file, &method_name)
                else {
                    blocked_default |= provider.members.iter().any(|member| {
                        source_getter(provider, member).is_some_and(|getter| {
                            if getter.member.kind == MemberKind::Property {
                                property_accessor_name(&getter.member.name) == method_name
                            } else {
                                getter.member.name == method_name
                            }
                        })
                    });
                    continue;
                };
                if member.is_nullable || !member.parameter_types.is_empty() {
                    blocked_default = true;
                    continue;
                }
                let Some(return_type) = member.type_name.as_deref().map(str::trim) else {
                    blocked_default = true;
                    continue;
                };
                if super::declaration_type_parameter_shadows(target, return_type) {
                    blocked_default = true;
                    continue;
                }
                let Some(target_file) = self.declaration_source_file(target) else {
                    blocked_default = true;
                    continue;
                };
                let Some(return_decl) =
                    super::exact_source_reference_type(self, target_file, return_type)
                else {
                    blocked_default = true;
                    continue;
                };
                if return_decl.is_jvm_value_class || !return_decl.type_params.is_empty() {
                    blocked_default = true;
                    continue;
                }
                defaults.push((direct_type, provider, target, member, return_decl));
            }
            if defaults.is_empty() {
                let distinct_returns = contracts
                    .iter()
                    .map(|contract| contract.return_type)
                    .collect::<HashSet<_>>();
                if blocked_default && distinct_returns.len() > 1 {
                    return Err(format!(
                        "cannot prove inherited default getter `{method_name}`"
                    ));
                }
                continue;
            }

            // Collapse repeated routes to one inherited default, then require
            // a single most-specific provider across all direct branches.
            defaults.sort_by_key(|(_, provider, target, member, _)| {
                (
                    provider.language != SourceLanguage::Java
                        || target.language != SourceLanguage::Java,
                    semantics::workspace_symbol(self, target),
                    member.name.clone(),
                )
            });
            defaults.dedup_by(|left, right| {
                semantics::workspace_symbol(self, left.2)
                    == semantics::workspace_symbol(self, right.2)
                    && left.3.name == right.3.name
            });
            let most_specific = defaults
                .iter()
                .enumerate()
                .filter(|(_, (_, _, target, _, _))| {
                    !defaults.iter().any(|(_, _, other, _, _)| {
                        semantics::workspace_symbol(self, other)
                            != semantics::workspace_symbol(self, target)
                            && super::proven_reference_subtype(
                                self,
                                self.declaration_source_file(other).unwrap_or(file),
                                &super::declaration_key(other),
                                self.declaration_source_file(target).unwrap_or(file),
                                &super::declaration_key(target),
                            )
                    })
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            if most_specific.len() != 1 {
                if !contracts.is_empty() {
                    return Err(format!(
                        "ambiguous inherited default getter `{method_name}`"
                    ));
                }
                continue;
            }
            let (_direct_type, provider, target, member, return_decl) = defaults[most_specific[0]];

            let mut has_wider_contract = false;
            let mut compatible = true;
            for contract in &contracts {
                if semantics::workspace_symbol(self, contract.owner)
                    == semantics::workspace_symbol(self, target)
                {
                    continue;
                }
                // A class member outranks interface defaults in Java method
                // resolution. Do not synthesize an interface-super bridge
                // that would override an inherited superclass method.
                if contract.owner.kind != DeclarationKind::Interface {
                    return Err(format!(
                        "superclass getter `{method_name}` takes precedence over interface defaults"
                    ));
                }
                let same_return = self
                    .declaration_source_file(contract.owner)
                    .and_then(|contract_file| {
                        super::exact_source_reference_type(
                            self,
                            contract_file,
                            contract.return_type,
                        )
                    })
                    .is_some_and(|contract_type| {
                        semantics::workspace_symbol(self, contract_type)
                            == semantics::workspace_symbol(self, return_decl)
                    });
                if same_return {
                    continue;
                }
                has_wider_contract = true;
                if !self.default_getter_has_proven_covariant_reference_return(
                    target,
                    member,
                    member.type_name.as_deref().unwrap_or(""),
                    contract.owner,
                    contract.member,
                    contract.return_type,
                ) {
                    compatible = false;
                    break;
                }
            }
            if !has_wider_contract {
                continue;
            }
            if !compatible {
                return Err(format!(
                    "cannot prove covariant inherited getter `{method_name}`"
                ));
            }
            if incomplete_hierarchy {
                return Err(format!(
                    "incomplete hierarchy prevents proving getter `{method_name}`"
                ));
            }

            bridges.push(CovariantDefaultGetterBridge {
                owner: semantics::workspace_symbol(self, owner),
                provider_owner: semantics::workspace_symbol(self, provider),
                target: semantics::workspace_symbol(self, target),
                method_name,
                return_type: super::declaration_key(return_decl),
            });
        }
        Ok(bridges)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn abstract_covariant_contracts_do_not_require_a_default_bridge() {
        let (root, path, index) = index_for(
            "package sample\nclass Code {}\ninterface Wide {\n fun getCode(): Any\n}\ninterface Narrow : Wide {\n override fun getCode(): Code\n}\nabstract class Child : Narrow, Wide {}\n",
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .unwrap()
                .is_empty()
        );
        let _ = fs::remove_dir_all(root);
    }

    fn index_for(source: &str) -> (PathBuf, PathBuf, SourceIndex) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "covariant-default-getter-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("Model.kt");
        fs::write(&path, source).unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        (root, path, index)
    }

    #[test]
    fn finds_one_covariant_default_across_a_diamond() {
        let (root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            interface Wide {
                fun getCode(): Any
            }
            interface Root {
                fun getCode(): Code = Code()
            }
            interface Left : Root {}
            interface Right : Root {}
            class Child : Left, Right, Wide {}
            "#,
        );
        let bridges = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(bridges.len(), 1);
        assert_eq!(bridges[0].method_name, "getCode");
        assert_eq!(bridges[0].return_type, "sample.Code");
        assert_eq!(bridges[0].target.name, "Root");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn same_return_and_explicit_class_override_need_no_bridge() {
        let (root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            interface Wide {
                fun getCode(): Code
            }
            interface Narrow : Wide {
                override fun getCode(): Code = Code()
            }
            class Inherited : Narrow, Wide {}
            class Explicit : Narrow, Wide {
                override fun getCode(): Code = Code()
            }
            "#,
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Inherited")
                .unwrap()
                .is_empty()
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Explicit")
                .unwrap()
                .is_empty()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn generic_and_aliased_default_returns_stay_unproven() {
        let (root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            typealias CodeAlias = Code
            interface Wide {
                fun getCode(): Any
            }
            interface Generic<T> : Wide {
                override fun getCode(): T = TODO()
            }
            interface Aliased : Wide {
                override fun getCode(): CodeAlias = Code()
            }
            class GenericChild : Generic<Code>, Wide {}
            class AliasChild : Aliased, Wide {}
            "#,
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "GenericChild")
                .is_err()
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "AliasChild")
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn incomplete_hierarchy_stays_unproven_only_when_a_bridge_is_needed() {
        let (root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): Code = Code()
            }
            class Child : Narrow, Wide, MissingContract {}
            class NoGetter : MissingContract {}
            "#,
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .is_err()
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "NoGetter")
                .unwrap()
                .is_empty()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn inherited_class_getter_prevents_interface_default_bridge() {
        let (root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            open class Base {
                fun getCode(): Code = Code()
            }
            interface Wide {
                fun getCode(): Any
            }
            interface Narrow : Wide {
                override fun getCode(): Code = Code()
            }
            class Child : Base(), Narrow, Wide {}
            "#,
        );
        assert!(
            index
                .covariant_default_getter_bridges(&path, "Child")
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn direct_child_interface_is_used_instead_of_redundant_root_super() {
        let (root, path, index) = index_for(
            r#"
            package sample
            class Code {}
            interface Wide {
                fun getCode(): Any
            }
            interface Root {
                fun getCode(): Code = Code()
            }
            interface ChildRoot : Root {}
            class Child : Root, ChildRoot, Wide {}
            "#,
        );
        let bridges = index
            .covariant_default_getter_bridges(&path, "Child")
            .unwrap();
        assert_eq!(bridges.len(), 1);
        assert_eq!(bridges[0].provider_owner.name, "ChildRoot");
        assert_eq!(bridges[0].target.name, "Root");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn imported_methodless_marker_is_skipped_but_unknown_import_is_not() {
        let (root, path, index) = index_for(
            r#"
            package sample
            import java.io.Serializable
            import java.lang.Cloneable as PlatformCloneable
            import external.contract.UnknownBase

            class ModelId {}
            interface Root : Serializable, PlatformCloneable {
                fun getIdentifier(): Any
            }
            interface Narrow : Root {
                override fun getIdentifier(): ModelId = ModelId()
            }
            interface Wide {
                fun getIdentifier(): Any
            }
            class MarkerChild : Narrow, Wide {}
            class UnknownChild : Narrow, Wide, UnknownBase {}
            "#,
        );
        let bridges = index
            .covariant_default_getter_bridges(&path, "MarkerChild")
            .unwrap();
        assert_eq!(bridges.len(), 1);
        assert_eq!(bridges[0].return_type, "sample.ModelId");
        assert!(
            index
                .covariant_default_getter_bridges(&path, "UnknownChild")
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }
}
