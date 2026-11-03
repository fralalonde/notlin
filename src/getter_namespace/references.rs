//! Bind callable references before a source method receives its JVM-name alias.
use crate::semantics::{
    self, FactStatus, KotlinType, SemanticProvider, SymbolId, SyntaxSemanticProvider, TypeRef,
};
use crate::transpiler::kt;
use crate::workspace::{SourceFile, SourceIndex};
use std::collections::BTreeSet;
use tree_sitter::Node;

pub(super) fn selector<'t>(node: Node<'t>, source: &str) -> Option<Node<'t>> {
    if !matches!(
        node.kind(),
        "navigation_expression" | "callable_reference" | "callable_reference_expression"
    ) || !node
        .children(&mut node.walk())
        .any(|child| kt::text(child, source) == "::")
    {
        return None;
    }
    node.named_children(&mut node.walk())
        .filter(|child| !child.is_extra())
        .last()
        .filter(|child| child.kind() == "identifier" && kt::text(*child, source) != "class")
}

pub(super) fn resolve(
    index: &SourceIndex,
    file: &SourceFile,
    source: &str,
    reference: Node<'_>,
    name: &str,
    provider: &SyntaxSemanticProvider,
) -> Result<SymbolId, String> {
    let unknown = || {
        format!(
            "getter namespace cannot bind callable reference `{}` at {}:{}",
            kt::text(reference, source),
            file.path.display(),
            reference.start_position().row + 1
        )
    };
    let selector = selector(reference, source).ok_or_else(unknown)?;
    let receiver = reference
        .named_children(&mut reference.walk())
        .find(|child| !child.is_extra() && child.id() != selector.id());
    let (owner, unbound) = if let Some(receiver) = receiver {
        let lookup = crate::property_callsite::receiver_binding_at(index, file, source, receiver);
        if let Some(ty) = lookup.canonical_ty() {
            let owner = crate::workspace::property_receiver_type_binding(index, file, ty)
                .and_then(|binding| binding.declaration)
                .ok_or_else(unknown)?;
            (semantics::workspace_symbol(index, owner), false)
        } else {
            if !matches!(
                lookup.resolution,
                crate::receiver_binding::Resolution::Unbound
            ) || lookup.opaque_receiver
            {
                return Err(unknown());
            }
            let owner = crate::workspace::property_receiver_type_binding(
                index,
                file,
                kt::text(receiver, source),
            )
            .and_then(|binding| binding.declaration)
            .ok_or_else(unknown)?;
            (
                semantics::workspace_symbol(index, owner),
                owner.kind != crate::workspace::DeclarationKind::Object,
            )
        }
    } else {
        if crate::function_callsite::local_shadow(reference, name, source) {
            return Err(unknown());
        }
        let mut scope = reference.parent();
        let owner = loop {
            let node = scope.ok_or_else(unknown)?;
            if matches!(node.kind(), "lambda_literal" | "object_literal")
                || crate::receiver_binding::extension_receiver_scope(node)
            {
                return Err(unknown());
            }
            if matches!(node.kind(), "class_declaration" | "object_declaration") {
                break semantics::symbol_id_for_node(source, node, &file.path);
            }
            scope = node.parent();
        };
        (owner, false)
    };
    let mut ancestry = vec![owner];
    let mut seen = BTreeSet::new();
    let mut methods = Vec::new();
    while let Some(owner) = ancestry.pop() {
        if !seen.insert(owner.clone()) {
            continue;
        }
        if !provider.has_complete_inheritance(&owner) {
            return Err(unknown());
        }
        let mut path = owner.owner_path.clone();
        path.push(owner.name.clone());
        for symbol in provider.symbols_named(name) {
            if symbol.id.file != owner.file || symbol.id.owner_path != path {
                continue;
            }
            if symbol.id.kind == "property" {
                return Err(unknown());
            }
            if matches!(symbol.id.kind.as_str(), "function" | "method")
                && symbol.id.receiver.is_none()
                && !provider.is_java_static_member(&symbol.id)
            {
                methods.push((owner.clone(), symbol.id.clone()));
            }
        }
        let FactStatus::Established(parents) = provider.instance_contract_parents(&owner) else {
            return Err(unknown());
        };
        ancestry.extend(parents);
    }
    // Direct declarations shadow only the same signature. Other inherited
    // overloads remain eligible, including getX(Class<T>) beside getX().
    let visible = methods
        .iter()
        .filter(|(owner, method)| {
            !methods.iter().any(|(other_owner, other)| {
                owner != other_owner
                    && method.parameters == other.parameters
                    && provider.inherits_type(other_owner, owner)
            })
        })
        .map(|(_, method)| method.clone())
        .collect::<BTreeSet<_>>();
    let expected =
        expected_arity(reference, source).and_then(|arity| arity.checked_sub(usize::from(unbound)));
    let selected = visible
        .into_iter()
        .filter(|method| expected.is_none_or(|arity| method.parameters.len() == arity))
        .collect::<Vec<_>>();
    match selected.as_slice() {
        [method] => Ok(method.clone()),
        _ => Err(unknown()),
    }
}

fn expected_arity(reference: Node<'_>, source: &str) -> Option<usize> {
    let mut parent = reference.parent();
    while let Some(node) = parent {
        match node.kind() {
            "parenthesized_expression" | "function_body" => (),
            "property_declaration" => {
                let variable = kt::child(node, "variable_declaration")?;
                return function_arity(kt::child(variable, "function_type")?, source);
            }
            "function_declaration" => {
                return function_arity(kt::child(node, "function_type")?, source);
            }
            _ => return None,
        }
        parent = node.parent();
    }
    None
}

fn function_arity(ty: Node<'_>, source: &str) -> Option<usize> {
    match TypeRef::parse(kt::text(ty, source)).ty {
        KotlinType::Function {
            receiver,
            parameters,
            ..
        } => Some(parameters.len() + usize::from(receiver.is_some())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn bound_reference(source: &str, wanted: &str) -> Result<SymbolId, String> {
        let root = std::env::temp_dir().join(format!(
            "notlin-reference-binding-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(!root.exists());
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("Scope.kt");
        std::fs::write(&path, source).unwrap();
        let index = SourceIndex::discover(&root).unwrap();
        let file = index.kotlin_files().next().unwrap();
        let mut provider =
            SyntaxSemanticProvider::new(vec![(file.path.clone(), source.to_owned())]);
        provider.enrich_workspace_type_facts(&index);
        let tree = crate::transpiler::parse_tree(source);
        let mut nodes = vec![tree.root_node()];
        let node = loop {
            let node = nodes.pop().expect("reference exists");
            if selector(node, source).is_some() && kt::text(node, source) == wanted {
                break node;
            }
            nodes.extend(node.named_children(&mut node.walk()));
        };
        let result = resolve(&index, file, source, node, "getId", &provider);
        assert!(root.is_absolute() && root.starts_with(std::env::temp_dir()));
        std::fs::remove_dir_all(root).unwrap();
        result
    }

    const BASE: &str = "package sample\ninterface Root {val id:Any}\nopen class Base(override val id:Any):Root\nclass Child:Base(\"stored\") {fun getId():String=\"lookup\"}\nclass Other {fun getId():String=\"other\"}\n";

    #[test]
    fn aliases_and_uppercase_value_binders_keep_distinct_owners() {
        let aliased = BASE.replacen(
            "package sample\n",
            "package sample\nimport sample.Child as Named\n",
            1,
        ) + "class Use {fun alias(child:Named) {val bound:()->String=child::getId;val unbound:(Named)->String=Named::getId}}\n";
        for reference in ["child::getId", "Named::getId"] {
            assert_eq!(
                bound_reference(&aliased, reference).unwrap().owner_path,
                ["Child"]
            );
        }
        let shadow = BASE.to_owned()
            + "class Use {fun read(Child:Other) {val ref:()->String=Child::getId}}\n";
        assert_eq!(
            bound_reference(&shadow, "Child::getId").unwrap().owner_path,
            ["Other"]
        );
    }

    #[test]
    fn an_unproved_value_shadow_cannot_fall_back_to_a_classifier() {
        let source = BASE.to_owned()
            + "class Use {val Child get()=Other();fun ref():()->String=Child::getId}\n";
        assert!(bound_reference(&source, "Child::getId").is_err());
    }

    #[test]
    fn singleton_references_are_bound_when_selecting_overload_arity() {
        let source = BASE.to_owned()
            + "object Singleton {fun getId():String=\"zero\";fun getId(value:String):String=value}\nclass Use {fun ref() {val call:(String)->String=Singleton::getId}}\n";
        let target = bound_reference(&source, "Singleton::getId").unwrap();
        assert_eq!(target.owner_path, ["Singleton"]);
        assert_eq!(target.parameters.len(), 1);
    }
}
