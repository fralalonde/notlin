//! Null propagation belongs to the receiver chain, outside member lowering.
//! Keep all arguments and later receiver hops inside the guarded branch.

use super::Expr;
use crate::semantics::FactStatus;
use crate::transpiler::kt;
use tree_sitter::Node;

impl Expr<'_, '_, '_> {
    /// Guard this selector, including its call arguments. A receiver's own
    /// safe calls are lowered recursively; an ordinary dot stays outside that
    /// guard (nullable-receiver extensions such as toString still run on null).
    pub(crate) fn safe_call_receiver<'n>(&self, node: Node<'n>) -> Option<(Node<'n>, Node<'n>)> {
        if self
            .unit
            .safe_receiver_aliases
            .contains_key(&(node.start_byte(), node.end_byte()))
        {
            return None;
        }
        match node.kind() {
            "call_expression" => self.safe_call_receiver(node.named_child(0)?),
            "assignment" => self.safe_call_receiver(kt::field(node, "left")?),
            "navigation_expression" => {
                let receiver = node.named_child(0)?;
                (!self
                    .unit
                    .guarded_navigations
                    .contains(&(node.start_byte(), node.end_byte()))
                    && node
                        .children(&mut node.walk())
                        .any(|child| child.kind() == "?."))
                .then_some((node, receiver))
            }
            _ => None,
        }
    }

    pub(crate) fn fresh_safe_receiver(&mut self) -> String {
        loop {
            let name = format!("__notlinSafe{}", self.unit.next_safe_receiver);
            self.unit.next_safe_receiver += 1;
            if !self.unit.source.contains(&name) {
                return name;
            }
        }
    }

    pub(crate) fn safe_call_expression(&mut self, node: Node<'_>) -> Option<String> {
        if !matches!(node.kind(), "call_expression" | "navigation_expression") {
            return None;
        }
        let (navigation, receiver) = self.safe_call_receiver(node)?;
        let receiver_java = self.transpile(receiver);
        let stable_local = receiver.kind() == "identifier"
            && receiver_java == self.unit.text(receiver).trim()
            && self.unit.var_types.contains_key(receiver_java.as_str());
        let (condition, alias) = if stable_local {
            (format!("{receiver_java} != null"), receiver_java)
        } else {
            // A pattern variable evaluates an effectful producer exactly once,
            // including in a field initializer or constructor delegation, where
            // an extra Java local declaration cannot be inserted. No lambda
            // captures or eager argument evaluation are introduced.
            let name = self.fresh_safe_receiver();
            let ty = self.safe_receiver_java_type(receiver);
            let alias = format!("(({ty}) {name})");
            (
                format!("(java.lang.Object) ({receiver_java}) instanceof java.lang.Object {name}"),
                alias,
            )
        };
        let nav_key = (navigation.start_byte(), navigation.end_byte());
        let receiver_key = (receiver.start_byte(), receiver.end_byte());
        self.unit.guarded_navigations.insert(nav_key);
        let previous = self.unit.safe_receiver_aliases.insert(receiver_key, alias);
        let mut branch = self.transpile(node);
        if node.kind() == "call_expression" && self.unit.semantic_provider.is_some_and(|provider| {
            let Some(callee) = crate::semantics::call_callee_node(node) else { return false };
            let name = self.unit.text(callee);
            let crate::semantics::FactStatus::Established(id) = crate::semantics::resolve_required_call(self.unit.source, self.unit.workspace_file.as_deref().unwrap_or(self.unit.file), node, callee, name, provider) else { return false };
            provider.symbols_named(name).iter().any(|symbol| symbol.id == id && matches!(&symbol.ty, crate::semantics::FactStatus::Established(ty) if matches!(&ty.ty, crate::semantics::KotlinType::Named { name, .. } if matches!(name.as_str(), "Unit" | "kotlin.Unit" | "void"))))
        }) {
            branch = format!("switch (0) {{ default -> {{ {branch}; yield kotlin.Unit.INSTANCE; }} }}");
        }
        self.unit.guarded_navigations.remove(&nav_key);
        self.unit.safe_receiver_aliases.remove(&receiver_key);
        if let Some(previous) = previous {
            self.unit
                .safe_receiver_aliases
                .insert(receiver_key, previous);
        }
        Some(format!("({condition} ? {branch} : null)"))
    }

    pub(crate) fn safe_receiver_java_type(&mut self, receiver: Node<'_>) -> String {
        let source = self.unit.source;
        let file = self
            .unit
            .workspace_file
            .as_deref()
            .unwrap_or(self.unit.file)
            .to_path_buf();
        if receiver.kind() == "navigation_expression"
            && let Some((base, member)) = self.unit.nav_base_member(receiver)
        {
            let base_type = self.safe_receiver_java_type(base);
            if let Some(workspace) = self.unit.workspace
                && let Some(source_file) = workspace.source_file(&file)
                && let Some(owner) = workspace.resolve_type(source_file, &base_type)
                && let Some(ty) = owner.members.iter().find_map(|candidate| {
                    ((candidate.name == member
                        && matches!(
                            candidate.kind,
                            crate::workspace::MemberKind::Property
                                | crate::workspace::MemberKind::Field
                        ))
                        || (candidate.name == crate::workspace::property_accessor_name(&member)
                            && candidate.kind == crate::workspace::MemberKind::Method))
                        .then_some(candidate.type_name.as_deref())
                        .flatten()
                })
            {
                if let Some(provider) = self.unit.semantic_provider
                    && let Some(producer) = workspace.declaration_source_file(owner)
                {
                    let mut ty = if owner.language == crate::workspace::SourceLanguage::Java {
                        crate::semantics::TypeRef::from_java_name(ty)
                    } else {
                        crate::semantics::TypeRef::parse(ty)
                    };
                    qualify_receiver_type(
                        &mut ty.ty,
                        &producer.path,
                        owner.package.as_deref().unwrap_or(""),
                        provider,
                    );
                    return java_type_spelling(&kotlin_type_text(&ty.ty, ty.nullable));
                }
                return java_type_spelling(ty);
            }
        }
        if let Some(provider) = self.unit.semantic_provider {
            let symbol = match receiver.kind() {
                "call_expression" => {
                    crate::semantics::call_callee_node(receiver).and_then(|callee| {
                        let name = callee.utf8_text(source.as_bytes()).ok()?;
                        match crate::semantics::resolve_required_call(
                            source, &file, receiver, callee, name, provider,
                        ) {
                            FactStatus::Established(id) => provider
                                .symbols_named(name)
                                .into_iter()
                                .find(|symbol| symbol.id == id),
                            _ => None,
                        }
                    })
                }
                _ => None,
            };
            if let Some(symbol) = symbol
                && let FactStatus::Established(ty) = &symbol.ty
            {
                // Reuse the ordinary type emitter, including generics, arrays
                // and nullable primitive boxing.
                let mut ty = ty.clone();
                qualify_receiver_type(&mut ty.ty, &symbol.id.file, &symbol.id.package, provider);
                return java_type_spelling(&kotlin_type_text(&ty.ty, ty.nullable));
            }
        }
        self.unit
            .infer_type(receiver)
            .replace("@Nullable ", "")
            .replace("@NotNull ", "")
    }
}

fn java_type_spelling(ty: &str) -> String {
    let text = format!("val __receiver: {ty}");
    let tree = crate::transpiler::parse_tree(&text);
    tree.root_node()
        .named_child(0)
        .and_then(|property| kt::child(property, "variable_declaration"))
        .and_then(|variable| {
            variable
                .named_children(&mut variable.walk())
                .find(|node| matches!(node.kind(), "user_type" | "nullable_type"))
        })
        .map(|node| kt::java_type(node, &text))
        .unwrap_or_else(|| "Object".to_string())
}

fn kotlin_type_text(ty: &crate::semantics::KotlinType, nullable: bool) -> String {
    use crate::semantics::{KotlinType, Variance};
    let core = match ty {
        KotlinType::Primitive(name)
        | KotlinType::TypeParameter(name)
        | KotlinType::Unknown(name) => name.clone(),
        KotlinType::Named { name, arguments } => {
            if arguments.is_empty() {
                name.clone()
            } else {
                let args = arguments
                    .iter()
                    .map(|arg| {
                        let Some(ty) = &arg.ty else {
                            return "*".to_string();
                        };
                        let variance = match arg.variance {
                            Variance::Invariant => "",
                            Variance::In => "in ",
                            Variance::Out => "out ",
                        };
                        format!("{variance}{}", kotlin_type_text(ty, false))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{name}<{args}>")
            }
        }
        KotlinType::Array(inner) => format!("Array<{}>", kotlin_type_text(inner, false)),
        KotlinType::Nullable(inner) => return kotlin_type_text(inner, true),
        KotlinType::Function { .. } => "Object".to_string(),
    };
    format!("{core}{}", if nullable { "?" } else { "" })
}

fn qualify_receiver_type(
    ty: &mut crate::semantics::KotlinType,
    file: &std::path::Path,
    package: &str,
    provider: &dyn crate::semantics::SemanticProvider,
) {
    use crate::semantics::KotlinType;
    match ty {
        KotlinType::Named { name, arguments } => {
            if let FactStatus::Established(id) = provider.resolve_type_in_file(file, package, name)
            {
                let mut parts = vec![id.package];
                parts.extend(id.owner_path);
                parts.push(id.name);
                *name = parts
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(".");
            } else if let Some(source) = provider.source_for_file(file)
                && let Some(canonical) = crate::semantics::standard_external_type(
                    source,
                    provider,
                    name,
                    file.extension()
                        .is_some_and(|extension| extension == "java"),
                )
            {
                *name = match canonical.as_str() {
                    "kotlin.collections.List" | "kotlin.collections.MutableList" => {
                        "java.util.List".to_string()
                    }
                    "kotlin.collections.Set" | "kotlin.collections.MutableSet" => {
                        "java.util.Set".to_string()
                    }
                    "kotlin.collections.Map" | "kotlin.collections.MutableMap" => {
                        "java.util.Map".to_string()
                    }
                    _ => canonical,
                };
            }
            for argument in arguments {
                if let Some(ty) = &mut argument.ty {
                    qualify_receiver_type(ty, file, package, provider);
                }
            }
        }
        KotlinType::Nullable(inner) | KotlinType::Array(inner) => {
            qualify_receiver_type(inner, file, package, provider)
        }
        _ => {}
    }
}
